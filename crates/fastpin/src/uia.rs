//! UI Automation / MSAA sub-element discovery for the region picker.
//!
//! This module is a thin async adapter over the vendored `snow-ui-selector`
//! crate — the exact selection backend snow-shot uses. That crate owns a
//! `ElementRegionService` with UIA and MSAA backends, a window spatial index,
//! per-monitor rect clipping, structural-node backtracking and the two-phase
//! (foreground + refinement) query protocol. Running it verbatim is what makes
//! FastPin's window+element behaviour match snow-shot rather than approximate it.
//!
//! The service is `!Send` (COM interfaces are apartment-bound), so it is created
//! and owned by a dedicated worker thread. The UI thread never waits on COM: it
//! posts a request and polls non-blocking replies. Replies carry an `epoch` so
//! results from an older hover are dropped, and `final_reply` marks the end of
//! the refinement phase so progressively richer paths can be applied frame by
//! frame.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use snow_ui_selector::{
    AccessibilityBackend, ElementRegionService, HitTestMode, Point as SelectorPoint, QueryControl,
};
use windows::Win32::Foundation::{HWND, POINT, RECT};

/// UI 侧等待一次 UIA 查询首帧的上限；超时即丢弃结果，UI 线程从不为 COM 阻塞。
pub const UIA_QUERY_TIMEOUT: Duration = Duration::from_millis(300);
/// STA 空闲时每轮等待请求的时长。远小于查询超时，不会拖慢超时判定。
const WORKER_IDLE_TICK: Duration = Duration::from_millis(50);
/// 最新请求的 epoch。refinement 的取消闭包据此在光标移动后立即停手。
static LATEST_EPOCH: OnceLock<Arc<AtomicU64>> = OnceLock::new();

fn latest_epoch() -> Arc<AtomicU64> {
    LATEST_EPOCH
        .get_or_init(|| Arc::new(AtomicU64::new(0)))
        .clone()
}

/// One UIA element rect and its nesting depth (0 = the app window, growing
/// toward the leaf under the cursor).
#[derive(Clone, Copy)]
pub struct ElementRect {
    pub rect: RECT,
    pub depth: u32,
}

/// 一次异步查询的回执。`final_reply` 标记 refinement 阶段的最后一帧；UI 在
/// 收到它之前保留 pending 并逐帧应用更丰富的结果。
pub struct UiaReply {
    pub epoch: u64,
    pub rects: Vec<ElementRect>,
    pub final_reply: bool,
}

/// 异步查询句柄：`failed_fast` 表示请求没能入队（工作线程队列满 / 已退出），
/// UI 侧据此计入超时/熔断，而不是静默当成“无 UIA 候选”。
pub struct UiaQuery {
    pub rx: Receiver<UiaReply>,
    pub failed_fast: bool,
}

/// 投递给 worker 的请求。`excluded` 是覆盖层自身的 HWND（按 usize 传，避免
/// 裸指针跨线程），`session` 用于识别新一轮截图并刷新窗口快照。
struct UiaRequest {
    excluded: usize,
    pt: POINT,
    epoch: u64,
    session: u64,
    reply: SyncSender<UiaReply>,
}

static WORKER_TX: OnceLock<Mutex<Option<SyncSender<UiaRequest>>>> = OnceLock::new();

fn worker_tx() -> Option<SyncSender<UiaRequest>> {
    let slot = WORKER_TX.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(tx) = guard.as_ref() {
        return Some(tx.clone());
    }
    // 只缓冲两条请求：提供程序卡死时不会让请求无限堆积。过期回执由 epoch
    // 丢弃，因此丢掉中间请求没有语义损失。
    let (tx, rx) = mpsc::sync_channel::<UiaRequest>(2);
    thread::Builder::new()
        .name("fastpin-uia-worker".to_string())
        .spawn(move || run_worker(rx))
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
            // snow-shot publishes deepest first; FastPin's convention is
            // root = 0, growing toward the leaf. The overlay re-derives depth
            // by containment anyway, so this is only informational.
            depth: (len - i) as u32,
        })
        .collect()
}

fn run_worker(rx: Receiver<UiaRequest>) {
    // The vendored service creates its own COM apartment (MTA) and must live on
    // one thread for its lifetime, so we do not initialise COM here.
    let epoch_flag = latest_epoch();
    let mut service: Option<ElementRegionService> = None;
    let mut service_session: u64 = u64::MAX;
    loop {
        let req = match rx.recv_timeout(WORKER_IDLE_TICK) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        // Create or refresh the window snapshot once per capture session.
        let excluded = [req.excluded];
        if service.is_none() {
            service = ElementRegionService::with_backend_and_displays(
                AccessibilityBackend::Uia,
                &excluded,
                None,
            )
            .ok();
            service_session = req.session;
        } else if service_session != req.session {
            if let Some(s) = service.as_mut() {
                let _ = s.refresh_excluding_ids(&excluded);
            }
            service_session = req.session;
        }

        let point = SelectorPoint {
            x: req.pt.x,
            y: req.pt.y,
            display_id: 0,
            window_id: None,
        };
        let Some(svc) = service.as_mut() else {
            let _ = req.reply.try_send(UiaReply {
                epoch: req.epoch,
                rects: Vec::new(),
                final_reply: true,
            });
            continue;
        };

        // Foreground frame: whatever the first pass resolves.
        let foreground = QueryControl::foreground();
        let first = svc.query(point, HitTestMode::UiElement, &foreground, &mut |_| {});
        let first_rects = match first {
            Ok(result) => result.path.map(convert_path).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        if req
            .reply
            .try_send(UiaReply {
                epoch: req.epoch,
                rects: first_rects,
                final_reply: false,
            })
            .is_err()
        {
            continue;
        }

        // Refinement: publishes progressively richer paths (snow-shot's 32ms
        // interval). A newer hover flips the epoch flag and stops us early.
        let cancelled = || epoch_flag.load(Ordering::SeqCst) > req.epoch;
        let refinement = QueryControl::refinement(&cancelled);
        let refined = svc.query(point, HitTestMode::UiElement, &refinement, &mut |path| {
            let _ = req.reply.try_send(UiaReply {
                epoch: req.epoch,
                rects: convert_path(path.to_vec()),
                final_reply: false,
            });
        });
        let final_rects = match refined {
            Ok(result) => result.path.map(convert_path).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let _ = req.reply.try_send(UiaReply {
            epoch: req.epoch,
            rects: final_rects,
            final_reply: true,
        });
    }
}

/// 在专用 worker 上发起一次查询，立即返回，绝不等待 COM。
pub fn elements_at_point_async(excluded: HWND, pt: POINT, epoch: u64, session: u64) -> UiaQuery {
    // Publish the newest epoch before queuing so an in-flight refinement stops.
    latest_epoch().store(epoch, Ordering::SeqCst);
    let mut attempt = 0;
    loop {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let request = UiaRequest {
            excluded: excluded.0 as usize,
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
