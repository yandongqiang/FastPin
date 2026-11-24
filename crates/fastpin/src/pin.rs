//! Pin-to-screen: the annotated crop becomes a topmost, borderless window that
//! stays on the desktop after the overlay closes (PixPin 钉图). Display, drag,
//! wheel zoom, Esc / double-click to close. Any number of pin windows can be
//! alive at once; the registry below keeps their handles so the windows are
//! not dropped.
//!
//! Stitching (拼接): dragging a lone pin near the exposed edge of another pin
//! snaps it flush and shows a ✓ on the seam. Confirming links the two into a
//! *group*: tiles that move, zoom and close together, and are composited into
//! one image on copy / save. Groups are never rasterized while on screen, so
//! a staircase-shaped group really is a set of rectangular windows and the
//! notches stay click-through. Alt while dragging bypasses snapping; Ctrl+D
//! detaches a tile from its group.
//!
//! Annotating (标注, Ctrl+E or the context menu) opens the drawing tools over
//! the whole group; see `pin_edit`. Right-click posts to the hidden window,
//! which shows a native popup menu and calls back with the chosen command.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    actions, canvas, div, img, prelude::*, px, size, App, AsyncApp, Bounds, Context, CursorStyle,
    DispatchPhase, ElementInputHandler, EntityInputHandler, FocusHandle, Focusable, ImageSource,
    KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point as GpuiPoint, Render, RenderImage, ScrollDelta, ScrollWheelEvent, UTF16Selection, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowOptions,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, CreatePopupMenu, DeferWindowPos, DestroyMenu, EndDeferWindowPos,
    GetClientRect, GetCursorPos, GetSystemMetrics, GetWindowRect, InsertMenuW, PostMessageW,
    SetForegroundWindow, SetWindowPos, TrackPopupMenu, HWND_TOPMOST, MF_BYPOSITION, MF_SEPARATOR,
    MF_STRING, SM_CXDRAG, SM_CYDRAG, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SYSTEM_METRICS_INDEX, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
};

use crate::geom::{Point, Rect};
use crate::overlay::annotate::{self, TextDraft, TextLayer};
use crate::overlay::editor::{Element, PressKind, Shape, Tool};
use crate::overlay::theme;
use crate::pin_edit;
use crate::pin_snap::{
    are_adjacent, compose, find_snap, flatten_gaps, rounded_rect, zoom_rect, IRect, Side,
    SnapRules, Tile, TileImage,
};
use crate::platform::{hook_target, WM_FASTPIN_PIN_MENU};

actions!(
    pin,
    [
        ClosePin,
        ConfirmStitch,
        CopyPin,
        SavePin,
        DetachPin,
        AnnotatePin,
        UndoPin,
        RedoPin
    ]
);

/// One live pin window. `group` is shared by every tile of a stitched group
/// and equals `id` for a lone pin.
#[derive(Clone, Copy)]
struct PinEntry {
    id: u64,
    group: u64,
    hwnd: HWND,
    handle: WindowHandle<PinView>,
}

/// 异步标注写回的一项：(tile id, BGRA, w, h, zoom)。
type PendingWrite = (u64, Vec<u8>, u32, u32, f32);

thread_local! {
    static PINS: RefCell<Vec<PinEntry>> = const { RefCell::new(Vec::new()) };
    static KEYS_BOUND: Cell<bool> = const { Cell::new(false) };
    /// 一个 group 的标注像素在后台 flatten 完成后先入队，再按完成顺序写回。
    /// 快速重开同一 group 时，后一笔的 tiles 必须包含前一笔的标注，
    /// 否则两个后台任务谁最后返回谁就把先前那笔覆盖掉。
    static PENDING_WRITES: RefCell<HashMap<u64, Vec<PendingWrite>>> = RefCell::new(HashMap::new());
    /// 正在冲刷队列的 group 集合。写回会 notify，途中再次进入同一 group 的
    /// snapshots 只读现有 view，外层 flush 会把队列里剩下的部分继续应用完。
    /// 按 group 记录而不是一个全局 bool：A 组冲刷时 B 组调用 flush 不该被吞，
    /// 否则 B 的队列永远留在 PENDING_WRITES 里。
    static FLUSHING_WRITES: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// group -> 仍在后台 flatten 的完成次数。大于 0 时新的完成必须等它归零
    /// 再抓 tiles，否则 tiles 里还没有前一笔标注，仍会互相覆盖。
    static INFLIGHT_FINISHES: RefCell<HashMap<u64, u64>> = RefCell::new(HashMap::new());
}
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 10.0;
const ZOOM_STEP: f32 = 1.1;

/// Snap engages inside this gap and lets go only past `SNAP_EXIT`, so the
/// window does not flicker on the boundary.
const SNAP_ENTER: i32 = 12;
const SNAP_EXIT: i32 = 20;
const MIN_SHARED_EDGE: i32 = 16;
const MAX_OVERLAP: f32 = 0.5;
/// Below this logical size the ✓ and ✕ buttons would cover the image.
const BUTTON_MIN_SIDE: f32 = 48.0;
/// Thickness of the eight resize hot zones, logical px. The pin window is
/// created borderless (`titlebar: None`), where the client area covers the
/// whole window and no system frame exists to grab, so the edges are ours.
const RESIZE_BAND: f32 = 7.0;
/// Smallest client side a resize drag may produce, physical px.
const MIN_RESIZE_SIDE: f64 = 64.0;
/// Largest zoom a resize drag may reach: 4x the image's native size.
const MAX_RESIZE_ZOOM: f32 = 4.0;

fn snap_rules(tolerance: i32) -> SnapRules {
    SnapRules {
        tolerance,
        min_shared: MIN_SHARED_EDGE,
        max_overlap: MAX_OVERLAP,
    }
}

/// Which client edges a resize drag moves; a corner sets one flag per axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResizeEdge {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl ResizeEdge {
    fn horizontal(self) -> bool {
        self.left || self.right
    }

    fn vertical(self) -> bool {
        self.top || self.bottom
    }

    /// Cursor for this edge, so the hot zone reads as resizable before the
    /// press. Zero flags (an interior point) gets none.
    pub(crate) fn cursor(self) -> Option<CursorStyle> {
        Some(match (self.horizontal(), self.vertical()) {
            (true, true) if self.left == self.top => CursorStyle::ResizeUpLeftDownRight,
            (true, true) => CursorStyle::ResizeUpRightDownLeft,
            (true, false) => CursorStyle::ResizeLeftRight,
            (false, true) => CursorStyle::ResizeUpDown,
            (false, false) => return None,
        })
    }
}

/// The resize edge under `pos`, both in logical client px. Corners win over
/// the sides because both axes are within `band`. `None` inside the window.
/// The band shrinks to a quarter of a side on tiny pins, so both flags of one
/// axis can never be set at once.
pub(crate) fn resize_edge_at(pos: (f32, f32), size: (f32, f32), band: f32) -> Option<ResizeEdge> {
    if pos.0 < 0.0 || pos.1 < 0.0 || pos.0 > size.0 || pos.1 > size.1 {
        return None;
    }
    let band = band.max(1.0).min(size.0 * 0.25).min(size.1 * 0.25).max(0.5);
    let edge = ResizeEdge {
        left: pos.0 <= band,
        right: pos.0 >= size.0 - band,
        top: pos.1 <= band,
        bottom: pos.1 >= size.1 - band,
    };
    (edge.horizontal() || edge.vertical()).then_some(edge)
}

/// What one resize move produces. `factor` is the uniform scale about
/// `anchor` (a screen-px point), so a group can follow with the same numbers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResizeOutcome {
    /// New client rect of the dragged tile, screen px.
    pub rect: Rect,
    pub zoom: f32,
    pub factor: f64,
    /// The point that stays put: the corner (or edge midpoint) opposite the
    /// grabbed one.
    pub anchor: (i32, i32),
}

/// Client rect for a resize drag of `edge` by `(dx, dy)` from `start`.
///
/// The aspect ratio is kept -- a pin always shows its image undistorted -- so
/// a side drag scales the other axis too, and the factor is the mean of both
/// axes on a corner. The frame stays between `MIN_RESIZE_SIDE` per side and
/// `MAX_RESIZE_ZOOM` relative to the image.
pub(crate) fn resize_rect(
    start: Rect,
    edge: ResizeEdge,
    dx: f64,
    dy: f64,
    start_zoom: f32,
) -> ResizeOutcome {
    let (w0, h0) = (start.width.max(1.0), start.height.max(1.0));
    let target_w = if edge.right {
        w0 + dx
    } else if edge.left {
        w0 - dx
    } else {
        w0
    };
    let target_h = if edge.bottom {
        h0 + dy
    } else if edge.top {
        h0 - dy
    } else {
        h0
    };
    let sx = target_w / w0;
    let sy = target_h / h0;
    let mut k = match (edge.horizontal(), edge.vertical()) {
        (true, true) => (sx + sy) / 2.0,
        (true, false) => sx,
        (false, true) => sy,
        (false, false) => 1.0,
    };
    let zoom0 = (start_zoom.max(0.001)) as f64;
    // The ceiling is the image's 4x size; the floor keeps both sides grabbable
    // but never exceeds the ceiling on a very thin crop.
    let k_max = (MAX_RESIZE_ZOOM as f64 / zoom0).max(1e-3);
    let k_min = ((MIN_RESIZE_SIDE / w0).max(MIN_RESIZE_SIDE / h0))
        .max(MIN_ZOOM as f64 / zoom0)
        .min(k_max);
    k = k.clamp(k_min, k_max);

    let width = w0 * k;
    let height = h0 * k;
    let x = if edge.left {
        start.right() - width
    } else if edge.right {
        start.x
    } else {
        start.x + (w0 - width) / 2.0
    };
    let y = if edge.top {
        start.bottom() - height
    } else if edge.bottom {
        start.y
    } else {
        start.y + (h0 - height) / 2.0
    };
    let anchor = (
        (if edge.left {
            start.right()
        } else if edge.right {
            start.x
        } else {
            start.x + w0 / 2.0
        })
        .round() as i32,
        (if edge.top {
            start.bottom()
        } else if edge.bottom {
            start.y
        } else {
            start.y + h0 / 2.0
        })
        .round() as i32,
    );
    ResizeOutcome {
        rect: Rect::new(x, y, width, height),
        zoom: (zoom0 * k) as f32,
        factor: k,
        anchor,
    }
}

/// A resize drag in progress. Everything is captured at press so each move
/// recomputes from the original rect instead of accumulating rounding error.
#[derive(Clone)]
struct ResizeDrag {
    edge: ResizeEdge,
    /// Precise client rect at press, screen px.
    start: Rect,
    start_zoom: f32,
    /// Cursor position at press, screen px.
    press: (i32, i32),
    /// Other tiles of the group with their rects and zooms at press: a group
    /// resizes as one image about the same anchor.
    members: Vec<(WindowHandle<PinView>, Rect, f32)>,
}

/// What compositing and annotating need to know about one tile.
pub(crate) struct TileSnapshot {
    pub id: u64,
    /// Client rect, screen physical px.
    pub rect: IRect,
    pub precise_rect: Rect,
    /// Tight BGRA at native size.
    pub pixels: Arc<Vec<u8>>,
    pub size: (u32, u32),
    pub zoom: f32,
}

/// Opens one pin window. `pixels` is the flattened annotated crop (BGRA,
/// physical pixels); `(screen_x, screen_y)` is where the crop sat on the
/// virtual desktop, so the pin lands exactly over what the user captured.
pub fn show_pin(
    cx: &mut App,
    pixels: Vec<u8>,
    w: u32,
    h: u32,
    stride: u32,
    screen_x: i32,
    screen_y: i32,
) -> crate::error::Result<()> {
    let buf = repack_opaque(&pixels, w, h, stride);
    let pixels = Arc::new(buf);
    let Some(image) = render_image(&pixels, w, h) else {
        crate::perf::log_app("pin: pixel buffer size mismatch");
        return Err(crate::error::Error::new("像素缓冲区尺寸不匹配"));
    };
    // A pin keeps one BGRA copy plus the render-image copy for its whole life.
    // Reserve both against the process-wide ceiling so many pins cannot add up
    // to an OOM even though each one passes the single-buffer check.
    let held = (pixels.len() as u64).saturating_mul(2);
    let permit = match crate::image_budget::try_reserve(held) {
        Ok(permit) => permit,
        Err(e) => {
            crate::perf::log_app(&format!("pin refused: {e}"));
            return Err(crate::error::Error::new(format!("内存预算不足：{e}")));
        }
    };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

    bind_pin_keys(cx);
    // Bounds here are logical; attach_to_screen repositions in physical pixels
    // once the window exists, same pattern as the overlay.
    let bounds = Bounds {
        origin: GpuiPoint::new(px(screen_x as f32), px(screen_y as f32)),
        size: size(px(w as f32), px(h as f32)),
    };
    let handle = match cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            focus: true,
            show: true,
            kind: gpui::WindowKind::Normal,
            is_movable: false,
            // Borderless: `titlebar: None` makes the client cover the whole
            // window, so there is no system frame to grab. The view draws and
            // handles its own eight resize hot zones instead.
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        |_window, cx| cx.new(|cx| PinView::new(cx, id, image, pixels, permit, w, h)),
    ) {
        Ok(handle) => handle,
        Err(e) => {
            crate::perf::log_app(&format!("pin window failed: {e}"));
            return Err(crate::error::Error::new(format!("贴图窗口创建失败：{e}")));
        }
    };

    let hwnd = handle.update(cx, |view, window, cx| {
        view.attach_to_screen(window, screen_x, screen_y);
        window.focus(&view.focus_handle, cx);
        window.on_window_should_close(cx, move |_, cx| {
            let group = group_of(id).unwrap_or(id);
            // 系统级关闭（Alt+F4 / 任务栏）不走 Esc、双击那条 finish 路径。
            // 先按 group 撤销编辑：否则 EDITS 里的 backdrop 和工具栏窗口会
            // 一直留着。写回账本由 abort_group 收尾，但有在途 finish 时它会
            // 保留 INFLIGHT 等 worker 自己归零，避免丢掉还没写回的标注。
            pin_edit::abort_group(cx, group);
            forget(&[id]);
            cx.refresh_windows();
            true
        });
        crate::platform::hwnd_of(window)
    });
    let Ok(hwnd) = hwnd else {
        return Err(crate::error::Error::new("贴图窗口初始化失败"));
    };
    PINS.with(|slot| {
        slot.borrow_mut().push(PinEntry {
            id,
            group: id,
            hwnd,
            handle,
        })
    });
    Ok(())
}

fn render_image(pixels: &Arc<Vec<u8>>, w: u32, h: u32) -> Option<Arc<RenderImage>> {
    let rgba = image::RgbaImage::from_raw(w, h, pixels.as_ref().clone())?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}

fn bind_pin_keys(cx: &mut App) {
    if KEYS_BOUND.get() {
        return;
    }
    cx.bind_keys(vec![
        KeyBinding::new("escape", ClosePin, Some("Pin")),
        KeyBinding::new("enter", ConfirmStitch, Some("Pin")),
        KeyBinding::new("ctrl-c", CopyPin, Some("Pin")),
        KeyBinding::new("ctrl-s", SavePin, Some("Pin")),
        KeyBinding::new("ctrl-d", DetachPin, Some("Pin")),
        KeyBinding::new("ctrl-e", AnnotatePin, Some("Pin")),
        KeyBinding::new("ctrl-z", UndoPin, Some("Pin")),
        KeyBinding::new("ctrl-y", RedoPin, Some("Pin")),
    ]);
    KEYS_BOUND.set(true);
}

// ---- registry -------------------------------------------------------------

fn entries() -> Vec<PinEntry> {
    PINS.with(|slot| slot.borrow().clone())
}

fn entry(id: u64) -> Option<PinEntry> {
    PINS.with(|slot| slot.borrow().iter().copied().find(|e| e.id == id))
}

pub(crate) fn entry_handle(id: u64) -> Option<WindowHandle<PinView>> {
    entry(id).map(|e| e.handle)
}

fn group_of(id: u64) -> Option<u64> {
    entry(id).map(|e| e.group)
}

fn group_entries(group: u64) -> Vec<PinEntry> {
    entries().into_iter().filter(|e| e.group == group).collect()
}

/// Other tiles of `id`'s group, `id` itself excluded.
fn group_members(id: u64) -> Vec<PinEntry> {
    let Some(me) = entry(id) else {
        return Vec::new();
    };
    entries()
        .into_iter()
        .filter(|e| e.group == me.group && e.id != id)
        .collect()
}

/// Merges `a`'s group into `b`'s.
fn link(a: u64, b: u64) {
    let (Some(ea), Some(eb)) = (entry(a), entry(b)) else {
        return;
    };
    PINS.with(|slot| {
        for e in slot.borrow_mut().iter_mut() {
            if e.group == ea.group {
                e.group = eb.group;
            }
        }
    });
}

/// Moves `id` into a fresh group of its own. A new number rather than `id`:
/// the group may currently be *named* by this pin's id and the others must
/// keep it.
fn detach(id: u64) {
    let fresh = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    PINS.with(|slot| {
        for e in slot.borrow_mut().iter_mut() {
            if e.id == id {
                e.group = fresh;
            }
        }
    });
}

fn forget(ids: &[u64]) {
    // 只摘掉窗口条目。写回账本不在这里清：正常「完成」后 worker 会经
    // end_finish 自己归零；若在途 flatten 还没写回就关窗（Alt+F4），清掉
    // INFLIGHT/PENDING 会让下一笔完成不再等本次写回，第一笔标注被覆盖。
    // 异常路径的收尾统一交给 pin_edit::abort_group → clear_group_writes，
    // 后者在存在在途 finish 时会保留账本。
    PINS.with(|slot| slot.borrow_mut().retain(|e| !ids.contains(&e.id)));
}

fn client_rect(hwnd: HWND) -> Option<IRect> {
    let mut origin = POINT::default();
    let mut client = RECT::default();
    unsafe {
        if !ClientToScreen(hwnd, &mut origin).as_bool() || GetClientRect(hwnd, &mut client).is_err()
        {
            return None;
        }
    }
    Some(IRect::from_origin_size(
        origin.x,
        origin.y,
        client.right - client.left,
        client.bottom - client.top,
    ))
}

fn outer_rect(hwnd: HWND) -> Option<IRect> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut rect).ok()?;
    }
    Some(IRect::new(rect.left, rect.top, rect.right, rect.bottom))
}

/// Moves a set of windows by one delta in a single batch, so a group never
/// tears apart mid-frame.
fn move_windows(hwnds: &[HWND], dx: i32, dy: i32) {
    if hwnds.is_empty() || (dx == 0 && dy == 0) {
        return;
    }
    unsafe {
        let Ok(mut hdwp) = BeginDeferWindowPos(hwnds.len() as i32) else {
            return;
        };
        for &hwnd in hwnds {
            let Some(outer) = outer_rect(hwnd) else {
                continue;
            };
            match DeferWindowPos(
                hdwp,
                hwnd,
                None,
                outer.left + dx,
                outer.top + dy,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSIZE,
            ) {
                Ok(next) => hdwp = next,
                // The batch is invalidated on failure. Break instead of
                // returning so EndDeferWindowPos still releases it; the
                // remaining windows simply keep their old position.
                Err(_) => break,
            }
        }
        let _ = EndDeferWindowPos(hdwp);
    }
}

fn set_client_rect(hwnd: HWND, desired: IRect) {
    let outer = window_rect_for_client_area(hwnd, desired);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            outer.left,
            outer.top,
            outer.width(),
            outer.height(),
            SWP_NOACTIVATE | SWP_NOZORDER,
        );
    }
}

/// Puts `desired` (screen physical px) on the window and lets GPUI adopt that
/// size before the next frame.
///
/// `set_client_rect` on its own only moves the OS window: its `SetWindowPos`
/// delivers `WM_SIZE` re-entrantly, from inside the event handler that is
/// already updating this window, where the platform's resize callback cannot
/// borrow the app again (`AsyncApp`'s `try_borrow_mut` fails) and is dropped.
/// `window.viewport_size()` therefore keeps the old value and every frame
/// keeps drawing at the old size until some later event arrives -- the pin
/// "only finishes resizing after a right click". `bounds_changed` re-reads the
/// size `WM_SIZE` already stored in the platform, so the frame this handler
/// schedules draws at the new size.
///
/// `Window::resize` is deliberately not used alongside: it posts its own
/// size-only `SetWindowPos` to the foreground executor (`SWP_NOMOVE`), which
/// lags the drag by a frame and would fight the anchored corner placed below.
/// One `SetWindowPos` stays the single geometry authority, so a fractional
/// rect never rounds twice and the window cannot jitter.
fn apply_client_rect(window: &mut Window, cx: &mut App, desired: Rect) {
    set_client_rect(crate::platform::hwnd_of(window), rounded_rect(desired));
    window.bounds_changed(cx);
}

/// Repaints every tile of `group` and its toolbar, if editing. A window that
/// is currently mid-update (the caller's own) is skipped; it notifies itself.
pub(crate) fn refresh_group(cx: &mut App, group: u64) {
    for e in group_entries(group) {
        let _ = e.handle.update(cx, |_, _, cx| cx.notify());
    }
    if let Some(edit) = pin_edit::edit_for_group(group) {
        let toolbar = edit.borrow().toolbar_handle();
        if let Some(t) = toolbar {
            let _ = t.update(cx, |_, _, cx| cx.notify());
        }
    }
}

// ---- 异步标注写回的排序 ---------------------------------------------------

/// flatten 在后台完成，先把像素按完成顺序放进 group 的队列，再由 flush 应用。
pub(crate) fn enqueue_pending_writes(group: u64, writes: Vec<PendingWrite>) {
    if writes.is_empty() {
        return;
    }
    PENDING_WRITES.with(|slot| {
        slot.borrow_mut().entry(group).or_default().extend(writes);
    });
}

/// 进入 `group` 的冲刷临界区。已在该集合中说明同一 group 正被冲刷（重入），
/// 返回 false；不同 group 互不干扰，各自都能拿到自己的冲刷机会。
fn begin_flush(group: u64) -> bool {
    FLUSHING_WRITES.with(|slot| slot.borrow_mut().insert(group))
}

/// 离开 `group` 的冲刷临界区。
fn end_flush(group: u64) {
    FLUSHING_WRITES.with(|slot| slot.borrow_mut().remove(&group));
}

/// 丢弃一个 group 的全部写回账本。幂等，由 pin_edit::abort_group（系统级关闭
/// 等异常路径的收尾）调用。
///
/// 关键：只要该 group 还有在途 finish（点过「完成」，后台 flatten 尚未写回），
/// 就必须整体跳过。worker 的 end_finish 会自己把 INFLIGHT 归零；提前清掉
/// INFLIGHT/PENDING 会让下一笔完成不再等待本次写回，第一笔标注随即被覆盖。
/// 即使关掉的是多贴图 group 中的一张，其余成员的写回也必须按原顺序落盘。
pub(crate) fn clear_group_writes(group: u64) {
    if has_inflight_finish(group) {
        return;
    }
    PENDING_WRITES.with(|slot| {
        slot.borrow_mut().remove(&group);
    });
    FLUSHING_WRITES.with(|slot| {
        slot.borrow_mut().remove(&group);
    });
    INFLIGHT_FINISHES.with(|slot| {
        slot.borrow_mut().remove(&group);
    });
}

/// 取出 `group` 的待写回队列（按入队顺序）。单独抽出来便于单测验证 FIFO。
fn take_pending_writes(group: u64) -> Vec<PendingWrite> {
    PENDING_WRITES
        .with(|slot| slot.borrow_mut().remove(&group))
        .unwrap_or_default()
}

/// 按入队顺序把像素写回 tile view。同一 group 正在冲刷时重入直接跳过，外层
/// flush 会继续处理队列里剩下的项，保证同一 group 的写回永远是先进先出。
/// 不同 group 的冲刷彼此独立，不会被对方的重入保护吞掉。
pub(crate) fn flush_pending_writes(cx: &mut App, group: u64) {
    if !begin_flush(group) {
        return;
    }
    let writes = take_pending_writes(group);
    for (id, px, w, h, zoom) in writes {
        let Some(handle) = entry_handle(id) else {
            continue;
        };
        // 和原来的 finish_async 一样，窗口若已被关闭就跳过；不能写回的项
        // 不会阻塞队列里后面的项。
        let _ = handle.update(cx, |view, _, cx| {
            view.replace_pixels(px, w, h, zoom);
            cx.notify();
        });
    }
    end_flush(group);
}

/// pin_edit::finish_async 在生成后台任务前登记，写回完成后解除。
pub(crate) fn begin_finish(group: u64) {
    INFLIGHT_FINISHES.with(|slot| {
        *slot.borrow_mut().entry(group).or_insert(0) += 1;
    });
}

/// 无论 UI 更新成功与否都要解除在途标记，否则后续完成会被永久卡住。
pub(crate) fn end_finish(group: u64) {
    INFLIGHT_FINISHES.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(count) = slot.get_mut(&group) else {
            return;
        };
        if *count <= 1 {
            slot.remove(&group);
        } else {
            *count -= 1;
        }
    });
}

fn has_inflight_finish(group: u64) -> bool {
    INFLIGHT_FINISHES.with(|slot| slot.borrow().get(&group).copied().unwrap_or(0) > 0)
}

/// 完成编辑：先等同一 group 之前启动的 flatten 全部写回，再抓 tiles。
/// 直接抓会让后一笔的标注基于不含前一笔的像素，两个后台任务互相覆盖。
/// 等在途任务归零后才在 UI 线程重新 snapshots，此时任何 view 都不在更新中，
/// 因此可以不传 `mine`，把整个 group（含调用方那块）读全。
pub(crate) fn finish_edit_deferred(cx: &mut App, group: u64, commit: bool) {
    if !commit {
        pin_edit::finish_async(cx, group, false, Vec::new());
        return;
    }
    let executor = cx.background_executor().clone();
    cx.spawn(async move |cx: &mut AsyncApp| {
        while has_inflight_finish(group) {
            executor.timer(Duration::from_millis(1)).await;
        }
        cx.update(|cx: &mut App| {
            let tiles = snapshots(cx, group, None);
            pin_edit::finish_async(cx, group, true, tiles);
        });
    })
    .detach();
}

/// Snapshots of every tile in `group` read through their windows; `mine` is
/// the caller's own tile, which cannot be read while it is being updated.
pub(crate) fn snapshots(cx: &mut App, group: u64, mine: Option<TileSnapshot>) -> Vec<TileSnapshot> {
    // 先把已完成但还没写回 view 的像素按序应用：后续 flatten 必须基于
    // 含上一次标注的 tiles，否则两笔标注会互相覆盖。
    flush_pending_writes(cx, group);
    let skip = mine.as_ref().map(|t| t.id);
    let mut out: Vec<TileSnapshot> = mine.into_iter().collect();
    for e in group_entries(group) {
        if Some(e.id) == skip {
            continue;
        }
        let (Ok(view), Some(rect)) = (e.handle.read(cx), client_rect(e.hwnd)) else {
            continue;
        };
        out.push(TileSnapshot {
            id: e.id,
            rect,
            precise_rect: view.precise_rect(rect),
            pixels: view.pixels.clone(),
            size: view.pixel_size,
            zoom: view.zoom,
        });
    }
    out
}

// ---- view -----------------------------------------------------------------

/// One OLE drag loop at a time, process-wide: the shell's modal loop and a
/// concurrent second `DoDragDrop` do not compose.
static EXPORT_DRAG_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Releases the drag gate on *every* exit path. A press that never crosses the
/// threshold, a `GetCursorPos` failure and a compose/stage failure on the
/// worker all previously left the gate stuck true for the rest of the process.
/// The guard is moved into the drag thread so the gate stays held for the whole
/// OLE loop and is dropped when that loop finishes.
struct ExportDragGuard;

impl Drop for ExportDragGuard {
    fn drop(&mut self) {
        EXPORT_DRAG_ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// The process-wide drag-export file retention. Leases live here because the
/// gesture thread ends, but the staged file must survive until the receiver
/// finishes its copy or the grace window closes. app.rs 的周期 sweep 与退出
/// 清理也经此访问同一本账。
pub(crate) fn drag_retention() -> &'static std::sync::Mutex<crate::dragexport::Retention> {
    use std::sync::{Mutex, OnceLock};
    static RETENTION: OnceLock<Mutex<crate::dragexport::Retention>> = OnceLock::new();
    RETENTION.get_or_init(|| Mutex::new(crate::dragexport::Retention::new()))
}

/// A snap that is showing its ✓: `target` is the pin this one is flush with.
#[derive(Clone, Copy)]
struct Pending {
    target: u64,
    side: Side,
}

pub struct PinView {
    id: u64,
    image: Arc<RenderImage>,
    /// Tight BGRA at native size, kept for compositing a group.
    pixels: Arc<Vec<u8>>,
    /// Process-wide ledger reservation for the two buffers held by this pin.
    /// `Option` so `replace_pixels` can hand the old permit back to the ledger
    /// before reserving the new one, without losing it if the reserve fails.
    permit: Option<crate::image_budget::MemoryPermit>,
    /// Physical pixel size of the flattened crop.
    pixel_size: (u32, u32),
    zoom: f32,
    layout_rect: Option<Rect>,
    /// Cursor position minus the window origin at drag start, screen coords.
    drag_offset: Option<(i32, i32)>,
    /// Screen cursor at a Ctrl+press that may grow into a drag-export gesture.
    /// Mutually exclusive with `drag_offset`: the armed gesture swallows the
    /// move, so the window does not follow the pointer.
    export_drag_origin: Option<(i32, i32)>,
    /// A resize drag in progress; mutually exclusive with `drag_offset`.
    resize: Option<ResizeDrag>,
    /// Edge under the pointer while idle, so the cursor shows what it does.
    hover_edge: Option<ResizeEdge>,
    pending: Option<Pending>,
    /// A stroke in progress started on this tile (edit mode).
    edit_down: Option<Point>,
    focus_handle: FocusHandle,
}

impl PinView {
    fn new(
        cx: &mut Context<Self>,
        id: u64,
        image: Arc<RenderImage>,
        pixels: Arc<Vec<u8>>,
        permit: crate::image_budget::MemoryPermit,
        w: u32,
        h: u32,
    ) -> Self {
        Self {
            id,
            image,
            pixels,
            permit: Some(permit),
            pixel_size: (w.max(1), h.max(1)),
            zoom: 1.0,
            layout_rect: None,
            drag_offset: None,
            export_drag_origin: None,
            resize: None,
            hover_edge: None,
            pending: None,
            edit_down: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Exact placement: GPUI's window_bounds are logical pixels, and the
    /// caption-free window still carries non-client insets, so set the outer
    /// rect whose client area matches the crop at its original screen position.
    fn attach_to_screen(&mut self, window: &mut Window, screen_x: i32, screen_y: i32) {
        let hwnd = crate::platform::hwnd_of(window);
        let desired = IRect::from_origin_size(
            screen_x,
            screen_y,
            self.pixel_size.0 as i32,
            self.pixel_size.1 as i32,
        );
        let outer = window_rect_for_client_area(hwnd, desired);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                outer.left,
                outer.top,
                outer.width(),
                outer.height(),
                SWP_SHOWWINDOW,
            );
        }
    }

    fn group(&self) -> u64 {
        group_of(self.id).unwrap_or(self.id)
    }

    fn editing(&self) -> bool {
        pin_edit::is_editing(self.group())
    }

    /// Swaps in new native pixels (after 完成 flattens the marks).
    pub(crate) fn replace_pixels(&mut self, pixels: Vec<u8>, w: u32, h: u32, zoom: f32) {
        let pixels = Arc::new(pixels);
        let Some(image) = render_image(&pixels, w, h) else {
            return;
        };
        let held = (pixels.len() as u64).saturating_mul(2);
        // 先把旧许可归还账本再预留新额度，避免两份预留同时生效时的瞬时双倍计费。
        // 旧额度按旧像素缓冲的大小记下：新额度若失败，尽力把它补回来，失败的
        // 编辑仍保持原有像素与渲染图不变。
        let old_bytes = (self.pixels.len() as u64).saturating_mul(2);
        drop(self.permit.take());
        let permit = match crate::image_budget::try_reserve(held) {
            Ok(permit) => permit,
            Err(e) => {
                crate::perf::log_app(&format!("pin edit refused: {e}"));
                self.permit = crate::image_budget::try_reserve(old_bytes).ok();
                return;
            }
        };
        self.permit = Some(permit);
        self.pixels = pixels;
        self.image = image;
        self.pixel_size = (w.max(1), h.max(1));
        self.zoom = zoom;
        if let Some(rect) = self.layout_rect.as_mut() {
            rect.width = w as f64 * zoom as f64;
            rect.height = h as f64 * zoom as f64;
        }
    }

    fn snapshot(&self, window: &Window) -> Option<TileSnapshot> {
        let rect = client_rect(crate::platform::hwnd_of(window))?;
        Some(TileSnapshot {
            id: self.id,
            rect,
            precise_rect: self.precise_rect(rect),
            pixels: self.pixels.clone(),
            size: self.pixel_size,
            zoom: self.zoom,
        })
    }

    fn precise_rect(&self, client: IRect) -> Rect {
        let mut rect = self.layout_rect.unwrap_or_else(|| client.precise());
        let rounded = rounded_rect(rect);
        // Dragging translates HWNDs in whole pixels without discarding residuals.
        rect.x += (client.left - rounded.left) as f64;
        rect.y += (client.top - rounded.top) as f64;
        rect
    }

    // ---- close / detach / stitch -------------------------------------------

    /// Esc: drops a text draft, else cancels an edit, else closes the group.
    fn close(&mut self, _: &ClosePin, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing() {
            let group = self.group();
            let dropped = pin_edit::edit_for_group(group)
                .is_some_and(|edit| edit.borrow_mut().text.take().is_some());
            if !dropped {
                pin_edit::finish(cx, group, false, Vec::new());
            }
            refresh_group(cx, group);
            cx.notify();
            return;
        }
        self.close_group(window, cx);
    }

    fn close_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let group = self.group();
        if self.editing() {
            pin_edit::finish(cx, group, false, Vec::new());
        }
        let members = group_members(self.id);
        let mut ids = vec![self.id];
        for m in &members {
            ids.push(m.id);
            let _ = m.handle.update(cx, |_, window, _| window.remove_window());
        }
        forget(&ids);
        clear_pendings_targeting(cx, &ids, self.id);
        window.remove_window();
    }

    fn detach(&mut self, _: &DetachPin, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing() {
            return;
        }
        let former = group_members(self.id);
        detach(self.id);
        refresh(cx, &former);
        cx.notify();
    }

    /// Enter: commits a text draft, else finishes an edit, else confirms a
    /// pending stitch.
    fn confirm(&mut self, _: &ConfirmStitch, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing() {
            let group = self.group();
            let committed = pin_edit::edit_for_group(group).is_some_and(|edit| {
                let e = &mut *edit.borrow_mut();
                let had = e.text.is_some();
                annotate::commit_text_draft(&mut e.session, &mut e.text);
                had
            });
            if committed {
                refresh_group(cx, group);
                cx.notify();
            } else {
                self.finish_edit(true, window, cx);
            }
            return;
        }
        self.confirm_stitch(window, cx);
    }

    /// Links this pin to the snap target if the two are still flush.
    fn confirm_stitch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let target_editing = group_of(p.target).is_some_and(pin_edit::is_editing);
        let flush = entry(p.target)
            .and_then(|t| {
                Some((
                    client_rect(t.hwnd)?,
                    client_rect(crate::platform::hwnd_of(window))?,
                ))
            })
            .is_some_and(|(a, b)| are_adjacent(a, b, MIN_SHARED_EDGE));
        if flush && !target_editing {
            if let Some(target) = entry(p.target) {
                if let (Ok(view), Some(other), Some(me)) = (
                    target.handle.read(cx),
                    client_rect(target.hwnd),
                    client_rect(crate::platform::hwnd_of(window)),
                ) {
                    let target = view.precise_rect(other);
                    let mut rect = self.precise_rect(me);
                    match p.side {
                        Side::Right => rect.x = target.right(),
                        Side::Left => rect.x = target.x - rect.width,
                        Side::Bottom => rect.y = target.bottom(),
                        Side::Top => rect.y = target.y - rect.height,
                    }
                    if matches!(p.side, Side::Right | Side::Left) {
                        if me.top == other.top {
                            rect.y = target.y;
                        } else if me.bottom == other.bottom {
                            rect.y = target.bottom() - rect.height;
                        }
                    } else if me.left == other.left {
                        rect.x = target.x;
                    } else if me.right == other.right {
                        rect.x = target.right() - rect.width;
                    }
                    self.layout_rect = Some(rect);
                    set_client_rect(crate::platform::hwnd_of(window), rounded_rect(rect));
                }
            }
            link(self.id, p.target);
            // Membership decides which tile carries the ✕.
            refresh(cx, &group_members(self.id));
        }
        cx.notify();
    }

    // ---- annotate -----------------------------------------------------------

    fn annotate(&mut self, _: &AnnotatePin, window: &mut Window, cx: &mut Context<Self>) {
        self.start_edit(window, cx);
    }

    fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing() {
            return;
        }
        self.pending = None;
        self.drag_offset = None;
        self.resize = None;
        // Ctrl+按下 arm 的导出手势若还没越过阈值就进了编辑态，必须在这里清掉：
        // 否则编辑中松开左键后它仍是 Some，下一次普通拖动会误触发 OLE 拖出。
        self.export_drag_origin = None;
        let group = self.group();
        let tiles = snapshots(cx, group, self.snapshot(window));
        pin_edit::start(cx, group, &tiles);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn finish_edit(&mut self, commit: bool, _window: &mut Window, cx: &mut Context<Self>) {
        // 完成 composites + rasterizes on a worker。抓 tiles 的时机由
        // finish_edit_deferred 统一控制：同 group 上一次 flatten 写回完成后
        // 才抓，避免快速重开时两笔标注互相覆盖。
        finish_edit_deferred(cx, self.group(), commit);
        self.edit_down = None;
        cx.notify();
    }

    fn undo(&mut self, _: &UndoPin, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_history(cx, true);
    }

    fn redo(&mut self, _: &RedoPin, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_history(cx, false);
    }

    fn edit_history(&mut self, cx: &mut Context<Self>, undo: bool) {
        let group = self.group();
        let Some(edit) = pin_edit::edit_for_group(group) else {
            return;
        };
        {
            let mut e = edit.borrow_mut();
            // The draft is not in history, so undoing past it would silently
            // drop the previous annotation while the text field stayed open.
            if undo {
                if e.text.take().is_none() {
                    e.session.undo();
                }
            } else if e.text.is_none() {
                e.session.redo();
            }
        }
        refresh_group(cx, group);
        cx.notify();
    }

    /// Edit-space point at this window's client origin.
    fn edit_offset(&self, window: &Window) -> Option<Point> {
        let edit = pin_edit::edit_for_group(self.group())?;
        let origin = edit.borrow().origin;
        let me = client_rect(crate::platform::hwnd_of(window))?;
        Some(Point {
            x: (me.left - origin.0) as f64,
            y: (me.top - origin.1) as f64,
        })
    }

    fn edit_point(&self, window: &Window, pos: GpuiPoint<Pixels>) -> Option<Point> {
        let off = self.edit_offset(window)?;
        let scale = window.scale_factor().max(0.01) as f64;
        Some(Point {
            x: off.x + f64::from(f32::from(pos.x)) * scale,
            y: off.y + f64::from(f32::from(pos.y)) * scale,
        })
    }

    fn edit_press(&mut self, pt: Point, cx: &mut Context<Self>) {
        let group = self.group();
        let Some(edit) = pin_edit::edit_for_group(group) else {
            return;
        };
        {
            let e = &mut *edit.borrow_mut();
            e.color_open = false;
            e.session.shape_open = false;
            annotate::commit_text_draft(&mut e.session, &mut e.text);
            match e.session.pointer_pressed(pt) {
                PressKind::RequestText => {
                    e.text = Some(TextDraft::new(pt));
                    self.edit_down = None;
                }
                PressKind::Capture => self.edit_down = Some(pt),
            }
        }
        refresh_group(cx, group);
        cx.notify();
    }

    fn edit_drag(&mut self, pt: Point, cx: &mut Context<Self>) {
        let Some(down) = self.edit_down else {
            return;
        };
        let group = self.group();
        let Some(edit) = pin_edit::edit_for_group(group) else {
            return;
        };
        {
            let mut e = edit.borrow_mut();
            if e.session.tool != Tool::Select && e.session.tool != Tool::Text {
                if !e.session.is_dragging() {
                    let _ = e.session.pointer_pressed(down);
                }
                e.session.pointer_moved(pt);
            } else if e.session.is_dragging() && e.session.selected_id.is_some() {
                e.session.pointer_moved(pt);
            }
        }
        refresh_group(cx, group);
        cx.notify();
    }

    fn edit_release(&mut self, cx: &mut Context<Self>) {
        if self.edit_down.take().is_none() {
            return;
        }
        let group = self.group();
        if let Some(edit) = pin_edit::edit_for_group(group) {
            edit.borrow_mut().session.pointer_released();
        }
        refresh_group(cx, group);
        cx.notify();
    }

    /// Only Backspace lives here. Characters arrive through the
    /// `EntityInputHandler` below (WM_CHAR and the IME result string).
    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key != "backspace" {
            return;
        }
        let group = self.group();
        let Some(edit) = pin_edit::edit_for_group(group) else {
            return;
        };
        let went = edit
            .borrow_mut()
            .text
            .as_mut()
            .is_some_and(|t| t.backspace());
        if went {
            refresh_group(cx, group);
            cx.notify();
        }
    }

    // ---- mouse ----------------------------------------------------------------

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        if self.editing() {
            if let Some(pt) = self.edit_point(window, event.position) {
                self.edit_press(pt, cx);
            }
            return;
        }
        if event.click_count >= 2 {
            self.close_group(window, cx);
            return;
        }
        // Ctrl+press arms a drag-export gesture (upstream snow-shot): the pin
        // itself never moves while it is armed. Resizing beats nothing here --
        // an armed press owns the pointer until release or threshold.
        if event.modifiers.secondary() {
            let mut cursor = POINT::default();
            if unsafe { GetCursorPos(&mut cursor) }.is_ok() {
                self.export_drag_origin = Some((cursor.x, cursor.y));
            }
            return;
        }
        // Edge hot zones beat the move drag: pressing the border resizes,
        // pressing inside moves.
        if let Some(edge) = self.resize_edge_under(window, event.position) {
            self.begin_resize(edge, event.position, window, cx);
            return;
        }
        let hwnd = crate::platform::hwnd_of(window);
        let mut cursor = POINT::default();
        let mut rect = RECT::default();
        unsafe {
            if GetCursorPos(&mut cursor).is_ok() && GetWindowRect(hwnd, &mut rect).is_ok() {
                self.drag_offset = Some((cursor.x - rect.left, cursor.y - rect.top));
            }
        }
        // Whoever was snapped against this pin loses its ✓: the seam is
        // about to move.
        clear_pendings_targeting(cx, &[self.id], self.id);
        cx.notify();
    }

    /// The resize edge under a logical client point, if any.
    fn resize_edge_under(&self, window: &Window, pos: GpuiPoint<Pixels>) -> Option<ResizeEdge> {
        let vp = window.viewport_size();
        resize_edge_at(
            (f32::from(pos.x), f32::from(pos.y)),
            (f32::from(vp.width), f32::from(vp.height)),
            RESIZE_BAND,
        )
    }

    /// Captures the frame, zoom, cursor and group members the drag scales
    /// from, then hands the pointer to the resize.
    fn begin_resize(
        &mut self,
        edge: ResizeEdge,
        pos: GpuiPoint<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hwnd = crate::platform::hwnd_of(window);
        let Some(client) = client_rect(hwnd) else {
            return;
        };
        let scale = window.scale_factor().max(0.01);
        // The press point in screen px, from the client rect we already have.
        let press = (
            client.left + (f32::from(pos.x) * scale).round() as i32,
            client.top + (f32::from(pos.y) * scale).round() as i32,
        );
        let members = group_members(self.id)
            .into_iter()
            .filter_map(|e| {
                let client = client_rect(e.hwnd)?;
                let view = e.handle.read(cx).ok()?;
                Some((e.handle, view.precise_rect(client), view.zoom))
            })
            .collect();
        self.resize = Some(ResizeDrag {
            edge,
            start: self.precise_rect(client),
            start_zoom: self.zoom,
            press,
            members,
        });
        self.drag_offset = None;
        self.pending = None;
        self.hover_edge = Some(edge);
        // The seam moves, so any ✓ pointing at this pin is stale.
        clear_pendings_targeting(cx, &[self.id], self.id);
        cx.notify();
    }

    /// One resize move: the frame is recomputed from the press state (so no
    /// rounding drift), then the group scales about the same anchor.
    fn drag_resize(&mut self, drag: ResizeDrag, window: &mut Window, cx: &mut Context<Self>) {
        let mut cursor = POINT::default();
        unsafe {
            if GetCursorPos(&mut cursor).is_err() {
                return;
            }
        }
        let out = resize_rect(
            drag.start,
            drag.edge,
            (cursor.x - drag.press.0) as f64,
            (cursor.y - drag.press.1) as f64,
            drag.start_zoom,
        );
        self.zoom = out.zoom;
        self.layout_rect = Some(out.rect);
        apply_client_rect(window, cx, out.rect);
        for (handle, rect0, zoom0) in &drag.members {
            let rect = zoom_rect(*rect0, out.anchor, out.factor);
            let zoom = *zoom0 * out.factor as f32;
            let _ = handle.update(cx, |view, window, cx| {
                view.zoom = zoom;
                view.layout_rect = Some(rect);
                apply_client_rect(window, cx, rect);
                cx.notify();
            });
        }
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging() {
            // Idle hover: show the resize cursor over the border hot zones.
            if !self.editing() && self.resize.is_none() {
                let edge = self.resize_edge_under(window, event.position);
                if edge != self.hover_edge {
                    self.hover_edge = edge;
                    cx.notify();
                }
            }
            return;
        }
        if self.editing() {
            if let Some(pt) = self.edit_point(window, event.position) {
                self.edit_drag(pt, cx);
            }
            return;
        }
        // The armed gesture crossing the system drag threshold starts the OLE
        // loop on a worker thread. The press is consumed: no move, no resize.
        if self.export_drag_origin.is_some() {
            self.begin_export_drag(window, cx);
            return;
        }
        if let Some(drag) = self.resize.clone() {
            self.drag_resize(drag, window, cx);
            return;
        }
        let Some((dx, dy)) = self.drag_offset else {
            return;
        };
        let hwnd = crate::platform::hwnd_of(window);
        let mut cursor = POINT::default();
        let (Some(outer), Some(client)) = (outer_rect(hwnd), client_rect(hwnd)) else {
            return;
        };
        unsafe {
            if GetCursorPos(&mut cursor).is_err() {
                return;
            }
        }
        // Raw target from the grab point, expressed as a client rect.
        let raw = client.translated(cursor.x - dx - outer.left, cursor.y - dy - outer.top);

        let members = group_members(self.id);
        let had_pending = self.pending.is_some();
        let mut target = raw;
        self.pending = None;
        // Only a lone pin joins a shape; a whole group is just moved.
        if members.is_empty() && !event.modifiers.alt {
            let others: Vec<PinEntry> = entries().into_iter().filter(|e| e.id != self.id).collect();
            let tiles: Vec<(u64, Tile)> = others
                .iter()
                .filter_map(|e| {
                    let rect = client_rect(e.hwnd)?;
                    Some((
                        e.id,
                        Tile {
                            rect,
                            group: e.group,
                        },
                    ))
                })
                .collect();
            let rects: Vec<Tile> = tiles.iter().map(|(_, t)| *t).collect();
            let tolerance = if had_pending { SNAP_EXIT } else { SNAP_ENTER };
            if let Some(snap) = find_snap(raw, &rects, snap_rules(tolerance)) {
                target = snap.rect;
                self.pending = Some(Pending {
                    target: tiles[snap.target].0,
                    side: snap.side,
                });
            }
        }

        let mut hwnds = vec![hwnd];
        hwnds.extend(members.iter().map(|m| m.hwnd));
        move_windows(&hwnds, target.left - client.left, target.top - client.top);
        // The ✓ rides on the seam, so any snapped move needs a repaint.
        if had_pending || self.pending.is_some() {
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.button {
            MouseButton::Left => {
                if self.editing() {
                    // 编辑中的松开同样要 disarm：此时 OLE 分支被 editing 挡住，
                    // 手势会一直残留到退出编辑态。
                    self.export_drag_origin = None;
                    self.edit_release(cx);
                } else if self.export_drag_origin.take().is_some() {
                    // Released before the threshold: not a drag, just a click.
                    cx.notify();
                } else if self.resize.take().is_some() {
                    // Resizing never starts a stitch: the group stays put.
                    cx.notify();
                } else if self.drag_offset.take().is_some() {
                    cx.notify();
                }
            }
            MouseButton::Right => self.open_menu(cx),
            _ => {}
        }
    }

    /// Right-click: a text draft is dropped (as in the overlay); otherwise the
    /// hidden window is asked to show the context menu. It runs the modal
    /// loop where no gpui window is mid-update, then calls back.
    fn open_menu(&mut self, cx: &mut Context<Self>) {
        let group = self.group();
        if let Some(edit) = pin_edit::edit_for_group(group) {
            if edit.borrow_mut().text.take().is_some() {
                refresh_group(cx, group);
                cx.notify();
                return;
            }
        }
        unsafe {
            let _ = PostMessageW(
                Some(hook_target()),
                WM_FASTPIN_PIN_MENU,
                WPARAM(self.id as usize),
                LPARAM(0),
            );
        }
    }

    /// Wheel zoom resizes the window with the image -- the frame hugs the
    /// pixels at every zoom level -- anchored on the cursor, so the image
    /// point under it stays put (the PixPin zoom feel). A group scales every
    /// tile about the same anchor, edge by edge, so seams stay closed.
    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Marks are laid out in screen pixels for the duration of an edit.
        if self.editing() {
            return;
        }
        let dy = match event.delta {
            ScrollDelta::Pixels(p) => f32::from(p.y),
            ScrollDelta::Lines(l) => l.y * 32.0,
        };
        if dy == 0.0 {
            return;
        }
        let factor = if dy > 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
        let hwnd = crate::platform::hwnd_of(window);
        let scale = window.scale_factor().max(0.01);
        let Some(client) = client_rect(hwnd) else {
            return;
        };
        // Cursor in screen pixels; event.position is logical, client-relative.
        let anchor = (
            (client.left as f32 + f32::from(event.position.x) * scale).round() as i32,
            (client.top as f32 + f32::from(event.position.y) * scale).round() as i32,
        );
        self.zoom_group(factor, anchor, window, cx);
    }

    /// Scales this tile and its group about `anchor` (screen px) by `factor`,
    /// clamped to the zoom range.
    fn zoom_group(
        &mut self,
        factor: f32,
        anchor: (i32, i32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A thin strip must not zoom into a zero-pixel window: keep the
        // smaller side at a grabbable 24 physical px.
        let min_dim = self.pixel_size.0.min(self.pixel_size.1) as f32;
        let floor = MIN_ZOOM.max(24.0 / min_dim);
        let new_zoom = (self.zoom * factor).clamp(floor, MAX_ZOOM);
        if new_zoom == self.zoom {
            return;
        }
        let factor = new_zoom / self.zoom;
        self.apply_zoom(window, cx, anchor, factor);
        for m in group_members(self.id) {
            let _ = m.handle.update(cx, |view, window, cx| {
                view.apply_zoom(window, cx, anchor, factor);
                cx.notify();
            });
        }
        cx.notify();
    }

    fn apply_zoom(&mut self, window: &mut Window, cx: &mut App, anchor: (i32, i32), factor: f32) {
        let Some(client) = client_rect(crate::platform::hwnd_of(window)) else {
            return;
        };
        // A group can hold members at different zooms; applying one shared
        // factor without a per-member clamp lets a small member slip under the
        // grabbable minimum. Clamp here so both the leader and every follower
        // stay inside [MIN_ZOOM, MAX_ZOOM].
        let min_dim = self.pixel_size.0.min(self.pixel_size.1) as f32;
        let floor = MIN_ZOOM.max(24.0 / min_dim.max(1.0));
        let new_zoom = (self.zoom * factor).clamp(floor, MAX_ZOOM);
        if new_zoom == self.zoom {
            return;
        }
        let rect = zoom_rect(
            self.precise_rect(client),
            anchor,
            new_zoom as f64 / self.zoom as f64,
        );
        self.zoom = new_zoom;
        self.layout_rect = Some(rect);
        apply_client_rect(window, cx, rect);
    }

    /// Back to 100%, the group anchored on its top-left corner.
    fn actual_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing() || self.zoom == 1.0 {
            return;
        }
        let Some(me) = client_rect(crate::platform::hwnd_of(window)) else {
            return;
        };
        let bbox = group_members(self.id)
            .iter()
            .filter_map(|m| client_rect(m.hwnd))
            .fold(me, |acc, r| acc.union(&r));
        let factor = 1.0 / self.zoom;
        self.zoom_group(factor, (bbox.left, bbox.top), window, cx);
    }

    // ---- export ---------------------------------------------------------------

    fn copy(&mut self, _: &CopyPin, window: &mut Window, cx: &mut Context<Self>) {
        self.export(window, cx, false, true);
    }

    fn save(&mut self, _: &SavePin, window: &mut Window, cx: &mut Context<Self>) {
        self.export(window, cx, true, false);
    }

    /// Composites this pin's group at its screen layout and hands the result
    /// to the regular export path. Compositing runs off the UI thread.
    fn export(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        write_file: bool,
        copy: bool,
    ) {
        if self.editing() {
            self.finish_edit(true, window, cx);
        }
        let tiles = snapshots(cx, self.group(), self.snapshot(window));
        std::thread::spawn(move || {
            let images: Vec<TileImage> = tiles
                .iter()
                .map(|t| TileImage {
                    rect: t.precise_rect,
                    pixels: t.pixels.as_slice(),
                    width: t.size.0,
                    height: t.size.1,
                    zoom: t.zoom,
                })
                .collect();
            let (mut px, w, h) = match compose(&images) {
                Ok(v) => v,
                Err(e) => {
                    // Background thread: the log is this path's error channel
                    // today, but it now names the requested size and limit.
                    crate::perf::log_app(&format!("pin export: {e}"));
                    return;
                }
            };
            if copy {
                flatten_gaps(&mut px, [255, 255, 255, 255]);
            }
            crate::app::export_pin_pixels(px, w, h, write_file, copy);
        });
    }

    /// Starts the OLE drag loop once the armed press crosses the drag
    /// threshold. The gesture is disarmed immediately (the loop runs on its
    /// own thread and reports through `export_dragging`), so a slow start
    /// cannot double-fire and a mid-drag release is not a move.
    fn begin_export_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let origin = self.export_drag_origin.take();
        if origin.is_none() {
            return;
        }
        // A global gate keeps two pins from racing the same threshold press
        // (one OLE loop per process is also what the shell tolerates best).
        if EXPORT_DRAG_ACTIVE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        // Hold the gate until the OLE loop ends (the guard moves into its
        // thread) or until any early return below drops it.
        let gate = ExportDragGuard;
        // The system threshold decides when a press becomes a drag, matching
        // Qt's startDragDistance. Not yet past it: keep the gesture armed.
        let (dx, dy) = unsafe {
            let mut cursor = POINT::default();
            if GetCursorPos(&mut cursor).is_err() {
                self.export_drag_origin = origin;
                return;
            }
            (cursor.x - origin.unwrap().0, cursor.y - origin.unwrap().1)
        };
        let threshold = unsafe {
            GetSystemMetrics(SYSTEM_METRICS_INDEX(if dx.abs() >= dy.abs() {
                SM_CXDRAG.0
            } else {
                SM_CYDRAG.0
            }))
        };
        if dx.abs() < threshold && dy.abs() < threshold {
            self.export_drag_origin = origin;
            return;
        }
        let tiles = snapshots(cx, self.group(), self.snapshot(window));
        std::thread::spawn(move || {
            // Keep the gate for the whole loop; compose/stage errors return
            // early and must release it too.
            let _gate = gate;
            let images: Vec<TileImage> = tiles
                .iter()
                .map(|t| TileImage {
                    rect: t.precise_rect,
                    pixels: t.pixels.as_slice(),
                    width: t.size.0,
                    height: t.size.1,
                    zoom: t.zoom,
                })
                .collect();
            let (mut px, _w, _h) = match compose(&images) {
                Ok(v) => v,
                Err(e) => {
                    crate::perf::log_app(&format!("drag export compose: {e}"));
                    return;
                }
            };
            // A dragged image pasted into a white-background target must not
            // bring its transparent gaps along, same as copy.
            flatten_gaps(&mut px, [255, 255, 255, 255]);
            let extension = {
                let export = &crate::config::get().screenshot.export;
                crate::codec::effective_format(export.format).extension()
            };
            let staged = crate::dragexport::stage(&px, extension);
            let path = match staged {
                Ok(p) => p,
                Err(e) => {
                    crate::perf::log_app(&format!("drag export stage: {e}"));
                    return;
                }
            };
            let bytes = px.len() as u64;
            // 先占位再启动 OLE：账本满额时拒绝本次拖出并回收刚暂存的文件。
            // 漏掉 reserve 会让 complete() 找不到记录，expires_at 永远是 None，
            // 文件连同目录永久残留在临时目录里。
            let reserved = drag_retention()
                .lock()
                .unwrap()
                .reserve(path.clone(), bytes);
            let lease = match reserved {
                Some(lease) => lease,
                None => {
                    crate::dragexport::discard(&path);
                    crate::perf::log_app("导出队列已满");
                    return;
                }
            };
            let (tx, rx) = std::sync::mpsc::channel();
            crate::drag::drag_file_async(path.clone(), move |outcome| {
                let _ = tx.send(outcome);
            });
            // OLE needs an MTA here: the writer thread below touches WIC.
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                );
            }
            // OLE 卡死时回调可能永远不来：无限 recv 会让租约的 expires_at
            // 永远是 None，白占一个名额和预算。超时按"拒绝"收尾，complete()
            // 会照常启动宽限期，使条目进入过期流程；迟到的 send 只会 Err，无害。
            let outcome = match rx.recv_timeout(Duration::from_secs(5 * 60)) {
                Ok(outcome) => outcome,
                Err(e) => {
                    crate::perf::log_app(&format!("拖出回调超时（{e}），按拒绝处理"));
                    crate::drag::DragOutcome::Failed
                }
            };
            let retention = &mut drag_retention().lock().unwrap();
            retention.complete(&lease, outcome.accepted());
            // A refused drop keeps its grace window too: the receiver may have
            // read the file mid-drop even though it refused the final effect.
            // `_gate` drops here, releasing the gate after every path.
        });
    }

    // ---- chrome ---------------------------------------------------------------

    /// A group shows one ✕, on the tile whose top-right corner is nearest the
    /// group's top-right corner. A lone pin is trivially that tile.
    fn holds_close_button(&self, window: &Window) -> bool {
        let members = group_members(self.id);
        if members.is_empty() {
            return true;
        }
        let Some(me) = client_rect(crate::platform::hwnd_of(window)) else {
            return true;
        };
        let rects: Vec<(u64, IRect)> = std::iter::once((self.id, me))
            .chain(
                members
                    .iter()
                    .filter_map(|m| Some((m.id, client_rect(m.hwnd)?))),
            )
            .collect();
        let bbox = rects.iter().skip(1).fold(me, |acc, (_, r)| acc.union(r));
        let corner_gap = |r: &IRect| (bbox.right - r.right) as i64 + (r.top - bbox.top) as i64;
        rects
            .iter()
            .min_by_key(|(id, r)| (corner_gap(r), *id))
            .is_some_and(|(id, _)| *id == self.id)
    }

    /// Where the ✓ sits, in logical client coordinates: centred on the seam
    /// with the target, just inside this window.
    fn stitch_button_origin(&self, window: &Window) -> Option<(f32, f32)> {
        let p = self.pending?;
        let me = client_rect(crate::platform::hwnd_of(window))?;
        let other = client_rect(entry(p.target)?.hwnd)?;
        let scale = window.scale_factor().max(0.01);
        let (w, h) = (me.width() as f32 / scale, me.height() as f32 / scale);
        let mid_y = (me.top.max(other.top) + me.bottom.min(other.bottom)) as f32 / 2.0;
        let mid_x = (me.left.max(other.left) + me.right.min(other.right)) as f32 / 2.0;
        let along_y = ((mid_y - me.top as f32) / scale - 11.0).clamp(6.0, (h - 28.0).max(6.0));
        let along_x = ((mid_x - me.left as f32) / scale - 11.0).clamp(6.0, (w - 28.0).max(6.0));
        Some(match p.side {
            // Attached to the target's right: the seam is this window's left edge.
            Side::Right => (6.0, along_y),
            Side::Left => (w - 28.0, along_y),
            Side::Bottom => (along_x, 6.0),
            Side::Top => (along_x, h - 28.0),
        })
    }
}

/// Repaints other pins whose chrome depends on group membership.
fn refresh(cx: &mut App, pins: &[PinEntry]) {
    for e in pins {
        let _ = e.handle.update(cx, |_, _, cx| cx.notify());
    }
}

/// Drops every other pin's pending snap that points at one of `ids`. `except`
/// is the caller's own window, which cannot be re-entered.
fn clear_pendings_targeting(cx: &mut App, ids: &[u64], except: u64) {
    for e in entries() {
        if e.id == except {
            continue;
        }
        let _ = e.handle.update(cx, |view, _, cx| {
            if view.pending.is_some_and(|p| ids.contains(&p.target)) {
                view.pending = None;
                cx.notify();
            }
        });
    }
}

impl Focusable for PinView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// IME plumbing for the group's text draft; see `annotate::TextDraft`.
impl EntityInputHandler for PinView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let edit = pin_edit::edit_for_group(self.group())?;
        let e = edit.borrow();
        Some(e.text.as_ref()?.text_for_range(range_utf16, actual_range))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let edit = pin_edit::edit_for_group(self.group())?;
        let e = edit.borrow();
        Some(e.text.as_ref()?.selected_range())
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let edit = pin_edit::edit_for_group(self.group())?;
        let e = edit.borrow();
        e.text.as_ref()?.marked_range()
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        if let Some(edit) = pin_edit::edit_for_group(self.group()) {
            if let Some(t) = edit.borrow_mut().text.as_mut() {
                t.unmark();
            }
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self.group();
        let Some(edit) = pin_edit::edit_for_group(group) else {
            return;
        };
        if let Some(t) = edit.borrow_mut().text.as_mut() {
            t.replace(range_utf16, new_text);
        }
        refresh_group(cx, group);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self.group();
        let Some(edit) = pin_edit::edit_for_group(group) else {
            return;
        };
        if let Some(t) = edit.borrow_mut().text.as_mut() {
            t.replace_and_mark(range_utf16, new_text);
        }
        refresh_group(cx, group);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let offset = self.edit_offset(window)?;
        let edit = pin_edit::edit_for_group(self.group())?;
        let e = edit.borrow();
        let scale = window.scale_factor().max(0.01);
        Some(e.text.as_ref()?.ime_bounds(element_bounds, offset, scale))
    }

    fn character_index_for_point(
        &mut self,
        _: GpuiPoint<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for PinView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The window always hugs the image, so drawing at the viewport size
        // keeps a group's tiles pixel-exact after edge-wise zooming.
        let viewport = window.viewport_size();
        let w = f32::from(viewport.width);
        let h = f32::from(viewport.height);
        let scale = window.scale_factor().max(0.01);
        let editing = pin_edit::edit_for_group(self.group());
        let buttons = editing.is_none() && w >= BUTTON_MIN_SIDE && h >= BUTTON_MIN_SIDE;
        let close_here = buttons && self.holds_close_button(window);
        let stitch_at = buttons.then(|| self.stitch_button_origin(window)).flatten();

        // Everything the edit layers need, gathered while the group's edit is
        // borrowed; the paint closure runs later and must own its data.
        let mut cursor = CursorStyle::Arrow;
        let mut edit_paint = None;
        let mut text_layer = None;
        let mut preparing = false;
        if let (Some(edit), Some(offset)) = (editing.as_ref(), self.edit_offset(window)) {
            let e = edit.borrow();
            preparing = e.preparing;
            let marks: Vec<Element> = e.session.visible().cloned().collect();
            let masks = annotate::mark_masks(&e.mask_cache, &marks);
            let mosaic = annotate::mosaic_tiles(&e.mosaic_cache, &marks, e.extent, |x, y, w, h| {
                e.readback(x, y, w, h)
            });
            // Badges render as divs (element_mask returns None for them), and
            // a held draft badge must preview under the cursor as it is placed.
            let mut badge_elems: Vec<Element> = e.session.elements.clone();
            if let Some(draft) = &e.session.draft {
                if matches!(draft.shape, Shape::Badge { .. }) {
                    badge_elems.push(draft.clone());
                }
            }
            let frame = Rect::new(0.0, 0.0, w as f64, h as f64);
            text_layer = Some(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(0.))
                    .size_full()
                    .children([
                        annotate::badge_layer(frame, offset, scale, &badge_elems)
                            .into_any_element(),
                        annotate::text_layer(TextLayer {
                            frame,
                            offset,
                            scale,
                            elements: &e.session.elements,
                            draft: e.text.as_ref(),
                            stroke: e.session.stroke_argb(),
                        })
                        .into_any_element(),
                    ])
                    .into_any_element(),
            );
            let input = e
                .text
                .is_some()
                .then(|| (self.focus_handle.clone(), cx.entity()));
            cursor = match (e.text.is_some(), e.session.tool) {
                (true, _) | (_, Tool::Text) => CursorStyle::IBeam,
                (_, Tool::Select) => CursorStyle::Arrow,
                _ => CursorStyle::Crosshair,
            };
            edit_paint = Some((marks, masks, mosaic, offset, input));
        }
        // Border hot zones show the resize cursor; a drag in progress keeps it.
        if editing.is_none() {
            if let Some(c) = self
                .resize
                .as_ref()
                .map(|r| r.edge)
                .or(self.hover_edge)
                .and_then(ResizeEdge::cursor)
            {
                cursor = c;
            }
        }
        // Drags and strokes must keep flowing once the cursor leaves this
        // window (mouse capture delivers them), but element listeners are
        // hover-gated: move and left-up are taken at window level instead.
        let on_move = cx.listener(Self::on_mouse_move);
        let on_up = cx.listener(Self::on_mouse_up);
        let mouse_layer = canvas(
            move |_, _, _| (),
            move |bounds, _, window, cx| {
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble {
                        on_move(e, window, cx);
                    }
                });
                window.on_mouse_event(move |e: &MouseUpEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                        on_up(e, window, cx);
                    }
                });
                if let Some((marks, masks, mosaic, offset, input)) = edit_paint {
                    if let Some((focus, entity)) = input {
                        window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);
                    }
                    annotate::paint_marks(window, &marks, &masks, &mosaic, offset, scale);
                }
            },
        )
        .absolute()
        .size_full();

        div()
            .id("pin-root")
            .size_full()
            .overflow_hidden()
            .key_context("Pin")
            .track_focus(&self.focus_handle)
            .cursor(cursor)
            .occlude()
            .rounded_md()
            .border_1()
            .border_color(theme::border().opacity(0.7))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::detach))
            .on_action(cx.listener(Self::annotate))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .child(
                img(ImageSource::Render(self.image.clone()))
                    .w(px(w))
                    .h(px(h)),
            )
            .child(
                div()
                    .absolute()
                    .size_full()
                    .child(mouse_layer)
                    .children(text_layer),
            )
            // The backdrop is composed on a worker. Show a visible state while
            // it is not ready instead of silently accepting strokes that cannot
            // yet sample the image for mosaic effects.
            .when(preparing, |el| {
                el.child(
                    div()
                        .absolute()
                        .top(px(6.))
                        .left(px(6.))
                        .rounded_md()
                        .bg(theme::elevated_surface().opacity(0.85))
                        .text_color(theme::primary_fg())
                        .text_size(px(12.))
                        .child("合成中…"),
                )
            })
            // A close affordance that does not cover small pins: under 48
            // logical px on either side, Esc / double-click remain the way out.
            .children(close_here.then(|| {
                round_button("pin-close", 6.0, "✕", theme::danger(), theme::on_danger())
                    .right(px(6.))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_group(window, cx);
                        }),
                    )
            }))
            .children(stitch_at.map(|(x, y)| {
                round_button("pin-stitch", y, "✓", theme::accent(), theme::on_accent())
                    .left(px(x))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.confirm_stitch(window, cx);
                        }),
                    )
            }))
    }
}

/// A 22 px round overlay button. The caller sets the horizontal anchor
/// (`left` or `right`) and the mouse-up handler; the press is swallowed here
/// so it neither starts a drag nor reads as a double-click close.
fn round_button(
    id: &'static str,
    y: f32,
    label: &'static str,
    hover_bg: gpui::Hsla,
    hover_fg: gpui::Hsla,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .absolute()
        .top(px(y))
        .w(px(22.))
        .h(px(22.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme::surface().opacity(0.85))
        .text_color(theme::primary_fg())
        .text_size(px(12.))
        .cursor(CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(label)
}

// ---- context menu -------------------------------------------------------------

const CMD_COPY: u32 = 1;
const CMD_SAVE: u32 = 2;
const CMD_ANNOTATE: u32 = 3;
const CMD_FINISH_EDIT: u32 = 4;
const CMD_CANCEL_EDIT: u32 = 5;
const CMD_STITCH: u32 = 6;
const CMD_DETACH: u32 = 7;
const CMD_ACTUAL_SIZE: u32 = 8;
const CMD_CLOSE: u32 = 9;
const CMD_CLOSE_ALL: u32 = 10;

pub(crate) enum MenuEntry {
    Item { cmd: u32, label: &'static str },
    Separator,
}

/// The menu for pin `id`, built from its current state. `None` when the pin
/// is gone.
pub(crate) fn menu_items(cx: &mut App, id: u64) -> Option<Vec<MenuEntry>> {
    let e = entry(id)?;
    let (pending, zoomed) = e
        .handle
        .read(cx)
        .map(|v| (v.pending.is_some(), v.zoom != 1.0))
        .ok()?;
    let editing = pin_edit::is_editing(e.group);
    let grouped = group_entries(e.group).len() > 1;
    use MenuEntry::*;
    let mut items = vec![
        Item {
            cmd: CMD_COPY,
            label: "复制\tCtrl+C",
        },
        Item {
            cmd: CMD_SAVE,
            label: "保存\tCtrl+S",
        },
        Separator,
    ];
    if editing {
        items.push(Item {
            cmd: CMD_FINISH_EDIT,
            label: "完成标注\tEnter",
        });
        items.push(Item {
            cmd: CMD_CANCEL_EDIT,
            label: "取消标注\tEsc",
        });
    } else {
        items.push(Item {
            cmd: CMD_ANNOTATE,
            label: "标注\tCtrl+E",
        });
        if pending {
            items.push(Item {
                cmd: CMD_STITCH,
                label: "确认拼接\tEnter",
            });
        }
        if grouped {
            items.push(Item {
                cmd: CMD_DETACH,
                label: "从组中拆开\tCtrl+D",
            });
        }
        if zoomed {
            items.push(Item {
                cmd: CMD_ACTUAL_SIZE,
                label: "实际大小",
            });
        }
    }
    items.push(Separator);
    items.push(Item {
        cmd: CMD_CLOSE,
        label: "关闭\tEsc",
    });
    items.push(Item {
        cmd: CMD_CLOSE_ALL,
        label: "关闭所有贴图",
    });
    Some(items)
}

/// Shows the native popup at the cursor, owned by `owner` (the hidden
/// window, as for the tray). Blocks in the menu's modal loop.
pub(crate) fn track_menu(owner: HWND, items: &[MenuEntry]) -> Option<u32> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (i, item) in items.iter().enumerate() {
            match item {
                MenuEntry::Separator => {
                    let _ = InsertMenuW(
                        menu,
                        i as u32,
                        MF_BYPOSITION | MF_SEPARATOR,
                        0,
                        PCWSTR::null(),
                    );
                }
                MenuEntry::Item { cmd, label } => {
                    // The menu copies the string on insert.
                    let text: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
                    let _ = InsertMenuW(
                        menu,
                        i as u32,
                        MF_BYPOSITION | MF_STRING,
                        *cmd as usize,
                        PCWSTR(text.as_ptr()),
                    );
                }
            }
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(owner);
        let cmd = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            Some(0),
            owner,
            None,
        );
        let _ = DestroyMenu(menu);
        let cmd = cmd.0 as u32;
        (cmd != 0).then_some(cmd)
    }
}

/// Number of live pinned images, shown on the tray's 关闭所有贴图 row.
pub fn count() -> usize {
    entries().len()
}

/// Close every pinned image. Shared by the tray's 关闭所有贴图 command and the
/// per-pin context menu's 关闭全部钉图 command.
pub fn close_all(cx: &mut App) {
    for e in entries() {
        if pin_edit::is_editing(e.group) {
            pin_edit::finish(cx, e.group, false, Vec::new());
        }
        let _ = e.handle.update(cx, |_, window, _| window.remove_window());
    }
    PINS.with(|slot| slot.borrow_mut().clear());
}

pub(crate) fn run_menu_command(cx: &mut App, id: u64, cmd: u32) {
    if cmd == CMD_CLOSE_ALL {
        close_all(cx);
        return;
    }
    let Some(handle) = entry_handle(id) else {
        return;
    };
    let _ = handle.update(cx, |view, window, cx| {
        // The menu took the foreground; keys should reach the pin again.
        window.focus(&view.focus_handle, cx);
        match cmd {
            CMD_COPY => view.export(window, cx, false, true),
            CMD_SAVE => view.export(window, cx, true, false),
            CMD_ANNOTATE => view.start_edit(window, cx),
            CMD_FINISH_EDIT => view.finish_edit(true, window, cx),
            CMD_CANCEL_EDIT => view.finish_edit(false, window, cx),
            CMD_STITCH => view.confirm_stitch(window, cx),
            CMD_DETACH => view.detach(&DetachPin, window, cx),
            CMD_ACTUAL_SIZE => view.actual_size(window, cx),
            CMD_CLOSE => view.close_group(window, cx),
            _ => {}
        }
    });
}

// ---- pixels / geometry ----------------------------------------------------------

/// Packs a possibly-padded BGRA buffer to a tight `width * 4` stride and
/// forces every pixel opaque -- the crop is opaque, but the source alpha can
/// be 0 from some desktop sources, which would render as blank in gpui.
fn repack_opaque(pixels: &[u8], w: u32, h: u32, stride: u32) -> Vec<u8> {
    // A short crop (or a bogus stride) used to panic on the slice below; under
    // `panic = "abort"` that killed the process. The caller already treats an
    // empty buffer as a size mismatch and reports it.
    let tight = match (w as u64).checked_mul(4) {
        Some(n) => n as usize,
        None => return Vec::new(),
    };
    let needed = match (stride as u64).checked_mul(h as u64) {
        Some(n) => n,
        None => return Vec::new(),
    };
    if tight == 0 || h == 0 || (stride as usize) < tight || (pixels.len() as u64) < needed {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(tight.saturating_mul(h as usize));
    for row in 0..h as usize {
        let start = row * stride as usize;
        out.extend_from_slice(&pixels[start..start + tight]);
    }
    // `out` is tightly packed BGRA, so the remainder is always empty.
    for px in out.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }
    out
}

/// Outer HWND rect whose client area equals `desired` (screen coordinates),
/// mirroring the overlay's helper. A caption-free window still reports small
/// non-client insets on some themes; without this the pinned image shifts off
/// the captured pixels.
pub(crate) fn window_rect_for_client_area(hwnd: HWND, desired: IRect) -> IRect {
    let mut window = RECT::default();
    let mut client = RECT::default();
    let mut origin = POINT::default();
    unsafe {
        if GetWindowRect(hwnd, &mut window).is_err()
            || GetClientRect(hwnd, &mut client).is_err()
            || !ClientToScreen(hwnd, &mut origin).as_bool()
        {
            return desired;
        }
    }
    let left = origin.x - window.left;
    let top = origin.y - window.top;
    let right = window.right - (origin.x + client.right - client.left);
    let bottom = window.bottom - (origin.y + client.bottom - client.top);
    IRect::new(
        desired.left - left,
        desired.top - top,
        desired.right + right,
        desired.bottom + bottom,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(left: bool, right: bool, top: bool, bottom: bool) -> ResizeEdge {
        ResizeEdge {
            left,
            right,
            top,
            bottom,
        }
    }

    /// 同一 group 的重开必须等所有在途 flatten 结束：计数要成对、互不串组。
    #[test]
    fn inflight_finish_counter_tracks_each_group_independently() {
        assert!(!has_inflight_finish(41));
        assert!(!has_inflight_finish(42));
        begin_finish(42);
        begin_finish(42);
        assert!(has_inflight_finish(42));
        assert!(!has_inflight_finish(41));
        end_finish(42);
        assert!(has_inflight_finish(42), "还有一次完成在途");
        end_finish(42);
        assert!(!has_inflight_finish(42));
        end_finish(42); // 多余解除不能把别的组状态带坏
        assert!(!has_inflight_finish(41));
        assert!(!has_inflight_finish(42));
    }

    /// 有在途 finish 时清账本必须整体跳过：worker 写完会自己 end_finish，
    /// 提前清掉 INFLIGHT/PENDING 会让下一笔完成不等本次写回，标注互相覆盖。
    #[test]
    fn clear_group_writes_keeps_ledgers_while_finish_is_inflight() {
        let write = |id: u64| (id, Vec::<u8>::new(), 2u32, 2u32, 1.0f32);
        enqueue_pending_writes(44, vec![write(7)]);
        begin_finish(44);

        clear_group_writes(44);
        assert!(has_inflight_finish(44), "在途完成不能被清掉");
        assert_eq!(
            take_pending_writes(44)
                .iter()
                .map(|w| w.0)
                .collect::<Vec<_>>(),
            vec![7],
            "待写回队列不能被清掉"
        );

        end_finish(44);
        assert!(!has_inflight_finish(44));
        // 没有在途 finish 时才正常清空残留账本。
        enqueue_pending_writes(44, vec![write(8)]);
        clear_group_writes(44);
        assert!(!has_inflight_finish(44));
        assert!(take_pending_writes(44).is_empty());
    }

    /// 冲刷的重入保护必须按 group 隔离：A 组冲刷途中 B 组仍能冲刷，
    /// 同一 group 的重入才被跳过，否则队列会永久卡住。
    #[test]
    fn flush_guard_is_per_group_not_global() {
        assert!(begin_flush(41));
        assert!(begin_flush(42), "B 组不该被 A 组的冲刷吞掉");
        assert!(!begin_flush(41), "同一 group 的重入必须被跳过");
        assert!(!begin_flush(42));
        end_flush(41);
        assert!(begin_flush(41), "A 组结束后应能再次冲刷");
        assert!(!begin_flush(42), "B 组仍在冲刷中");
        end_flush(41);
        end_flush(42);
        assert!(begin_flush(41));
        assert!(begin_flush(42));
        end_flush(41);
        end_flush(42);
    }

    /// 待写回队列按入队顺序取出，且按 group 分开。
    #[test]
    fn pending_writes_are_drained_fifo_and_per_group() {
        let write = |id: u64| (id, Vec::<u8>::new(), 2u32, 2u32, 1.0f32);
        enqueue_pending_writes(41, vec![write(1), write(2)]);
        enqueue_pending_writes(42, vec![write(3)]);
        enqueue_pending_writes(41, vec![write(4)]);
        let a = take_pending_writes(41);
        assert_eq!(a.iter().map(|w| w.0).collect::<Vec<_>>(), vec![1, 2, 4]);
        assert!(take_pending_writes(41).is_empty(), "取出后队列应清空");
        let b = take_pending_writes(42);
        assert_eq!(b.iter().map(|w| w.0).collect::<Vec<_>>(), vec![3]);
    }

    /// The eight hot zones: four sides, four corners, nothing inside.
    #[test]
    fn resize_hot_zones_hit_edges_corners_and_inside() {
        let size = (200.0, 100.0);
        let band = RESIZE_BAND;
        assert_eq!(
            resize_edge_at((2.0, 50.0), size, band),
            Some(edge(true, false, false, false))
        );
        assert_eq!(
            resize_edge_at((198.0, 50.0), size, band),
            Some(edge(false, true, false, false))
        );
        assert_eq!(
            resize_edge_at((100.0, 1.0), size, band),
            Some(edge(false, false, true, false))
        );
        assert_eq!(
            resize_edge_at((100.0, 99.0), size, band),
            Some(edge(false, false, false, true))
        );
        assert_eq!(
            resize_edge_at((1.0, 1.0), size, band),
            Some(edge(true, false, true, false))
        );
        assert_eq!(
            resize_edge_at((199.0, 1.0), size, band),
            Some(edge(false, true, true, false))
        );
        assert_eq!(
            resize_edge_at((1.0, 99.0), size, band),
            Some(edge(true, false, false, true))
        );
        assert_eq!(
            resize_edge_at((199.0, 99.0), size, band),
            Some(edge(false, true, false, true))
        );
        // Interior, and points outside the client area, resize nothing.
        assert_eq!(resize_edge_at((100.0, 50.0), size, band), None);
        assert_eq!(resize_edge_at((-1.0, 50.0), size, band), None);
        assert_eq!(resize_edge_at((201.0, 50.0), size, band), None);
        assert_eq!(resize_edge_at((100.0, -1.0), size, band), None);
        // A tiny pin shrinks the band instead of marking both sides at once.
        let tiny = (18.0, 18.0);
        for x in [0.0, 4.0, 9.0, 14.0, 18.0] {
            if let Some(e) = resize_edge_at((x, 9.0), tiny, band) {
                assert!(!(e.left && e.right), "x {x} marked both sides");
                assert!(!(e.top && e.bottom), "x {x} marked both ends");
            }
        }
    }

    /// A side drag pins the opposite edge; the other axis grows symmetrically
    /// so the image keeps its aspect.
    #[test]
    fn resize_from_a_side_keeps_the_opposite_edge_fixed() {
        let start = Rect::new(100.0, 100.0, 200.0, 100.0);
        let out = resize_rect(start, edge(false, true, false, false), 100.0, 0.0, 1.0);
        // x stays anchored on the left edge; y grows both ways about the
        // original centre.
        assert_eq!(out.rect, Rect::new(100.0, 75.0, 300.0, 150.0));
        assert_eq!(
            out.rect.y + out.rect.height / 2.0,
            start.y + start.height / 2.0
        );
        assert!((out.zoom - 1.5).abs() < 1e-6);
        assert!((out.factor - 1.5).abs() < 1e-6);
        // The invariant the group path relies on.
        assert_eq!(zoom_rect(start, out.anchor, out.factor), out.rect);

        // Dragging the left edge outward moves only x; the right edge stays.
        let out = resize_rect(start, edge(true, false, false, false), -50.0, 0.0, 1.0);
        assert_eq!(out.rect.right(), start.right());
        assert_eq!(out.rect.width, 250.0);
        assert_eq!(out.rect.height, 125.0);
        assert_eq!(out.rect.x, 50.0);

        // A vertical drag keeps y and centres x on the original centre.
        let out = resize_rect(start, edge(false, false, false, true), 0.0, 50.0, 1.0);
        assert_eq!(out.rect.y, start.y);
        assert_eq!(out.rect.height, 150.0);
        assert_eq!(out.rect.width, 300.0);
        assert_eq!(
            out.rect.x + out.rect.width / 2.0,
            start.x + start.width / 2.0
        );
    }

    /// A corner drag anchors the opposite corner and uses the mean of the two
    /// axis ratios.
    #[test]
    fn resize_from_a_corner_keeps_the_opposite_corner_fixed() {
        let start = Rect::new(100.0, 100.0, 200.0, 100.0);
        let out = resize_rect(start, edge(false, true, false, true), 40.0, 30.0, 1.0);
        // (1.2 + 1.3) / 2 = 1.25
        assert_eq!(out.rect, Rect::new(100.0, 100.0, 250.0, 125.0));
        assert!((out.zoom - 1.25).abs() < 1e-6);
        assert_eq!(zoom_rect(start, out.anchor, out.factor), out.rect);

        // Grabbing the top-left corner pins the bottom-right one.
        let out = resize_rect(start, edge(true, false, true, false), -20.0, -10.0, 1.0);
        assert_eq!(out.rect.right(), start.right());
        assert_eq!(out.rect.bottom(), start.bottom());
        assert_eq!(zoom_rect(start, out.anchor, out.factor), out.rect);
    }

    /// Inward drags stop at 64 px per side; outward drags stop at 4x the
    /// image's native size.
    #[test]
    fn resize_clamps_min_side_and_max_zoom() {
        let start = Rect::new(100.0, 100.0, 200.0, 100.0);
        let shrink = resize_rect(start, edge(false, true, false, false), -10000.0, 0.0, 1.0);
        assert_eq!(shrink.rect.width, MIN_RESIZE_SIDE * 2.0);
        assert_eq!(shrink.rect.height, MIN_RESIZE_SIDE);
        // The frame stays inside the original bounds, anchored on the left.
        assert_eq!(shrink.rect.x, start.x);
        assert!(shrink.rect.right() <= start.right());

        let grow = resize_rect(start, edge(false, true, false, false), 10000.0, 0.0, 1.0);
        assert!((grow.zoom - MAX_RESIZE_ZOOM).abs() < 1e-6);
        assert_eq!(grow.rect.width, 200.0 * MAX_RESIZE_ZOOM as f64);

        // Already zoomed in 2x: the ceiling is a 2x factor, not 4x.
        let grow = resize_rect(start, edge(false, true, false, false), 10000.0, 0.0, 2.0);
        assert!((grow.zoom - MAX_RESIZE_ZOOM).abs() < 1e-6);
        assert!((grow.factor - 2.0).abs() < 1e-6);

        // A thin strip can never reach 64 px on the short side without
        // exceeding the zoom ceiling, which wins.
        let thin = Rect::new(0.0, 0.0, 200.0, 10.0);
        let out = resize_rect(thin, edge(false, true, false, false), -10000.0, 0.0, 1.0);
        assert!((out.zoom - MAX_RESIZE_ZOOM).abs() < 1e-6);
        assert_eq!(out.rect.height, 10.0 * MAX_RESIZE_ZOOM as f64);
    }

    /// `zoom` is the frame width over the image width, so the wheel zoom's
    /// arithmetic stays valid after a drag.
    #[test]
    fn resize_zoom_tracks_the_frame_width() {
        let start = Rect::new(100.0, 100.0, 200.0, 100.0);
        for dx in [-150.0, -50.0, 0.0, 33.0, 500.0] {
            let out = resize_rect(start, edge(false, true, false, false), dx, 0.0, 1.0);
            let ratio = out.rect.width / start.width;
            assert!(
                (ratio - out.zoom as f64).abs() < 1e-6,
                "dx {dx}: zoom {} vs ratio {ratio}",
                out.zoom
            );
            assert!((out.rect.width / out.rect.height - 2.0).abs() < 1e-6);
        }
    }

    #[test]
    fn resize_cursor_matches_each_edge() {
        assert_eq!(
            edge(true, false, false, false).cursor(),
            Some(CursorStyle::ResizeLeftRight)
        );
        assert_eq!(
            edge(false, true, false, false).cursor(),
            Some(CursorStyle::ResizeLeftRight)
        );
        assert_eq!(
            edge(false, false, true, false).cursor(),
            Some(CursorStyle::ResizeUpDown)
        );
        assert_eq!(
            edge(false, false, false, true).cursor(),
            Some(CursorStyle::ResizeUpDown)
        );
        assert_eq!(
            edge(true, false, true, false).cursor(),
            Some(CursorStyle::ResizeUpLeftDownRight)
        );
        assert_eq!(
            edge(false, true, false, true).cursor(),
            Some(CursorStyle::ResizeUpLeftDownRight)
        );
        assert_eq!(
            edge(false, true, true, false).cursor(),
            Some(CursorStyle::ResizeUpRightDownLeft)
        );
        assert_eq!(edge(false, false, false, false).cursor(), None);
    }

    /// Every other tile of a group follows the same factor about the same
    /// anchor, so seams stay flush.
    #[test]
    fn resize_scales_the_group_about_the_anchor() {
        let a = Rect::new(100.0, 100.0, 200.0, 100.0);
        let b = Rect::new(300.0, 100.0, 200.0, 100.0);
        let out = resize_rect(a, edge(false, true, false, false), 100.0, 0.0, 1.0);
        let b2 = zoom_rect(b, out.anchor, out.factor);
        assert!((out.rect.right() - b2.x).abs() < 1e-6, "seam stays closed");
        assert!((b2.width / b.width - out.factor).abs() < 1e-6);
        assert_eq!(b2.y, out.rect.y);
        assert_eq!(b2.height, out.rect.height);
    }

    /// One move hands the window a single physical-px rect that carries both
    /// the new size and the anchored corner: rounding it for `SetWindowPos`
    /// must not move the edge the pointer is dragging or the edge that stays
    /// put, and group members rounded the same way still meet it flush.
    #[test]
    fn resize_delivers_size_and_anchor_together() {
        let start = Rect::new(120.0, 80.0, 240.0, 120.0);
        let cases = [
            (edge(false, true, false, false), 60.0, 0.0),
            (edge(true, false, false, false), -60.0, 0.0),
            (edge(false, false, false, true), 0.0, 30.0),
            (edge(false, false, true, false), 0.0, -30.0),
            (edge(false, true, false, true), 60.0, 30.0),
            (edge(true, false, true, false), -60.0, -30.0),
        ];
        for (e, dx, dy) in cases {
            let out = resize_rect(start, e, dx, dy, 1.0);
            let window = rounded_rect(out.rect);
            // What the window is told: its own new width/height, once.
            assert_eq!(window.width() as f64, out.rect.width);
            assert_eq!(window.height() as f64, out.rect.height);
            // The corner (or edge midpoint) opposite the grab stays put.
            if e.left {
                assert_eq!(window.right, out.anchor.0);
            } else if e.right {
                assert_eq!(window.left, out.anchor.0);
            }
            if e.top {
                assert_eq!(window.bottom, out.anchor.1);
            } else if e.bottom {
                assert_eq!(window.top, out.anchor.1);
            }
            // A member flush on the anchored side keeps meeting the tile.
            if e.right {
                let neighbour =
                    zoom_rect(Rect::new(0.0, 80.0, 120.0, 120.0), out.anchor, out.factor);
                assert_eq!(
                    rounded_rect(neighbour).right,
                    window.left,
                    "seam stays closed"
                );
            }
            if e.bottom {
                let neighbour = zoom_rect(
                    Rect::new(120.0, -40.0, 240.0, 120.0),
                    out.anchor,
                    out.factor,
                );
                assert_eq!(
                    rounded_rect(neighbour).bottom,
                    window.top,
                    "seam stays closed"
                );
            }
        }
    }

    /// The drag works in physical pixels end to end: a pointer travel of
    /// `logical * scale` device px grows the frame by exactly that much, so a
    /// 125% / 150% window is never scaled twice, and `zoom` stays the frame
    /// width over the image width.
    #[test]
    fn resize_applies_dpi_scale_once() {
        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            let logical = (200.0_f32, 100.0);
            let start = Rect::new(
                0.0,
                0.0,
                (logical.0 * scale).round() as f64,
                (logical.1 * scale).round() as f64,
            );
            // Pressing the logical right edge and travelling 24 logical px.
            let travel = (24.0 * scale).round() as f64;
            let out = resize_rect(start, edge(false, true, false, false), travel, 0.0, 1.0);
            assert!((out.rect.width - (start.width + travel)).abs() < 1e-6);
            assert_eq!(out.rect.x, start.x);
            assert!(
                (out.zoom as f64 - out.rect.width / start.width).abs() < 1e-5,
                "scale {scale}: zoom {} vs ratio {}",
                out.zoom,
                out.rect.width / start.width
            );
        }
    }
}
