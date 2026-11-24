//! UI Automation sub-element discovery for the region picker.
//!
//! `EnumChildWindows` finds the panes of classic Win32 apps, but modern apps
//! (WPF, Electron, Chromium, the Windows shell) paint their UI inside a single
//! HWND. UIA exposes those panes as elements, which is the "element" half of
//! the picker's `WindowSubElement` mode.
//!
//! We query by *window handle* rather than `ElementFromPoint`: the picker's own
//! overlay is always on top and hit-testable, so a point query would return our
//! window instead of the app underneath. Starting from the app HWND and walking
//! down only the branch whose rect contains the cursor avoids that, and is far
//! cheaper than asking UIA for the whole subtree.
//!
//! Every UIA property read is a cross-process COM call. A hung accessibility
//! provider can block such a call indefinitely, so queries run on a dedicated
//! STA worker thread: the UI thread only posts a request and polls a receiver,
//! and never waits on COM. Replies carry an `epoch` so results from an older
//! hover are dropped instead of being painted after the cursor moved.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTreeWalker,
};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, MSG, PM_REMOVE};

/// Bounds the tree walk so a pathological accessibility tree cannot stall the
/// picker. UIA calls cross COM, so each visit has a real cost. The budget now
/// matches snow-shot's `snow-ui-selector` (deep trees in Chromium/Electron need
/// far more than a handful of levels before reaching the pane under the cursor).
const MAX_DEPTH: u32 = 32;
const MAX_VISITS: usize = 512;
/// 与 snow-shot 一致：只要扩张方向有效就保留（>0），细分隔线也交给上层
/// `candidates_for` 的最小尺寸过滤，不在 COM 遍历阶段丢候选。
const MIN_EXTENT: i32 = 2;
/// 单次查询最多发布的矩形数，与 snow-shot 的 `MAX_RECTS` 对齐。
const MAX_RECTS: usize = 100;
/// UI 侧等待一次 UIA 查询的上限；超时即丢弃结果，UI 线程从不为 COM 阻塞。
/// 300ms 给跨进程 COM 更多时间（snow-shot 首轮 168ms + 1500ms 持续 refinement，
/// 我们是单轮，因此取 300ms）。
pub const UIA_QUERY_TIMEOUT: Duration = Duration::from_millis(300);
/// STA 空闲时每轮等待请求的时长。远小于上面的查询超时，不会拖慢超时判定。
const WORKER_IDLE_TICK: Duration = Duration::from_millis(50);
/// 单轮最多派发的消息数，避免消息洪泛把 worker 永久占住。
const MAX_MESSAGES_PER_TICK: usize = 16;

thread_local! {
    // COM interfaces are apartment-bound, so a thread-local cache is the only
    // safe place to keep the automation object between cursor moves. The
    // dedicated worker thread owns this cache for the process lifetime.
    static AUTOMATION: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
}

fn automation() -> Option<IUIAutomation> {
    AUTOMATION.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(auto) = slot.as_ref() {
            return Some(auto.clone());
        }
        let auto = unsafe {
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?
        };
        *slot = Some(auto.clone());
        Some(auto)
    })
}

/// One UIA element rect and its nesting depth (0 = the app window, growing
/// toward the leaf under the cursor).
#[derive(Clone, Copy)]
pub struct ElementRect {
    pub rect: RECT,
    pub depth: u32,
}

/// 一次异步查询的回执，带请求 epoch 供 UI 线程丢弃过期结果。
pub struct UiaReply {
    pub epoch: u64,
    pub rects: Vec<ElementRect>,
}

/// 异步查询句柄：`rx` 是回执通道，`failed_fast` 表示请求根本没能入队
/// （工作线程队列满 / 已退出）。
///
/// 过去 `try_send` 失败会连 `reply_tx` 一起丢弃，UI 侧只看到一个立即断开的
/// receiver，既不算超时也不触发熔断，卡死的 provider 会让每个鼠标像素都白发
/// 请求。显式带出这个状态后，UI 侧可以把"入队失败"当成一次超时计数。
pub struct UiaQuery {
    pub rx: Receiver<UiaReply>,
    pub failed_fast: bool,
}

/// 投递给 STA 工作线程的请求。
///
/// `HWND` 包着裸指针、不是 `Send`，所以用 `isize` 传递，在工作线程上重建
/// （与 `platform::hook` 里跨线程保存窗口句柄的方式一致）。`POINT` 是纯
/// i32 结构，可以直接跨线程。
struct UiaRequest {
    window: isize,
    pt: POINT,
    epoch: u64,
    /// 容量为 1：即使 UI 已经丢弃接收端，工作线程回传时也不会阻塞。
    reply: SyncSender<UiaReply>,
}

static WORKER_TX: OnceLock<Mutex<Option<SyncSender<UiaRequest>>>> = OnceLock::new();

/// 返回（必要时启动）专用 STA UIA 工作线程的投递端。
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

fn run_worker(rx: Receiver<UiaRequest>) {
    // UIA 的 COM 对象是 apartment-bound，必须在 STA 中创建和使用。
    let sta = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if sta.is_err() {
        return;
    }
    loop {
        // 不用阻塞式 recv：空闲时每 50ms 醒一次抽消息。依赖消息泵投递回调的
        // provider 因此不会把 worker 卡死（STA 里没有消息循环是常见死锁源）。
        match rx.recv_timeout(WORKER_IDLE_TICK) {
            Ok(req) => {
                let window = HWND(req.window as *mut core::ffi::c_void);
                let rects = elements_at_point(window, req.pt);
                let _ = req.reply.try_send(UiaReply {
                    epoch: req.epoch,
                    rects,
                });
            }
            Err(RecvTimeoutError::Timeout) => pump_messages(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // 投递端被丢弃（线程退出 / worker 重建）时走这里，STA 正常释放。
    unsafe { CoUninitialize() };
}

/// 非阻塞地抽掉 STA 消息队列里的 COM 回调 / 跨 apartment 调用。
/// 限批次，既不占死 worker，也不引入同步 COM 等待。
fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        for _ in 0..MAX_MESSAGES_PER_TICK {
            if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                break;
            }
            let _ = DispatchMessageW(&msg);
        }
    }
}

/// 在专用 STA 工作线程上查询 UIA 子元素，立即返回，绝不等待 COM。
///
/// 成功入队时 `rx` 至多收到一条 `UiaReply`（带 `epoch`）；调用方只应接受与
/// 当前 epoch 相同的回执，过期的一律丢弃。队列满 / 工作线程已退出时返回
/// `failed_fast: true`，调用方要把它计为一次超时以驱动熔断，而不是静默当成
/// “无 UIA 候选”——那是卡死 provider 下熔断永不触发的根因。
pub fn elements_at_point_async(window: HWND, pt: POINT, epoch: u64) -> UiaQuery {
    let raw = window.0 as isize;
    let mut attempt = 0;
    loop {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let request = UiaRequest {
            window: raw,
            pt,
            epoch,
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
            // 队列满意味着 worker 已被前序请求占住。这里绝不等待，也不静默
            // 丢请求；把失败状态显式交给 UI 侧计入超时/熔断。
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
                // 工作线程意外退出：清掉缓存，下一轮重新拉起一个 STA。
                attempt += 1;
                if let Some(slot) = WORKER_TX.get() {
                    *slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
                }
            }
        }
    }
}

/// Collects UIA element rects inside `window` whose bounds contain `pt`.
///
/// Returns an empty list on any COM failure (UIA is best-effort: the child-HWND
/// candidates and the whole window remain available). The calling thread must
/// already be an initialized STA: `elements_at_point_async` runs this on the
/// dedicated worker, so a hung provider can never stall the UI thread.
pub fn elements_at_point(window: HWND, pt: POINT) -> Vec<ElementRect> {
    let Some(auto) = automation() else {
        return Vec::new();
    };
    unsafe {
        let Ok(root) = auto.ElementFromHandle(window) else {
            return Vec::new();
        };
        // The control view leaves out the raw tree's non-visual nodes: fewer
        // COM calls and only boundaries a user would actually recognise.
        let Ok(walker) = auto.ControlViewWalker() else {
            return Vec::new();
        };
        let Some(rect) = visible_rect(&root, pt) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut visits = 0usize;
        // The root is the window itself, already supplied by `detect_candidates`;
        // only publish its descendants as sub-element candidates.
        descend(&walker, &root, rect, pt, 0, &mut visits, &mut out);
        out
    }
}

/// Returns the element's rect when it is visible and contains the cursor.
/// Every UIA property read is a COM call, so all the rejection rules live in
/// this one place. The size floor is deliberately tiny (snow-shot only insists
/// on >0): dropping hairlines here used to prune whole descendant branches
/// before the pure-code policy layer could see them.
fn visible_rect(element: &IUIAutomationElement, pt: POINT) -> Option<RECT> {
    unsafe {
        if element
            .CurrentIsOffscreen()
            .map(|off| off.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        let rect = element.CurrentBoundingRectangle().ok()?;
        if rect.right - rect.left < MIN_EXTENT || rect.bottom - rect.top < MIN_EXTENT {
            return None;
        }
        if pt.x < rect.left || pt.x >= rect.right || pt.y < rect.top || pt.y >= rect.bottom {
            return None;
        }
        Some(rect)
    }
}

/// Walks down the one child branch that still contains the cursor. Each
/// containing descendant is a candidate; siblings outside that branch cannot
/// contribute a boundary and are skipped after the first containing match.
fn descend(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
    rect: RECT,
    pt: POINT,
    depth: u32,
    visits: &mut usize,
    out: &mut Vec<ElementRect>,
) {
    // Depth 0 is the window, not a sub-element.
    if depth > 0 {
        // 与 snow-shot 的 MAX_RECTS 对齐：满了就停，不再为多余的后代付 COM 开销。
        if out.len() >= MAX_RECTS {
            return;
        }
        out.push(ElementRect { rect, depth });
    }
    if depth >= MAX_DEPTH || *visits >= MAX_VISITS || out.len() >= MAX_RECTS {
        return;
    }
    unsafe {
        let Ok(mut child) = walker.GetFirstChildElement(element) else {
            return;
        };
        loop {
            if *visits >= MAX_VISITS {
                return;
            }
            *visits += 1;
            if let Some(child_rect) = visible_rect(&child, pt) {
                descend(walker, &child, child_rect, pt, depth + 1, visits, out);
                // Only one sibling can own the point; the remaining ones are
                // irrelevant to this hover.
                return;
            }
            let Ok(next) = walker.GetNextSiblingElement(&child) else {
                return;
            };
            child = next;
        }
    }
}
