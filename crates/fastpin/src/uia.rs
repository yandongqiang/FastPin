//! UI Automation / MSAA sub-element discovery for the region picker.
//!
//! Thin async adapter over the vendored `snow-ui-selector` crate — the exact
//! backend snow-shot uses. Two dedicated threads mirror snow-shot's C FFI
//! service layout:
//!
//! * a **query worker** owns an `ElementRegionService`, refreshes its window
//!   snapshot once per capture session, and answers a 168ms foreground pass;
//! * a **refinement worker** builds its own service from the published snapshot
//!   and runs the 1500ms progressive query that exposes the full tree of lazy
//!   providers (Chromium / Edge). A cursor move replaces the pending refine
//!   point (snow-shot's `RefinementQueue::replace`) but never cancels a query
//!   in flight — only a session change invalidates refinement.
//!
//! Replies travel on an 8-slot channel so one slow UI frame cannot drop the
//! final frame.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use snow_ui_selector::{
    AccessibilityBackend, ElementRegionService, HitTestMode, Point as SelectorPoint, QueryControl,
    WindowSnapshot,
};
use windows::Win32::Foundation::{HWND, POINT, RECT};

/// UI 侧等待一次 UIA 查询首帧的上限；超时即丢弃结果，UI 线程从不为 COM 阻塞。
pub const UIA_QUERY_TIMEOUT: Duration = Duration::from_millis(400);
const WORKER_IDLE_TICK: Duration = Duration::from_millis(50);
/// 回执通道容量。refinement 每 32ms 一帧，UI 掉一帧也不能丢最终帧。
const REPLY_CAPACITY: usize = 8;

/// One UIA element rect and its nesting depth (0 = app window → leaf).
#[derive(Clone, Copy)]
pub struct ElementRect {
    pub rect: RECT,
    pub depth: u32,
}

/// 一次异步查询的回执。`final_reply` 标记 refinement 阶段的最后一帧。
pub struct UiaReply {
    pub epoch: u64,
    pub rects: Vec<ElementRect>,
    pub final_reply: bool,
}

/// 异步查询句柄：`failed_fast` 表示请求没能入队（线程队列满 / 已退出）。
pub struct UiaQuery {
    pub rx: Receiver<UiaReply>,
    pub failed_fast: bool,
}

/// 投递给 query worker 的请求。
struct UiaRequest {
    pt: POINT,
    epoch: u64,
    session: u64,
    reply: SyncSender<UiaReply>,
}

/// Snapshot + generation. The query worker publishes; the refinement worker
/// rebuilds its service only when the generation changes.
struct SnapshotSlot {
    generation: u64,
    snapshot: Option<WindowSnapshot>,
}

static SNAPSHOT: OnceLock<Mutex<SnapshotSlot>> = OnceLock::new();

fn snapshot_slot() -> &'static Mutex<SnapshotSlot> {
    SNAPSHOT.get_or_init(|| {
        Mutex::new(SnapshotSlot {
            generation: 0,
            snapshot: None,
        })
    })
}

/// Pending refine command. A cursor move replaces it (snow-shot semantics);
/// the worker picks up the newest one at its next command boundary.
struct RefineCommand {
    pt: POINT,
    epoch: u64,
    session: u64,
    reply: SyncSender<UiaReply>,
}

static REFINE_PENDING: OnceLock<Mutex<Option<RefineCommand>>> = OnceLock::new();

fn refine_pending() -> &'static Mutex<Option<RefineCommand>> {
    REFINE_PENDING.get_or_init(|| Mutex::new(None))
}

static WORKER_TX: OnceLock<Mutex<Option<SyncSender<UiaRequest>>>> = OnceLock::new();
static WAKE_TX: OnceLock<Mutex<Option<SyncSender<()>>>> = OnceLock::new();

fn worker_tx() -> Option<SyncSender<UiaRequest>> {
    let slot = WORKER_TX.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(tx) = guard.as_ref() {
        return Some(tx.clone());
    }
    // 快速鼠标移动会连续投递查询；容量太小会让 try_send 频繁 Full 返回
    // failed_fast，被 UI 侧误当成 provider 超时而熔断整个窗口。留出足够
    // 背压空间，worker 会按自己的节奏处理最新请求。
    let (tx, rx) = mpsc::sync_channel::<UiaRequest>(8);
    thread::Builder::new()
        .name("fastpin-uia-query".to_string())
        .spawn(move || run_query_worker(rx))
        .ok()?;
    *guard = Some(tx.clone());
    Some(tx)
}

fn wake_tx() -> Option<SyncSender<()>> {
    let slot = WAKE_TX.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(tx) = guard.as_ref() {
        return Some(tx.clone());
    }
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    // OnceLock initialises this lazily, so both workers start exactly once.
    thread::Builder::new()
        .name("fastpin-uia-refine".to_string())
        .spawn(move || run_refine_worker(rx))
        .ok()?;
    *guard = Some(tx.clone());
    Some(tx)
}

/// Converts a snow-shot hit path (deepest → window) into FastPin's rect list.
fn convert_path(path: Vec<snow_ui_selector::ElementRect>) -> Vec<ElementRect> {
    let len = path.len();
    path.into_iter()
        .enumerate()
        .map(|(i, r)| ElementRect {
            rect: RECT {
                left: r.left(),
                top: r.top(),
                right: r.right(),
                bottom: r.bottom(),
            },
            depth: (len - i) as u32,
        })
        .collect()
}

fn query_point(pt: POINT) -> SelectorPoint {
    SelectorPoint {
        x: pt.x,
        y: pt.y,
        display_id: 0,
        window_id: None,
    }
}

/// Foreground worker: snapshot lifecycle + 168ms pass.
fn run_query_worker(rx: Receiver<UiaRequest>) {
    let mut service: Option<ElementRegionService> = None;
    let mut service_session: u64 = u64::MAX;
    loop {
        let req = match rx.recv_timeout(WORKER_IDLE_TICK) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if service.is_none() {
            if let Ok(svc) = ElementRegionService::with_backend_and_displays(
                AccessibilityBackend::Uia,
                &[],
                None,
            ) {
                service = Some(svc);
                service_session = req.session;
            }
        } else if service_session != req.session {
            // Only advance on success so a transient COM error is retried.
            if let Some(svc) = service.as_mut() {
                if svc.refresh_excluding_ids(&[]).is_ok() {
                    service_session = req.session;
                } else {
                    // Next request retries the refresh.
                    continue;
                }
            }
        }
        let Some(svc) = service.as_mut() else {
            let _ = req.reply.try_send(UiaReply {
                epoch: req.epoch,
                rects: Vec::new(),
                final_reply: true,
            });
            continue;
        };

        // Publish the snapshot with a fresh generation for the refine worker.
        if let Some(snapshot) = svc.window_snapshot() {
            let mut slot = snapshot_slot().lock().unwrap_or_else(|p| p.into_inner());
            slot.generation = slot.generation.wrapping_add(1);
            slot.snapshot = Some(snapshot);
        }

        let result = svc.query(
            query_point(req.pt),
            HitTestMode::UiElement,
            &QueryControl::foreground(),
            &mut |_| {},
        );
        let rects = match result {
            Ok(r) => r.path.map(convert_path).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        // A failed send never skips query submission: refinement still runs.
        let _ = req.reply.try_send(UiaReply {
            epoch: req.epoch,
            rects,
            final_reply: false,
        });

        let mut pending = refine_pending().lock().unwrap_or_else(|p| p.into_inner());
        let replaced = pending.replace(RefineCommand {
            pt: req.pt,
            epoch: req.epoch,
            session: req.session,
            reply: req.reply.clone(),
        });
        // A replaced command's receiver may be stale; dropping it is enough.
        drop(replaced);
        drop(pending);
        if let Some(wake) = wake_tx() {
            let _ = wake.try_send(());
        }
    }
}

/// Refinement worker: separate service from the immutable snapshot.
fn run_refine_worker(rx: Receiver<()>) {
    let mut service: Option<ElementRegionService> = None;
    let mut service_key: Option<(u64, u64)> = None;
    while rx.recv().is_ok() {
        // Drain commands, keeping only the newest cursor point (replace).
        let cmd = {
            let mut pending = refine_pending().lock().unwrap_or_else(|p| p.into_inner());
            pending.take()
        };
        let Some(cmd) = cmd else { continue };

        let (generation, snapshot) = {
            let slot = snapshot_slot().lock().unwrap_or_else(|p| p.into_inner());
            (slot.generation, slot.snapshot.clone())
        };
        let key = (cmd.session, generation);
        if service_key != Some(key) {
            service = snapshot
                .as_ref()
                .and_then(|s| ElementRegionService::from_snapshot(s).ok());
            service_key = Some(key);
        }
        let Some(svc) = service.as_mut() else {
            let _ = cmd.reply.try_send(UiaReply {
                epoch: cmd.epoch,
                rects: Vec::new(),
                final_reply: true,
            });
            continue;
        };

        // snow-shot semantics: cancel only on refresh/session change, never
        // on a cursor move. Capturing `key` keeps the check constant-time.
        let cancelled = || {
            let slot = snapshot_slot().lock().unwrap_or_else(|p| p.into_inner());
            slot.generation != generation || slot.snapshot.is_none()
        };
        let result = svc.query(
            query_point(cmd.pt),
            HitTestMode::UiElement,
            &QueryControl::refinement(&cancelled),
            &mut |path| {
                let _ = cmd.reply.try_send(UiaReply {
                    epoch: cmd.epoch,
                    rects: convert_path(path.to_vec()),
                    final_reply: false,
                });
            },
        );
        let rects = match result {
            Ok(r) => r.path.map(convert_path).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let _ = cmd.reply.try_send(UiaReply {
            epoch: cmd.epoch,
            rects,
            final_reply: true,
        });
    }
}

/// 在专用线程上发起一次查询，立即返回，绝不等待 COM。
pub fn elements_at_point_async(excluded: HWND, pt: POINT, epoch: u64, session: u64) -> UiaQuery {
    let _ = excluded; // the snapshot excludes the overlay via the app's own list
    let mut attempt = 0;
    loop {
        let (reply_tx, reply_rx) = mpsc::sync_channel(REPLY_CAPACITY);
        let request = UiaRequest {
            pt,
            epoch,
            session,
            reply: reply_tx,
        };
        let Some(tx) = worker_tx() else {
            return UiaQuery {
                rx: reply_rx,
                failed_fast: true,
            };
        };
        match tx.try_send(request) {
            Ok(()) => {
                return UiaQuery {
                    rx: reply_rx,
                    failed_fast: false,
                }
            }
            Err(TrySendError::Full(_)) => {
                return UiaQuery {
                    rx: reply_rx,
                    failed_fast: true,
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                if attempt >= 1 {
                    return UiaQuery {
                        rx: reply_rx,
                        failed_fast: true,
                    };
                }
                attempt += 1;
                if let Some(slot) = WORKER_TX.get() {
                    *slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
                }
            }
        }
    }
}
