//! Background half of the long-capture tick (P1-01).
//!
//! The GPUI main thread may only touch the D3D11 device/context, the `Window`
//! and GPUI state, so the capture and the staging-map readback stay there. The
//! D3D11 immediate context is single-threaded and `cx.notify` is UI-only, so
//! neither can move. Everything after the readback can: normalize (the 90°
//! rotation a horizontal session needs), grayscale, SAD alignment, stitching
//! and the per-tick `diag.log` line all run here, on one long-lived worker that
//! owns `prev_gray`, the `Stitcher` and the `StallTracker`. The UI thread sends
//! one [`Job`] at a time and applies the matching [`Done`] when it comes back,
//! so no frame-sized work happens between a capture and its verdict on the main
//! thread.
//!
//! Ownership is what makes that safe: the UI thread never holds the stitcher
//! buffer across ticks, and a session that ends drops its [`LongWorker`], which
//! cancels and (when it can) joins the thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::longcapture::{
    bgra_to_gray, classify_scroll_diag, normalize_bgra, rotate_bgra270, slide_stitch_rows, Axis,
    ScrollReport, StallPhase, StallTracker, StitchError, StitchStats, Stitcher, ACCEPT_SCORE,
    MOTION_CHANGED,
};

/// Sampled pixel-change fraction above which a rejected tick is reported as
/// "the page moved faster than the overlap" instead of silence.
pub const FAST_SCROLL_CHANGED: f64 = 0.45;

/// Rows of the frame edge used as the alignment strip, same as the old
/// UI-thread tick: a quarter of the height, capped at 32 rows.
const STRIP_MAX: usize = 32;

/// How long a `finish` hand-off may wait for an in-flight frame. Finishing is
/// a terminal, user-initiated action and the wait is bounded by one stitch of
/// the already-captured buffer -- the same work the UI thread used to do
/// inline, so the block is never worse than before, only rarer.
pub const FINISH_WAIT: Duration = Duration::from_secs(5);

/// How long `shutdown` waits for the thread to acknowledge before detaching.
/// A worker that is blocked on its channel exits immediately; one that is
/// mid-stitch is left to finish the command it already owns (the cancel flag
/// stops it at the next boundary) instead of blocking a window closing.
const SHUTDOWN_WAIT: Duration = Duration::from_millis(50);

/// One captured frame, in screen space, handed to the worker.
pub struct Job {
    /// Session identity. A job whose generation does not match the worker's
    /// session is refused, and the matching result is never applied.
    pub generation: u64,
    /// Bumped when the axis is re-baselined inside one session; results from
    /// the previous axis are dropped by the UI even if their generation is
    /// still current.
    pub epoch: u64,
    /// Monotonic per session. The UI uses it to drop duplicated or
    /// out-of-order results.
    pub seq: u64,
    pub axis: Axis,
    pub width: usize,
    pub height: usize,
    /// Tight BGRA, `width * 4` stride.
    pub bgra: Vec<u8>,
    /// Time the UI thread spent capturing and reading this frame back.
    pub capture_ms: f64,
    /// Polls skipped earlier because the previous job was still running.
    pub late: u64,
}

/// Per-stage timings for one tick, in milliseconds. `capture` is measured on
/// the UI thread and carried in the [`Job`]; the rest are measured here.
#[derive(Clone, Copy, Debug, Default)]
pub struct Spans {
    pub capture: f64,
    pub normalize: f64,
    pub gray: f64,
    pub sad: f64,
    pub stitch: f64,
    pub log: f64,
    /// Whole worker-side pass for this frame.
    pub process: f64,
}

impl Spans {
    /// Capture + worker, i.e. the wall time one tick cost the session.
    pub fn total(&self) -> f64 {
        self.capture + self.process
    }
}

/// What the UI should do with a finished frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Rows were stitched on this tick.
    Progress { is_up: bool, new_rows: usize },
    /// No progress yet, but under the stall threshold: keep the status.
    Idle,
    /// The page looks still; the host lowers the poll rate.
    Paused,
    /// A fixed header/footer band is covering the strip.
    StaticEdge,
    /// The frame moved but no offset could be confirmed: a slow, sub-pixel or
    /// otherwise unalignable scroll. Not a pause -- the host keeps the full
    /// poll rate and nudges the user. `verification_failed` distinguishes a
    /// candidate that the independent-band check refused.
    Unaligned { verification_failed: bool },
    /// The scroll outran the overlap strip.
    FastScroll,
    /// The page stayed still for `STALL_TICKS` frames while every candidate
    /// offset failed verification (P1-05). This is a state, never an end: the
    /// host keeps polling at the decimated rate and the next stitched tick
    /// restores full speed.
    Stalled { static_edge: bool },
    /// The frame changed, but only in a few bands: a static page with a local
    /// animation (caret / clock / video), not a scroll. Never "画面移动" and
    /// never an end: the host shows a neutral "waiting for scroll" status and
    /// keeps the full poll rate.
    LocalChange,
    /// A transient per-frame glitch (width mismatch, short buffer, unusable
    /// frame) was skipped. The session stays alive and the next good frame is
    /// captured as usual; this is not a stop.
    FrameSkipped,
}

/// Why the worker ended the session. The bar stays up for 完成/取消 after any
/// of these, exactly like the height-cap path did before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Adding the segment would pass the stitcher's height cap.
    HeightCap,
    /// A frame or the stitcher canvas no longer fits the image budget.
    Budget,
    /// A captured frame's geometry was transiently inconsistent (a WGC frame
    /// with a different width / short buffer / unusable rows). In the normal
    /// tick path these are skipped and the session continues; this is the
    /// accurate text if a stop is unavoidable -- never "画面变化过大".
    FrameSize,
}

/// A frame that was fully processed (stitched or not).
#[derive(Clone, Debug)]
pub struct FrameResult {
    pub verdict: Verdict,
    /// Whether the host should decimate polling (`StallTracker::paused`).
    pub paused: bool,
    pub segments: usize,
    pub total_height: usize,
    pub stats: StitchStats,
    pub spans: Spans,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Frame(FrameResult),
    Stopped {
        reason: StopReason,
    },
    /// The worker refused the job: stale generation/epoch, or cancelled.
    Discarded,
}

/// One result, tagged with the identity of the job it answers.
#[derive(Clone, Debug)]
pub struct Done {
    pub generation: u64,
    pub epoch: u64,
    pub seq: u64,
    pub outcome: Outcome,
}

/// Non-blocking read of the worker's result channel.
pub enum Poll {
    Done(Done),
    Empty,
    /// The thread ended (finished or panicked), so no result can ever arrive:
    /// the caller must stop instead of waiting on its in-flight job.
    Gone,
}

/// Fresh session state handed over by an axis switch.
pub struct Rebase {
    pub generation: u64,
    pub epoch: u64,
    pub axis: Axis,
    pub stitcher: Stitcher,
    pub prev_gray: Vec<u8>,
}

/// UI-side bookkeeping for the one-job-in-flight protocol.
///
/// Split out of the GPUI view so the generation/epoch/sequence rules can be
/// tested without a window: this type is the only thing that decides whether a
/// finished job may touch the session.
#[derive(Debug)]
pub struct ResultGate {
    generation: u64,
    epoch: u64,
    next_seq: u64,
    in_flight: Option<u64>,
    applied_seq: Option<u64>,
}

impl ResultGate {
    pub fn new(generation: u64) -> Self {
        ResultGate {
            generation,
            epoch: 0,
            next_seq: 0,
            in_flight: None,
            applied_seq: None,
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Claims the next sequence number, or `None` while a job is still out.
    /// Never queueing a second frame is what keeps a slow worker from building
    /// an unbounded backlog: the poll just skips that beat.
    pub fn issue(&mut self, epoch: u64) -> Option<u64> {
        if self.in_flight.is_some() || epoch != self.epoch {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.in_flight = Some(seq);
        Some(seq)
    }

    /// A new axis re-baselines the session: results from the old epoch are no
    /// longer applicable, and the in-flight slot is released because the stale
    /// result will be dropped when it arrives.
    pub fn rebaseline(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.in_flight = None;
    }

    /// Whether `done` may be applied. A matching sequence always clears the
    /// in-flight slot, even when the result itself is dropped, so a stale job
    /// can never wedge the session.
    pub fn accept(&mut self, done: &Done) -> bool {
        if self.in_flight == Some(done.seq) {
            self.in_flight = None;
        }
        if done.generation != self.generation {
            return false;
        }
        if done.epoch != self.epoch {
            return false;
        }
        if let Some(applied) = self.applied_seq {
            if done.seq <= applied {
                return false;
            }
        }
        self.applied_seq = Some(done.seq);
        true
    }
}

/// Owned by the worker thread for the whole session.
struct SessionState {
    generation: u64,
    epoch: u64,
    axis: Axis,
    stitcher: Stitcher,
    /// The stitch ANCHOR: grayscale of the last frame whose rows were
    /// committed. Only a successful stitch replaces it. The primary report,
    /// the stall tracker, the verdict and the diagnostic line are all measured
    /// against it, exactly as before this fix.
    prev_gray: Vec<u8>,
    /// The ALIGNMENT reference: grayscale of the most recently captured frame.
    /// Advanced on EVERY tick, not only on a stitch, so a smooth / sub-pixel
    /// frame can never leave the baseline frozen at a clean settled frame it
    /// can no longer match (the deadlock the diagnostic log showed).
    align_gray: Vec<u8>,
    /// Rows scrolled since the anchor, each confirmed by a verified
    /// frame-to-frame alignment, but not yet committed to the stitcher.
    /// Signed: positive is downward, negative upward. This is a SUM: it is a
    /// lower bound of the displacement from the anchor (a tick whose delta was
    /// refused contributes nothing), so [`slide_stitch_rows`] measures that
    /// displacement anchor-to-current and only falls back to this when the
    /// measurement is not available.
    pending: i64,
    stall: StallTracker,
    /// Frames processed, for the diagnostic `t{n}` column.
    ticks: u64,
    /// `diag.log` write of the previous tick: a line cannot report its own
    /// write, so the cost of the last one is printed instead.
    last_log_ms: f64,
}

impl SessionState {
    fn rebased(rb: Rebase) -> Self {
        SessionState {
            generation: rb.generation,
            epoch: rb.epoch,
            axis: rb.axis,
            stitcher: rb.stitcher,
            align_gray: rb.prev_gray.clone(),
            prev_gray: rb.prev_gray,
            pending: 0,
            stall: StallTracker::new(),
            ticks: 0,
            last_log_ms: 0.0,
        }
    }
}

/// Final buffer in screen space: tight BGRA plus its size.
type FinishReply = Result<(Vec<u8>, u32, u32), String>;

enum Cmd {
    Frame(Job),
    Rebaseline(Rebase),
    Finish(SyncSender<FinishReply>),
    Shutdown,
}

/// One thread, one session: owns `prev_gray`, the `Stitcher` and the
/// `StallTracker`, and therefore the only place frame-sized work happens.
pub struct LongWorker {
    tx: Option<Sender<Cmd>>,
    out: Receiver<Done>,
    exited: Receiver<()>,
    handle: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
}

impl LongWorker {
    /// Starts a session on the already-initialized first frame (the start path
    /// still builds the stitcher on the UI thread so a budget refusal can be
    /// reported before the capture phase begins).
    pub fn new(
        generation: u64,
        axis: Axis,
        stitcher: Stitcher,
        prev_gray: Vec<u8>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let (exit_tx, exit_rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let session = SessionState {
            generation,
            epoch: 0,
            axis,
            stitcher,
            align_gray: prev_gray.clone(),
            prev_gray,
            pending: 0,
            stall: StallTracker::new(),
            ticks: 0,
            last_log_ms: 0.0,
        };
        let handle = thread::Builder::new()
            .name("fastpin-long-worker".to_string())
            .spawn(move || run(rx, out_tx, exit_tx, flag, session))?;
        Ok(LongWorker {
            tx: Some(tx),
            out: out_rx,
            exited: exit_rx,
            handle: Some(handle),
            cancel,
        })
    }

    /// Queues one frame. Returns false when the worker is gone, which the tick
    /// treats as "this sample was skipped".
    pub fn submit(&self, job: Job) -> bool {
        if self.cancel.load(Ordering::SeqCst) {
            return false;
        }
        match self.tx.as_ref() {
            Some(tx) => tx.send(Cmd::Frame(job)).is_ok(),
            None => false,
        }
    }

    /// Replaces the session baseline after an axis switch.
    pub fn rebaseline(&self, rebase: Rebase) {
        if let Some(tx) = self.tx.as_ref() {
            let _ = tx.send(Cmd::Rebaseline(rebase));
        }
    }

    /// Non-blocking read of one finished result.
    pub fn poll(&self) -> Poll {
        match self.out.try_recv() {
            Ok(done) => Poll::Done(done),
            Err(TryRecvError::Empty) => Poll::Empty,
            Err(TryRecvError::Disconnected) => Poll::Gone,
        }
    }

    /// Stops accepting work and makes the thread exit at the next command
    /// boundary. Used by `shutdown`, and by tests to prove a cancelled worker
    /// never produces an applicable result.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Terminal hand-off for 完成: the worker folds the headroom away, rotates
    /// a horizontal capture back to screen space and returns the pixels. Also
    /// ends the worker, since the session is over.
    pub fn finish(&mut self, wait: Duration) -> Result<(Vec<u8>, u32, u32), String> {
        let Some(tx) = self.tx.as_ref() else {
            return Err("长截图后台任务已停止".to_string());
        };
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        tx.send(Cmd::Finish(reply_tx))
            .map_err(|_| "长截图后台任务已停止".to_string())?;
        match reply_rx.recv_timeout(wait) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err("长截图整理超时".to_string()),
            Err(RecvTimeoutError::Disconnected) => Err("长截图后台任务已停止".to_string()),
        }
    }

    /// Cancels and reclaims the thread if it is idle. A worker that is
    /// mid-stitch is detached instead: it owns all of its state, checks the
    /// cancel flag at the next command boundary and exits by itself, so neither
    /// path leaks the thread into a new session or blocks the UI for a full
    /// frame pass.
    pub fn shutdown(&mut self) {
        // `Drop` runs after an explicit `shutdown` on the cancel path; make the
        // second call free instead of waiting on a signal already consumed.
        if self.tx.is_none() {
            return;
        }
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Cmd::Shutdown);
        }
        if self.exited.recv_timeout(SHUTDOWN_WAIT).is_ok() {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        } else {
            // Detach: dropping the handle is the documented fallback.
            self.handle.take();
        }
    }
}

impl Drop for LongWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    rx: Receiver<Cmd>,
    out: Sender<Done>,
    exited: Sender<()>,
    cancel: Arc<AtomicBool>,
    session: SessionState,
) {
    let mut session = Some(session);
    while let Ok(cmd) = rx.recv() {
        // The flag is set before the channel closes, so a job queued behind a
        // cancel is refused instead of stitched into a session nobody owns.
        if cancel.load(Ordering::SeqCst) && !matches!(cmd, Cmd::Finish(_)) {
            break;
        }
        match cmd {
            Cmd::Frame(job) => {
                let (generation, epoch, seq) = (job.generation, job.epoch, job.seq);
                let outcome = match session.as_mut() {
                    Some(state) => process(state, &job),
                    None => Outcome::Discarded,
                };
                if out
                    .send(Done {
                        generation,
                        epoch,
                        seq,
                        outcome,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Cmd::Rebaseline(rb) => session = Some(SessionState::rebased(rb)),
            Cmd::Finish(reply) => {
                let result = match session.take() {
                    Some(state) => finalize(state),
                    None => Err("长截图后台任务已停止".to_string()),
                };
                let _ = reply.send(result);
                break;
            }
            Cmd::Shutdown => break,
        }
    }
    // The stitcher (possibly hundreds of MB) is released here, on this thread.
    drop(session);
    let _ = exited.send(());
}

/// Folds the headroom away and rotates a horizontal session back to screen
/// space, exactly as the export path used to do inline.
fn finalize(state: SessionState) -> Result<(Vec<u8>, u32, u32), String> {
    let (px, w, h) = state.stitcher.into_pixels();
    match state.axis {
        Axis::Vertical => Ok((px, w, h)),
        Axis::Horizontal => rotate_bgra270(&px, w as usize, h as usize)
            .map(|(px, w, h)| (px, w as u32, h as u32))
            .map_err(|e| e.to_string()),
    }
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

fn process(state: &mut SessionState, job: &Job) -> Outcome {
    let started = Instant::now();
    let mut spans = Spans {
        capture: job.capture_ms,
        ..Spans::default()
    };
    // A frame from a closed session, a previous axis or a replaced baseline
    // must never reach the pixels, even if it was already in the queue when
    // the session changed.
    if job.generation != state.generation || job.epoch != state.epoch || job.axis != state.axis {
        return Outcome::Discarded;
    }

    let t = Instant::now();
    let (norm, nw, nh) = match normalize_bgra(&job.bgra, job.width, job.height, state.axis) {
        Ok(v) => v,
        Err(e) => {
            crate::perf::log_diag(&format!("long-capture stopped: {e}"));
            return Outcome::Stopped {
                reason: StopReason::Budget,
            };
        }
    };
    spans.normalize = ms(t);

    let t = Instant::now();
    let gray = match bgra_to_gray(norm.as_ref(), nw, nh) {
        Ok(g) => g,
        Err(e) => {
            crate::perf::log_diag(&format!("long-capture stopped: {e}"));
            return Outcome::Stopped {
                reason: StopReason::Budget,
            };
        }
    };
    spans.gray = ms(t);

    state.ticks = state.ticks.wrapping_add(1);
    let strip = (nh / 4).min(STRIP_MAX);
    // `classify_scroll_diag` tries DOWN first and only runs the up pass when
    // down was rejected, then confirms every accepted offset with the
    // independent-band/seam check (P1-05).
    let t = Instant::now();
    // Primary: against the anchor (last stitched frame). Unchanged semantics
    // -- it drives the stall tracker, the verdict and the diagnostic line, so
    // a repeated unalignable frame still reads as motion, not a pause.
    let mut report = classify_scroll_diag(&state.prev_gray, &gray, nw, nh, strip, ACCEPT_SCORE);
    // Sliding: against the most recently captured frame, a fresh reference.
    // After a stitch at a clean settled frame, the very next in-flight
    // (sub-pixel / smooth-scroll) frame cannot be aligned to that clean
    // anchor; freezing the anchor on every reject then re-judges every later
    // frame against the same dead baseline until the accumulated scroll
    // outruns the overlap strip (the 83% `verify=0` deadlock).
    let slide = classify_scroll_diag(&state.align_gray, &gray, nw, nh, strip, ACCEPT_SCORE);
    spans.sad = ms(t);

    // THE FIX: advance the sliding alignment reference on EVERY tick, so it
    // can never go stale. Only grayscale is retained (one extra plane); the
    // rows we stitch always come from the CURRENT frame, which already holds
    // every un-stitched row, so no BGRA history / image_budget is needed.
    state.align_gray = gray.clone();

    // Accumulate the verified per-frame delta since the anchor. The deltas
    // telescope into the displacement from the anchor while every tick of the
    // run is measured, which is the common case; a refused tick leaves its own
    // rows out (see `slide_stitch_rows`, which re-measures the total instead of
    // trusting this sum). `pending` is still what the diagnostic line prints.
    if let Some((is_up, _, d)) = slide.direction {
        state.pending += if is_up { -(d as i64) } else { d as i64 };
    }

    // Decide the stitch. The anchor report wins (it verified the cumulative
    // offset against the anchor itself); otherwise the row count comes from
    // `slide_stitch_rows`, which measures the displacement anchor-to-current
    // instead of trusting the accumulated sum. A refused stitch keeps
    // `pending` for a later tick.
    let stitch = match report.direction {
        Some((is_up, offset, new_rows)) => Some((is_up, offset, new_rows, false)),
        None => slide_stitch_rows(&state.prev_gray, &gray, nw, nh, strip, state.pending)
            .map(|s| (s.is_up, s.offset_y, s.new_rows, s.measured)),
    };

    match stitch {
        Some((is_up, offset_y, new_rows, measured)) => {
            let via_slide = report.direction.is_none();
            if via_slide {
                // A sliding-driven stitch: make the stall tracker and the log
                // describe the progress the primary (deadlocked) report hid.
                report.direction = Some((is_up, offset_y, new_rows));
                report.down_score = slide.down_score;
                report.down_ratio = slide.down_ratio;
                report.up_score = slide.up_score;
                report.up_ratio = slide.up_ratio;
                report.overlap_score = slide.overlap_score;
                report.seam_score = slide.seam_score;
            }
            state.stall.observe(&report);
            let t = Instant::now();
            // Both directions enforce the height cap and the image budget
            // inside the stitcher; a refusal leaves the buffer untouched.
            let outcome = if is_up {
                state.stitcher.prepend(norm.as_ref(), nw, nh, new_rows)
            } else {
                state.stitcher.append(norm.as_ref(), nw, nh, offset_y)
            };
            spans.stitch = ms(t);
            let (score, ratio) = if is_up {
                (report.up_score, report.up_ratio)
            } else {
                (report.down_score, report.down_ratio)
            };
            let total = state.stitcher.total_height();
            let segments = state.stitcher.segment_count();
            // Everything before the write; the write's own cost is printed by
            // the next tick and folded into the returned spans below.
            spans.process = ms(started);
            let t = Instant::now();
            crate::perf::log_diag(&format!(
                "long-tick t{} axis={} elapsed_ms={:.1} ui_ms={:.1} proc_ms={:.1} \
                 norm_ms={:.1} gray_ms={:.1} sad_ms={:.1} stitch_ms={:.1} \
                 log_prev_ms={:.1} score={score:.3} runner_up={ratio:.2} seam={:.3} \
                 verify={:.3} is_up={is_up} new_rows={new_rows} total={total}px \
                 seg={segments} late={} copied={} slide={} src={}",
                state.ticks,
                state.axis.label(),
                spans.total(),
                spans.capture,
                spans.process,
                spans.normalize,
                spans.gray,
                spans.sad,
                spans.stitch,
                state.last_log_ms,
                report.seam_score,
                report.overlap_score,
                job.late,
                state.stitcher.stats().copied_bytes,
                via_slide as u8,
                if !via_slide {
                    "edge"
                } else if measured {
                    "measure"
                } else {
                    "sum"
                },
            ));
            spans.log = ms(t);
            state.last_log_ms = spans.log;
            spans.process = ms(started);
            if let Err(e) = outcome {
                if is_transient(&e) {
                    // Recoverable glitch (WGC frame width / buffer / row count
                    // fluctuated for one capture). Skip just this frame: the
                    // stitcher refused the write and left its buffer untouched,
                    // and prev_gray is deliberately NOT advanced here (it is
                    // replaced only on a successful stitch below), so the next
                    // good frame still aligns against a baseline whose width
                    // matches the stitcher. No stitched content is dropped and
                    // the session keeps polling.
                    //
                    // The glitch frame is also a bad sliding reference: keep
                    // both references (and the pending rows) at the anchor so
                    // the next good frame aligns against a width-matching one.
                    state.align_gray = state.prev_gray.clone();
                    state.pending = 0;
                    crate::perf::log_diag(&format!("long-tick skip: {e}"));
                    return Outcome::Frame(FrameResult {
                        verdict: Verdict::FrameSkipped,
                        paused: state.stall.paused(),
                        segments,
                        total_height: total,
                        stats: state.stitcher.stats(),
                        spans,
                    });
                }
                crate::perf::log_diag(&format!("long-tick stop: {e}"));
                return Outcome::Stopped {
                    reason: stop_reason(&e),
                };
            }
            // Re-anchor on this frame: it is now both the stitch anchor and
            // (already) the sliding reference. The committed rows cover the
            // whole run, so the sum carries nothing over to the next tick.
            state.prev_gray = gray;
            state.pending = 0;
            Outcome::Frame(FrameResult {
                verdict: Verdict::Progress { is_up, new_rows },
                paused: state.stall.paused(),
                segments,
                total_height: total,
                stats: state.stitcher.stats(),
                spans,
            })
        }
        None => {
            spans.process = ms(started);
            let t = Instant::now();
            crate::perf::log_diag(&format!(
                "long-tick t{} axis={} elapsed_ms={:.1} ui_ms={:.1} proc_ms={:.1} \
                 norm_ms={:.1} gray_ms={:.1} sad_ms={:.1} log_prev_ms={:.1} \
                 score={:.3}/{:.3} runner_up={:.2}/{:.2} is_up=- new_rows=0 \
                 changed={:.2} static_edge={} inner_moved={}({}) \
                 rejected_by_verify={} verify={:.3} seam={:.3} idle={} late={} \
                 pending={}",
                state.ticks,
                state.axis.label(),
                spans.total(),
                spans.capture,
                spans.process,
                spans.normalize,
                spans.gray,
                spans.sad,
                state.last_log_ms,
                report.down_score,
                report.up_score,
                report.down_ratio,
                report.up_ratio,
                report.changed,
                report.static_edge,
                report.inner_content_moved,
                report.inner_new_rows,
                report.rejected_by_verification,
                report.overlap_score,
                report.seam_score,
                state.stall.idle_ticks() + 1,
                job.late,
                state.pending,
            ));
            spans.log = ms(t);
            state.last_log_ms = spans.log;
            // P1-05: consecutive no-progress ticks are a state, not silence.
            // Only a still page counts towards the pause; a moving one keeps
            // the full poll rate and gets an "unaligned" nudge instead of
            // "no scrolling detected".
            let phase = state.stall.observe(&report);
            spans.process = ms(started);
            let verdict = idle_verdict(phase, &report);
            Outcome::Frame(FrameResult {
                verdict,
                paused: state.stall.paused(),
                segments: state.stitcher.segment_count(),
                total_height: state.stitcher.total_height(),
                stats: state.stitcher.stats(),
                spans,
            })
        }
    }
}

/// Maps one no-progress tick to the verdict the host shows.
///
/// `Stalled` deliberately becomes a [`Verdict`] instead of an
/// [`Outcome::Stopped`]: it is the state "the strip offered an offset that the
/// seam check refused", which a later tick may leave again (the page changed
/// under it). Only hard limits and the user's 完成/取消 end a session.
///
/// A moving frame is never reported as still: a confirmed motion with no
/// offset is an "unaligned" nudge, and only a frame that really stopped
/// (and stayed stopped for `STALL_TICKS`) reaches [`Verdict::Paused`].
fn idle_verdict(phase: StallPhase, report: &ScrollReport) -> Verdict {
    let band = report.static_edge && report.inner_content_moved;
    match phase {
        // No motion for the whole threshold: the strip cannot be trusted to
        // describe a user pause, but an unalignable one is still not "still".
        StallPhase::Stalled => Verdict::Stalled { static_edge: band },
        StallPhase::Paused => Verdict::Paused,
        StallPhase::Progress | StallPhase::Watching => {
            if report.local_change {
                // Static page with a local animation, not a scroll. Neutral
                // "waiting for scroll" status, never "画面移动". observe() still
                // returns Watching (changed > MOTION_CHANGED), so polling is
                // not decimated and the session is never ended.
                Verdict::LocalChange
            } else if band {
                // A fixed band covering the strip while the page moves behind
                // it: the actionable hint, not a stall.
                Verdict::StaticEdge
            } else if report.rejected_by_verification {
                // A candidate was found but the seam/band check refused it.
                Verdict::Unaligned {
                    verification_failed: true,
                }
            } else if report.changed > FAST_SCROLL_CHANGED {
                // The scroll outran the overlap strip: not a reason to end
                // the session, just to nudge the user.
                Verdict::FastScroll
            } else if report.changed > MOTION_CHANGED {
                // The page clearly moved, the matcher just could not lock on
                // (a slow or sub-pixel scroll). Never "no scrolling detected".
                Verdict::Unaligned {
                    verification_failed: false,
                }
            } else {
                Verdict::Idle
            }
        }
    }
}

/// Total, accurate mapping from a stitch refusal to the reason the bar would
/// show if a stop were unavoidable. Transient per-frame glitches map to
/// [`StopReason::FrameSize`] (never "画面变化过大"); the tick path skips those
/// instead of stopping, but this keeps one authoritative source for the text.
fn stop_reason(e: &StitchError) -> StopReason {
    match e {
        StitchError::HeightCap { .. } => StopReason::HeightCap,
        StitchError::Budget(_) => StopReason::Budget,
        StitchError::InvalidFrame { .. }
        | StitchError::WidthMismatch { .. }
        | StitchError::ShortFrame { .. } => StopReason::FrameSize,
    }
}

/// A recoverable, transient per-frame glitch. These are skipped (the frame is
/// dropped, the session and its polling continue) rather than ending the
/// session. Only the real hard limits (height cap / image budget) stop.
fn is_transient(e: &StitchError) -> bool {
    stop_reason(e) == StopReason::FrameSize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::longcapture::STALL_TICKS;

    const W: usize = 8;
    const H: usize = 32;
    const MAX_H: usize = 16384;

    /// Deterministic byte stream: every row is unique, so SAD has a single
    /// minimum and a synthetic scroll aligns exactly.
    fn synthetic_page(width: usize, rows: usize, seed: u32) -> Vec<u8> {
        let mut state = seed;
        let mut page = Vec::with_capacity(width * 4 * rows);
        for _ in 0..width * 4 * rows {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            page.push((state >> 24) as u8);
        }
        page
    }

    fn frame(page: &[u8], width: usize, start_row: usize, height: usize) -> Vec<u8> {
        let row = width * 4;
        page[start_row * row..(start_row + height) * row].to_vec()
    }

    fn session(first: &[u8]) -> (Stitcher, Vec<u8>) {
        let mut stitcher = Stitcher::new(W, H, MAX_H).unwrap();
        assert!(stitcher.fill_first(first));
        let gray = bgra_to_gray(first, W, H).unwrap();
        (stitcher, gray)
    }

    fn worker(first: &[u8], generation: u64) -> LongWorker {
        let (stitcher, gray) = session(first);
        LongWorker::new(generation, Axis::Vertical, stitcher, gray).unwrap()
    }

    fn job(seq: u64, generation: u64, epoch: u64, bgra: Vec<u8>) -> Job {
        Job {
            generation,
            epoch,
            seq,
            axis: Axis::Vertical,
            width: W,
            height: H,
            bgra,
            capture_ms: 0.0,
            late: 0,
        }
    }

    /// Polls the non-blocking result channel; the worker is normally done in a
    /// few milliseconds, this only guards a wedged test.
    fn wait(worker: &LongWorker) -> Done {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match worker.poll() {
                Poll::Done(done) => return done,
                Poll::Empty => {}
                Poll::Gone => panic!("worker exited without a result"),
            }
            assert!(Instant::now() < deadline, "worker produced no result");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn assert_progress(done: &Done) -> usize {
        match &done.outcome {
            Outcome::Frame(f) => match f.verdict {
                Verdict::Progress { new_rows, .. } => new_rows,
                other => panic!("expected progress, got {other:?}"),
            },
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    /// End-to-end session harness: N synthetic scroll steps through the real
    /// worker must produce exactly the rows the stitcher should hold.
    #[test]
    fn worker_stitches_a_synthetic_scroll_session() {
        let page = synthetic_page(W, H + 5 * 3 + 8, 0x1234_5678);
        let first = frame(&page, W, 0, H);
        let mut worker = worker(&first, 1);
        let mut gate = ResultGate::new(1);

        for (i, start) in [5usize, 10, 15].into_iter().enumerate() {
            let seq = gate.issue(gate.epoch()).expect("one job at a time");
            assert!(worker.submit(job(seq, 1, 0, frame(&page, W, start, H))));
            let done = wait(&worker);
            assert!(gate.accept(&done), "step {i} was dropped");
            assert_eq!(assert_progress(&done), 5, "step {i}");
        }

        let (px, w, h) = worker.finish(FINISH_WAIT).unwrap();
        assert_eq!((w, h), (W as u32, (H + 15) as u32));
        assert_eq!(px.len(), W * (H + 15) * 4);
    }

    /// A job tagged with an older session generation never touches the pixels.
    #[test]
    fn worker_discards_a_job_from_an_old_generation() {
        let page = synthetic_page(W, H + 8, 0x99);
        let first = frame(&page, W, 0, H);
        let worker = worker(&first, 7);
        assert!(worker.submit(job(0, 6, 0, frame(&page, W, 5, H))));
        let done = wait(&worker);
        assert_eq!(done.generation, 6);
        assert!(matches!(done.outcome, Outcome::Discarded));

        let mut gate = ResultGate::new(7);
        let _ = gate.issue(0);
        assert!(!gate.accept(&done), "a stale generation must not apply");
    }

    /// Cancelling stops the worker from returning anything the UI could apply.
    #[test]
    fn cancelled_worker_produces_no_applicable_result() {
        let page = synthetic_page(W, H + 8, 0xAB);
        let first = frame(&page, W, 0, H);
        let mut worker = worker(&first, 3);
        worker.cancel();
        assert!(!worker.submit(job(0, 3, 0, frame(&page, W, 5, H))));
        assert!(matches!(worker.poll(), Poll::Empty));
        worker.shutdown();
    }

    /// An axis switch bumps the epoch; the late result of the old axis is
    /// dropped even though the session generation is still the current one.
    #[test]
    fn result_from_the_previous_axis_is_dropped() {
        let page = synthetic_page(W, H + 8, 0xCD);
        let first = frame(&page, W, 0, H);
        let worker = worker(&first, 1);
        let mut gate = ResultGate::new(1);
        let seq = gate.issue(0).unwrap();
        assert!(worker.submit(job(seq, 1, 0, frame(&page, W, 5, H))));
        let done = wait(&worker);

        // The user toggles the axis before the result is folded in.
        gate.rebaseline(1);
        assert!(!gate.accept(&done), "old-epoch pixels must not be applied");

        // Re-baseline on the other axis: the worker swaps its stitcher, and a
        // frame in the new orientation must be accepted by the new baseline.
        let (norm, nw, nh) = normalize_bgra(&first, W, H, Axis::Horizontal).unwrap();
        let mut stitcher = Stitcher::new(nw, nh, MAX_H).unwrap();
        assert!(stitcher.fill_first(norm.as_ref()));
        let gray = bgra_to_gray(norm.as_ref(), nw, nh).unwrap();
        worker.rebaseline(Rebase {
            generation: 1,
            epoch: 1,
            axis: Axis::Horizontal,
            stitcher,
            prev_gray: gray,
        });
        let seq = gate.issue(1).unwrap();
        let mut next = job(seq, 1, 1, frame(&page, W, 5, H));
        next.axis = Axis::Horizontal;
        assert!(worker.submit(next));
        let done = wait(&worker);
        assert!(gate.accept(&done), "the new epoch must apply");
        assert!(
            !matches!(done.outcome, Outcome::Discarded),
            "the re-baselined worker must accept the new axis"
        );
    }

    #[test]
    fn gate_allows_one_job_in_flight() {
        let mut gate = ResultGate::new(1);
        assert!(gate.issue(0).is_some());
        assert!(gate.in_flight());
        assert!(gate.issue(0).is_none(), "a second frame would queue");
    }

    #[test]
    fn gate_drops_duplicate_and_out_of_order_results() {
        let mut gate = ResultGate::new(1);
        let first = gate.issue(0).unwrap();
        let done_first = Done {
            generation: 1,
            epoch: 0,
            seq: first,
            outcome: Outcome::Discarded,
        };
        assert!(gate.accept(&done_first));
        let second = gate.issue(0).unwrap();
        assert!(!gate.accept(&done_first), "a duplicate must not re-apply");
        let done_second = Done {
            generation: 1,
            epoch: 0,
            seq: second,
            outcome: Outcome::Discarded,
        };
        assert!(gate.accept(&done_second));
    }

    /// The spans are the measurement the report quotes: assert they are
    /// recorded, and print them (a 640x360 synthetic page, debug profile) for
    /// `--nocapture` runs.
    #[test]
    fn worker_records_per_stage_spans() {
        let (w, h) = (640usize, 360usize);
        let page = synthetic_page(w, h + 8, 0xEF);
        let first = frame(&page, w, 0, h);
        let mut stitcher = Stitcher::new(w, h, MAX_H).unwrap();
        assert!(stitcher.fill_first(&first));
        let gray = bgra_to_gray(&first, w, h).unwrap();
        let worker = LongWorker::new(1, Axis::Vertical, stitcher, gray).unwrap();
        let mut gate = ResultGate::new(1);
        let seq = gate.issue(0).unwrap();
        assert!(worker.submit(Job {
            generation: 1,
            epoch: 0,
            seq,
            axis: Axis::Vertical,
            width: w,
            height: h,
            bgra: frame(&page, w, 5, h),
            capture_ms: 1.5,
            late: 0,
        }));
        let done = wait(&worker);
        assert!(gate.accept(&done));
        let Outcome::Frame(f) = done.outcome else {
            panic!("expected a frame");
        };
        println!(
            "synthetic tick {w}x{h}: ui={:.3} norm={:.3} gray={:.3} sad={:.3} \
             stitch={:.3} log={:.3} proc={:.3} total={:.3} ms",
            f.spans.capture,
            f.spans.normalize,
            f.spans.gray,
            f.spans.sad,
            f.spans.stitch,
            f.spans.log,
            f.spans.process,
            f.spans.total(),
        );
        assert!(f.spans.capture >= 1.5);
        assert!(f.spans.process >= f.spans.sad);
        assert!(f.spans.total() >= f.spans.process);
    }

    /// A viewport whose interior changed but whose edge strips did not: the
    /// shape a fixed header/footer band produces. The bottom/top strip still
    /// finds itself in place, so no offset is confirmed, while `changed` stays
    /// above `STATIC_EDGE_CHANGED` and below `FAST_SCROLL_CHANGED` -- the
    /// stall-tracker path that used to end the session.
    fn altered_frame(page: &[u8], start_row: usize, from_row: usize, rows: usize) -> Vec<u8> {
        let mut f = frame(page, W, start_row, H);
        let row = W * 4;
        for r in from_row..(from_row + rows).min(H) {
            for b in &mut f[r * row..(r + 1) * row] {
                *b = 255 - *b;
            }
        }
        f
    }

    fn is_state_verdict(verdict: Verdict) -> bool {
        matches!(
            verdict,
            Verdict::Idle
                | Verdict::Paused
                | Verdict::StaticEdge
                | Verdict::Unaligned { .. }
                | Verdict::FastScroll
                | Verdict::Stalled { .. }
                | Verdict::LocalChange
                | Verdict::FrameSkipped
        )
    }

    /// P1-05 regression: an unalignable frame used to return
    /// `Outcome::Stopped`, so the poll loop ended and further scrolling was
    /// captured nowhere. It is now a recoverable verdict, and a frame that
    /// moved is reported as unaligned, never as a still page.
    #[test]
    fn stall_does_not_end_the_session() {
        let page = synthetic_page(W, H + 24, 0x5A11_0001);
        let first = frame(&page, W, 0, H);
        let worker = worker(&first, 1);
        let mut gate = ResultGate::new(1);
        let stalled = altered_frame(&page, 0, 8, 10);

        let mut hint_seen = false;
        for tick in 0..(STALL_TICKS + 5) {
            let seq = gate.issue(gate.epoch()).expect("one job at a time");
            assert!(worker.submit(job(seq, 1, 0, stalled.clone())));
            let done = wait(&worker);
            assert!(gate.accept(&done), "tick {tick} was dropped");
            match &done.outcome {
                Outcome::Frame(f) => {
                    assert!(
                        is_state_verdict(f.verdict),
                        "tick {tick}: unexpected verdict {:?}",
                        f.verdict
                    );
                    assert!(
                        !matches!(f.verdict, Verdict::Paused),
                        "tick {tick}: a moving frame must not read as still"
                    );
                    assert!(!f.paused, "tick {tick}: motion keeps the full poll rate");
                    hint_seen |= matches!(
                        f.verdict,
                        Verdict::Unaligned { .. } | Verdict::StaticEdge | Verdict::FastScroll
                    );
                }
                other => panic!("tick {tick}: a stall must not end the session, got {other:?}"),
            }
        }
        assert!(
            hint_seen,
            "an unalignable moving frame must surface a hint, not silence"
        );
        // The worker is still alive and answers one more frame.
        let seq = gate.issue(gate.epoch()).unwrap();
        assert!(worker.submit(job(seq, 1, 0, first.clone())));
        assert!(matches!(wait(&worker).outcome, Outcome::Frame(_)));
    }

    /// The decimated session resumes the moment a tick confirms an offset:
    /// progress, full-speed polling and a growing stitcher, all without
    /// restarting the session.
    #[test]
    fn capture_resumes_after_a_stall() {
        let page = synthetic_page(W, H + 40, 0x5A11_0002);
        let first = frame(&page, W, 0, H);
        let worker = worker(&first, 2);
        let mut gate = ResultGate::new(2);

        // A still page decimates the poll rate after the longer threshold.
        let mut paused_seen = false;
        for tick in 0..(STALL_TICKS + 2) {
            let seq = gate.issue(gate.epoch()).unwrap();
            assert!(worker.submit(job(seq, 2, 0, first.clone())));
            let done = wait(&worker);
            assert!(gate.accept(&done), "tick {tick} was dropped");
            let Outcome::Frame(f) = done.outcome else {
                panic!("tick {tick}: the session must not stop");
            };
            paused_seen |= f.paused;
        }
        assert!(paused_seen, "a still page must decimate its polling");

        // `prev_gray` was never replaced, so this is a confirmed 5-row scroll
        // against the session baseline.
        let seq = gate.issue(gate.epoch()).unwrap();
        assert!(worker.submit(job(seq, 2, 0, frame(&page, W, 5, H))));
        let done = wait(&worker);
        assert!(gate.accept(&done));
        let Outcome::Frame(f) = done.outcome else {
            panic!("a confirmed shift must produce a frame");
        };
        assert!(
            matches!(
                f.verdict,
                Verdict::Progress {
                    is_up: false,
                    new_rows: 5
                }
            ),
            "expected 5 stitched rows, got {:?}",
            f.verdict
        );
        assert!(!f.paused, "a stitched tick restores the full poll rate");
        assert_eq!(f.segments, 2, "the stitch added a segment");
        assert_eq!(f.total_height, H + 5);
    }

    /// A still page is decimated, never ended: the result stream keeps
    /// producing frames, `paused` rises, and the next scroll is stitched.
    #[test]
    fn paused_polling_keeps_capturing() {
        let page = synthetic_page(W, H + 40, 0x5A11_0003);
        let first = frame(&page, W, 0, H);
        let worker = worker(&first, 3);
        let mut gate = ResultGate::new(3);

        let mut paused_seen = false;
        for tick in 0..(STALL_TICKS + 4) {
            let seq = gate.issue(gate.epoch()).unwrap();
            assert!(worker.submit(job(seq, 3, 0, first.clone())));
            let done = wait(&worker);
            assert!(gate.accept(&done), "tick {tick} was dropped");
            let Outcome::Frame(f) = done.outcome else {
                panic!("tick {tick}: a still page must not end the session");
            };
            assert!(is_state_verdict(f.verdict), "tick {tick}: {:?}", f.verdict);
            paused_seen |= f.paused;
        }
        assert!(paused_seen, "a still page must lower the poll rate");

        // The decimated loop is still sampling, so a resumed scroll stitches.
        let seq = gate.issue(gate.epoch()).unwrap();
        assert!(worker.submit(job(seq, 3, 0, frame(&page, W, 4, H))));
        let done = wait(&worker);
        assert!(gate.accept(&done));
        let Outcome::Frame(f) = done.outcome else {
            panic!("the resumed scroll must be captured");
        };
        assert!(
            matches!(
                f.verdict,
                Verdict::Progress {
                    is_up: false,
                    new_rows: 4
                }
            ),
            "expected 4 stitched rows, got {:?}",
            f.verdict
        );
        assert!(!f.paused);
        assert_eq!(f.segments, 2);
    }

    /// The mapping itself: `Stalled` is a verdict (with the band flag for the
    /// status line), never a stop.
    #[test]
    fn stalled_phase_maps_to_a_recoverable_verdict() {
        let band = ScrollReport {
            changed: 0.2,
            static_edge: true,
            inner_content_moved: true,
            ..ScrollReport::default()
        };
        assert_eq!(
            idle_verdict(StallPhase::Stalled, &band),
            Verdict::Stalled { static_edge: true }
        );
        let unalignable = ScrollReport {
            changed: 0.2,
            ..ScrollReport::default()
        };
        assert_eq!(
            idle_verdict(StallPhase::Stalled, &unalignable),
            Verdict::Stalled { static_edge: false }
        );
        assert_eq!(
            idle_verdict(StallPhase::Paused, &unalignable),
            Verdict::Paused
        );
        assert_eq!(
            idle_verdict(StallPhase::Watching, &band),
            Verdict::StaticEdge
        );
        // A moving frame without an offset is a nudge, never "still".
        assert_eq!(
            idle_verdict(StallPhase::Watching, &unalignable),
            Verdict::Unaligned {
                verification_failed: false
            }
        );
    }

    /// A seam-check refusal is its own message ("continue scrolling"), not
    /// "no scrolling detected", and it never decimates polling.
    #[test]
    fn rejected_stitch_is_reported_as_unaligned_not_still() {
        let rejected = ScrollReport {
            changed: 0.2,
            rejected_by_verification: true,
            ..ScrollReport::default()
        };
        let mut stall = StallTracker::new();
        for tick in 0..(STALL_TICKS * 2) {
            let phase = stall.observe(&rejected);
            assert_eq!(phase, StallPhase::Watching, "tick {tick}");
            assert!(!stall.paused(), "tick {tick}: motion is never a pause");
            assert_eq!(
                idle_verdict(phase, &rejected),
                Verdict::Unaligned {
                    verification_failed: true
                },
                "tick {tick}"
            );
        }
    }

    /// A frame whose accepted offset is refused by the independent-band/seam
    /// check must keep the session alive: every tick is a recoverable
    /// `Outcome::Frame` (`Unaligned { verification_failed: true }`), never
    /// `Outcome::Stopped`, never a pause, and the captured buffer stays intact
    /// so a later real scroll can still stitch.
    #[test]
    fn verification_failure_does_not_end_the_session() {
        const TW: usize = 32;
        const TH: usize = 240;
        let page = synthetic_page(TW, 400, 0x0BAD_0005);
        let first = frame(&page, TW, 100, TH);
        let mut stitcher = Stitcher::new(TW, TH, MAX_H).unwrap();
        assert!(stitcher.fill_first(&first));
        let gray = bgra_to_gray(&first, TW, TH).unwrap();
        let mut worker = LongWorker::new(1, Axis::Vertical, stitcher, gray).unwrap();
        let mut gate = ResultGate::new(1);

        // A frame whose bottom strip is spliced to a wrong offset: the strip
        // matches, but classify_scroll_diag's verify_shift refuses the stitch.
        let mut bad = frame(&page, TW, 108, TH);
        let rb = TW * 4;
        bad[(TH - 48) * rb..(TH - 16) * rb].copy_from_slice(&first[(TH - 32) * rb..TH * rb]);

        for tick in 0..(STALL_TICKS + 5) {
            let seq = gate.issue(gate.epoch()).expect("one job at a time");
            let j = Job {
                generation: 1,
                epoch: gate.epoch(),
                seq,
                axis: Axis::Vertical,
                width: TW,
                height: TH,
                bgra: bad.clone(),
                capture_ms: 0.0,
                late: 0,
            };
            assert!(worker.submit(j));
            let done = wait(&worker);
            assert!(gate.accept(&done), "tick {tick} dropped");
            match &done.outcome {
                Outcome::Frame(f) => assert_eq!(
                    f.verdict,
                    Verdict::Unaligned {
                        verification_failed: true
                    },
                    "tick {tick}: a verification failure is a recoverable Frame, not a stop"
                ),
                other => panic!("tick {tick}: verification failure must not stop: {other:?}"),
            }
        }
        // The session is still finishable and no captured row was lost.
        let (px, w, h) = worker.finish(FINISH_WAIT).unwrap();
        assert_eq!((w as usize, h as usize), (TW, TH));
        assert_eq!(px, first, "stitched content is untouched");
    }

    /// A fixed header/footer band keeps its hint while the page moves behind
    /// it; without confirmed inner motion the same signature is an alignment
    /// problem, and neither case reads as a still page.
    #[test]
    fn static_band_still_reports_the_band_hint() {
        let band = ScrollReport {
            changed: 0.2,
            static_edge: true,
            inner_content_moved: true,
            ..ScrollReport::default()
        };
        let mut stall = StallTracker::new();
        for tick in 0..(STALL_TICKS + 5) {
            let phase = stall.observe(&band);
            assert!(
                !stall.paused(),
                "tick {tick}: a moving page keeps full speed"
            );
            assert_eq!(
                idle_verdict(phase, &band),
                Verdict::StaticEdge,
                "tick {tick}"
            );
        }
        let unconfirmed = ScrollReport {
            changed: 0.2,
            static_edge: true,
            ..ScrollReport::default()
        };
        assert_eq!(
            idle_verdict(StallPhase::Watching, &unconfirmed),
            Verdict::Unaligned {
                verification_failed: false
            }
        );
    }

    // ---- static page with a local animation is not "画面移动" -------------

    /// Solid BGRA whose grayscale is exactly `gray` (B=G=R=gray, 29+150+77=256).
    fn solid_bgra(width: usize, height: usize, gray: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            out.extend_from_slice(&[gray, gray, gray, 255]);
        }
        out
    }

    /// A: the user never scrolled; only a small block (a caret / clock /
    /// video) keeps changing. The old whole-frame metric reads "moved", the
    /// distribution says "local". Host must show a neutral status, never
    /// "画面移动", never end the session, never decimate polling.
    #[test]
    fn local_animation_is_not_a_moving_page() {
        let first = solid_bgra(W, H, 60);
        // 3 of 32 rows driven to a contrasting luma (diff 160 > 24), the
        // other 29 rows untouched. Fully deterministic frames.
        let mut animated = solid_bgra(W, H, 60);
        for r in 4..7 {
            let start = r * W * 4;
            let end = (r + 1) * W * 4;
            for byte in &mut animated[start..end] {
                *byte = 220;
            }
        }
        let mut worker = worker(&first, 1);
        let mut gate = ResultGate::new(1);
        for tick in 0..(STALL_TICKS + 5) {
            let seq = gate.issue(gate.epoch()).expect("one job at a time");
            assert!(worker.submit(job(seq, 1, 0, animated.clone())));
            let done = wait(&worker);
            assert!(gate.accept(&done), "tick {tick} dropped");
            match &done.outcome {
                Outcome::Frame(f) => {
                    assert_eq!(
                        f.verdict,
                        Verdict::LocalChange,
                        "tick {tick}: {:?}",
                        f.verdict
                    );
                    // Neutral: never the Unaligned "画面移动..." wording.
                    assert!(!matches!(f.verdict, Verdict::Unaligned { .. }));
                    assert!(!f.paused, "tick {tick}: full poll rate is kept");
                }
                other => panic!("tick {tick}: a static page must not end: {other:?}"),
            }
        }
        // The session is still finishable and no captured row was lost.
        let (px, w, h) = worker.finish(FINISH_WAIT).unwrap();
        assert_eq!((w as usize, h as usize), (W, H));
        assert_eq!(px, first, "stitched content is untouched");
    }

    /// A regression guard: the local-animation fix must not swallow a real
    /// uniform scroll (preserves `moving_frame_never_reads_as_idle`).
    #[test]
    fn a_real_scroll_is_still_detected() {
        let page = synthetic_page(W, H + 16, 0xC0FF_EE01);
        let prev = bgra_to_gray(&frame(&page, W, 0, H), W, H).unwrap();
        let curr = bgra_to_gray(&frame(&page, W, 5, H), W, H).unwrap();
        // Uniform scroll: changes are spread through every band.
        assert!(!crate::longcapture::change_is_local(&prev, &curr, W, H));
        // The direction classifier still confirms a genuine 5-row stitch.
        let strip = (H / 4).min(32);
        let report = classify_scroll_diag(&prev, &curr, W, H, strip, ACCEPT_SCORE);
        assert!(
            report.direction.is_some(),
            "a real scroll is detected: {report:?}"
        );
        assert!(
            !report.local_change,
            "a confirmed stitch is never marked local"
        );

        // A moving, non-local unaligned report is still a nudge, never Idle
        // and never rewritten as LocalChange.
        let moving = ScrollReport {
            changed: 0.15,
            ..ScrollReport::default()
        };
        let mut stall = StallTracker::new();
        assert_eq!(stall.observe(&moving), StallPhase::Watching);
        assert_eq!(
            idle_verdict(StallPhase::Watching, &moving),
            Verdict::Unaligned {
                verification_failed: false
            }
        );
        let fast = ScrollReport {
            changed: 0.6,
            ..ScrollReport::default()
        };
        assert_eq!(
            idle_verdict(StallPhase::Watching, &fast),
            Verdict::FastScroll
        );
    }

    /// B: a transient per-frame glitch (width mismatch / unusable frame /
    /// short buffer) is skipped, not "画面变化过大"/stopped, and the session
    /// recovers on the very next good frame.
    #[test]
    fn transient_frame_error_does_not_end_the_session() {
        let transient = [
            StitchError::WidthMismatch {
                expected: W,
                got: W + 1,
            },
            StitchError::InvalidFrame {
                width: W,
                height: H,
                rows: 0,
            },
            StitchError::ShortFrame {
                needed: 16,
                available: 8,
            },
        ];
        for e in &transient {
            assert!(is_transient(e), "{e:?}: a frame glitch must be skippable");
            assert_eq!(
                stop_reason(e),
                StopReason::FrameSize,
                "{e:?}: accurate reason, never 画面变化过大"
            );
        }
        // Skipping is a recoverable state, not a stop: the bar keeps polling.
        assert!(is_state_verdict(Verdict::FrameSkipped));
        let skipped = FrameResult {
            verdict: Verdict::FrameSkipped,
            paused: false,
            segments: 1,
            total_height: H,
            stats: StitchStats::default(),
            spans: Spans::default(),
        };
        assert!(!matches!(Outcome::Frame(skipped), Outcome::Stopped { .. }));

        // Skipping leaves the stitcher untouched (prev_gray stays at the last
        // good, width-matching frame), so the next frame stitches normally.
        let page = synthetic_page(W, H + 16, 0x7A11_0001);
        let first = frame(&page, W, 0, H);
        let mut s = Stitcher::new(W, H, MAX_H).unwrap();
        assert!(s.fill_first(&first));
        let before = s.total_height();
        let wrong = frame(&page, W, 0, H);
        assert!(matches!(
            s.append(&wrong, W + 1, H, 0),
            Err(StitchError::WidthMismatch { .. })
        ));
        assert_eq!(
            s.total_height(),
            before,
            "skipped frame leaves buffer intact"
        );
        assert_eq!(s.segment_count(), 1);
        let good = frame(&page, W, 5, H);
        assert!(s.append(&good, W, H, H - 5).is_ok());
        assert_eq!(s.total_height(), before + 5, "next frame stitches again");
        let (px, w, h) = s.into_pixels();
        assert_eq!((w as usize, h as usize), (W, H + 5));
        assert_eq!(&px[..first.len()], &first[..], "stitched rows intact");
    }

    /// B regression: the real hard limits still terminate the session.
    #[test]
    fn height_cap_and_budget_still_stop() {
        use crate::image_budget::{BudgetError, MAX_SIDE};
        let cap = StitchError::HeightCap {
            total: 10,
            extra: 5,
            max: 12,
        };
        let budget = StitchError::Budget(BudgetError::TooWide { width: 1, max: 1 });
        assert!(!is_transient(&cap));
        assert!(!is_transient(&budget));
        assert_eq!(stop_reason(&cap), StopReason::HeightCap);
        assert_eq!(stop_reason(&budget), StopReason::Budget);

        // End-to-end height cap: 5 new rows exceed a H+3 cap.
        let page = synthetic_page(W, H + 16, 0x0CA9_0001);
        let first = frame(&page, W, 0, H);
        let mut stitcher = Stitcher::new(W, H, H + 3).unwrap();
        assert!(stitcher.fill_first(&first));
        let gray = bgra_to_gray(&first, W, H).unwrap();
        let worker = LongWorker::new(1, Axis::Vertical, stitcher, gray).unwrap();
        let mut gate = ResultGate::new(1);
        let seq = gate.issue(gate.epoch()).unwrap();
        assert!(worker.submit(job(seq, 1, 0, frame(&page, W, 5, H))));
        let done = wait(&worker);
        assert!(gate.accept(&done));
        let reason = match &done.outcome {
            Outcome::Stopped { reason } => Some(*reason),
            _ => None,
        };
        assert_eq!(
            reason,
            Some(StopReason::HeightCap),
            "a height cap must still stop: {:?}",
            done.outcome
        );

        // End-to-end image budget: a frame wider than MAX_SIDE is refused by
        // bgra_to_gray before any stitch, surfacing as a Budget stop.
        let mut s2 = Stitcher::new(W, H, MAX_H).unwrap();
        assert!(s2.fill_first(&first));
        let g2 = bgra_to_gray(&first, W, H).unwrap();
        let wide_worker = LongWorker::new(2, Axis::Vertical, s2, g2).unwrap();
        let mut gate2 = ResultGate::new(2);
        let seq = gate2.issue(gate2.epoch()).unwrap();
        let mut wide_job = job(seq, 2, 0, first.clone());
        wide_job.width = MAX_SIDE as usize + 1;
        assert!(wide_worker.submit(wide_job));
        let done = wait(&wide_worker);
        assert!(gate2.accept(&done));
        let reason = match &done.outcome {
            Outcome::Stopped { reason } => Some(*reason),
            _ => None,
        };
        assert_eq!(
            reason,
            Some(StopReason::Budget),
            "an image-budget limit must still stop: {:?}",
            done.outcome
        );
    }

    // ---- regression: the alignment baseline deadlocks between stitches ----

    /// A "smooth-scroll / compositor" frame: every row is a 3-row box blur of
    /// the page starting at `start_row`, i.e. the sub-pixel resampling of an
    /// in-flight wheel animation. All such frames share the same resampling,
    /// so two of them align exactly; a clean settled frame does NOT match a
    /// blurred one (their overlap never clears `VERIFY_MIN_SCORE`).
    ///
    /// This models the diagnostic log: after a stitch at a clean settled
    /// frame, the first in-flight frame cannot align against that clean
    /// baseline, and because `prev_gray` only advanced on a stitch the session
    /// then re-judged every later frame against the same dead baseline -- the
    /// 22-46 identical `verify=0` ticks and no stitches.
    fn smooth_frame(page: &[u8], width: usize, start_row: usize, height: usize) -> Vec<u8> {
        let row = width * 4;
        let mut out = vec![0u8; row * height];
        for y in 0..height {
            for x in 0..row {
                let a = page[(start_row + y) * row + x] as u32;
                let b = page[(start_row + y + 1) * row + x] as u32;
                let c = page[(start_row + y + 2) * row + x] as u32;
                out[y * row + x] = ((a + b + c) / 3) as u8;
            }
        }
        out
    }

    fn tall_worker(first: &[u8], w: usize, h: usize, generation: u64) -> LongWorker {
        let mut stitcher = Stitcher::new(w, h, MAX_H).unwrap();
        assert!(stitcher.fill_first(first));
        let gray = bgra_to_gray(first, w, h).unwrap();
        LongWorker::new(generation, Axis::Vertical, stitcher, gray).unwrap()
    }

    fn tall_job(seq: u64, generation: u64, epoch: u64, w: usize, h: usize, bgra: Vec<u8>) -> Job {
        Job {
            generation,
            epoch,
            seq,
            axis: Axis::Vertical,
            width: w,
            height: h,
            bgra,
            capture_ms: 0.0,
            late: 0,
        }
    }

    fn is_progress(done: &Done) -> bool {
        matches!(
            done.outcome,
            Outcome::Frame(FrameResult {
                verdict: Verdict::Progress { .. },
                ..
            })
        )
    }

    /// RED on the current code: a clean settled anchor followed by a run of
    /// smooth-scroll frames (all sub-pixel resampled) can never be aligned
    /// against the frozen clean baseline, so no frame stitches. The fixed
    /// worker keeps a sliding alignment reference and stitches every scroll
    /// frame.
    #[test]
    fn reject_baseline_deadlocks_after_a_smooth_scroll() {
        const TW: usize = 32;
        const TH: usize = 240;
        let page = synthetic_page(TW, TH + 8 * 40 + 8, 0x5A00_0001);
        let first = frame(&page, TW, 0, TH);
        let mut worker = tall_worker(&first, TW, TH, 1);
        let mut gate = ResultGate::new(1);

        let mut progress = 0usize;
        // tick 0 is the same scroll position but resampled (no new rows);
        // ticks 1.. are 40-row smooth-scroll steps.
        for (i, start) in [0usize, 40, 80, 120, 160, 200, 240, 280].iter().enumerate() {
            let seq = gate.issue(gate.epoch()).unwrap();
            let f = smooth_frame(&page, TW, *start, TH);
            assert!(worker.submit(tall_job(seq, 1, 0, TW, TH, f)));
            let done = wait(&worker);
            assert!(gate.accept(&done), "tick {i} dropped");
            if is_progress(&done) {
                progress += 1;
            }
        }
        assert!(
            progress >= 7,
            "every smooth-scroll frame after the first must stitch, got {progress}/8"
        );
        // No row scrolled during the rejects was dropped or duplicated.
        let (px, w, h) = worker.finish(FINISH_WAIT).unwrap();
        assert_eq!((w as usize, h as usize), (TW, TH + 7 * 40));
        let row = TW * 4;
        let mut expected = first.clone();
        for start in [40usize, 80, 120, 160, 200, 240, 280] {
            let f = smooth_frame(&page, TW, start, TH);
            expected.extend_from_slice(&f[(TH - 40) * row..TH * row]);
        }
        assert_eq!(px, expected, "every smooth-scrolled row is in the image");
    }

    /// RED on the current code: 105-row wheel notches captured as in-flight
    /// smooth frames never match the frozen clean baseline, so the session
    /// stitches nothing; the fixed worker stitches every frame.
    #[test]
    fn natural_wheel_scroll_stitches_every_frame() {
        const TW: usize = 32;
        const TH: usize = 400;
        let page = synthetic_page(TW, TH + 6 * 105 + 8, 0x5A00_0002);
        let first = frame(&page, TW, 0, TH);
        let mut worker = tall_worker(&first, TW, TH, 1);
        let mut gate = ResultGate::new(1);

        let mut progress = 0usize;
        for (i, start) in [0usize, 105, 210, 315, 420, 525, 630].iter().enumerate() {
            let seq = gate.issue(gate.epoch()).unwrap();
            let f = smooth_frame(&page, TW, *start, TH);
            assert!(worker.submit(tall_job(seq, 1, 0, TW, TH, f)));
            let done = wait(&worker);
            assert!(gate.accept(&done), "tick {i} dropped");
            if is_progress(&done) {
                progress += 1;
            }
        }
        assert_eq!(
            progress, 6,
            "every 105-row wheel step must stitch, got {progress}/7"
        );
        // No wheel step was dropped or duplicated.
        let (px, w, h) = worker.finish(FINISH_WAIT).unwrap();
        assert_eq!((w as usize, h as usize), (TW, TH + 6 * 105));
        let row = TW * 4;
        let mut expected = first.clone();
        for start in [105usize, 210, 315, 420, 525, 630] {
            let f = smooth_frame(&page, TW, start, TH);
            expected.extend_from_slice(&f[(TH - 105) * row..TH * row]);
        }
        assert_eq!(px, expected, "every 105-row wheel step is in the image");
    }

    /// "不丢内容": ticks that the anchor report rejects but during which no
    /// rows scroll (static / local animation) must not create gaps. Every
    /// scrolled row still lands in the exported image.
    #[test]
    fn rejected_ticks_stitch_every_scrolled_row() {
        const TW: usize = 32;
        const TH: usize = 240;
        let page = synthetic_page(TW, TH + 4 * 40 + 8, 0x5A00_0003);
        let first = frame(&page, TW, 0, TH);
        let mut worker = tall_worker(&first, TW, TH, 1);
        let mut gate = ResultGate::new(1);

        // A no-scroll resampled frame primes the sliding reference; then each
        // 40-row step is followed by a static frame at the same position, so
        // half the ticks are rejected by the anchor report.
        let starts = [0usize, 40, 40, 80, 80, 120, 120, 160, 160];
        let mut progress = 0usize;
        for (i, start) in starts.iter().enumerate() {
            let seq = gate.issue(gate.epoch()).unwrap();
            let f = smooth_frame(&page, TW, *start, TH);
            assert!(worker.submit(tall_job(seq, 1, 0, TW, TH, f)));
            let done = wait(&worker);
            assert!(gate.accept(&done), "tick {i} dropped");
            if is_progress(&done) {
                progress += 1;
            }
        }
        assert_eq!(progress, 4, "one stitch per scroll step");

        let (px, w, h) = worker.finish(FINISH_WAIT).unwrap();
        assert_eq!((w as usize, h as usize), (TW, TH + 4 * 40));
        let row = TW * 4;
        let mut expected = first.clone();
        for start in [40usize, 80, 120, 160] {
            let f = smooth_frame(&page, TW, start, TH);
            expected.extend_from_slice(&f[(TH - 40) * row..TH * row]);
        }
        assert_eq!(px, expected, "no row was dropped across the reject ticks");
    }

    /// "不误拼": exactly periodic rows match the strip several ways at once.
    /// The sliding pass must NOT turn that ambiguity into a stitch -- the
    /// runner-up / independent-band guards still refuse every offset.
    #[test]
    fn periodic_content_is_not_stitched_via_the_sliding_path() {
        const TW: usize = 32;
        const TH: usize = 240;
        const PERIOD: usize = 8;
        let row = TW * 4;
        // 8 distinct rows repeated: the 32-row strip matches itself 4 ways.
        let mut base = vec![0u8; PERIOD * row];
        let mut state = 0x9E37_0007u32;
        for b in base.iter_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (state >> 24) as u8;
        }
        let mut page = vec![0u8; (TH + 40) * row];
        for r in 0..TH + 40 {
            let src = (r % PERIOD) * row;
            page[r * row..(r + 1) * row].copy_from_slice(&base[src..src + row]);
        }
        let first = frame(&page, TW, 0, TH);
        let mut worker = tall_worker(&first, TW, TH, 1);
        let mut gate = ResultGate::new(1);

        let mut progress = 0usize;
        for (i, start) in [0usize, 3, 6, 9, 12, 15, 18, 21].iter().enumerate() {
            let seq = gate.issue(gate.epoch()).unwrap();
            let f = frame(&page, TW, *start, TH);
            assert!(worker.submit(tall_job(seq, 1, 0, TW, TH, f)));
            let done = wait(&worker);
            assert!(gate.accept(&done), "tick {i} dropped");
            if is_progress(&done) {
                progress += 1;
            }
        }
        assert_eq!(progress, 0, "periodic content must never be stitched");
        let (px, w, h) = worker.finish(FINISH_WAIT).unwrap();
        assert_eq!((w as usize, h as usize), (TW, TH), "stitcher untouched");
        assert_eq!(px, first);
    }

    /// The signed `pending` handles upward smooth scrolls: fresh rows enter at
    /// the top and are prepended, still covering every step.
    #[test]
    fn upward_smooth_scroll_stitches_every_frame() {
        const TW: usize = 32;
        const TH: usize = 240;
        let page = synthetic_page(TW, TH + 8 * 40 + 8, 0x5A00_0004);
        let first = frame(&page, TW, 320, TH);
        let mut worker = tall_worker(&first, TW, TH, 1);
        let mut gate = ResultGate::new(1);

        let mut progress = 0usize;
        let mut stitched_rows = 0usize;
        for (i, start) in [320usize, 280, 240, 200, 160, 120, 80, 40]
            .iter()
            .enumerate()
        {
            let seq = gate.issue(gate.epoch()).unwrap();
            let f = smooth_frame(&page, TW, *start, TH);
            assert!(worker.submit(tall_job(seq, 1, 0, TW, TH, f)));
            let done = wait(&worker);
            assert!(gate.accept(&done), "tick {i} dropped");
            if let Outcome::Frame(fr) = &done.outcome {
                if let Verdict::Progress { new_rows, .. } = fr.verdict {
                    progress += 1;
                    stitched_rows += new_rows;
                }
            }
        }
        // The session must never deadlock. A tick that the matcher refused is
        // fine: its rows are carried in `pending` and stitched together with
        // the next confirmed frame (a single 80-row stitch here), so the total
        // is exactly the 7 * 40 rows the page scrolled.
        assert!(progress >= 6, "an up-scroll must keep progressing");
        assert_eq!(stitched_rows, 7 * 40, "no up-scrolled row is lost");

        let (px, w, h) = worker.finish(FINISH_WAIT).unwrap();
        assert_eq!((w as usize, h as usize), (TW, TH + 7 * 40));
        let row = TW * 4;
        let mut expected = Vec::new();
        // Prepend order: the last up step ends up at the very top.
        for start in [40usize, 80, 120, 160, 200, 240, 280] {
            let f = smooth_frame(&page, TW, start, TH);
            expected.extend_from_slice(&f[..40 * row]);
        }
        expected.extend_from_slice(&first);
        assert_eq!(px, expected, "every up-scrolled row is in the image");
    }

    /// A re-baseline (axis switch / new epoch) rebuilds BOTH references and
    /// clears the pending rows, so no stale sliding baseline or accumulated
    /// offset from the old epoch can leak into the new session.
    #[test]
    fn rebaseline_resets_the_sliding_reference() {
        const TW: usize = 32;
        const TH: usize = 240;
        let page = synthetic_page(TW, TH + 40 + 8, 0x5A00_0005);
        let first = frame(&page, TW, 0, TH);
        let mut worker = tall_worker(&first, TW, TH, 1);
        let mut gate = ResultGate::new(1);

        // Move the sliding reference away from the anchor with a smooth frame.
        let seq = gate.issue(0).unwrap();
        assert!(worker.submit(tall_job(seq, 1, 0, TW, TH, smooth_frame(&page, TW, 0, TH))));
        let _ = wait(&worker);

        gate.rebaseline(1);
        let (norm, nw, nh) = normalize_bgra(&first, TW, TH, Axis::Vertical).unwrap();
        let mut stitcher = Stitcher::new(nw, nh, MAX_H).unwrap();
        assert!(stitcher.fill_first(norm.as_ref()));
        let gray = bgra_to_gray(norm.as_ref(), nw, nh).unwrap();
        worker.rebaseline(Rebase {
            generation: 1,
            epoch: 1,
            axis: Axis::Vertical,
            stitcher,
            prev_gray: gray,
        });

        // A clean 5-row scroll must stitch exactly 5 rows against the fresh
        // baseline, not a leftover pending offset from the previous epoch.
        let seq = gate.issue(1).unwrap();
        assert!(worker.submit(tall_job(seq, 1, 1, TW, TH, frame(&page, TW, 5, TH))));
        let done = wait(&worker);
        assert!(gate.accept(&done), "the new epoch must apply");
        match &done.outcome {
            Outcome::Frame(f) => assert!(
                matches!(
                    f.verdict,
                    Verdict::Progress {
                        is_up: false,
                        new_rows: 5
                    }
                ),
                "unexpected verdict {:?}",
                f.verdict
            ),
            other => panic!("expected a frame, got {other:?}"),
        }
        let (px, w, h) = worker.finish(FINISH_WAIT).unwrap();
        assert_eq!((w as usize, h as usize), (TW, TH + 5));
        assert_eq!(px.len(), TW * (TH + 5) * 4);
    }
}
