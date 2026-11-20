//! Background half of the long-capture tick (P1-01).
//!
//! The GPUI main thread may only touch the D3D11 device/context, the `Window`
//! and GPUI state, so the capture and the staging-map readback stay there. The
//! D3D11 immediate context is single-threaded and `cx.notify` is UI-only, so
//! neither can move. Everything after the readback can: ORB feature alignment
//! and stitching (via the vendored snow-shot `snow-stitch-images` crate) and the
//! per-tick `diag.log` line all run here, on one long-lived worker that owns the
//! [`OrbStitcher`] and the `StallTracker`. The UI thread sends one [`Job`] at a
//! time and applies the matching [`Done`] when it comes back, so no frame-sized
//! work happens between a capture and its verdict on the main thread.
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
    Axis, ScrollReport, StallPhase, StallTracker, StitchError, StitchStats, MOTION_CHANGED,
};
use crate::orb_stitcher::OrbStitcher;

/// Sampled pixel-change fraction above which a rejected tick is reported as
/// "the page moved faster than the overlap" instead of silence.
pub const FAST_SCROLL_CHANGED: f64 = 0.45;

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
///
/// Field names are kept for diagnostic/log compatibility with the old SAD
/// pipeline. With the ORB engine `normalize`/`gray` are no longer separate
/// stages (the adapter folds BGRA→RGBA into one pass), so they are reported as
/// zero; `sad` now carries the whole `OrbStitcher::push` (feature detection,
/// matching and canvas write) and `stitch` is likewise folded into it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Spans {
    pub capture: f64,
    pub normalize: f64,
    pub gray: f64,
    pub sad: f64,
    pub stitch: f64,
    pub log: f64,
    /// Time spent generating the live-preview thumbnail (progress frames only).
    pub preview: f64,
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
    /// The content jumped to a new view (navigation / reflow), detected by the
    /// estimator's 3-frame SceneCut streak. Not a pause, not a match failure:
    /// the host keeps full poll speed and shows a distinct hint.
    SceneCut,
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

/// Target width of the live preview thumbnail (screen pixels). The height
/// follows the captured aspect ratio and is capped by `OrbStitcher`.
const PREVIEW_WIDTH: u32 = 320;

/// One incremental band of the live preview, generated on the worker thread and
/// sent to the UI while scrolling. `pixels` is tight BGRA (stride =
/// `width * 4`), the same convention as every other pixel buffer in FastPin.
///
/// The UI accumulates bands into one buffer: a vertical session appends rows at
/// the bottom (`top_offset = full_height - height`, `prepend_shift = 0`) or
/// inserts them at the top for an upward scroll (`top_offset = 0`,
/// `prepend_shift = height`). `reset` means the band is a complete render and
/// the accumulator must be discarded first.
#[derive(Clone, Debug)]
pub struct LivePreview {
    pub pixels: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub top_offset: u32,
    pub full_height: u32,
    pub prepend_shift: u32,
    pub reset: bool,
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
    /// A fresh thumbnail on frames that changed the canvas (or the first
    /// frame). `None` means the UI keeps showing its previous preview.
    pub preview: Option<LivePreview>,
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
    pub stitcher: OrbStitcher,
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
    /// ORB feature stitcher (snow-shot). It owns the canvas, the synthetic
    /// reference frame and the ORB descriptors; there is no separate grayscale
    /// anchor / sliding reference / pending-row bookkeeping any more — snow's
    /// estimator handles smooth / sub-pixel motion directly.
    stitcher: OrbStitcher,
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
            stall: StallTracker::new(),
            ticks: 0,
            last_log_ms: 0.0,
        }
    }
}

/// Final buffer in screen space: tight BGRA plus its size.
type FinishReply = Result<(Vec<u8>, u32, u32), String>;

/// A hover-preview render: a read-only slice of the canvas, so it can run
/// between frames without disturbing the stitcher's incremental state.
#[derive(Clone, Copy, Debug)]
pub struct RegionJob {
    pub generation: u64,
    /// First canvas row in screen space, inclusive.
    pub top: u32,
    /// Exclusive end canvas row in screen space.
    pub bottom: u32,
    /// Upper bound on the cross axis of the result.
    pub target_width: u32,
}

type RegionReply = Option<(Vec<u8>, u32, u32)>;

enum Cmd {
    Frame(Job),
    /// Boxed: `Rebase` carries a whole `OrbStitcher`, which would otherwise
    /// make every `Cmd` — including the hot `Frame` path — pay for its size.
    Rebaseline(Box<Rebase>),
    /// A hover preview. Never queued ahead of a frame by the caller, and the
    /// reply is bounded so a session torn down mid-render cannot block.
    Render(RegionJob, SyncSender<RegionReply>),
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
    pub fn new(generation: u64, axis: Axis, stitcher: OrbStitcher) -> std::io::Result<Self> {
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
            let _ = tx.send(Cmd::Rebaseline(Box::new(rebase)));
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

    /// Renders one canvas slice for the hover preview.
    ///
    /// Runs on the worker thread because the stitcher lives there; `None` means
    /// the session was already gone, which the caller treats as "no preview".
    pub fn render_region(&self, job: RegionJob, wait: Duration) -> RegionReply {
        let tx = self.tx.as_ref()?;
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if tx.send(Cmd::Render(job, reply_tx)).is_err() {
            return None;
        }
        reply_rx.recv_timeout(wait).unwrap_or_default()
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
            Cmd::Rebaseline(rb) => session = Some(SessionState::rebased(*rb)),
            Cmd::Render(job, reply) => {
                // A render is a read-only query: a stale generation, or a
                // session that ended, answers with nothing rather than an
                // error, because the hover preview is best-effort.
                let pixels = match session.as_ref() {
                    Some(state) if state.generation == job.generation => state
                        .stitcher
                        .render_region(job.top, job.bottom, job.target_width)
                        .unwrap_or_else(|e| {
                            crate::perf::log_diag(&format!("hover preview skipped: {e}"));
                            None
                        }),
                    _ => None,
                };
                let _ = reply.send(pixels);
            }
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

/// Materializes the ORB canvas into screen-space BGRA. The adapter handles the
/// RGBA→BGRA conversion and the inverse rotation of a horizontal session.
fn finalize(state: SessionState) -> Result<(Vec<u8>, u32, u32), String> {
    state.stitcher.finish().map_err(|e| e.to_string())
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

/// Generates a live-preview band on the worker thread. A preview failure is
/// never fatal -- the UI simply keeps showing its previous thumbnail.
fn live_preview(stitcher: &mut OrbStitcher) -> Option<LivePreview> {
    match stitcher.preview_patch(PREVIEW_WIDTH) {
        Ok(Some(patch)) => Some(LivePreview {
            pixels: Arc::new(patch.pixels),
            width: patch.width,
            height: patch.height,
            top_offset: patch.top_offset,
            full_height: patch.full_height,
            prepend_shift: patch.prepend_shift,
            reset: patch.reset,
        }),
        Ok(None) => None,
        Err(e) => {
            crate::perf::log_diag(&format!("live preview skipped: {e}"));
            None
        }
    }
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

    state.ticks = state.ticks.wrapping_add(1);
    // The whole alignment + composition now lives in the ORB stitcher. The
    // `sad` span is kept as the legacy name of the per-frame alignment cost.
    let t = Instant::now();
    let outcome = state.stitcher.push(&job.bgra, job.width, job.height);
    spans.sad = ms(t);

    // snow has no segment notion; the number of pushed frames is what the
    // overlay shows in the status line, so use that as a stand-in.
    let segments = state.stitcher.input_count();
    let total = state.stitcher.canvas_height();

    let orb = match outcome {
        Ok(v) => v,
        Err(e) => {
            crate::perf::log_diag(&format!("long-capture stopped: {e}"));
            if is_transient(&e) {
                crate::perf::log_diag(&format!("long-tick skip: {e}"));
                return Outcome::Frame(FrameResult {
                    verdict: Verdict::FrameSkipped,
                    paused: state.stall.paused(),
                    segments,
                    total_height: total,
                    stats: state.stitcher.stats(),
                    spans,
                    preview: None,
                });
            }
            return Outcome::Stopped {
                reason: stop_reason(&e),
            };
        }
    };

    // The first frame only initializes the stitcher; it carries no motion.
    // It did establish the canvas, so seed the preview from it.
    let Some(orb) = orb else {
        spans.process = ms(started);
        return Outcome::Frame(FrameResult {
            verdict: Verdict::Idle,
            paused: state.stall.paused(),
            segments,
            total_height: total,
            stats: state.stitcher.stats(),
            spans,
            preview: live_preview(&mut state.stitcher),
        });
    };

    let report = orb.report;
    spans.process = ms(started);
    let t = Instant::now();
    crate::perf::log_diag(&format!(
        "long-tick t{} axis={} elapsed_ms={:.1} ui_ms={:.1} proc_ms={:.1} \
         orb_ms={:.1} score={:.3} is_up={} new_rows={} total={}px \
         seg={} late={}",
        state.ticks,
        state.axis.label(),
        spans.total(),
        spans.capture,
        spans.process,
        spans.sad,
        report.overlap_score,
        report.direction.map(|d| d.0).unwrap_or(false),
        orb.growth,
        total,
        segments,
        job.late,
    ));
    spans.log = ms(t);
    state.last_log_ms = spans.log;

    let phase = state.stall.observe(&report);
    let verdict = if orb.accepted && orb.growth > 0 {
        let is_up = report.direction.map(|d| d.0).unwrap_or(false);
        Verdict::Progress {
            is_up,
            new_rows: orb.growth,
        }
    } else {
        idle_verdict(phase, &report)
    };

    // Refresh the thumbnail only when the canvas actually grew. On idle /
    // unaligned ticks the UI keeps its previous preview, so a stall does not
    // pay for a full-canvas downsample on every poll.
    let preview = if orb.accepted && orb.growth > 0 {
        let t = Instant::now();
        let p = live_preview(&mut state.stitcher);
        spans.preview = ms(t);
        p
    } else {
        None
    };

    Outcome::Frame(FrameResult {
        verdict,
        paused: state.stall.paused(),
        segments,
        total_height: total,
        stats: state.stitcher.stats(),
        spans,
        preview,
    })
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
        StallPhase::SceneCut => Verdict::SceneCut,
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
    use std::time::Duration;

    // The document generator mirrors snow-shot's own `document()` test helper:
    // a deterministic high-frequency texture that ORB can lock onto at any
    // row. A plain gradient/solid frame is deliberately avoided — feature
    // matching needs texture, which is exactly why the old SAD matcher failed
    // on smooth scrolling.
    const W: usize = 320;
    const H: usize = 256;
    const MAX_H: usize = 16384;

    fn doc_row(x: usize, y: usize) -> u8 {
        let mut hash = (x as u32).wrapping_mul(0xc2b2_ae35) ^ (y as u32).wrapping_mul(0x27d4_eb2d);
        hash ^= hash >> 16;
        hash = hash.wrapping_mul(0x7feb_352d);
        hash ^= hash >> 15;
        (hash >> 24) as u8
    }

    fn page(width: usize, rows: usize) -> Vec<u8> {
        let mut px = vec![0u8; width * rows * 4];
        for y in 0..rows {
            for x in 0..width {
                let v = doc_row(x, y);
                let i = (y * width + x) * 4;
                px[i] = v;
                px[i + 1] = v;
                px[i + 2] = v;
                px[i + 3] = 255;
            }
        }
        px
    }

    fn frame(page: &[u8], width: usize, start_row: usize, height: usize) -> Vec<u8> {
        page[start_row * width * 4..(start_row + height) * width * 4].to_vec()
    }

    fn worker(first: &[u8], generation: u64) -> LongWorker {
        let mut stitcher = OrbStitcher::new(Axis::Vertical, W, H, MAX_H).unwrap();
        stitcher.push(first, W, H).unwrap();
        LongWorker::new(generation, Axis::Vertical, stitcher).unwrap()
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

    fn wait(worker: &LongWorker) -> Done {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match worker.poll() {
                Poll::Done(d) => return d,
                Poll::Empty => {
                    assert!(Instant::now() < deadline, "timeout waiting for worker");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Poll::Gone => panic!("worker exited before producing a result"),
            }
        }
    }

    fn assert_progress(done: &Done, is_up: bool, new_rows: usize) {
        let Outcome::Frame(frame) = &done.outcome else {
            panic!("expected a frame outcome, got {:?}", done.outcome);
        };
        assert_eq!(
            frame.verdict,
            Verdict::Progress { is_up, new_rows },
            "unexpected verdict: {:?}",
            frame.verdict
        );
    }

    #[test]
    fn worker_stitches_a_synthetic_scroll_session() {
        let pg = page(W, H + 200);
        let mut w = worker(&frame(&pg, W, 0, H), 1);
        for (i, start) in [40usize, 80, 120].into_iter().enumerate() {
            w.submit(job(i as u64 + 1, 1, 0, frame(&pg, W, start, H)));
            assert_progress(&wait(&w), false, 40);
        }
        let (px, fw, fh) = w.finish(Duration::from_secs(2)).unwrap();
        assert_eq!((fw as usize, fh as usize), (W, H + 120));
        assert_eq!(px.len(), W * (H + 120) * 4);
    }

    #[test]
    fn progress_frames_carry_a_live_preview() {
        let pg = page(W, H + 200);
        let w = worker(&frame(&pg, W, 0, H), 1);
        // Each accepted progress frame must carry a downsampled BGRA band whose
        // geometry fits the (growing) canvas, and the bands must accumulate into
        // that full preview without gaps or overlaps.
        let row_bytes = |band: &super::LivePreview| band.width as usize * 4;
        let mut accumulated = 0u32;
        for (i, start) in [40usize, 80, 120].into_iter().enumerate() {
            w.submit(job(i as u64 + 1, 1, 0, frame(&pg, W, start, H)));
            let done = wait(&w);
            let Outcome::Frame(result) = &done.outcome else {
                panic!("expected a frame outcome, got {:?}", done.outcome);
            };
            let preview = result
                .preview
                .as_ref()
                .expect("a progress frame must carry a live preview");
            assert_eq!(
                preview.pixels.len(),
                row_bytes(preview) * preview.height as usize
            );
            // Downsampled: never wider than the full capture.
            assert!(preview.width <= W as u32);
            // 320-wide capture at the 320-wide preview target is 1:1, so the
            // preview height tracks the canvas exactly.
            assert_eq!(
                preview.full_height,
                (H + (i + 1) * 40) as u32,
                "full_height must follow the growing canvas"
            );
            if i == 0 {
                assert!(preview.reset, "first band must be a complete render");
                assert_eq!(preview.top_offset, 0);
                // A complete render covers the whole canvas, so its band is as
                // tall as the preview it produces.
                assert_eq!(preview.height, preview.full_height);
            } else {
                assert!(!preview.reset, "later bands must be incremental");
                assert_eq!(preview.prepend_shift, 0);
                // The band redraws snow's whole edge band, splice overlap
                // included, and always ends at the preview's new bottom.
                assert_eq!(
                    preview.top_offset + preview.height,
                    preview.full_height,
                    "an append band must end at the bottom of the preview"
                );
                assert_eq!(
                    preview.height,
                    (H / 2) as u32,
                    "the band spans the rewired edge band, not just the new rows"
                );
            }
            accumulated = preview.full_height;
        }
        assert_eq!(accumulated, (H + 120) as u32);
    }

    #[test]
    fn worker_stitches_an_upward_scroll_as_prepend() {
        let pg = page(W, H + 200);
        // Start deep in the document, then scroll up.
        let mut w = worker(&frame(&pg, W, 120, H), 1);
        for (i, start) in [80usize, 40, 0].into_iter().enumerate() {
            w.submit(job(i as u64 + 1, 1, 0, frame(&pg, W, start, H)));
            assert_progress(&wait(&w), true, 40);
        }
        let (_px, fw, fh) = w.finish(Duration::from_secs(2)).unwrap();
        assert_eq!((fw as usize, fh as usize), (W, H + 120));
    }

    #[test]
    fn worker_discards_a_job_from_an_old_generation() {
        let pg = page(W, H + 100);
        let w = worker(&frame(&pg, W, 0, H), 7);
        w.submit(job(1, 6, 0, frame(&pg, W, 40, H)));
        let done = wait(&w);
        assert!(matches!(done.outcome, Outcome::Discarded));
    }

    #[test]
    fn gate_allows_one_job_in_flight() {
        let mut gate = ResultGate::new(1);
        assert_eq!(gate.issue(0), Some(0));
        assert_eq!(gate.issue(0), None); // in flight
        let done = Done {
            generation: 1,
            epoch: 0,
            seq: 0,
            outcome: Outcome::Frame(FrameResult {
                verdict: Verdict::Idle,
                paused: false,
                segments: 1,
                total_height: H,
                stats: StitchStats::default(),
                spans: Spans::default(),
                preview: None,
            }),
        };
        assert!(gate.accept(&done));
        assert_eq!(gate.issue(0), Some(1));
    }

    #[test]
    fn gate_drops_duplicate_and_out_of_order_results() {
        let mut gate = ResultGate::new(1);
        assert_eq!(gate.issue(0), Some(0));
        let done = Done {
            generation: 1,
            epoch: 0,
            seq: 0,
            outcome: Outcome::Discarded,
        };
        assert!(gate.accept(&done));
        assert!(!gate.accept(&done)); // duplicate / already applied
    }

    #[test]
    fn result_from_the_previous_axis_is_dropped() {
        let pg = page(W, H + 100);
        let w = worker(&frame(&pg, W, 0, H), 1);
        // Rebaseline to a fresh ORB stitcher on the other axis.
        let mut next = OrbStitcher::new(Axis::Horizontal, W, H, MAX_H).unwrap();
        next.push(&frame(&pg, W, 0, H), W, H).unwrap();
        w.rebaseline(Rebase {
            generation: 1,
            epoch: 1,
            axis: Axis::Horizontal,
            stitcher: next,
        });
        // A frame stamped with the old epoch is discarded.
        w.submit(job(1, 1, 0, frame(&pg, W, 40, H)));
        let done = wait(&w);
        assert!(matches!(done.outcome, Outcome::Discarded));
    }

    #[test]
    fn cancelled_worker_produces_no_applicable_result() {
        let pg = page(W, H + 100);
        let w = worker(&frame(&pg, W, 0, H), 1);
        w.cancel();
        w.submit(job(1, 1, 0, frame(&pg, W, 40, H)));
        match w.poll() {
            Poll::Done(d) => assert!(!matches!(d.outcome, Outcome::Frame(_))),
            Poll::Empty | Poll::Gone => {}
        }
    }

    #[test]
    fn stalled_phase_maps_to_a_recoverable_verdict() {
        // A frozen page produces no motion; the verdict must still be a
        // recoverable state, never a hard stop, so scrolling can resume.
        let report = ScrollReport {
            changed: 0.0,
            ..ScrollReport::default()
        };
        let verdict = idle_verdict(StallPhase::Paused, &report);
        assert!(matches!(verdict, Verdict::Paused));
    }

    /// A scroll that outruns the overlap strip once must not poison the session.
    ///
    /// The failure this guards: after one refused offset snow keeps comparing
    /// against the stale `synthetic_reference` instead of the frame the user is
    /// now looking at, so every later frame also fails to match and the capture
    /// stops after the first few rows.
    #[test]
    fn one_overshoot_does_not_stop_the_rest_of_the_capture() {
        let pg = page(W, H + 600);
        let w = worker(&frame(&pg, W, 0, H), 1);

        // 40 px as usual, then one jump far past the 60% motion ceiling.
        w.submit(job(1, 1, 0, frame(&pg, W, 40, H)));
        assert_progress(&wait(&w), false, 40);
        w.submit(job(2, 1, 0, frame(&pg, W, 40 + 220, H)));
        let overshoot = wait(&w);
        let Outcome::Frame(overshoot_frame) = &overshoot.outcome else {
            panic!(
                "an overshoot must not end the session: {:?}",
                overshoot.outcome
            );
        };
        assert!(
            !matches!(overshoot_frame.verdict, Verdict::Progress { .. }),
            "an unalignable jump cannot report progress: {:?}",
            overshoot_frame.verdict
        );

        // Back to a scrollable pace: the session must resume from where the
        // page actually is, not from the stale reference.
        w.submit(job(3, 1, 0, frame(&pg, W, 40 + 220 + 40, H)));
        let resumed = wait(&w);
        let Outcome::Frame(resumed_frame) = &resumed.outcome else {
            panic!("session died after one overshoot: {:?}", resumed.outcome);
        };
        assert!(
            matches!(resumed_frame.verdict, Verdict::Progress { .. }),
            "capture did not resume after one overshoot: {:?}",
            resumed_frame.verdict
        );
        w.cancel();
    }

    /// A pause in one place then a jump elsewhere: same stale-reference trap,
    /// reached through the `Contained`/`NoMovement` path instead of a too-large
    /// offset.
    #[test]
    fn a_pause_then_a_jump_does_not_poison_the_reference() {
        let pg = page(W, H + 600);
        let w = worker(&frame(&pg, W, 0, H), 1);

        w.submit(job(1, 1, 0, frame(&pg, W, 40, H)));
        assert_progress(&wait(&w), false, 40);

        // The page leaves and comes back to the same place: nothing to commit,
        // so snow keeps its reference and only refreshes `previous_raw`.
        w.submit(job(2, 1, 0, frame(&pg, W, 40, H)));
        let _ = wait(&w);

        // Now scroll on from there.
        w.submit(job(3, 1, 0, frame(&pg, W, 120, H)));
        let done = wait(&w);
        let Outcome::Frame(paused_frame) = &done.outcome else {
            panic!("session died after a pause: {:?}", done.outcome);
        };
        assert!(
            matches!(paused_frame.verdict, Verdict::Progress { .. }),
            "capture did not resume after a pause: {:?}",
            paused_frame.verdict
        );
        w.cancel();
    }
}
