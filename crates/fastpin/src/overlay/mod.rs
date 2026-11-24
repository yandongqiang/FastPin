//! GPUI freeze overlay: dim, marching ants, window snap, 15×15 magnifier.

pub(crate) mod annotate;
mod assets;
pub(crate) mod composite;
pub(crate) mod editor;
pub(crate) mod text_raster;
pub(crate) mod theme;
pub(crate) mod toolbar;

pub use assets::{AppAssets, ToolbarAssets};

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    actions, black, canvas, div, fill, hsla, img, outline, point, prelude::*, px, rgb, size, white,
    App, BorderStyle, Bounds, BoxShadow, ContentMask, Context, CursorStyle, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, ImageSource, InteractiveElement,
    KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathBuilder, Pixels, Point as GpuiPoint, RenderImage, SharedString, Subscription,
    UTF16Selection, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
    WindowOptions,
};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain1;
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CombineRgn, CreateRectRgn, DeleteObject, SetWindowRgn, RGN_DIFF,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClientRect, GetCursorPos, GetWindowLongPtrW, GetWindowRect,
    IsIconic, IsWindow, IsWindowVisible, PostMessageW, SetForegroundWindow,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW, WDA_EXCLUDEFROMCAPTURE, WHEEL_DELTA,
    WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use crate::capture::{crop_sdr_bgra, CapturedDesktop, Gpu, WgcLive};
use crate::error::Result;
use crate::geom::{
    hit_test, move_rect, physical_source_rect, resize_rect, Point, Rect, SelectionHitKind,
};
use crate::gpui_bridge::present_freeze_from_texture;
use crate::longcapture::Axis;
use crate::longworker::{
    Done, Job, LongWorker, Outcome, Poll, Rebase, ResultGate, StopReason, Verdict, FINISH_WAIT,
};
use crate::orb_stitcher::OrbStitcher;
use crate::overlay::annotate::{
    MaskCache, MosaicCache, MosaicTiles, SpotlightDimCache, SwatchesModel, TextDraft, TextLayer,
    ToolbarModel,
};
use crate::overlay::composite::{CoverageRun, ElementRuns};
use crate::overlay::editor::{Element, PressKind, Session, Tool};
use crate::overlay::toolbar::PHASE1_ITEMS;
use crate::perf::PerfSession;
use crate::platform::WM_APP_READY_SENTINEL;

const MAG_COUNT: u32 = 15;
const MAG_PIXEL: f32 = 10.0;
const MIN_RECT: f64 = 5.0;
/// 同一窗口连续超时这么多次后熔断其 UIA 候选，直到 hover 移到别的窗口。
const UIA_TIMEOUT_TRIP: u8 = 3;
/// 鼠标每轴移动不超过这个像素数时，仍复用上一次成功应用的 UIA 结果。
/// 否则鼠标每换一个像素就把 UIA 候选清空，回执永远追不上光标。
const UIA_POINT_TOLERANCE: i32 = 24;
/// 复用缓存矩形时，按当前点重新验证允许的外扩像素数。旧点处的元素矩形若不
/// 经这一步就进入候选表，触顶 100 时会挤掉当前位置的有效边界。
const UIA_CACHE_PAD: i32 = 8;

/// 缓存复用判定：目标窗口未变、且屏幕点仍在容差内时才复用上一次成功应用
/// 的 UIA 结果。纯函数，便于单测窗口切换 / 超距移动 / 无目标三种失效场景。
fn uia_cache_usable(
    last_target: Option<HWND>,
    cur_target: Option<HWND>,
    last_point: POINT,
    pt: POINT,
) -> bool {
    last_target.is_some()
        && last_target == cur_target
        && (pt.x - last_point.x).abs() <= UIA_POINT_TOLERANCE
        && (pt.y - last_point.y).abs() <= UIA_POINT_TOLERANCE
}

/// 复用缓存前按当前屏幕点重新验证：只保留（各边外扩 `pad` 像素后）包含当前
/// 点的矩形。当前点落在窗口空白区时旧位置的子元素不再入选，避免它们挤占
/// 候选名额。RECT 的 `contains` 是半开区间 `[left, right) × [top, bottom)`。
fn filter_uia_for_point(rects: &[RECT], pt: POINT, pad: i32) -> Vec<RECT> {
    let x = pt.x as i64;
    let y = pt.y as i64;
    let pad = pad as i64;
    rects
        .iter()
        .copied()
        .filter(|r| {
            x >= r.left as i64 - pad
                && x < r.right as i64 + pad
                && y >= r.top as i64 - pad
                && y < r.bottom as i64 + pad
        })
        .collect()
}

/// 一次异步 UIA 查询的状态。查询在专用 STA 线程上运行，这里只做非阻塞轮询。
struct UiaPending {
    rx: std::sync::mpsc::Receiver<crate::uia::UiaReply>,
    epoch: u64,
    /// 发起查询时锁定的目标窗口。200ms 内 HWND 可能关闭并被复用，应用结果前
    /// 必须重新确认当前 hover 窗口仍是它，否则会把吸附框画到新窗口上。
    target: HWND,
    /// 发起查询时的屏幕坐标；只有仍是当前 hover 点时才应用结果。
    point: POINT,
    started: Instant,
}

/// 完成 / Enter 按「输出」页的两个独立开关决定是否写文件与复制；
/// 工具栏的保存按钮另有语义，只写文件、不复制（见 `save`）。
fn complete_export_flags() -> (bool, bool) {
    let export = crate::config::get().screenshot.export;
    (export.auto_save, export.auto_copy)
}

/// 仅复制（Ctrl+Alt+W 快速截图）无论「输出」页的两个开关如何，都只写剪贴板：
/// 关掉 auto_copy 后不能让热键什么都不做，开启 auto_save 后也不能偷偷写文件。
fn export_flags_for(copy_only: bool) -> (bool, bool) {
    if copy_only {
        (false, true)
    } else {
        complete_export_flags()
    }
}

/// C# RegionCaptureWindow: dash {5,5}, offset = elapsed * -15.
pub const ANTS_DASH: [f32; 2] = [5.0, 5.0];
pub const ANTS_SPEED: f32 = -15.0;

pub fn marching_ants_dash_offset(elapsed_secs: f32) -> f32 {
    elapsed_secs * ANTS_SPEED
}

pub fn bench_auto_hide_enabled() -> bool {
    std::env::var_os("FASTPIN_BENCH").is_some()
}

actions!(
    overlay,
    [
        ConfirmSelection,
        CancelOverlay,
        UndoEdit,
        RedoEdit,
        CopyPickedColor,
        CycleColorFormat
    ]
);

thread_local! {
    static OVERLAY_WINDOW: RefCell<Option<WindowHandle<OverlayView>>> = const { RefCell::new(None) };
    // 同一个覆盖层窗口会被后续截图复用，光比对句柄分不清新旧捕获。每开始一
    // 张新图就换一代，finish 排的延迟隐藏只能作用于它自己那一代。
    static OVERLAY_GENERATION: Cell<u64> = const { Cell::new(0) };
    static KEYS_BOUND: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct OverlayResult {
    pub confirmed: bool,
    pub write_file: bool,
    pub copy: bool,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub marks: Vec<Element>,
}

fn empty_result() -> OverlayResult {
    OverlayResult {
        confirmed: false,
        write_file: false,
        copy: false,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        marks: Vec::new(),
    }
}

/// Region overlay root view. Freeze is DComp visual 0; this paints UI visual 1.
pub struct OverlayView {
    pub selection: Rect,
    pub hover: Option<Rect>,
    pub mouse: Point,
    pub freeze_size: (u32, u32),
    focus_handle: FocusHandle,
    cap: Option<CapturedDesktop>,
    perf: Rc<RefCell<Option<PerfSession>>>,
    freeze_swap: Option<IDXGISwapChain1>,
    /// Device generation the attached freeze visual belongs to. `None` while
    /// there is no published visual (or after a reset dropped it).
    freeze_generation: Option<u64>,
    /// A post-reset rebuild task is already scheduled.
    recovering_gpu: bool,
    origin_x: i32,
    origin_y: i32,
    phase: Phase,
    down: bool,
    down_pt: Point,
    from_drag: bool,
    has_sel: bool,
    windows: Vec<Rect>,
    /// Smart-selection candidates for the point under the cursor: the finest
    /// boundary first, coarser ancestors behind it. Empty when the mode is
    /// disabled or the cursor is over dead space, which is exactly when a free
    /// drag must be allowed to start.
    candidates: Vec<crate::smart_selection::Candidate>,
    /// The mode this capture was opened with. Read once so flipping the setting
    /// cannot change the picker's behaviour underneath an open overlay.
    smart_selection: crate::config::SmartSelectionMode,
    /// Where the last candidate lookup happened, so a mouse move that stays on
    /// the same pixel does not re-enumerate every child window.
    candidates_at: Option<Point>,
    /// 正在进行的 UIA 查询；UIA 在专用 STA 线程上运行，这里只非阻塞轮询，
    /// 超时或 epoch 过期的回执一律丢弃，绝不等待跨进程 COM。
    uia_pending: Option<UiaPending>,
    /// 最近一次成功应用的 UIA 矩形。鼠标小幅移动时复用，避免每次换像素都把
    /// UIA 候选清空；新查询不会清它，只有窗口切换 / 新一轮截图才失效。
    uia_last: Vec<RECT>,
    /// `uia_last` 对应的目标窗口（顶层应用窗口，不是 overlay 自己）。
    uia_last_target: Option<HWND>,
    /// `uia_last` 对应的查询屏幕点，用于容差比较。
    uia_last_point: POINT,
    /// UIA 查询序号，用于丢弃鼠标移动后返回的过期结果。
    uia_epoch: u64,
    /// 连续超时次数；达到阈值后暂时禁用当前窗口的 UIA 候选（熔断）。
    uia_timeouts: u8,
    /// 被熔断的 UIA 提供程序所属顶层窗口；hover 移到别的窗口时解除。
    uia_blocked_hwnd: Option<HWND>,
    /// Monitor bounds in capture-relative coordinates, captured once per
    /// capture instead of enumerated every frame -- the overlay repaints at
    /// 60fps for the marching ants, and `active_monitor_rect` is called
    /// several times per frame.
    monitor_rects: Vec<Rect>,
    /// DPI / 显示器变化后的几何重同步任务是否已排队（避免观察点重复触发）。
    resync_pending: bool,
    /// 显示器观察点：随 view 一起释放时退订，避免 app 级观察点无人回收。
    display_subscription: Option<Subscription>,
    hit: SelectionHitKind,
    sel_at_press: Rect,
    start: Instant,
    visible: bool,
    mag: Option<(Vec<u8>, u32, u32, u32)>,
    /// Clamped (x, y) the last magnifier readback was taken at, so a mouse
    /// move that lands on the same source pixel skips the GPU round trip.
    mag_origin: Option<(u32, u32)>,
    editor: Session,
    text_edit: Option<TextDraft>,
    /// Last mosaic readback, keyed by the elements that produced it. The overlay
    /// repaints every frame; without this each mosaic costs a GPU readback per frame.
    /// Shared via `Rc` so a cache hit does not clone a tiles Vec that can hold
    /// thousands of entries.
    mosaic_cache: MosaicCache,
    /// Coverage masks for the annotation elements, keyed by the exact element list.
    /// The overlay repaints every frame for the marching ants; rasterizing the
    /// strokes again only happens when the annotations actually change.
    mask_cache: MaskCache,
    /// Runs of the whole-image spotlight dim layer, keyed by the marks and the
    /// selection rect so the full-canvas mask is not rebuilt every frame.
    spotlight_cache: SpotlightDimCache,
    panel: Option<Panel>,
    /// Read when the overlay is shown, not per frame: the settings window can
    /// rewrite it while this view is alive, and the bar must not change shape
    /// underneath a capture the user is already working in.
    hidden_toolbar_items: Vec<String>,
    /// 工具栏 / 面板的位置策略，同 `hidden_toolbar_items` 开场读一次，避免
    /// Settings 在截屏途中改写配置让画面中途跳位。
    toolbar_position: crate::config::ToolbarPosition,
    panel_position: crate::config::PanelPosition,
    /// 「记住位置」策略使用的已存左上角（截屏逻辑像素，未乘缩放）。
    /// 为 `None` 时布局退回「跟随选区」。
    toolbar_remember_pos: Option<(f32, f32)>,
    panel_remember_pos: Option<(f32, f32)>,
    /// Copy-only capture: mouse-up on a fresh selection confirms immediately,
    /// skipping the toolbar — "框选后直接复制".
    copy_only: bool,
    /// 截屏显示开关：打开覆盖层时从配置读一次，整场截屏内保持不变，
    /// Settings 窗口在截屏途中改写配置也不会让画面中途变形。
    show_magnifier: bool,
    show_info_bar: bool,
    show_guides: bool,
    show_shortcut_hints: bool,
    /// Screen colour picker state: the colour under the magnifier crosshair and
    /// the format it is shown in. `C` copies it, `F` cycles the format. Ported
    /// from snow-shot's `ScreenshotColorPicker` controller/window pair.
    picked_color: Option<crate::colorpicker::Color>,
    color_format: crate::colorpicker::ColorFormat,
    last_color_notice: Option<SharedString>,
    /// The colour popup is open. Session state, not settings: the picker changes
    /// the colour of the next annotation, it does not rewrite the saved default.
    color_open: bool,
    /// Active scrolling-capture session, if any.
    long: Option<LongCaptureState>,
    /// Session counter: each start bumps it, so a stale poll task wakes up,
    /// sees its generation is gone, and exits instead of appending into a
    /// new session.
    long_seq: u64,
}

/// OCR / translation output: read-only text plus copy, same as the C# panel.
struct Panel {
    title: SharedString,
    body: SharedString,
    busy: bool,
}

const OCR_TITLE: &str = "文字识别";
const TRANSLATE_TITLE: &str = "翻译";
const COPIED_TITLE: &str = "已复制";
const PANEL_W: f32 = 320.;
const PANEL_MAX_H: f32 = 260.;
/// Title row, gaps and padding; the copy button row adds `PANEL_COPY_H`.
const PANEL_CHROME: f32 = 48.;
const PANEL_COPY_H: f32 = 32.;
/// The body scrolls, so even a cramped monitor keeps this much text visible.
const PANEL_MIN_BODY_H: f32 = 48.;
const PANEL_MARGIN: f32 = 4.;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Selecting,
    Active,
    /// Scrolling capture running: the bar is swapped for the long-capture
    /// bar, the selection is frozen, and the selection area is click-through.
    LongCapturing,
}

/// One scrolling-capture session, ported from the C# LongCaptureRunner: poll
/// the region every `LONG_POLL`, SAD-align each frame against the previous
/// one, and append the rows that scrolled into view. The user scrolls by
/// hand; 完成 stitches and exports, 取消 restores the editor.
struct LongCaptureState {
    /// Absolute screen coordinates of the polled region.
    abs_x: i32,
    abs_y: i32,
    w: u32,
    h: u32,
    /// Screen-space scroll axis. Horizontal sessions rotate every frame so the
    /// SAD matcher and stitcher keep working on a vertical axis.
    axis: Axis,
    status: String,
    /// Bumped per session so a stale poll task from a cancelled session
    /// cannot append into a new one.
    gen: u64,
    /// Device generation the session's GPU resources belong to. A reset makes
    /// every one of them stale, so the poll loop stops instead of spinning on
    /// failed WGC crops.
    gpu_generation: u64,
    /// Height cap hit: polling stops, the bar stays up for 完成/取消.
    maxed: bool,
    /// Persistent WGC session for this capture's monitor, if one could be
    /// started. `None` falls back to `capture_region_bgra` per tick (a fresh
    /// WGC start every poll), same as before this session existed.
    live: Option<LongLive>,
    /// Polls served, used by the paused-page decimation.
    poll_ticks: u64,
    /// Polls that found the previous frame still in the worker: the tick that
    /// would have exceeded `LONG_POLL`, reported in the next diag line.
    late_ticks: u64,
    /// Consecutive capture failures (device reset, size change, protected
    /// content). After a few, the session reports a real error instead of
    /// polling forever with a "scroll the page" hint that can never progress.
    capture_failures: u32,
    /// Latest `StallTracker::paused` from the worker (P1-05 decimation).
    stall_paused: bool,
    /// Latest stitched counters, mirrored from the last applied result so the
    /// bar can draw without touching the worker's buffer.
    segments: usize,
    /// 沿 stitch 轴的已拼接长度（worker 从 `canvas_height()` 取得）：纵向为画布
    /// 高，横向为旋转后的画布长边 L。横向屏幕高是常量，绝不能拿它当增长值，
    /// 否则 trim / 状态栏 / 自动滚动都会停在首屏。
    total_height: usize,
    /// Latest live-preview thumbnail, rendered once when a progress frame
    /// arrives and cached here so the bar draws without touching the worker.
    /// `None` until the first frame established a canvas.
    preview: Option<(Arc<RenderImage>, u32, u32)>,
    /// Trim selection over the stitched canvas, in canvas rows: the thumbnail's
    /// two handles pick what 完成 exports. Ported from snow-shot's
    /// `ScreenshotScrollingTrimRange`; starts as the whole canvas so an
    /// untouched session exports everything.
    trim: crate::longtrim::TrimRange,
    /// Active thumbnail-handle drag, if any. Held across mouse moves so the
    /// handle keeps following the pointer even once it leaves the handle's hit
    /// radius.
    trim_drag: Option<TrimDrag>,
    /// Tight BGRA accumulator for the incremental preview bands the worker
    /// sends. Each band is blitted into this buffer (bottom for a downward
    /// scroll, top for an upward one) and `preview` is then rebuilt from it, so
    /// the UI shows the whole growing thumbnail while only the new rows ever
    /// cross the thread boundary.
    preview_buf: Vec<u8>,
    /// Width of the accumulated bands; 0 before the first band.
    preview_buf_w: u32,
    /// Height of the accumulated content; 0 before the first band.
    preview_buf_h: u32,
    /// Generation/epoch/sequence gate: the only thing that decides whether a
    /// finished job may touch this session (P1-01).
    gate: ResultGate,
    /// Owns `prev_gray` + `Stitcher`; every frame-sized pass runs there.
    worker: LongWorker,
    /// Top-level window under the selection centre: the one
    /// `focus_window_under` handed the foreground to, and the one auto-scroll
    /// posts its wheel messages to. `None` when no window could be found.
    target: Option<HWND>,
    /// Opt-in automatic scrolling (default off). Session-only: never written
    /// to `config`, so a later capture cannot start scrolling by itself.
    auto_scroll: AutoScroll,
    /// Adaptive frame-rate controller. Watches capture/stitch cost per frame
    /// and lowers the poll rate while either stage is overloaded; the poll
    /// loop asks it for the next timer interval on every tick.
    cadence: crate::capture_cadence::CaptureCadence,
    /// Hover preview over the thumbnail: while the pointer rests on it the
    /// capture pauses and that band of the canvas is rendered at full
    /// resolution, so the stitched page can be checked mid-scroll.
    hover: crate::hoverpreview::HoverPreview,
    /// The rendered hover band plus its canvas range, cached so drawing costs
    /// nothing between requests.
    hover_image: Option<(Arc<RenderImage>, u32, u32)>,
    /// Canvas rows the hover band covers, for highlighting them on the
    /// thumbnail. Kept next to the image so the two can never disagree.
    hover_rows: Option<(u32, u32)>,
}

/// The canvas rows the hover preview should render for a pointer at `y` (in
/// thumbnail logical pixels, measured from the thumbnail's top edge).
///
/// The band is one viewport tall and centred on the pointer's row, clamped to
/// the canvas -- the same shape snow-shot's thumbnail asks for, so the preview
/// shows what one more scroll step would have captured rather than a sliver.
/// `None` when the pointer is off the thumbnail or the canvas is empty.
fn hover_rows_at(y: f32, thumb_h: f32, canvas: u32, viewport_h: u32) -> Option<(u32, u32)> {
    if !(y.is_finite()) || thumb_h <= 0.0 || canvas == 0 {
        return None;
    }
    if y < 0.0 || y > thumb_h {
        return None;
    }
    let row = crate::longtrim::row_at(y, thumb_h, canvas);
    let span = viewport_h.clamp(1, canvas);
    let top = row
        .saturating_sub(span / 2)
        .min(canvas.saturating_sub(span));
    Some((top, top + span))
}

#[cfg(test)]
mod hover_rows_tests {
    use super::hover_rows_at;

    #[test]
    fn a_pointer_band_is_one_viewport_tall_and_centred() {
        // 1000 canvas rows shown in a 200 px thumbnail; a 200-row viewport.
        let (top, bottom) = hover_rows_at(100.0, 200.0, 1000, 200).unwrap();
        assert_eq!(bottom - top, 200);
        assert_eq!(top, 400, "the pointer's row (500) is the band's centre");
    }

    #[test]
    fn a_band_near_the_top_or_bottom_is_clamped_into_the_canvas() {
        let (top, _) = hover_rows_at(0.0, 200.0, 1000, 200).unwrap();
        assert_eq!(top, 0);
        let (top, bottom) = hover_rows_at(200.0, 200.0, 1000, 200).unwrap();
        assert_eq!((top, bottom), (800, 1000));
    }

    #[test]
    fn a_band_is_never_taller_than_the_canvas() {
        let (top, bottom) = hover_rows_at(10.0, 200.0, 50, 400).unwrap();
        assert_eq!((top, bottom), (0, 50));
    }

    #[test]
    fn a_pointer_outside_the_thumbnail_asks_for_nothing() {
        assert!(hover_rows_at(-1.0, 200.0, 1000, 200).is_none());
        assert!(hover_rows_at(201.0, 200.0, 1000, 200).is_none());
        assert!(hover_rows_at(f32::NAN, 200.0, 1000, 200).is_none());
        assert!(hover_rows_at(10.0, 0.0, 1000, 200).is_none());
        assert!(hover_rows_at(10.0, 200.0, 0, 200).is_none());
    }
}

fn long_status_hint(axis: Axis) -> String {
    match axis {
        Axis::Vertical => "滚动页面以继续截图".to_string(),
        Axis::Horizontal => "左右滚动页面以继续截图".to_string(),
    }
}

/// What a thumbnail drag is currently moving.
///
/// The drag is recorded when the pointer goes down on a handle or inside the
/// selected band and stays active until mouse-up, so the handle keeps following
/// the pointer after it has moved away from the handle itself.
struct TrimDrag {
    /// stitch 轴：纵向读指针的 y，横向读 x，和绘制 / 命中测试保持同一轴。
    axis: Axis,
    edge: crate::longtrim::TrimHit,
    /// Trim selection at press time. Every move recomputes from this instead of
    /// from the live range, so the pointer delta is never applied twice.
    start_range: crate::longtrim::TrimRange,
    /// Pointer position along the stitch axis in window-local logical pixels
    /// when the drag started. Moves apply `current - start_ptr`, which is all a
    /// drag needs: the handlers never learn the thumbnail's laid-out position.
    start_ptr: f32,
    /// Drawn thumbnail extent along the stitch axis in logical pixels: dh for a
    /// vertical capture, dw for a horizontal one. Captured at press time so a
    /// preview that grows mid-drag cannot rescale the pointer delta.
    thumb_extent: f32,
}

/// Wraps a tight BGRA thumbnail buffer in a GPUI `RenderImage`. The pixel
/// convention matches `pin.rs`: BGRA bytes go straight into an `RgbaImage` and
/// GPUI handles the channel order on upload. Returns `None` on a size mismatch
/// so a malformed preview can never panic the overlay.
fn render_image(pixels: &Arc<Vec<u8>>, w: u32, h: u32) -> Option<Arc<RenderImage>> {
    let rgba = image::RgbaImage::from_raw(w, h, pixels.as_ref().clone())?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}

/// Blits one incremental preview band into the session's accumulated buffer and
/// rebuilds the cached `RenderImage` from it.
///
/// The worker only renders the canvas rows that a frame actually added, so this
/// is the whole accumulation contract: a `reset` band replaces the buffer, a
/// prepend inserts rows at the top (shifting the older content down), and an
/// append writes rows at `top_offset`. Row strides are `width * 4` throughout,
/// matching tight BGRA.
fn apply_preview_band(
    buf: &mut Vec<u8>,
    buf_w: &mut u32,
    buf_h: &mut u32,
    band: &crate::longworker::LivePreview,
) -> Option<(Arc<Vec<u8>>, u32, u32)> {
    let w = band.width as usize;
    let full_h = band.full_height as usize;
    let band_h = band.height as usize;
    if w == 0 || full_h == 0 || band_h == 0 {
        return None;
    }
    if band.reset || *buf_w != band.width {
        buf.clear();
        *buf_w = band.width;
        *buf_h = 0;
    }
    let row_bytes = w * 4;
    let needed = row_bytes * full_h;
    if buf.len() < needed {
        buf.resize(needed, 0);
    }
    // A prepend lands at the top and pushes the older rows down; the band then
    // covers exactly those inserted rows, so nothing stale survives at [0, band_h).
    if band.prepend_shift > 0 {
        let shift = (band.prepend_shift as usize).min(full_h);
        let old_h = (*buf_h as usize).min(full_h.saturating_sub(shift));
        if shift > 0 && old_h > 0 {
            buf.copy_within(0..old_h * row_bytes, shift * row_bytes);
        }
    }
    let y0 = (band.top_offset as usize).min(full_h.saturating_sub(band_h));
    let src_len = band.pixels.len().min(band_h * row_bytes);
    let dst_start = y0 * row_bytes;
    buf[dst_start..dst_start + src_len].copy_from_slice(&band.pixels[..src_len]);
    *buf_h = band.full_height;
    Some((Arc::new(buf.clone()), band.width, band.full_height))
}

/// Axis every long-capture session starts on. Deliberately a constant rather
/// than `infer_axis(selection)`: horizontal capture is entered only by the
/// user clicking the bar's ↕/↔ button, so no selection shape, config value or
/// previous session can start a session sideways.
const INITIAL_LONG_AXIS: Axis = Axis::Vertical;

/// The only axis transition in a session: the bar's direction button being
/// clicked. `clicked` is that button's event; there is no other caller, which
/// is what keeps `Axis::Horizontal` user-initiated.
fn axis_after_bar_click(current: Axis, clicked: bool) -> Axis {
    if clicked {
        current.flipped()
    } else {
        current
    }
}

/// Consecutive unconfirmed notches before the page is treated as at its end:
/// 10 x 600 ms = 6 s of scrolling without a single new row.
const AUTO_SCROLL_MAX_IDLE_POSTS: u32 = 10;

const AUTO_SCROLL_POST_FAILED: &str = "无法向目标窗口发送滚动，请手动滚动";
const AUTO_SCROLL_NO_PROGRESS: &str = "自动滚动已停止：未检测到更多内容";

/// Smallest sub-element worth snapping to. Hairline borders and separators make
/// poor crop edges, so they are dropped before the hover walk.
const MIN_CANDIDATE_EXTENT: u32 = 6;

/// Pacing and give-up accounting for opt-in auto-scroll. Pure bookkeeping --
/// no Win32 and no timer: the host asks [`AutoScroll::plan`] on each poll,
/// posts the wheel itself, reports the result with
/// [`AutoScroll::observe_post`], and feeds the stitched extent（沿 stitch 轴的
/// 增长长度，横向为长边宽度）to [`AutoScroll::observe_height`]. Everything
/// about when to post and when to give up is therefore testable without a
/// window.
#[derive(Debug, Default)]
struct AutoScroll {
    enabled: bool,
    last_post: Option<Instant>,
    /// Notches posted since the stitched image last grew.
    idle_posts: u32,
    /// 上一帧的 stitch 轴长度（横向为长边宽 L），用来判断是否真的在增长。
    last_extent: Option<usize>,
    /// Beat interval and step magnitude. One source of truth shared with the
    /// manual single step, so the two can never drift apart.
    step: crate::scroll_step::ScrollStep,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AutoScrollStep {
    /// Post one notch now.
    Post,
    /// Nothing to post: off, a frame is in flight, or the beat has not elapsed.
    Wait,
    /// Too many notches produced no new rows: auto-scroll turned itself off.
    StopNoProgress,
}

impl AutoScroll {
    /// Session starts from the persisted cadence. The config struct clamps on
    /// the way in, so a hand-edited file can never outrun the stitcher.
    fn with_step(step: crate::scroll_step::ScrollStep) -> Self {
        Self {
            enabled: false,
            last_post: None,
            idle_posts: 0,
            last_extent: None,
            step,
        }
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turns the switch on or off, restarting the beat either way. Never
    /// touches the axis, the stitcher or the buffer: turning it off restores
    /// the exact manual behaviour.
    fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        self.last_post = None;
        self.idle_posts = 0;
        self.last_extent = None;
    }

    /// Folds the session's stitched extent in after each applied frame: a
    /// longer image means content arrived, so the give-up counter restarts.
    /// 横向必须传增长后的长边 L，而不是常量屏幕高，否则 10 次空转后会误判
    /// "未检测到滚动"并自行关闭。
    fn observe_height(&mut self, extent: usize) {
        if self.last_extent.is_some_and(|h| extent > h) {
            self.idle_posts = 0;
        }
        self.last_extent = Some(extent);
    }

    /// Folds one post's result in. Returns whether auto-scroll stays on: a
    /// refused message disables it at once, because a switch that is on while
    /// nothing scrolls is the failure this feature must never have.
    fn observe_post(&mut self, posted: bool) -> bool {
        if !posted {
            self.enabled = false;
        }
        self.enabled
    }

    /// Decides this tick. `in_flight` is `true` while the worker still owns
    /// the previous frame, in which case no notch may be posted -- it would
    /// scroll the page past the frame being stitched.
    fn plan(&mut self, now: Instant, in_flight: bool) -> AutoScrollStep {
        if !self.enabled {
            return AutoScrollStep::Wait;
        }
        if in_flight {
            return AutoScrollStep::Wait;
        }
        let due = self
            .last_post
            .is_none_or(|t| now.saturating_duration_since(t) >= self.step.interval);
        if !due {
            return AutoScrollStep::Wait;
        }
        if self.idle_posts >= AUTO_SCROLL_MAX_IDLE_POSTS {
            self.enabled = false;
            return AutoScrollStep::StopNoProgress;
        }
        self.last_post = Some(now);
        self.idle_posts = self.idle_posts.saturating_add(1);
        AutoScrollStep::Post
    }

    /// The configured step magnitude, in native notches.
    fn notch_multiplier(&self) -> i32 {
        self.step.notch_multiplier()
    }

    /// A manual step restarts the beat, so the automatic post cannot fire
    /// immediately on top of it: two steps within one interval would move the
    /// page twice under the same in-flight frame and could outrun the
    /// estimator's motion window.
    fn observe_manual_step(&mut self, now: Instant) {
        self.last_post = Some(now);
    }
}

/// Message and signed delta of one step that continues the capture direction.
/// Win32 signs: `WM_MOUSEWHEEL` is positive when the wheel turns forward (away
/// from the user), which scrolls most apps further down the page;
/// `WM_MOUSEHWHEEL` is positive to the right. Both are the direction the
/// stitched image grows, so both take a positive delta. `multiplier` widens one
/// step to several native notches; it is never negative, so a step can only
/// ever move the page the way the capture is growing.
fn wheel_notch(axis: Axis, multiplier: i32) -> (u32, i32) {
    let delta = (WHEEL_DELTA as i32).saturating_mul(multiplier.max(1));
    match axis {
        Axis::Vertical => (WM_MOUSEWHEEL, delta),
        Axis::Horizontal => (WM_MOUSEHWHEEL, delta),
    }
}

/// `MAKELPARAM(x, y)` in screen coordinates: the wheel messages go to a
/// focused window while the cursor sits elsewhere, so the target needs the
/// selection centre, not the pointer position.
fn wheel_lparam(x: i32, y: i32) -> isize {
    (((y as u16 as u32) << 16) | (x as u16 as u32)) as i32 as isize
}

/// Posts one scroll step (possibly several notches) to `hwnd` at screen point
/// `center`.
///
/// `false` when the window is gone or refused the message; the caller then
/// turns auto-scroll off with a visible reason instead of keeping a switch
/// that cannot do anything.
fn post_wheel(hwnd: HWND, axis: Axis, center: POINT, multiplier: i32) -> bool {
    if hwnd.0.is_null() || unsafe { !IsWindow(Some(hwnd)).as_bool() } {
        return false;
    }
    let (msg, delta) = wheel_notch(axis, multiplier);
    // Low word of wParam is the key state (none), high word the signed delta.
    let wparam = ((delta as u32) << 16) as usize;
    let lparam = LPARAM(wheel_lparam(center.x, center.y));
    unsafe { PostMessageW(Some(hwnd), msg, WPARAM(wparam), lparam).is_ok() }
}

/// Whether the long-capture poll loop must keep running.
///
/// A stall, a pause, a static edge or a fast scroll is a *state* the bar
/// reflects; none of them appears here, so no status can end the loop. Only a
/// superseded session (`generation_matches == false`), a hard stop (`maxed`:
/// height cap, image budget, GPU reset, worker gone) or leaving the capture
/// phase (完成/取消) stops it. A later stitched tick brings the session back
/// to full speed, and the decimation keeps sampling meanwhile.
fn should_keep_polling(generation_matches: bool, maxed: bool, capturing: bool) -> bool {
    generation_matches && !maxed && capturing
}

/// A running WGC session plus what `capture_region_live` needs to crop it:
/// the GPU handle it was started on and the monitor's origin, so an absolute
/// screen rect can be converted to monitor-local coordinates.
struct LongLive {
    session: WgcLive,
    gpu: Gpu,
    /// Device generation the WGC session and `gpu` handle were created on.
    generation: u64,
    sdr_white: f32,
    mon_x: i32,
    mon_y: i32,
}

/// How often a frame is sampled. Was 450 ms (the C# cadence) with a full SAD
/// sweep per direction on the UI thread; ORB feature matching now runs on the
/// worker and recovers offsets up to 0.6 x the viewport between samples, so a
/// 150 ms cadence keeps plenty of overlap per small-step scroll without
/// queueing frames the worker cannot align.
const LONG_POLL: Duration = Duration::from_millis(150);
/// D3D texture height limit, same cap as the C# LongImageBuffer. Derived from
/// the shared image budget so the stitcher and the export path cannot drift.
const LONG_MAX_H: usize = crate::image_budget::MAX_SIDE as usize;
/// While the page has been still for `STALL_TICKS`, only every Nth poll
/// captures a frame (P1-05). A reader who paused should not cost a WGC crop,
/// a rotation and a full-frame grayscale pass every 150 ms; a 4x slower poll
/// notices the resumed scroll within 600 ms.
const SLOW_POLL_EVERY: u64 = 4;
/// Cross-axis cap for the hover preview raster. The band is shown at up to
/// 240 px wide on the bar, so rendering wider than this only costs memory; a
/// region read at this width still has 4x the detail of the thumbnail.
const HOVER_PREVIEW_WIDTH: u32 = 640;
/// How long the tick waits on a hover render. It runs on the worker thread, so
/// this is a stall bound rather than an expected duration; a timeout simply
/// drops the preview and the capture resumes.
const HOVER_PREVIEW_WAIT: Duration = Duration::from_millis(400);

/// 延迟清 passthrough 洞的守卫。设备复位后清洞被推迟到下一个任务，期间用户
/// 可能已经开启新的长截图会话并设置了新的洞；只有会话序号没前进、且当前没有
/// 新会话占用洞时才允许清，否则会把新会话刚挖的洞一起抹掉。
fn should_clear_long_passthrough(captured_seq: u64, current_seq: u64, new_session: bool) -> bool {
    captured_seq == current_seq && !new_session
}

/// Click-through for the selection while a long capture runs: the wheel must
/// reach the app underneath, not the overlay. HTTRANSPARENT from a WM_NCHITTEST
/// subclass does not do this -- it only walks windows of the same thread -- and
/// WS_EX_TRANSPARENT needs WS_EX_LAYERED, which gpui's DirectComposition window
/// (WS_EX_NOREDIRECTIONBITMAP) cannot take. Cutting the selection out of the
/// window region does work across processes, keeps the backdrop and the
/// long-capture bar clickable, and stops painting the stale freeze over the
/// page the user is scrolling.
fn set_long_passthrough(hwnd: HWND, rect: Option<RECT>) {
    unsafe {
        let Some(r) = rect else {
            let _ = SetWindowRgn(hwnd, None, true);
            return;
        };
        // SetWindowRgn takes window coordinates, and gpui's popup insets the
        // client area, so the screen rect is offset by the window origin.
        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &mut wr).is_err() {
            return;
        }
        let full = CreateRectRgn(0, 0, wr.right - wr.left, wr.bottom - wr.top);
        let hole = CreateRectRgn(
            r.left - wr.left,
            r.top - wr.top,
            r.right - wr.left,
            r.bottom - wr.top,
        );
        CombineRgn(Some(full), Some(full), Some(hole), RGN_DIFF);
        let _ = DeleteObject(hole.into());
        // On success the region belongs to the system; only a failed call
        // leaves it ours to free.
        if SetWindowRgn(hwnd, Some(full), true) == 0 {
            let _ = DeleteObject(full.into());
        }
    }
}

/// Pulls the hole back so the long-capture bar is not inside it. `place_toolbar`
/// clamps the bar into the monitor, so a selection that fills the screen leaves
/// it sitting over the selection -- and nothing inside the hole is drawn or
/// clickable, which would strip the 完成 button. The band it gives up only
/// stops being click-through; the capture reads the screen, not the overlay.
fn hole_clear_of_bar(mut hole: RECT, bar_top: i32, bar_bottom: i32) -> RECT {
    if bar_bottom <= hole.top || bar_top >= hole.bottom {
        return hole;
    }
    if bar_top < (hole.top + hole.bottom) / 2 {
        hole.top = bar_bottom.min(hole.bottom);
    } else {
        hole.bottom = bar_top.max(hole.top);
    }
    hole
}

/// WM_MOUSEWHEEL goes to the window with keyboard focus, not the one under
/// the cursor, so the region hole alone cannot make the page scroll: the
/// focused overlay would swallow every wheel tick the capture is waiting for. Handing the foreground to the topmost window under the
/// selection center is what actually lets the user scroll. Returns that window
/// so auto-scroll can post to the same target.
fn focus_window_under(overlay: HWND, pt: POINT) -> Option<HWND> {
    struct Ctx {
        exclude: HWND,
        pt: POINT,
        found: HWND,
    }
    unsafe extern "system" fn walk(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        unsafe {
            let ctx = &mut *(lparam.0 as *mut Ctx);
            if hwnd == ctx.exclude {
                return windows::core::BOOL(1);
            }
            // Without the cloak filter the foreground goes to a suspended UWP
            // frame that still reports a full-screen rect, and the wheel then
            // reaches nobody.
            let Some(wr) = real_window_rect(hwnd) else {
                return windows::core::BOOL(1);
            };
            if ctx.pt.x >= wr.left
                && ctx.pt.x < wr.right
                && ctx.pt.y >= wr.top
                && ctx.pt.y < wr.bottom
            {
                ctx.found = hwnd;
                return windows::core::BOOL(0);
            }
            windows::core::BOOL(1)
        }
    }
    let mut ctx = Ctx {
        exclude: overlay,
        pt,
        found: HWND::default(),
    };
    unsafe {
        // EnumWindows walks top-level windows in z-order: the overlay is
        // topmost and skipped, so the first hit is the scrolled app.
        let _ = EnumWindows(Some(walk), LPARAM(&mut ctx as *mut _ as isize));
        if ctx.found.0.is_null() {
            None
        } else {
            let _ = SetForegroundWindow(ctx.found);
            Some(ctx.found)
        }
    }
}

pub fn show_overlay(
    cx: &mut App,
    cap: CapturedDesktop,
    perf: &mut PerfSession,
    copy_only: bool,
) -> Entity<OverlayView> {
    bind_overlay_keys(cx);
    let session = std::mem::replace(perf, PerfSession::start());
    let smart_selection = crate::config::get().screenshot.smart_selection;
    match show_overlay_inner(cx, cap, session, copy_only, smart_selection) {
        Ok(entity) => entity,
        Err(e) => {
            eprintln!("overlay: {e}");
            crate::app::complete_region_shot(
                empty_result(),
                None,
                None,
                if copy_only { "copyOnly" } else { "region" },
                cx,
            );
            cx.new(OverlayView::placeholder)
        }
    }
}

fn bind_overlay_keys(cx: &mut App) {
    if KEYS_BOUND.get() {
        return;
    }
    cx.bind_keys(vec![
        KeyBinding::new("enter", ConfirmSelection, Some("Overlay")),
        KeyBinding::new("escape", CancelOverlay, Some("Overlay")),
        KeyBinding::new("ctrl-z", UndoEdit, Some("Overlay")),
        KeyBinding::new("ctrl-y", RedoEdit, Some("Overlay")),
        // `c` / `f` are handled in `on_key_down` rather than bound here: a
        // binding is dispatched before character input, so it would swallow
        // the letters while the text tool is active.
    ]);
    KEYS_BOUND.set(true);
}

fn show_overlay_inner(
    cx: &mut App,
    cap: CapturedDesktop,
    perf: PerfSession,
    copy_only: bool,
    smart_selection: crate::config::SmartSelectionMode,
) -> Result<Entity<OverlayView>> {
    // 无论复用旧窗口还是新建窗口，这都是一张全新的截图；上一张 finish 时
    // 挂起的延迟隐藏必须在回调里发现自己已经过期。
    OVERLAY_GENERATION.with(|g| g.set(g.get().wrapping_add(1)));
    let swap = match cx.with_gpu_context(|device, context| {
        present_freeze_from_texture(
            device,
            context,
            &cap.sdr_display,
            cap.width,
            cap.height,
            cap.generation,
        )
    }) {
        Some(Ok(s)) => s,
        Some(Err(e)) => return Err(e),
        None => return Err(crate::error::Error::new("GPU context unavailable")),
    };

    if let Some(handle) = OVERLAY_WINDOW.with(|slot| *slot.borrow()) {
        if let Ok(entity) = handle.entity(cx) {
            let _ = handle.update(cx, |view, window, cx| {
                view.reset_from_capture(cap, perf, swap, copy_only, smart_selection, window);
                view.attach_and_show(window, cx);
            });
            return Ok(entity);
        }
    }

    let bounds = Bounds {
        origin: point(px(cap.origin_x as f32), px(cap.origin_y as f32)),
        size: size(px(cap.width as f32), px(cap.height as f32)),
    };
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                focus: true,
                show: false,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    OverlayView::from_capture(
                        cx,
                        cap,
                        perf,
                        swap,
                        copy_only,
                        smart_selection,
                        window,
                    )
                })
            },
        )
        .map_err(|e| crate::error::Error::new(e.to_string()))?;

    OVERLAY_WINDOW.with(|slot| {
        *slot.borrow_mut() = Some(handle);
    });

    let entity = handle
        .entity(cx)
        .map_err(|e| crate::error::Error::new(e.to_string()))?;
    let _ = handle.update(cx, |view, window, cx| {
        view.attach_and_show(window, cx);
    });
    Ok(entity)
}

/// The single, shared toolbar item list used for sizing, painting and hit
/// testing. Free function (not a method) so unit tests can drive the exact
/// production hit-test without constructing a window-bound `OverlayView`: a
/// real click coordinate -> `toolbar_item_at` -> the id the button dispatched.
/// Line style (solid / dashed / cloud) is a property of closed shapes only, so
/// those buttons live on the bar only while a shape tool is active.
pub(crate) fn toolbar_items_for(_tool: Tool, hidden: &[String]) -> Vec<&'static str> {
    PHASE1_ITEMS
        .iter()
        .copied()
        // Line style is a property, not a tool: it lives on the second row now,
        // for every tool that draws an outline.
        .filter(|id| !toolbar::is_line_style(id))
        .filter(|id| toolbar::is_visible(id, hidden))
        .collect()
}

impl OverlayView {
    fn placeholder(cx: &mut Context<Self>) -> Self {
        let screenshot = crate::config::get().screenshot;
        Self {
            selection: Rect::new(0.0, 0.0, 0.0, 0.0),
            hover: None,
            mouse: Point { x: 0.0, y: 0.0 },
            freeze_size: (1, 1),
            focus_handle: cx.focus_handle(),
            cap: None,
            perf: Rc::new(RefCell::new(None)),
            freeze_swap: None,
            freeze_generation: None,
            recovering_gpu: false,
            origin_x: 0,
            origin_y: 0,
            phase: Phase::Selecting,
            down: false,
            down_pt: Point { x: 0.0, y: 0.0 },
            from_drag: false,
            has_sel: false,
            windows: Vec::new(),
            candidates: Vec::new(),
            smart_selection: crate::config::SmartSelectionMode::default(),
            candidates_at: None,
            uia_pending: None,
            uia_last: Vec::new(),
            uia_last_target: None,
            uia_last_point: POINT::default(),
            uia_epoch: 0,
            uia_timeouts: 0,
            uia_blocked_hwnd: None,
            monitor_rects: Vec::new(),
            resync_pending: false,
            display_subscription: None,
            hit: SelectionHitKind::None,
            sel_at_press: Rect::new(0.0, 0.0, 0.0, 0.0),
            start: Instant::now(),
            visible: false,
            mag: None,
            mag_origin: None,
            editor: Session::new(),
            text_edit: None,
            mosaic_cache: RefCell::new(None),
            mask_cache: RefCell::new(None),
            spotlight_cache: RefCell::new(None),
            panel: None,
            hidden_toolbar_items: crate::config::get().screenshot.hidden_toolbar_items,
            toolbar_position: screenshot.toolbar_position,
            panel_position: screenshot.panel_position,
            toolbar_remember_pos: screenshot.toolbar_remember_pos,
            panel_remember_pos: screenshot.panel_remember_pos,
            copy_only: false,
            show_magnifier: screenshot.show_magnifier,
            show_info_bar: screenshot.show_info_bar,
            show_guides: screenshot.show_guides,
            show_shortcut_hints: screenshot.show_shortcut_hints,
            picked_color: None,
            color_format: crate::colorpicker::ColorFormat::Hex,
            last_color_notice: None,
            color_open: false,
            long: None,
            long_seq: 0,
        }
    }

    fn from_capture(
        cx: &mut Context<Self>,
        cap: CapturedDesktop,
        perf: PerfSession,
        swap: IDXGISwapChain1,
        copy_only: bool,
        smart_selection: crate::config::SmartSelectionMode,
        window: &mut Window,
    ) -> Self {
        let freeze_generation = Some(cap.generation);
        let screenshot = crate::config::get().screenshot;
        let mut view = Self {
            selection: Rect::new(0.0, 0.0, 0.0, 0.0),
            hover: None,
            mouse: Point { x: 0.0, y: 0.0 },
            freeze_size: (cap.width, cap.height),
            focus_handle: cx.focus_handle(),
            cap: Some(cap),
            perf: Rc::new(RefCell::new(Some(perf))),
            freeze_swap: Some(swap),
            freeze_generation,
            recovering_gpu: false,
            origin_x: 0,
            origin_y: 0,
            phase: Phase::Selecting,
            down: false,
            down_pt: Point { x: 0.0, y: 0.0 },
            from_drag: false,
            has_sel: false,
            windows: Vec::new(),
            candidates: Vec::new(),
            smart_selection,
            candidates_at: None,
            uia_pending: None,
            uia_last: Vec::new(),
            uia_last_target: None,
            uia_last_point: POINT::default(),
            uia_epoch: 0,
            uia_timeouts: 0,
            uia_blocked_hwnd: None,
            monitor_rects: Vec::new(),
            resync_pending: false,
            display_subscription: None,
            hit: SelectionHitKind::None,
            sel_at_press: Rect::new(0.0, 0.0, 0.0, 0.0),
            start: Instant::now(),
            visible: true,
            mag: None,
            mag_origin: None,
            editor: Session::new(),
            text_edit: None,
            mosaic_cache: RefCell::new(None),
            mask_cache: RefCell::new(None),
            spotlight_cache: RefCell::new(None),
            panel: None,
            hidden_toolbar_items: crate::config::get().screenshot.hidden_toolbar_items,
            toolbar_position: screenshot.toolbar_position,
            panel_position: screenshot.panel_position,
            toolbar_remember_pos: screenshot.toolbar_remember_pos,
            panel_remember_pos: screenshot.panel_remember_pos,
            copy_only,
            show_magnifier: screenshot.show_magnifier,
            show_info_bar: screenshot.show_info_bar,
            show_guides: screenshot.show_guides,
            show_shortcut_hints: screenshot.show_shortcut_hints,
            picked_color: None,
            color_format: crate::colorpicker::ColorFormat::Hex,
            last_color_notice: None,
            color_open: false,
            long: None,
            long_seq: 0,
        };
        view.sync_from_cap(window);
        view.register_display_resync(cx, window);
        view
    }

    fn reset_from_capture(
        &mut self,
        cap: CapturedDesktop,
        perf: PerfSession,
        swap: IDXGISwapChain1,
        copy_only: bool,
        smart_selection: crate::config::SmartSelectionMode,
        window: &mut Window,
    ) {
        let screenshot = crate::config::get().screenshot;
        self.selection = Rect::new(0.0, 0.0, 0.0, 0.0);
        self.hover = None;
        self.mouse = Point { x: 0.0, y: 0.0 };
        self.freeze_size = (cap.width, cap.height);
        self.cap = Some(cap);
        self.perf = Rc::new(RefCell::new(Some(perf)));
        self.freeze_swap = Some(swap);
        self.freeze_generation = self.cap.as_ref().map(|c| c.generation);
        self.recovering_gpu = false;
        self.phase = Phase::Selecting;
        self.down = false;
        self.from_drag = false;
        self.has_sel = false;
        self.hit = SelectionHitKind::None;
        self.start = Instant::now();
        self.visible = true;
        self.mag = None;
        self.mag_origin = None;
        self.editor = Session::new();
        self.text_edit = None;
        self.mask_cache = RefCell::new(None);
        self.panel = None;
        self.hidden_toolbar_items = crate::config::get().screenshot.hidden_toolbar_items;
        self.toolbar_position = screenshot.toolbar_position;
        self.panel_position = screenshot.panel_position;
        self.toolbar_remember_pos = screenshot.toolbar_remember_pos;
        self.panel_remember_pos = screenshot.panel_remember_pos;
        self.copy_only = copy_only;
        self.show_magnifier = screenshot.show_magnifier;
        self.show_info_bar = screenshot.show_info_bar;
        self.show_guides = screenshot.show_guides;
        self.show_shortcut_hints = screenshot.show_shortcut_hints;
        self.picked_color = None;
        self.last_color_notice = None;
        self.smart_selection = smart_selection;
        self.candidates.clear();
        self.candidates_at = None;
        // 上一张图的 UIA 回执和缓存都不能落到这一张图上。
        self.uia_pending = None;
        self.uia_last.clear();
        self.uia_last_target = None;
        self.uia_timeouts = 0;
        self.uia_blocked_hwnd = None;
        self.color_open = false;
        self.long = None;
        self.long_seq = self.long_seq.wrapping_add(1);
        set_long_passthrough(crate::platform::hwnd_of(window), None);
        self.sync_from_cap(window);
    }

    /// 记一次 UIA 失败；连续 UIA_TIMEOUT_TRIP 次后熔断该窗口，之后不再投递。
    fn note_uia_timeout(&mut self, target: HWND) {
        self.uia_timeouts = self.uia_timeouts.saturating_add(1);
        if self.uia_timeouts >= UIA_TIMEOUT_TRIP {
            self.uia_blocked_hwnd = Some(target);
            self.uia_timeouts = 0;
        }
    }

    /// 发起一次异步 UIA 查询。只有目标窗口与上次熔断的窗口不同时才重新启用。
    fn request_uia(&mut self, self_hwnd: HWND, pt: POINT) {
        if !self.smart_selection.wants_sub_elements() {
            self.uia_pending = None;
            return;
        }
        let Some(target) = window_under_point(self_hwnd, pt) else {
            self.uia_pending = None;
            return;
        };
        if self.uia_blocked_hwnd == Some(target) {
            self.uia_pending = None;
            return;
        }
        self.uia_epoch = self.uia_epoch.wrapping_add(1);
        let query = crate::uia::elements_at_point_async(target, pt, self.uia_epoch);
        if query.failed_fast {
            // 队列满 / worker 已退出：等价于一次超时。必须在这里就计数——
            // 鼠标快速移动时 pending 可能下一帧就被清掉，poll 根本看不到它。
            self.uia_pending = None;
            self.note_uia_timeout(target);
            return;
        }
        self.uia_pending = Some(UiaPending {
            rx: query.rx,
            epoch: self.uia_epoch,
            target,
            point: pt,
            started: Instant::now(),
        });
    }

    /// 非阻塞轮询 UIA 回执。只接受当前 epoch、当前 hover 点、当前目标窗口的
    /// 结果；超时、断开或过期的回执一律丢弃。连续失败达到阈值后熔断该窗口。
    fn poll_uia_result(&mut self, window: &Window) {
        // 把借用收进局部作用域，回调里可以自由改写 self 的字段。
        let received = {
            let Some(pending) = self.uia_pending.as_ref() else {
                return;
            };
            let info = (
                pending.epoch,
                pending.point,
                pending.started,
                pending.target,
            );
            (pending.rx.try_recv(), info)
        };
        let (recv, (epoch, point, started, target)) = received;
        match recv {
            Ok(reply) => {
                self.uia_pending = None;
                if reply.epoch != epoch {
                    // 鼠标已经移动，旧结果直接丢弃。
                    return;
                }
                // 查询时的屏幕点与当前 hover 点允许小幅偏差（每轴 <=24px），
                // 否则鼠标稍动一两个像素就会把来之不易的结果整个丢弃。
                let now = POINT {
                    x: (self.mouse.x + self.origin_x as f64) as i32,
                    y: (self.mouse.y + self.origin_y as f64) as i32,
                };
                if (now.x - point.x).abs() > UIA_POINT_TOLERANCE
                    || (now.y - point.y).abs() > UIA_POINT_TOLERANCE
                {
                    return;
                }
                let hwnd = crate::platform::hwnd_of(window);
                // HWND 可能在 300ms 内关闭并被新窗口复用：用当前点重新确认
                // hover 窗口仍是查询目标，否则这批复选框属于别的窗口，必须丢弃。
                // window_under_point 只走 Win32 枚举，不会触发同步 COM。
                if window_under_point(hwnd, now) != Some(target) {
                    return;
                }
                self.uia_timeouts = 0;
                self.uia_blocked_hwnd = None;
                let uia_rects: Vec<RECT> = reply.rects.into_iter().map(|el| el.rect).collect();
                self.candidates = detect_candidates(
                    self.smart_selection,
                    hwnd,
                    point,
                    self.origin_x,
                    self.origin_y,
                    self.freeze_size.0,
                    self.freeze_size.1,
                    &uia_rects,
                );
                // 缓存成功应用的矩形：后续鼠标小幅移动时继续复用，直到窗口
                // 切换或新一轮截图让它失效。request_uia 不碰这份缓存。
                self.uia_last = uia_rects;
                self.uia_last_target = Some(target);
                self.uia_last_point = point;
                self.hover = crate::smart_selection::preview_rect(
                    self.smart_selection,
                    &self.candidates,
                    self.mouse,
                );
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if started.elapsed() > crate::uia::UIA_QUERY_TIMEOUT {
                    self.uia_pending = None;
                    self.note_uia_timeout(target);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // 回执通道断开（worker 退出 / 请求被丢弃）也算一次失败，
                // 过去这里只清 pending，导致熔断计数永远到不了阈值。
                self.uia_pending = None;
                self.note_uia_timeout(target);
            }
        }
    }

    fn sync_from_cap(&mut self, window: &Window) {
        let (ox, oy, w, h) = match &self.cap {
            Some(c) => (c.origin_x, c.origin_y, c.width, c.height),
            None => return,
        };
        self.origin_x = ox;
        self.origin_y = oy;
        self.freeze_size = (w, h);
        self.monitor_rects = crate::capture::enumerate_monitors()
            .map(|mons| {
                mons.iter()
                    .map(|m| {
                        Rect::new((m.x - ox) as f64, (m.y - oy) as f64, m.w as f64, m.h as f64)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let hwnd = crate::platform::hwnd_of(window);
        self.windows = detect_windows(hwnd, ox, oy);
        // Read the cursor before probing candidates: the first frame used to
        // snap to the previous mouse position because the probe ran before
        // `GetCursorPos`.
        let mut mouse = Point { x: 0.0, y: 0.0 };
        let mut pt = POINT::default();
        let got_cursor = unsafe { GetCursorPos(&mut pt).is_ok() };
        if got_cursor {
            mouse.x = (pt.x - ox) as f64;
            mouse.y = (pt.y - oy) as f64;
        } else {
            pt = POINT {
                x: (self.mouse.x + ox as f64) as i32,
                y: (self.mouse.y + oy as f64) as i32,
            };
            mouse = Point {
                x: self.mouse.x,
                y: self.mouse.y,
            };
        }
        self.mouse = mouse;
        self.down_pt = mouse;
        self.candidates = detect_candidates(
            self.smart_selection,
            hwnd,
            pt,
            ox,
            oy,
            self.freeze_size.0,
            self.freeze_size.1,
            &[],
        );
        // UIA 在后台线程查询；结果由 render 里的 poll_uia_result 应用。
        self.request_uia(hwnd, pt);
        self.hover = hit_window(&self.windows, mouse);
    }

    /// DPI 变化 / 显示器热插拔时 GPUI 只按逻辑像素重排窗口，而冻结图与
    /// `monitor_rects` 都是物理像素。挂两个观察点：窗口 bounds 变化（含
    /// WM_DPICHANGED 触发的 WM_SIZE/WM_MOVE）和显示器配置变化，统一走延迟
    /// 重同步，避免在处理 WM_SIZE 期间再次 SetWindowPos 造成重入。
    fn register_display_resync(&mut self, cx: &mut Context<Self>, window: &mut Window) {
        cx.observe_window_bounds(window, |view, _window, cx| {
            view.schedule_display_resync(cx);
        })
        .detach();
        // 显示器观察点注册在 app 级，必须持有 Subscription 随 view 一起退订；
        // 窗口 bounds 观察点则随窗口销毁自动失效，仍可 detach。
        self.display_subscription = Some(cx.observe_displays(|_, cx| {
            // 全局句柄可能已指向新一轮截图；update 时自然拿到当前 view。
            if let Some(handle) = OVERLAY_WINDOW.with(|s| *s.borrow()) {
                let _ = handle.update(cx, |view, _window, cx| {
                    view.schedule_display_resync(cx);
                });
            }
        }));
    }

    fn schedule_display_resync(&mut self, cx: &mut Context<Self>) {
        if self.resync_pending || !self.visible {
            return;
        }
        self.resync_pending = true;
        cx.spawn(async move |_this, cx| {
            // 先让 GPUI 处理完本次 resize/move，再按物理坐标写回窗口位置。
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            cx.update(|cx| {
                if let Some(handle) = OVERLAY_WINDOW.with(|s| *s.borrow()) {
                    let _ = handle.update(cx, |view, window, cx| {
                        view.apply_display_resync(window, cx);
                    });
                }
            });
        })
        .detach();
    }

    fn apply_display_resync(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.resync_pending = false;
        let (ox, oy, w, h) = match &self.cap {
            Some(c) => (c.origin_x, c.origin_y, c.width, c.height),
            None => return,
        };
        // 只刷新与物理像素绑定的几何缓存；选区 / 拖动 / UIA 状态不动，避免
        // 用户正在框选时被重置。
        self.monitor_rects = crate::capture::enumerate_monitors()
            .map(|mons| {
                mons.iter()
                    .map(|m| {
                        Rect::new((m.x - ox) as f64, (m.y - oy) as f64, m.w as f64, m.h as f64)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let hwnd = crate::platform::hwnd_of(window);
        let desired = RECT {
            left: ox,
            top: oy,
            right: ox + w as i32,
            bottom: oy + h as i32,
        };
        let outer = window_rect_for_client_area(hwnd, desired);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                outer.left,
                outer.top,
                outer.right - outer.left,
                outer.bottom - outer.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        cx.notify();
    }

    fn attach_and_show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(swap) = &self.freeze_swap {
            if let Err(e) = window.set_background_swap_chain(Some(swap)) {
                eprintln!("set_background_swap_chain: {e}");
            }
        }
        let hwnd = crate::platform::hwnd_of(window);
        let desired = RECT {
            left: self.origin_x,
            top: self.origin_y,
            right: self.origin_x + self.freeze_size.0 as i32,
            bottom: self.origin_y + self.freeze_size.1 as i32,
        };
        let outer = window_rect_for_client_area(hwnd, desired);
        unsafe {
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                outer.left,
                outer.top,
                outer.right - outer.left,
                outer.bottom - outer.top,
                SWP_SHOWWINDOW,
            );
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        window.focus(&self.focus_handle, cx);
        window.on_window_should_close(cx, |_window, cx| {
            if let Some(handle) = OVERLAY_WINDOW.with(|s| *s.borrow()) {
                let _ = handle.update(cx, |view, window, cx| {
                    view.cancel(window, cx);
                });
            }
            false
        });
        let perf = self.perf.clone();
        window.on_app_ready_sentinel({
            let perf = self.perf.clone();
            move || {
                if let Some(p) = perf.borrow_mut().as_mut() {
                    p.mark("T7");
                }
            }
        });
        window.on_next_frame(move |window, _cx| {
            let _ = window.wait_for_commit_completion();
            if let Some(p) = perf.borrow_mut().as_mut() {
                p.mark("T6");
            }
            let hwnd = crate::platform::hwnd_of(window);
            unsafe {
                let _ = SetForegroundWindow(hwnd);
                let _ = PostMessageW(Some(hwnd), WM_APP_READY_SENTINEL, WPARAM(0), LPARAM(0));
            }
        });
        // Do not call request_animation_frame here — it requires a paint-time
        // current_view. focus() already refresh()ed; notify + vsync will draw.
        if bench_auto_hide_enabled() {
            self.schedule_bench_auto_hide(cx);
        }
        self.refresh_magnifier_gpu(cx);
        cx.notify();
    }

    /// Painted-frame hook: after a device reset the DirectComposition tree is
    /// recreated and the freeze visual is empty. Schedule exactly one rebuild
    /// per generation (the state machine also refuses while resetting).
    fn maybe_recover_gpu(&mut self, cx: &mut Context<Self>) {
        if !self.visible || self.recovering_gpu {
            return;
        }
        let state = crate::gpu_lifecycle::snapshot();
        if !crate::gpu_lifecycle::should_rebuild_freeze(state, self.freeze_generation) {
            return;
        }
        self.recovering_gpu = true;
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1))
                .await;
            cx.update(|cx| {
                if let Some(handle) = OVERLAY_WINDOW.with(|s| *s.borrow()) {
                    let _ = handle.update(cx, |view, window, cx| {
                        view.recover_gpu_after_reset(window, cx);
                    });
                }
            });
        })
        .detach();
    }

    /// Rebuild the freeze frame on the new device and re-attach it to the
    /// recreated DirectComposition tree. A failed capture/present closes the
    /// overlay cleanly instead of leaving it on an empty visual.
    fn recover_gpu_after_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recovering_gpu = false;
        if !self.visible {
            return;
        }
        let generation = crate::gpu_lifecycle::current_generation();
        if !crate::gpu_lifecycle::is_usable(generation) {
            // Still resetting: the next painted frame retries.
            return;
        }
        // Every live WGC session belongs to the old device generation.
        if self.long.take().is_some() {
            set_long_passthrough(crate::platform::hwnd_of(window), None);
            self.phase = Phase::Active;
            self.show_panel(
                "显示设备已重置",
                "长截图已停止，请重新开始".to_string(),
                false,
                cx,
            );
        }
        self.cap = None;
        self.freeze_swap = None;
        self.freeze_generation = None;

        let rebuilt = (|| -> crate::error::Result<(CapturedDesktop, IDXGISwapChain1)> {
            let mut perf = PerfSession::start();
            let cap = crate::app::capture_from_app(cx, true, None, &mut perf)?;
            if cap.generation != generation {
                return Err(crate::error::Error::new(
                    "GPU reset recovery captured a stale generation",
                ));
            }
            let swap = match cx.with_gpu_context(|device, context| {
                present_freeze_from_texture(
                    device,
                    context,
                    &cap.sdr_display,
                    cap.width,
                    cap.height,
                    cap.generation,
                )
            }) {
                Some(Ok(swap)) => swap,
                Some(Err(e)) => return Err(e),
                None => return Err(crate::error::Error::new("GPU context unavailable")),
            };
            Ok((cap, swap))
        })();

        match rebuilt {
            Ok((cap, swap)) => {
                self.freeze_size = (cap.width, cap.height);
                self.cap = Some(cap);
                self.freeze_swap = Some(swap);
                self.freeze_generation = Some(generation);
                self.phase = Phase::Active;
                self.mag = None;
                self.mag_origin = None;
                self.sync_from_cap(window);
                // Re-publish into the tree gpui just recreated; gpui keeps the
                // freeze visual below the UI visual.
                self.attach_and_show(window, cx);
                self.show_panel("显示设备已重置", "截图窗口已恢复".to_string(), false, cx);
                if crate::gpu_lifecycle::acknowledge_recovery().is_ok() {
                    crate::perf::log_app("gpu reset: overlay freeze visual rebuilt");
                }
            }
            Err(e) => {
                crate::perf::log_app(&format!("gpu reset: overlay recovery failed: {e}"));
                // 冻结视觉没有重建成功，不能确认恢复：保持在 Recreated，
                // 让下一次捕获继续按新 generation 重建，绝不谎报 Ready。
                let _ = crate::gpu_lifecycle::mark_recovery_failed();
                self.finish(empty_result(), window, cx);
            }
        }
    }

    fn schedule_bench_auto_hide(&self, cx: &mut Context<Self>) {
        let perf = self.perf.clone();
        cx.spawn(async move |_this, cx| {
            for _ in 0..5_000 {
                cx.background_executor()
                    .timer(Duration::from_millis(1))
                    .await;
                let ready = perf
                    .borrow()
                    .as_ref()
                    .map(|p| p.has_mark("T7"))
                    .unwrap_or(true);
                if ready {
                    break;
                }
            }
            cx.update(|cx| {
                if let Some(handle) = OVERLAY_WINDOW.with(|s| *s.borrow()) {
                    let _ = handle.update(cx, |view, window, cx| {
                        view.cancel(window, cx);
                    });
                }
            });
        })
        .detach();
    }

    fn hide_window(window: &Window) {
        unsafe {
            let _ = ShowWindow(crate::platform::hwnd_of(window), SW_HIDE);
        }
    }

    fn pointer_to_capture(pos: GpuiPoint<Pixels>, scale: f32) -> Point {
        Point {
            x: (f32::from(pos.x) * scale) as f64,
            y: (f32::from(pos.y) * scale) as f64,
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.visible {
            return;
        }
        // The toolbar and the popup occlude and stop propagation, so reaching
        // here means the click landed outside both.
        self.color_open = false;
        self.editor.shape_open = false;
        // Any click dismisses the last "copied colour / format" notice.
        self.last_color_notice = None;
        if event.button == MouseButton::Right {
            // Cancel happens on release (same as the C# WM_RBUTTONUP path),
            // so a right press mid-capture does nothing yet.
            if self.phase == Phase::LongCapturing {
                return;
            }
            if self.text_edit.take().is_some() {
                cx.notify();
                return;
            }
            self.cancel(window, cx);
            return;
        }
        // Middle click can be bound to the same finished-crop shortcuts as a
        // left double-click. It only has an effect once a crop exists.
        if event.button == MouseButton::Middle
            && self.trigger_click_action(crate::config::get().screenshot.middle_click, window, cx)
        {
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        if self.phase == Phase::LongCapturing {
            // The selection is click-through; what little lands here is the
            // dim backdrop, which must not start a drag or an edit. Clicking
            // the backdrop also activates this window, which would steal the
            // wheel back -- hand the foreground to the scroll target again.
            if let Some(state) = self.long.as_mut() {
                let center = POINT {
                    x: state.abs_x + (state.w / 2) as i32,
                    y: state.abs_y + (state.h / 2) as i32,
                };
                state.target = focus_window_under(crate::platform::hwnd_of(window), center);
            }
            return;
        }
        let scale = window.scale_factor();
        self.mouse = Self::pointer_to_capture(event.position, scale);
        if self.phase == Phase::Active {
            if let Some(id) = self.hit_toolbar_item(self.mouse, scale) {
                crate::perf::log_diag(&format!("root mouse_down: toolbar hit id={id}"));
                self.handle_toolbar(id, window, cx);
                return;
            }
        }
        crate::perf::log_diag(&format!(
            "root mouse_down: canvas phase={:?} tool={:?} mouse=({:.0},{:.0})",
            self.phase, self.editor.tool, self.mouse.x, self.mouse.y
        ));
        self.commit_text_draft();
        // A left double-click on a finished crop runs the configured shortcut.
        // When it closes the overlay the matching release is swallowed by the
        // `!visible` guard in on_mouse_up.
        if event.click_count >= 2
            && self.trigger_click_action(
                crate::config::get().screenshot.left_double_click,
                window,
                cx,
            )
        {
            return;
        }
        self.down = true;
        self.down_pt = self.mouse;
        self.hit = SelectionHitKind::None;
        if self.phase == Phase::Active && self.has_sel {
            self.on_active_press();
        }
        cx.notify();
    }

    /// Fold a pending text draft into the document. Any click that leaves the text
    /// field -- canvas or toolbar -- has to run this or the typed text is lost.
    fn commit_text_draft(&mut self) {
        annotate::commit_text_draft(&mut self.editor, &mut self.text_edit);
    }

    fn on_active_press(&mut self) {
        match self.editor.tool {
            Tool::Select => {
                self.hit = hit_test(self.selection, self.mouse, 8.0);
                // A press fully outside the crop means "start over" when 点击外部
                // is configured to recreate. Drawing tools keep their own press
                // handling below and are deliberately left alone.
                if self.hit == SelectionHitKind::None
                    && matches!(
                        crate::config::get().screenshot.click_outside,
                        crate::config::ClickOutside::Recreate
                    )
                {
                    self.restart_selection();
                    return;
                }
                self.sel_at_press = self.selection;
                let on_handle =
                    self.hit != SelectionHitKind::None && self.hit != SelectionHitKind::Inside;
                if !on_handle {
                    self.editor.pointer_pressed(self.mouse);
                    if self.editor.selected_id.is_some() {
                        self.hit = SelectionHitKind::None;
                    }
                }
            }
            // Drawing tools only start inside the crop. Clipping alone would let a
            // stroke begin off-frame and appear out of nowhere, and a text caret
            // placed outside would be invisible while the user typed into it.
            _ if !self.selection.contains(self.mouse) => {
                self.down = false;
            }
            _ => match self.editor.pointer_pressed(self.mouse) {
                PressKind::RequestText => {
                    self.down = false;
                    self.text_edit = Some(TextDraft::new(self.mouse));
                }
                PressKind::Capture => {}
            },
        }
    }

    /// Clears the finished crop and returns to the fresh-drag phase so the drag
    /// that follows this press defines a replacement selection. Used by
    /// 点击外部 = 重新框选; `down`/`down_pt` stay set for that drag.
    fn restart_selection(&mut self) {
        self.selection = Rect::new(0.0, 0.0, 0.0, 0.0);
        self.has_sel = false;
        self.from_drag = false;
        self.hit = SelectionHitKind::None;
        self.hover = None;
        self.candidates_at = None;
        self.uia_pending = None;
        self.phase = Phase::Selecting;
    }

    /// Runs the action bound to a left double-click / middle click on a finished
    /// crop. Returns true when the overlay was closed, so the caller stops
    /// processing the event.
    fn trigger_click_action(
        &mut self,
        action: crate::config::ClickAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::config::ClickAction;
        if !self.visible || self.phase != Phase::Active || !self.has_sel {
            return false;
        }
        match action {
            ClickAction::None => false,
            ClickAction::Complete => {
                self.confirm(window, cx);
                true
            }
            ClickAction::Copy => {
                self.export_selection_and_finish(false, true, window, cx);
                true
            }
            ClickAction::Save => {
                self.export_selection_and_finish(true, false, window, cx);
                true
            }
        }
    }

    /// Exports the finished crop with fixed write/copy flags and closes the
    /// overlay. `confirm` honours the export-page switches instead; a click
    /// shortcut needs a deterministic destination, so it uses this path.
    fn export_selection_and_finish(
        &mut self,
        write_file: bool,
        copy: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut result) = self.result_from_current() else {
            return;
        };
        result.write_file = write_file;
        result.copy = copy;
        let source = self.capture_source();
        if let Some(cap) = self.cap.as_ref() {
            crate::app::export_annotated_region(cx, cap, &result, source);
        }
        self.finish(empty_result(), window, cx);
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.visible {
            return;
        }
        // A trim drag outranks the canvas tools: it owns the pointer from press
        // to release, even after it leaves the thumbnail. Recomputing from the
        // press-time range keeps the pointer delta authoritative.
        if let Some(drag) = self.long.as_ref().and_then(|s| s.trim_drag.as_ref()) {
            let pointer = f32::from(if drag.axis == Axis::Horizontal {
                event.position.x
            } else {
                event.position.y
            }) / window.scale_factor();
            let delta = pointer - drag.start_ptr;
            let edge = drag.edge;
            let start_range = drag.start_range;
            let thumb_extent = drag.thumb_extent;
            if let Some(state) = self.long.as_mut() {
                let canvas = state.total_height.max(1) as u32;
                let next = match edge {
                    crate::longtrim::TrimHit::Handle(which) => crate::longtrim::dragged_edge(
                        start_range,
                        which,
                        match which {
                            crate::longtrim::TrimEdge::Top => start_range.top,
                            crate::longtrim::TrimEdge::Bottom => start_range.bottom,
                        },
                        delta,
                        thumb_extent,
                        canvas,
                    ),
                    crate::longtrim::TrimHit::Band => {
                        crate::longtrim::dragged_band(start_range, delta, thumb_extent, canvas)
                    }
                    crate::longtrim::TrimHit::None => None,
                };
                if let Some(next) = next {
                    state.trim = next;
                }
            }
            cx.notify();
            return;
        }
        self.mouse = Self::pointer_to_capture(event.position, window.scale_factor());
        self.on_move(window);
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        // Ending a trim drag: release the pointer before anything else looks at
        // it, so the click that ends a drag is not also read as a canvas click.
        if event.button == MouseButton::Left {
            if let Some(state) = self.long.as_mut() {
                if state.trim_drag.take().is_some() {
                    cx.notify();
                    return;
                }
            }
        }
        if event.button == MouseButton::Right {
            if self.text_edit.take().is_some() {
                cx.notify();
                return;
            }
            // Mid-capture a right-click only cancels the session (the C#
            // HandleRightRelease path); the overlay itself stays open.
            if self.phase == Phase::LongCapturing {
                self.cancel_long_capture(window, cx);
                return;
            }
            self.cancel(window, cx);
            return;
        }
        if self.phase == Phase::LongCapturing {
            return;
        }
        if event.button == MouseButton::Middle {
            // A middle-click shortcut fires on press; its release is swallowed
            // here like any other non-left button.
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        self.mouse = Self::pointer_to_capture(event.position, window.scale_factor());
        if self.on_up() {
            self.confirm(window, cx);
        }
        cx.notify();
    }

    fn on_move(&mut self, window: &mut Window) {
        if self.down && self.phase == Phase::Selecting {
            let dx = (self.mouse.x - self.down_pt.x).abs();
            let dy = (self.mouse.y - self.down_pt.y).abs();
            if dx > MIN_RECT || dy > MIN_RECT {
                self.selection =
                    Rect::normalized(self.down_pt.x, self.down_pt.y, self.mouse.x, self.mouse.y);
                self.from_drag = true;
                self.has_sel = true;
            }
        } else if self.down && self.phase == Phase::Active {
            if self.editor.tool != Tool::Select && self.editor.tool != Tool::Text {
                if !self.editor.is_dragging() {
                    let _ = self.editor.pointer_pressed(self.down_pt);
                }
                self.editor.pointer_moved(self.mouse);
                return;
            }
            if self.editor.is_dragging() && self.editor.selected_id.is_some() {
                self.editor.pointer_moved(self.mouse);
                return;
            }
            let bounds = Rect::new(
                0.0,
                0.0,
                self.freeze_size.0 as f64,
                self.freeze_size.1 as f64,
            );
            if self.hit == SelectionHitKind::Inside {
                self.selection = move_rect(
                    self.sel_at_press,
                    self.mouse.x - self.down_pt.x,
                    self.mouse.y - self.down_pt.y,
                    bounds,
                );
            } else if self.hit != SelectionHitKind::None {
                self.selection =
                    resize_rect(self.sel_at_press, self.hit, self.mouse, bounds, MIN_RECT);
            }
        } else if self.phase == Phase::Selecting {
            // Recompute candidates when the mouse moves to a new pixel.
            if self.candidates_at != Some(self.mouse) {
                let hwnd = crate::platform::hwnd_of(window);
                let pt = POINT {
                    x: (self.mouse.x + self.origin_x as f64) as i32,
                    y: (self.mouse.y + self.origin_y as f64) as i32,
                };
                let cached = uia_cache_usable(
                    self.uia_last_target,
                    window_under_point(hwnd, pt),
                    self.uia_last_point,
                    pt,
                );
                // 只有缓存不可用时才丢弃在途查询并重新发起。缓存可用时保留
                // pending，让在途回执仍能被 poll 到并刷新 uia_last_point；
                // 否则每换一像素就 uia_epoch+=1，旧回执必被 epoch 不匹配
                // 丢弃，UIA 结果只在鼠标完全停下时才落地。
                if !cached {
                    self.uia_pending = None;
                    self.request_uia(hwnd, pt);
                }
                // 复用前按当前点重新验证：旧点处的元素矩形不能原样进入候选表，
                // 否则触顶 100 时会挤掉当前位置的有效边界。
                let filtered;
                let uia: &[RECT] = if cached {
                    filtered = filter_uia_for_point(&self.uia_last, pt, UIA_CACHE_PAD);
                    &filtered
                } else {
                    &[]
                };
                let candidates = detect_candidates(
                    self.smart_selection,
                    hwnd,
                    pt,
                    self.origin_x,
                    self.origin_y,
                    self.freeze_size.0,
                    self.freeze_size.1,
                    uia,
                );
                self.candidates = candidates;
                self.candidates_at = Some(self.mouse);
            }
            // Candidates are ordered deepest-first, but still ask for the one
            // containing the cursor so a stale or empty list cannot snap to an
            // unrelated pane.
            self.hover = crate::smart_selection::preview_rect(
                self.smart_selection,
                &self.candidates,
                self.mouse,
            );
        }
    }

    /// Returns true when the fresh selection should be confirmed right away:
    /// the copy-only flow never enters the toolbar phase.
    fn on_up(&mut self) -> bool {
        if !self.down {
            return false;
        }
        self.down = false;
        if self.phase == Phase::Active {
            self.editor.pointer_released();
            return false;
        }
        if self.phase == Phase::Selecting {
            let dx = (self.mouse.x - self.down_pt.x).abs();
            let dy = (self.mouse.y - self.down_pt.y).abs();
            let selected = if dx <= MIN_RECT && dy <= MIN_RECT {
                if let Some(rect) = crate::smart_selection::preview_rect(
                    self.smart_selection,
                    &self.candidates,
                    self.mouse,
                ) {
                    self.selection = rect;
                    self.from_drag = false;
                    self.has_sel = true;
                    true
                } else {
                    false
                }
            } else {
                self.has_sel && self.selection.width > MIN_RECT && self.selection.height > MIN_RECT
            };
            if selected {
                if self.copy_only {
                    return true;
                }
                self.phase = Phase::Active;
            }
        }
        false
    }

    fn current_rect(&self) -> Option<Rect> {
        if matches!(self.phase, Phase::Active | Phase::LongCapturing) && self.has_sel {
            return Some(self.selection);
        }
        if self.down && self.has_sel && self.selection.width > MIN_RECT {
            return Some(self.selection);
        }
        self.hover
    }

    fn result_from_current(&self) -> Option<OverlayResult> {
        let rect = if self.has_sel {
            self.selection
        } else {
            self.hover?
        };
        if rect.width < 2.0 || rect.height < 2.0 {
            return None;
        }
        let from_drag = if self.has_sel { self.from_drag } else { false };
        let (x, y, w, h) = physical_source_rect(
            rect,
            from_drag,
            self.freeze_size.0 as f64,
            self.freeze_size.1 as f64,
            self.freeze_size.0 as i32,
            self.freeze_size.1 as i32,
        );
        if w < 2 || h < 2 {
            return None;
        }
        let (write_file, copy) = export_flags_for(self.copy_only);
        Some(OverlayResult {
            confirmed: true,
            write_file,
            copy,
            x: x as u32,
            y: y as u32,
            w: w as u32,
            h: h as u32,
            marks: self.editor.elements.clone(),
        })
    }

    fn confirm_action(
        &mut self,
        _: &ConfirmSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.text_edit.is_some() {
            self.commit_text_draft();
            cx.notify();
            return;
        }
        // The C# build ignores Enter mid-capture: 完成 on the bar is the only
        // way out with pixels, so a stray Enter cannot end a scroll early.
        if self.phase == Phase::LongCapturing {
            return;
        }
        self.confirm(window, cx);
    }

    fn cancel_action(&mut self, _: &CancelOverlay, window: &mut Window, cx: &mut Context<Self>) {
        if self.text_edit.take().is_some() {
            cx.notify();
            return;
        }
        // Esc mid-capture drops the session and returns to the editor, same
        // as the C# LongCaptureCancel path -- it never closes the overlay.
        if self.phase == Phase::LongCapturing {
            self.cancel_long_capture(window, cx);
            return;
        }
        self.cancel(window, cx);
    }

    fn undo_action(&mut self, _: &UndoEdit, _: &mut Window, cx: &mut Context<Self>) {
        // The draft is not in history, so undoing past it would silently drop the
        // previous annotation while the text field stayed on screen.
        if self.text_edit.take().is_none() {
            self.editor.undo();
        }
        cx.notify();
    }

    fn redo_action(&mut self, _: &RedoEdit, _: &mut Window, cx: &mut Context<Self>) {
        if self.text_edit.is_none() {
            self.editor.redo();
        }
        cx.notify();
    }

    /// Only Backspace lives here. Characters arrive through the
    /// `EntityInputHandler` below (WM_CHAR and the IME result string), which is
    /// also what makes Chinese input work; inserting `key_char` as well would
    /// type every Latin key twice.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(edit) = self.text_edit.as_mut() {
            if event.keystroke.key == "backspace" && edit.backspace() {
                cx.notify();
            }
            return;
        }
        // Plain `c` / `f` are colour-picker shortcuts outside text editing.
        // Checking the modifiers keeps `ctrl-c` and `shift-f` out.
        if event.keystroke.modifiers == Default::default() {
            match event.keystroke.key.as_str() {
                "c" => self.copy_picked_color(&CopyPickedColor, window, cx),
                "f" => self.cycle_picked_color_format(&CycleColorFormat, window, cx),
                _ => {}
            }
        }
    }

    /// Capture-origin tag persisted with history records. The overlay is only
    /// opened for region or copy-only shots; long previews tag themselves.
    fn capture_source(&self) -> &'static str {
        if self.copy_only {
            "copyOnly"
        } else {
            "region"
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        let Some(result) = self.result_from_current() else {
            return;
        };
        self.finish(result, window, cx);
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut result) = self.result_from_current() else {
            return;
        };
        result.copy = false;
        result.write_file = true;
        let source = self.capture_source();
        if let Some(cap) = self.cap.as_ref() {
            crate::app::export_annotated_region(cx, cap, &result, source);
        }
        // 「保存」即结束本次截图：不 finish 的话覆盖层保持可见、BUSY 不清，
        // 用户随后点「完成」会再导出一次同一张图，写两份文件。
        self.finish(empty_result(), window, cx);
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        self.finish(empty_result(), window, cx);
    }

    fn finish(&mut self, result: OverlayResult, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = false;
        // Whatever path ends the capture, the click-through and any poll task
        // must not outlive it.
        self.long = None;
        set_long_passthrough(crate::platform::hwnd_of(window), None);
        // Hide only once the now-empty frame has been committed, so nothing of this
        // capture survives in gpui's swap chain for the next one to flash. Hiding is
        // idempotent, and a full-screen topmost window that never goes away would be
        // far worse than a flash, so a timer hides it regardless if that frame never
        // arrives. Both callbacks are bound to this capture's generation: if another
        // screenshot has already started, they must not hide its (reused) window
        // while that new view still thinks it is visible.
        let gen = OVERLAY_GENERATION.with(|g| g.get());
        window.on_next_frame(move |window, _cx| {
            if OVERLAY_GENERATION.with(|g| g.get()) != gen {
                return;
            }
            let _ = window.wait_for_commit_completion();
            Self::hide_window(window);
        });
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            cx.update(|cx| {
                if OVERLAY_GENERATION.with(|g| g.get()) != gen {
                    return;
                }
                if let Some(handle) = OVERLAY_WINDOW.with(|s| *s.borrow()) {
                    let _ = handle.update(cx, |_view, window, _cx| {
                        Self::hide_window(window);
                    });
                }
            });
        })
        .detach();
        let source = self.capture_source();
        let cap = self.cap.take();
        let perf = self.perf.borrow_mut().take();
        crate::app::complete_region_shot(result, cap, perf, source, cx);
        cx.notify();
    }

    fn active_monitor_rect(&self) -> Rect {
        // Capture-relative, so compare against the selection's center in the
        // same space (monitor rects are already shifted by origin_x/origin_y).
        let cx = self.selection.x + self.selection.width / 2.0;
        let cy = self.selection.y + self.selection.height / 2.0;
        for m in &self.monitor_rects {
            if cx >= m.x && cx < m.x + m.width && cy >= m.y && cy < m.y + m.height {
                return *m;
            }
        }
        Rect::new(
            0.0,
            0.0,
            self.freeze_size.0 as f64,
            self.freeze_size.1 as f64,
        )
    }

    /// One list for sizing, hit testing and painting. Sizing the bar for every
    /// tool while painting a filtered subset drifts every slot, so a click on
    /// the last button lands on its neighbour.
    fn visible_items(&self) -> Vec<&'static str> {
        toolbar_items_for(self.editor.tool, &self.hidden_toolbar_items)
    }

    fn toolbar_geom(&self, scale: f32) -> Option<Rect> {
        if self.phase != Phase::Active || !self.has_sel {
            return None;
        }
        let n = self.visible_items().len();
        let (w, h) = toolbar::bar_size(n);
        let s = scale as f64;
        // Remembered corners are persisted in logical capture pixels; the bar
        // geometry (like the monitor rect) is in physical pixels.
        let remembered = self
            .toolbar_remember_pos
            .map(|(x, y)| (x as f64 * s, y as f64 * s));
        let pos = toolbar::place_toolbar_with_policy(
            self.selection,
            w * s,
            h * s,
            self.active_monitor_rect(),
            toolbar::ANCHOR_GAP * s,
            self.toolbar_position,
            remembered,
        );
        Some(Rect::new(pos.x, pos.y, w * s, h * s))
    }

    fn hit_toolbar_item(&self, p: Point, scale: f32) -> Option<&'static str> {
        let geom = self.toolbar_geom(scale)?;
        annotate::toolbar_item_at(p, geom, &self.visible_items())
    }

    fn handle_toolbar(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        // Diagnostic ladder: reaching here proves the button's on_mouse_down
        // -> on_item closure actually dispatched the id to the host.
        crate::perf::log_diag(&format!("button dispatched id={id}"));
        // Buttons stop propagation, so `on_mouse_down` never runs for these clicks.
        if annotate::handle_common_toolbar(
            &mut self.editor,
            &mut self.text_edit,
            &mut self.color_open,
            id,
        ) {
            cx.notify();
            return;
        }
        match id {
            toolbar::OCR => self.run_ocr(cx),
            toolbar::TRANSLATE => self.run_translate(cx),
            toolbar::PIN => self.pin(window, cx),
            toolbar::LONG_CAPTURE => self.start_long_capture(window, cx),
            toolbar::SAVE => self.save(window, cx),
            toolbar::COMPLETE => self.confirm(window, cx),
            toolbar::CANCEL => self.cancel(window, cx),
            _ => {}
        }
    }

    fn mark_masks(&self, marks: &[Element]) -> Vec<Option<Rc<ElementRuns>>> {
        annotate::mark_masks(&self.mask_cache, marks)
    }

    fn spotlight_dim_runs(&self, bounds: Rect, marks: &[Element]) -> Option<Rc<Vec<CoverageRun>>> {
        annotate::spotlight_dim_runs(&self.spotlight_cache, bounds, marks)
    }

    /// What OCR reads is the annotated crop, same as the C# `FlattenSelection`:
    /// a mosaic over a password must hide it from the recognizer too.
    /// 取出当前选区经标注合成后的像素。失败时带回可读的中文原因，供 OCR /
    /// 翻译 / 贴图按钮在面板上明确告知，而不是点了没反应。
    fn selection_pixels(
        &self,
        cx: &mut Context<Self>,
    ) -> std::result::Result<(Vec<u8>, u32, u32, u32), String> {
        let result = self
            .result_from_current()
            .ok_or_else(|| "没有可处理的选区".to_string())?;
        let cap = self
            .cap
            .as_ref()
            .ok_or_else(|| "截图数据已释放，请重新截图".to_string())?;
        let (mut px, w, h, stride) =
            crate::app::crop_from_app(cx, cap, result.x, result.y, result.w, result.h)
                .map_err(|e| format!("裁剪选区失败：{e}"))?;
        composite::apply_annotations(
            &mut px,
            w,
            h,
            stride,
            result.x as f64,
            result.y as f64,
            &result.marks,
        );
        Ok((px, w, h, stride))
    }

    fn busy(&self) -> bool {
        self.panel.as_ref().is_some_and(|p| p.busy)
    }

    fn show_panel(&mut self, title: &str, body: String, busy: bool, cx: &mut Context<Self>) {
        self.panel = Some(Panel {
            title: title.to_string().into(),
            body: body.into(),
            busy,
        });
        cx.notify();
    }

    /// Enters scrolling capture on the current selection (the C#
    /// `LongCapture_Click`): the toolbar swaps for the long-capture bar, the
    /// selection turns click-through so the app beneath can scroll, and a poll
    /// task stitches one frame every `LONG_POLL`.
    fn start_long_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phase != Phase::Active || !self.has_sel || self.busy() {
            return;
        }
        self.commit_text_draft();
        let Some(result) = self.result_from_current() else {
            return;
        };
        let abs_x = self.origin_x + result.x as i32;
        let abs_y = self.origin_y + result.y as i32;
        let (w, h) = (result.w as usize, result.h as usize);
        // Every session starts vertical: a wide selection no longer implies a
        // horizontal page, and neither the previous session nor the config can
        // carry an axis in. The bar's ↕/↔ button is the only way across.
        let axis = INITIAL_LONG_AXIS;
        // The freeze frame may be stale by the time the user reaches for this
        // button, so -- like the C# runner -- the first frame is a fresh
        // capture, not a crop of it. A persistent WGC session on the covering
        // monitor turns every later poll into a crop of its newest frame
        // instead of a fresh WGC start every poll.
        let live = self.start_live_session(abs_x, abs_y, result.w, result.h);
        let live_first = live.as_ref().and_then(|l| {
            crate::app::capture_region_live(
                cx,
                &l.session,
                &l.gpu,
                l.sdr_white,
                l.mon_x,
                l.mon_y,
                abs_x,
                abs_y,
                result.w,
                result.h,
            )
            .ok()
            .flatten()
        });
        let (live, (px, pw, ph, _)) = match live_first {
            Some(frame) => (live, frame),
            None => {
                if live.is_some() {
                    crate::perf::log_app(
                        "long-capture: live session produced no first frame, falling back",
                    );
                }
                match crate::app::capture_region_bgra(cx, abs_x, abs_y, result.w, result.h) {
                    Ok(frame) => (None, frame),
                    Err(e) => {
                        self.show_panel("长截图失败", e.to_string(), false, cx);
                        return;
                    }
                }
            }
        };
        if pw != result.w || ph != result.h {
            return;
        }
        // The 16384 px cap applies in matcher space, i.e. to the scroll axis:
        // for a horizontal session that is the stitched long side (exported
        // width), which is exactly the D3D limit that matters.
        let mut stitcher = match OrbStitcher::new(axis, w, h, LONG_MAX_H) {
            Ok(s) => s,
            Err(e) => {
                self.show_panel("长截图失败", e.to_string(), false, cx);
                return;
            }
        };
        // Seed the stitcher with the first frame. This only initializes snow's
        // canvas / ORB reference (no matching), and the neutral decision is
        // discarded here — the worker owns every later frame.
        if let Err(e) = stitcher.push(&px, w, h) {
            self.show_panel("长截图失败", e.to_string(), false, cx);
            return;
        }
        // 首帧已建立画布：沿 stitch 轴的增长长度就是 total_height / trim 的
        // 初始 extent。横向会话里它是选区宽 w（旋转后成为 snow 画布高），
        // 不是屏幕高 h —— 后者是常量，会让 trim 从一开始就钉在首屏。
        let initial_extent = stitcher.canvas_extent();
        self.long_seq = self.long_seq.wrapping_add(1);
        let gen = self.long_seq;
        let gpu_generation = self
            .cap
            .as_ref()
            .map(|c| c.generation)
            .unwrap_or_else(crate::gpu_lifecycle::current_generation);
        // The first frame is the session baseline: it is built here so a
        // budget refusal can still be shown before the phase changes, then
        // handed to the worker, which owns it (and every later frame) for the
        // rest of the session.
        let worker = match LongWorker::new(gen, axis, stitcher) {
            Ok(w) => w,
            Err(e) => {
                self.show_panel("长截图失败", format!("后台任务启动失败：{e}"), false, cx);
                return;
            }
        };
        let (segments, total_height) = (1, initial_extent);
        self.long = Some(LongCaptureState {
            abs_x,
            abs_y,
            w: result.w,
            h: result.h,
            axis,
            status: long_status_hint(axis),
            gen,
            gpu_generation,
            maxed: false,
            live,
            poll_ticks: 0,
            late_ticks: 0,
            capture_failures: 0,
            stall_paused: false,
            segments,
            total_height,
            preview: None,
            preview_buf: Vec::new(),
            preview_buf_w: 0,
            preview_buf_h: 0,
            // A fresh session selects everything: 完成 without touching a
            // handle exports the whole stitch, exactly as before this existed.
            // 用增长维度初始化（横向为长边宽），不是常量屏幕高。
            trim: crate::longtrim::TrimRange::full(initial_extent.max(1) as u32),
            trim_drag: None,
            gate: ResultGate::new(gen),
            worker,
            // Filled in just below by `focus_window_under`; auto-scroll cannot
            // post anything until that window is known.
            target: None,
            auto_scroll: AutoScroll::with_step(crate::config::get().screenshot.auto_scroll.step()),
            cadence: crate::capture_cadence::CaptureCadence::new(),
            hover: crate::hoverpreview::HoverPreview::new(),
            hover_image: None,
            hover_rows: None,
        });
        // The OCR/translate panel has no meaning mid-capture; the C# build
        // collapses ResultPanel here too.
        self.panel = None;
        self.phase = Phase::LongCapturing;
        // The hover preview is a scrolling-phase feature: it only makes sense
        // once there is a thumbnail to hover.
        if let Some(state) = self.long.as_mut() {
            state.hover.set_enabled(true);
        }
        let (bar, bar_h) =
            self.long_bar_placement(window.scale_factor(), 0.0, self.active_monitor_rect());
        let rect = hole_clear_of_bar(
            RECT {
                left: abs_x,
                top: abs_y,
                right: abs_x + result.w as i32,
                bottom: abs_y + result.h as i32,
            },
            self.origin_y + bar.y as i32,
            self.origin_y + (bar.y + bar_h).ceil() as i32,
        );
        set_long_passthrough(crate::platform::hwnd_of(window), Some(rect));
        let center = POINT {
            x: abs_x + result.w as i32 / 2,
            y: abs_y + result.h as i32 / 2,
        };
        let target = focus_window_under(crate::platform::hwnd_of(window), center);
        if let Some(state) = self.long.as_mut() {
            state.target = target;
            // Auto-scroll starts off by default (Manual); the Auto setting
            // turns it on for this session the moment the target is known,
            // without persisting anything back to config.
            if matches!(
                crate::config::get().screenshot.long_capture_auto_start,
                crate::config::LongCaptureAutoStart::Auto
            ) {
                if state.target.is_some() {
                    state.auto_scroll.set_enabled(true);
                    state.status = "自动滚动：开".to_string();
                } else {
                    state.status = AUTO_SCROLL_POST_FAILED.to_string();
                }
            }
        }
        self.spawn_long_poll(gen, cx);
        cx.notify();
    }

    /// Starts a persistent WGC session for the monitor that fully contains the
    /// polled region, so every long-capture poll can crop the session's
    /// newest frame instead of paying for a fresh WGC start every 450 ms.
    /// Any failure -- no containing monitor, a start error, or no frame
    /// within the wait -- returns `None`; the caller then falls back to
    /// `capture_region_bgra` for the whole session.
    /// ponytail: exclusive-fullscreen apps and DXGI-duplication-only monitors
    /// have no WGC frame to deliver here either, so they take the same
    /// fallback path as any other start failure -- no separate detection.
    fn start_live_session(&self, abs_x: i32, abs_y: i32, w: u32, h: u32) -> Option<LongLive> {
        // 只有 Auto 模式才尝试持续 WGC live 会话；显式选择 Wgc / Dxgi 时
        // 越过它，整场长截图都走 fallback（由选中的捕获模式接管）。
        if !matches!(
            crate::config::get().screenshot.capture_mode,
            crate::config::CaptureMode::Auto
        ) {
            return None;
        }
        let cap = self.cap.as_ref()?;
        let monitors = crate::capture::enumerate_monitors().ok()?;
        let mon = monitors.iter().find(|m| {
            abs_x >= m.x
                && abs_y >= m.y
                && abs_x + w as i32 <= m.x + m.w
                && abs_y + h as i32 <= m.y + m.h
        })?;
        let show_cursor = crate::config::get().screenshot.show_cursor;
        let session = crate::capture::start_live(&cap.gpu, mon, mon.hdr, show_cursor).ok()?;
        // A cold WGC session used to busy-wait up to 500 ms here, on the UI
        // thread, before the bar even appeared. The very first frame has a
        // fresh-capture fallback anyway, so only give the live session a short
        // head start and let later ticks pick it up once it warms up.
        if !session.wait_first(Duration::from_millis(50)) {
            return None;
        }
        Some(LongLive {
            session,
            gpu: cap.gpu.clone(),
            generation: cap.generation,
            sdr_white: cap.sdr_white_nits,
            mon_x: mon.x,
            mon_y: mon.y,
        })
    }

    fn spawn_long_poll(&self, gen: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                // Adaptive cadence: the interval follows the slower of the
                // capture / stitch costs measured on the frames processed so
                // far. Falls back to the fixed 150 ms until a session exists
                // or after it ends.
                let interval = this
                    .update(cx, |this, _cx| {
                        this.long
                            .as_ref()
                            .map(|state| state.cadence.interval())
                            .unwrap_or(LONG_POLL)
                    })
                    .unwrap_or(LONG_POLL);
                cx.background_executor().timer(interval).await;
                match this.update(cx, |this, cx| this.long_tick(gen, cx)) {
                    Ok(true) => {}
                    // View gone, session over, or the cap stopped the polling.
                    _ => break,
                }
            }
        })
        .detach();
    }

    /// One poll: fresh frame, then hand it to the worker, which aligns it
    /// against the previous one and appends the rows that scrolled into view.
    /// Returns false when the loop should stop.
    ///
    /// The UI thread only ever does the capture/readback (the D3D11 immediate
    /// context is single-threaded, so the staging `Map` cannot move), then
    /// applies the verdict of the previous frame. All of normalize, gray, SAD,
    /// stitch and the diag write happen on the worker (P1-01).
    fn long_tick(&mut self, gen: u64, cx: &mut Context<Self>) -> bool {
        let started = Instant::now();
        // A device reset invalidates the live WGC session and the capture
        // generation. Stop the session with a visible reason instead of
        // polling forever on a dead session.
        let gpu_stale = self.long.as_ref().is_some_and(|state| {
            !crate::gpu_lifecycle::is_usable(state.gpu_generation)
                || state
                    .live
                    .as_ref()
                    .is_some_and(|live| !crate::gpu_lifecycle::is_usable(live.generation))
        });
        if gpu_stale {
            self.abort_long_for_gpu_reset(cx);
            return false;
        }
        let Some(state) = self.long.as_ref() else {
            return false;
        };
        if !should_keep_polling(
            state.gen == gen,
            state.maxed,
            self.phase == Phase::LongCapturing,
        ) {
            return false;
        }
        // Fold in whatever the worker finished since the last poll. Only one
        // job is ever in flight, so this is normally exactly one result; a
        // stale one (old generation or old axis) is dropped by the gate.
        loop {
            let polled = match self.long.as_ref() {
                Some(state) => state.worker.poll(),
                None => return false,
            };
            match polled {
                Poll::Empty => break,
                Poll::Done(done) => {
                    if !self.apply_long_done(done, cx) {
                        return false;
                    }
                }
                // The thread ended under an active session: no result can
                // arrive, so stop instead of waiting on the in-flight job.
                Poll::Gone => {
                    if let Some(state) = self.long.as_mut() {
                        state.maxed = true;
                        state.status = "长截图后台任务已停止".to_string();
                    }
                    cx.notify();
                    return false;
                }
            }
        }
        let Some(state) = self.long.as_mut() else {
            return false;
        };
        if !should_keep_polling(true, state.maxed, self.phase == Phase::LongCapturing) {
            return false;
        }
        // Hover preview handshake. A hover pauses the capture so the canvas
        // cannot move under the band being rendered; the pause only becomes
        // real once no job is in flight, which is exactly the state this tick
        // waits for before it submits anything.
        if state.hover.paused() && !state.hover.want_pause() {
            if let Some(request) = state.hover.next_request() {
                let (epoch, revision) = (state.hover.epoch(), state.hover.content_revision());
                let job = crate::longworker::RegionJob {
                    generation: gen,
                    top: request.top,
                    bottom: request.bottom,
                    target_width: HOVER_PREVIEW_WIDTH,
                };
                match state.worker.render_region(job, HOVER_PREVIEW_WAIT) {
                    Some((pixels, w, h)) => {
                        if state.hover.accept(request, epoch, revision, true) {
                            let rows = (request.top, request.bottom);
                            state.hover_rows = Some(rows);
                            state.hover_image =
                                render_image(&Arc::new(pixels), w, h).map(|img| (img, w, h));
                        }
                    }
                    None => {
                        state.hover.accept(request, epoch, revision, false);
                        state.hover_image = None;
                        state.hover_rows = None;
                    }
                }
                cx.notify();
            }
        }
        let Some(state) = self.long.as_mut() else {
            return false;
        };
        // While a hover holds the capture no frame is submitted: the canvas is
        // what the preview is reading.
        if state.hover.paused() {
            if !state.gate.in_flight() {
                state.hover.pause_ready();
            }
            state.poll_ticks = state.poll_ticks.wrapping_add(1);
            return true;
        }
        // Auto-scroll runs on its own beat, independent of the frame cadence:
        // a paused page (which decimates polling) is exactly what it is for.
        // Its plan refuses to post while a job is in flight.
        let in_flight = state.gate.in_flight();
        self.pump_auto_scroll(started, in_flight, cx);
        let Some(state) = self.long.as_mut() else {
            return false;
        };
        // Still processing the previous frame: skip this sample. The cadence
        // then follows the worker instead of queueing frames it cannot keep
        // up with, and the skip is counted for the next diag line.
        if state.gate.in_flight() {
            state.late_ticks = state.late_ticks.wrapping_add(1);
            state.poll_ticks = state.poll_ticks.wrapping_add(1);
            // Consecutive in-flight polls are backpressure: the worker cannot
            // drain as fast as we are polling, so lower the target rate.
            state.cadence.record_pressure(state.late_ticks as usize);
            return true;
        }
        // Reduced cadence while the page is still (P1-05): skip the capture
        // and the align pass, but keep the poll clock moving so the
        // decimation stays in phase with the loop.
        if state.stall_paused && state.poll_ticks % SLOW_POLL_EVERY != 0 {
            state.poll_ticks = state.poll_ticks.wrapping_add(1);
            return true;
        }
        let (ax, ay, w, h) = (state.abs_x, state.abs_y, state.w, state.h);
        let (axis, epoch) = (state.axis, state.gate.epoch());
        let captured = match state.live.as_ref() {
            Some(live) => crate::app::capture_region_live(
                cx,
                &live.session,
                &live.gpu,
                live.sdr_white,
                live.mon_x,
                live.mon_y,
                ax,
                ay,
                w,
                h,
            )
            .map(|frame| frame.map(|(px, pw, ph, _)| (px, pw, ph))),
            None => crate::app::capture_region_bgra(cx, ax, ay, w, h)
                .map(|(px, pw, ph, _)| Some((px, pw, ph))),
        };
        let px = match captured {
            // No new frame since the last poll: nothing scrolled, not an error.
            Ok(None) => return true,
            Ok(Some((px, pw, ph))) if pw == w && ph == h => {
                state.capture_failures = 0;
                px
            }
            // A wrong size or a hard error is a failed sample. A few are
            // transient; a long run used to poll forever with a misleading
            // "scroll the page" status and no way to tell the user why.
            Ok(_) | Err(_) => {
                state.capture_failures = state.capture_failures.saturating_add(1);
                if state.capture_failures >= 5 {
                    state.status = "采集连续失败，请取消后重新开始长截图".to_string();
                    crate::perf::log_app("long capture stopped after 5 capture failures");
                    return false;
                }
                return true;
            }
        };
        state.poll_ticks = state.poll_ticks.wrapping_add(1);
        let Some(seq) = state.gate.issue(epoch) else {
            return true;
        };
        let capture_ms = started.elapsed().as_secs_f64() * 1e3;
        let late = std::mem::take(&mut state.late_ticks);
        // The frame got through, so the previous burst is over: let the next
        // one cut the rate again instead of compounding on a stale counter.
        state.cadence.clear_pressure();
        let job = Job {
            generation: gen,
            epoch,
            seq,
            axis,
            width: w as usize,
            height: h as usize,
            bgra: px,
            capture_ms,
            late,
        };
        if !state.worker.submit(job) {
            // Worker gone (or cancelled): end the polling; the bar stays up.
            state.maxed = true;
            state.status = "长截图后台任务已停止".to_string();
            cx.notify();
            return false;
        }
        true
    }

    /// One auto-scroll step, driven from the poll tick. The beat and the
    /// give-up rule live in [`AutoScroll`]; only the post itself touches
    /// Win32. `in_flight` means the worker still owns the previous frame, so
    /// nothing may be posted (the page would move past the frame being
    /// stitched).
    fn pump_auto_scroll(&mut self, now: Instant, in_flight: bool, cx: &mut Context<Self>) {
        let Some(state) = self.long.as_mut() else {
            return;
        };
        if !state.auto_scroll.enabled() {
            return;
        }
        match state.auto_scroll.plan(now, in_flight) {
            AutoScrollStep::Post => {
                let center = POINT {
                    x: state.abs_x + (state.w / 2) as i32,
                    y: state.abs_y + (state.h / 2) as i32,
                };
                let multiplier = state.auto_scroll.notch_multiplier();
                let posted = state
                    .target
                    .is_some_and(|hwnd| post_wheel(hwnd, state.axis, center, multiplier));
                if !state.auto_scroll.observe_post(posted) {
                    state.status = AUTO_SCROLL_POST_FAILED.to_string();
                    crate::perf::log_diag("long-capture: auto scroll post failed");
                    cx.notify();
                }
            }
            AutoScrollStep::StopNoProgress => {
                state.status = AUTO_SCROLL_NO_PROGRESS.to_string();
                crate::perf::log_diag("long-capture: auto scroll stopped, no new rows");
                cx.notify();
            }
            AutoScrollStep::Wait => {}
        }
    }

    /// 自动滚动 on the long-capture bar: flips the session switch only. No
    /// re-baseline, no axis change, no buffer reset -- turning it off restores
    /// the manual behaviour exactly.
    fn toggle_auto_scroll(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.long.as_mut() else {
            return;
        };
        let on = !state.auto_scroll.enabled();
        state.auto_scroll.set_enabled(on);
        if on && state.target.is_none() {
            // Nothing to post to: say so now instead of collecting empty
            // beats until the give-up rule fires.
            state.auto_scroll.set_enabled(false);
            state.status = AUTO_SCROLL_POST_FAILED.to_string();
        } else {
            state.status = if on {
                "自动滚动：开".to_string()
            } else {
                long_status_hint(state.axis)
            };
        }
        cx.notify();
    }

    /// Single-step scroll: posts exactly one configured step to the capture
    /// target, on demand, whether or not auto-scroll is running.
    ///
    /// This is the manual counterpart of the auto beat and the API snow-shot
    /// exposes as `scrollOnce`. It deliberately ignores the beat and the
    /// in-flight guard's *timing*: it is a user action, so the caller decides
    /// when to press it. It still refuses to run with no target, and it does
    /// not touch `idle_posts`, so a manual nudge can neither trip nor silence
    /// the automatic give-up rule.
    fn step_scroll_once(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.long.as_mut() else {
            return;
        };
        let Some(hwnd) = state.target else {
            state.status = AUTO_SCROLL_POST_FAILED.to_string();
            cx.notify();
            return;
        };
        let center = POINT {
            x: state.abs_x + (state.w / 2) as i32,
            y: state.abs_y + (state.h / 2) as i32,
        };
        let multiplier = state.auto_scroll.notch_multiplier();
        if post_wheel(hwnd, state.axis, center, multiplier) {
            // A manual step means the user is driving the wheel: turn
            // auto-scroll off so its own beat can't fight the user's scroll.
            state.auto_scroll.set_enabled(false);
            state.auto_scroll.observe_manual_step(Instant::now());
            state.status = format!("已滚动一步（{multiplier} 格）");
        } else {
            state.status = AUTO_SCROLL_POST_FAILED.to_string();
            crate::perf::log_diag("long-capture: manual step post failed");
        }
        cx.notify();
    }

    /// Applies one finished worker result. Returns false when polling must
    /// stop (the session ended or the worker refused the frame).
    fn apply_long_done(&mut self, done: Done, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.long.as_mut() else {
            return false;
        };
        // The gate is the only place a result is accepted: a result from a
        // closed session or a replaced axis is dropped before it can reach the
        // pixels or the status line.
        if !state.gate.accept(&done) {
            return true;
        }
        match done.outcome {
            Outcome::Discarded => true,
            Outcome::Stopped { reason } => {
                // Every hard stop is surfaced: the bar keeps the last frame
                // and 完成/取消 available, and the status says which cap was
                // hit instead of a bare "已达最大高度" for all of them.
                state.maxed = true;
                state.status = match reason {
                    StopReason::HeightCap => "已达最大高度".to_string(),
                    StopReason::Budget => "图像超出内存预算".to_string(),
                    StopReason::FrameSize => "捕获帧尺寸变化，请重试".to_string(),
                };
                cx.notify();
                false
            }
            Outcome::Frame(frame) => {
                // Feed the adaptive cadence with this frame's two real costs:
                // capture on the UI thread, stitch (ORB + canvas) on the worker.
                // The slower stage drives the next poll interval.
                state
                    .cadence
                    .record_frame(frame.spans.capture, frame.spans.process);
                state.segments = frame.segments;
                // `frame.total_height` 由 worker 从 `canvas_height()` 取得：横向时
                // 是旋转后的长边 L（增长维度），纵向时是画布高。统一用它维护
                // total_height / trim / status / 自动滚动，不再用常量屏幕高。
                let extent = frame.total_height;
                state.total_height = extent;
                state.stall_paused = frame.paused;
                // A progress frame carries only the canvas rows it added; blit
                // them into the accumulated buffer and rebuild the thumbnail
                // from that. Idle frames leave the previous preview in place.
                // `reset` also drives the trim update below, so read it before
                // the band is moved into the blit.
                let preview_reset = frame.preview.as_ref().is_some_and(|band| band.reset);
                if let Some(band) = frame.preview {
                    if let Some((buf, w, h)) = apply_preview_band(
                        &mut state.preview_buf,
                        &mut state.preview_buf_w,
                        &mut state.preview_buf_h,
                        &band,
                    ) {
                        if let Some(image) = render_image(&buf, w, h) {
                            state.preview = Some((image, w, h));
                        }
                    }
                }
                // New rows confirm that scrolling works, whether they came
                // from auto-scroll or from the user's own wheel. 横向传入的是
                // 增长的长边 L，常量屏幕高会让自动滚动误判"未检测到滚动"。
                state.auto_scroll.observe_height(extent);
                // Keep the trim selection meaningful as the canvas grows. The
                // growth direction comes from the frame that grew it: a reset
                // (first frame, axis flip) selects everything again, an upward
                // scroll inserted rows above the selection, anything else
                // appended below it. Ported from the snow-shot controller's
                // Initial/Replaced, PrependedUp and append branches.
                let canvas = extent as u32;
                state.trim = if preview_reset {
                    crate::longtrim::TrimRange::after_reset(canvas)
                } else {
                    match frame.verdict {
                        Verdict::Progress {
                            is_up: true,
                            new_rows,
                        } => state.trim.after_prepend(new_rows as u32, canvas),
                        _ => state.trim.after_append(canvas),
                    }
                }
                .unwrap_or(crate::longtrim::TrimRange::full(canvas));
                // A drag in flight is anchored to a range that may no longer be
                // meaningful after a reset; drop it rather than let the next
                // move jump the handle across the canvas.
                if state
                    .trim_drag
                    .as_ref()
                    .is_some_and(|drag| drag.start_range.bottom > canvas)
                {
                    state.trim_drag = None;
                }
                match frame.verdict {
                    Verdict::Progress { .. } => {
                        state.status = format!("已捕获 {} 段 · 共 {} px", frame.segments, extent);
                    }
                    Verdict::Paused => {
                        state.status = "未检测到滚动，继续等待…".to_string();
                    }
                    // Nothing to say: keep the previous status.
                    Verdict::Idle => {}
                    // A static page with a local animation: neutral status,
                    // never "画面移动", and the session stays alive.
                    Verdict::LocalChange => {
                        state.status = format!(
                            "已捕获 {} 段 · 共 {} px · 等待滚动…",
                            frame.segments, extent
                        );
                    }
                    // A transient bad frame was skipped; keep capturing.
                    Verdict::FrameSkipped => {
                        state.status = "捕获帧瞬时异常，已跳过该帧".to_string();
                    }
                    Verdict::SceneCut => {
                        // The page navigated or reflowed: full poll speed, no
                        // end, and an honest status instead of "match failed".
                        state.status = "检测到页面内容切换，请继续滚动".to_string();
                    }
                    Verdict::StaticEdge => {
                        // The band is covering the strip right now: say so on
                        // the first tick instead of after 1.5 s.
                        state.status = "固定页眉/页脚遮挡，请缩小选区".to_string();
                    }
                    Verdict::FastScroll => {
                        state.status = "滚动过快，请慢一点".to_string();
                    }
                    // The page moved but no offset could be confirmed. This is
                    // the normal mid-scroll state, never a pause: say what the
                    // matcher saw instead of "no scrolling detected".
                    Verdict::Unaligned {
                        verification_failed,
                    } => {
                        state.status = if verification_failed {
                            // Soft, actionable wording: a refused offset is
                            // not a failure and does not stop the session.
                            "本次位移未确认，请继续滚动".to_string()
                        } else {
                            "画面移动但未能对齐，请稍慢或稍等…".to_string()
                        };
                    }
                    // The stall threshold was reached while the page itself
                    // did not move. Keep the session: `frame.paused` already
                    // put the loop on the slow cadence, and the next stitched
                    // tick restores full speed and the "已捕获 N 段" status.
                    Verdict::Stalled { static_edge } => {
                        state.status = if static_edge {
                            "固定页眉/页脚遮挡，请缩小选区".to_string()
                        } else {
                            "内容未能对齐，请继续滚动".to_string()
                        };
                    }
                }
                cx.notify();
                true
            }
        }
    }

    /// Captures one fresh frame of the long-capture region, preferring the
    /// persistent session and falling back to a full capture.
    fn long_frame(
        &self,
        cx: &mut Context<Self>,
        ax: i32,
        ay: i32,
        w: u32,
        h: u32,
    ) -> Option<Vec<u8>> {
        if let Some(live) = self.long.as_ref().and_then(|s| s.live.as_ref()) {
            if let Ok(Some((px, pw, ph, _))) = crate::app::capture_region_live(
                cx,
                &live.session,
                &live.gpu,
                live.sdr_white,
                live.mon_x,
                live.mon_y,
                ax,
                ay,
                w,
                h,
            ) {
                if pw == w && ph == h {
                    return Some(px);
                }
            }
        }
        match crate::app::capture_region_bgra(cx, ax, ay, w, h) {
            Ok((px, pw, ph, _)) if pw == w && ph == h => Some(px),
            _ => None,
        }
    }

    /// Direction toggle on the long-capture bar: flip vertical <-> horizontal
    /// and re-baseline from a fresh frame. Resetting the stitcher is what keeps
    /// the two axes from ever mixing their pixel rows. This is the only path
    /// to a horizontal session, and it stops auto-scroll: the user is taking
    /// over the wheel again.
    fn switch_long_axis(&mut self, cx: &mut Context<Self>) {
        let Some((ax, ay, w, h, next)) = self.long.as_ref().map(|s| {
            (
                s.abs_x,
                s.abs_y,
                s.w,
                s.h,
                axis_after_bar_click(s.axis, true),
            )
        }) else {
            return;
        };
        let Some(px) = self.long_frame(cx, ax, ay, w, h) else {
            return;
        };
        let (wu, hu) = (w as usize, h as usize);
        let mut stitcher = match OrbStitcher::new(next, wu, hu, LONG_MAX_H) {
            Ok(s) => s,
            Err(e) => {
                crate::perf::log_diag(&format!("long-capture axis switch: {e}"));
                if let Some(state) = self.long.as_mut() {
                    state.status = "图像超出内存预算".to_string();
                }
                cx.notify();
                return;
            }
        };
        if let Err(e) = stitcher.push(&px, wu, hu) {
            crate::perf::log_diag(&format!("long-capture axis switch: {e}"));
            if let Some(state) = self.long.as_mut() {
                state.status = "图像超出内存预算".to_string();
            }
            cx.notify();
            return;
        }
        // 新 stitcher 已推入首帧：增长维度从它读取（横向为选区宽 w，旋转后
        // 成为 snow 画布高），不能再沿用屏幕高 h —— 那会让 trim 与状态栏
        // 停在首屏。首帧到达后 apply_long_done 还会按同一维度再 reset 一次。
        let nh = stitcher.canvas_extent();
        if let Some(state) = self.long.as_mut() {
            // A fresh epoch invalidates the in-flight frame of the old axis and
            // hands the worker a new baseline, so the two axes can never mix
            // their pixel rows.
            let epoch = state.gate.epoch().wrapping_add(1);
            let generation = state.gen;
            state.worker.rebaseline(Rebase {
                generation,
                epoch,
                axis: next,
                stitcher,
            });
            state.gate.rebaseline(epoch);
            state.axis = next;
            state.maxed = false;
            state.stall_paused = false;
            state.auto_scroll.set_enabled(false);
            state.segments = 1;
            state.total_height = nh;
            state.trim = crate::longtrim::TrimRange::full(nh.max(1) as u32);
            state.status = long_status_hint(next);
        }
        crate::perf::log_diag(&format!("long-capture: axis switched to {}", next.label()));
        cx.notify();
    }

    /// 完成 on the bar: stitch whatever was captured, hand the result to the
    /// review window, then close the overlay.
    ///
    /// The worker owns the stitcher now, so 完成 is a terminal hand-off: it
    /// folds the headroom away, rotates a horizontal capture back to screen
    /// space and returns the pixels. That wait is bounded by one pass over the
    /// already-captured buffer and only happens when the user ends the session.
    ///
    /// The pixels go to `longpreview` rather than straight to the screenshot
    /// folder, so the user sees the stitch before it lands anywhere — snow-shot
    /// does the same with its `ImageStitchingWindow` and treats closing that
    /// window without saving as discarding the result.
    fn finish_long_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut state) = self.long.take() else {
            return;
        };
        set_long_passthrough(crate::platform::hwnd_of(window), None);
        self.phase = Phase::Active;
        // The session is over: the hover preview holds a pause that must never
        // outlive the canvas it was reading.
        state.hover.reset();
        match state.worker.finish(FINISH_WAIT) {
            Ok((px, w, h)) => {
                // The stitched image is the whole canvas; honour the trim
                // selection the user made on the thumbnail. A selection that no
                // longer covers exactly the canvas (a frame landed between the
                // last blit and 完成) still yields the intersection, and a
                // degenerate one falls back to the full image rather than
                // exporting nothing.
                let (px, w, h) = state
                    .trim
                    .crop_bgra_axis(&px, w, h, state.axis)
                    .unwrap_or((px, w, h));
                crate::longpreview::show_long_preview(cx, px, w, h);
            }
            Err(msg) => {
                // The stitched buffer was already within budget, so this is a
                // failed allocation or a stopped worker rather than a size
                // refusal; keep the reason visible instead of closing silently.
                crate::perf::log_app(&format!("long capture export: {msg}"));
                self.show_panel("长截图失败", msg, false, cx);
                return;
            }
        }
        self.finish(empty_result(), window, cx);
    }

    /// 取消 / Esc / right-click: drop the session, back to the editor.
    fn cancel_long_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mut state) = self.long.take() {
            state.hover.reset();
            state.worker.shutdown();
        } else {
            return;
        }
        set_long_passthrough(crate::platform::hwnd_of(window), None);
        self.phase = Phase::Active;
        cx.notify();
    }

    /// The GPU device was lost/reset under a running long capture. The WGC
    /// live session and every stitched GPU crop are stale, so end the session
    /// with a status the user can read instead of keeping the poll loop alive.
    fn abort_long_for_gpu_reset(&mut self, cx: &mut Context<Self>) {
        if self.long.take().is_none() {
            return;
        }
        // 捕获被中止会话的序号：下面的清洞任务被推迟执行，届时新会话可能
        // 已经调用 set_long_passthrough(Some(rect))。用序号把旧任务挡掉。
        let seq = self.long_seq;
        self.phase = Phase::Active;
        self.show_panel(
            "显示设备已重置",
            "长截图已停止，请重新开始".to_string(),
            false,
            cx,
        );
        // Clearing the click-through hole needs the window; do it once the
        // foreground task can borrow the overlay handle again.
        cx.spawn(async move |_this, cx| {
            cx.update(|cx| {
                if let Some(handle) = OVERLAY_WINDOW.with(|s| *s.borrow()) {
                    let _ = handle.update(cx, |view, window, _cx| {
                        if !should_clear_long_passthrough(seq, view.long_seq, view.long.is_some()) {
                            return;
                        }
                        set_long_passthrough(crate::platform::hwnd_of(window), None);
                    });
                }
            });
        })
        .detach();
    }

    fn run_ocr(&mut self, cx: &mut Context<Self>) {
        // Inference is ~130 ms; without this a double click queues two of them.
        if self.busy() {
            return;
        }
        let (px, w, h, stride) = match self.selection_pixels(cx) {
            Ok(pixels) => pixels,
            Err(e) => {
                self.show_panel("失败", e, false, cx);
                return;
            }
        };
        self.show_panel(OCR_TITLE, "识别中…".into(), true, cx);
        let task = cx.background_spawn(async move { crate::ocr::recognize(&px, w, h, stride) });
        cx.spawn(async move |this, cx| {
            let recognized = task.await;
            let _ = this.update(cx, |this, cx| match recognized {
                Ok(text) if text.trim().is_empty() => {
                    this.show_panel(OCR_TITLE, "未识别到文字".into(), false, cx)
                }
                Ok(text) => this.show_panel(OCR_TITLE, text, false, cx),
                Err(e) => this.show_panel("识别失败", e.to_string(), false, cx),
            });
        })
        .detach();
    }

    /// Recognizes first, like the C# button. It does not reuse the last OCR:
    /// inference is ~130 ms, and a cache keyed on nothing would translate the
    /// previous selection after the user reframes.
    fn run_translate(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let (px, w, h, stride) = match self.selection_pixels(cx) {
            Ok(pixels) => pixels,
            Err(e) => {
                self.show_panel("失败", e, false, cx);
                return;
            }
        };
        self.show_panel(TRANSLATE_TITLE, "翻译中…".into(), true, cx);
        let task = cx.background_spawn(async move {
            let source = crate::ocr::recognize(&px, w, h, stride)?;
            if source.trim().is_empty() {
                return Err(crate::error::Error::new("未识别到文字"));
            }
            crate::translate::translate(&source)
        });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |this, cx| match outcome {
                Ok(translated) => this.show_panel(TRANSLATE_TITLE, translated, false, cx),
                Err(e) => this.show_panel("翻译失败", e.to_string(), false, cx),
            });
        })
        .detach();
    }

    /// Pins the annotated crop to the screen and ends this capture: the pin
    /// window holds the pixels, so no file or clipboard export runs.
    fn pin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some(result) = self.result_from_current() else {
            self.show_panel("失败", "没有可贴图的选区".into(), false, cx);
            return;
        };
        let (px, w, h, stride) = match self.selection_pixels(cx) {
            Ok(pixels) => pixels,
            Err(e) => {
                self.show_panel("失败", e, false, cx);
                return;
            }
        };
        let screen_x = self.origin_x + result.x as i32;
        let screen_y = self.origin_y + result.y as i32;
        if let Err(e) = crate::pin::show_pin(cx, px, w, h, stride, screen_x, screen_y) {
            // 贴图失败时保留截图界面，让用户能重试；关窗会连失败原因一起藏起来。
            self.show_panel("失败", format!("贴图失败：{e}"), false, cx);
            return;
        }
        self.finish(empty_result(), window, cx);
    }

    fn copy_panel(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.panel.as_ref() else {
            return;
        };
        if panel.busy {
            return;
        }
        // 先克隆出正文再放手 self.panel 的借用，下面失败分支要改面板本身。
        let body = panel.body.to_string();
        if let Err(e) = crate::codec::set_clipboard_text(&body) {
            crate::perf::log_app(&format!("clipboard text failed: {e}"));
            self.show_panel("失败", format!("复制到剪贴板失败：{e}"), false, cx);
            return;
        }
        self.show_panel(COPIED_TITLE, body, false, cx);
    }

    /// Pixelated tiles plus the ids of the elements that produced them; an element
    /// missing from that list still needs a placeholder drawn for it.
    /// Pixelated tiles plus the ids of the elements that produced them; an element
    /// missing from that list still needs a placeholder drawn for it.
    fn mosaic_tiles(&self, marks: &[Element], cx: &mut App) -> Rc<MosaicTiles> {
        let Some(cap) = self.cap.as_ref() else {
            return Rc::new(MosaicTiles::default());
        };
        annotate::mosaic_tiles(
            &self.mosaic_cache,
            marks,
            (cap.width, cap.height),
            |x, y, w, h| {
                cx.with_gpu_context(|_d, ctx| crop_sdr_bgra(cap, ctx, x, y, w, h))?
                    .ok()
            },
        )
    }

    fn refresh_magnifier_gpu(&mut self, cx: &mut App) {
        let Some(cap) = self.cap.as_ref() else {
            self.mag = None;
            return;
        };
        let half = (MAG_COUNT / 2) as i32;
        let max_x = cap.width.saturating_sub(MAG_COUNT) as i32;
        let max_y = cap.height.saturating_sub(MAG_COUNT) as i32;
        let x = (self.mouse.x.floor() as i32 - half).clamp(0, max_x) as u32;
        let y = (self.mouse.y.floor() as i32 - half).clamp(0, max_y) as u32;
        // A staging texture + CopySubresourceRegion + Map is a GPU sync; skip it
        // when the cursor has not moved to a new source pixel since the last
        // readback (high-polling mice fire hundreds of moves per painted frame).
        if self.mag_origin == Some((x, y)) && self.mag.is_some() {
            return;
        }
        self.mag = cx
            .with_gpu_context(|_device, ctx| {
                crate::capture::crop_sdr_bgra(cap, ctx, x, y, MAG_COUNT, MAG_COUNT)
            })
            .and_then(|r| r.ok());
        self.mag_origin = if self.mag.is_some() {
            Some((x, y))
        } else {
            None
        };
        // The magnifier is a MAG_COUNT×MAG_COUNT crop centred on the cursor, so
        // its middle pixel is exactly the colour under the crosshair.
        self.picked_color = self.mag.as_ref().and_then(|(buf, w, h, stride)| {
            let half = MAG_COUNT / 2;
            crate::colorpicker::Color::from_bgra(buf, half, half, *w, *h, *stride)
        });
    }

    /// `C`: copy the colour under the cursor in the current format. Mirrors
    /// snow-shot's `copyColorToClipboard` — it flushes the picker first so the
    /// text always matches the pixel currently under the crosshair.
    fn copy_picked_color(
        &mut self,
        _: &CopyPickedColor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Never steal keys from a text draft — the IME already got this `c`.
        if self.text_edit.is_some() {
            return;
        }
        self.refresh_magnifier_gpu(cx);
        let Some(color) = self.picked_color else {
            return;
        };
        let text = crate::colorpicker::format_color(color, self.color_format);
        match crate::codec::set_clipboard_text(&text) {
            Ok(()) => {
                self.last_color_notice = Some(format!("已复制颜色 {text}").into());
            }
            Err(_) => {
                self.last_color_notice = Some("复制颜色失败".into());
            }
        }
        cx.notify();
    }

    /// `F`: cycle Hex → HexWithoutHash → Rgb → Hsl, matching snow-shot's
    /// `ScreenshotColorPickerWindow::cycleColorFormat`.
    fn cycle_picked_color_format(
        &mut self,
        _: &CycleColorFormat,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.text_edit.is_some() {
            return;
        }
        self.color_format = self.color_format.next();
        if let Some(color) = self.picked_color {
            self.last_color_notice = Some(
                format!(
                    "颜色格式 {}",
                    crate::colorpicker::format_color(color, self.color_format)
                )
                .into(),
            );
        }
        cx.notify();
    }

    fn toolbar_element(
        &self,
        scale: f32,
        cx: &mut Context<Self>,
        items: &[&'static str],
        geom: Option<Rect>,
    ) -> Option<impl IntoElement> {
        let geom = geom?;
        let model = ToolbarModel {
            origin: (geom.x as f32 / scale, geom.y as f32 / scale),
            items: items.to_vec(),
            tool_id: self.editor.tool.id(),
            can_undo: self.editor.history.can_undo(),
            can_redo: self.editor.history.can_redo(),
            color_open: self.color_open,
            stroke: self.editor.stroke_argb(),
            line_style: self.editor.line_style,
            shape_open: self.editor.shape_open,
        };
        Some(annotate::toolbar_element(
            model,
            Rc::new(|id, this: &mut Self, window, cx| this.handle_toolbar(id, window, cx)),
            cx,
        ))
    }

    /// The second row. It hangs off the first row on the side away from the
    /// selection -- the same rule `place_toolbar` used to pick the bar's side
    /// -- and is clamped to the monitor like every other popup. Empty for the
    /// select tool, which draws nothing.
    fn props_element(
        &self,
        scale: f32,
        cx: &mut Context<Self>,
        geom: Option<Rect>,
        mon: Rect,
    ) -> Option<impl IntoElement> {
        let (_cells, row, _below) = self.props_placement(scale, geom, mon)?;
        Some(annotate::props_element(
            annotate::PropsModel {
                origin: (row.x as f32, row.y as f32),
                tool: self.editor.tool,
                stroke_enabled: self.editor.stroke_enabled,
                fill_enabled: self.editor.fill_enabled,
                stroke_width: self.editor.stroke_width,
                line_style: self.editor.line_style,
                arrow_ends: self.editor.arrow_ends,
                arrow_head: self.editor.arrow_head,
                badge_style: self.editor.badge_style,
                badge_size: self.editor.badge_size,
                spotlight_kind: self.editor.spotlight_kind,
            },
            Rc::new(|id, this: &mut Self, window, cx| this.handle_toolbar(id, window, cx)),
            Rc::new(|f, this: &mut Self, _, cx| {
                let width = toolbar::width_for_fraction(f);
                this.editor.set_stroke_width(width);
                cx.notify();
            }),
            Rc::new(|f, this: &mut Self, _, cx| {
                let radius = toolbar::badge_radius_for_fraction(f);
                this.editor.set_badge_size(radius);
                cx.notify();
            }),
            cx,
        ))
    }

    /// Logical second-row rect plus the side it hangs on.
    fn props_placement(
        &self,
        scale: f32,
        geom: Option<Rect>,
        mon: Rect,
    ) -> Option<(Vec<toolbar::PropCell>, Rect, bool)> {
        let cells = annotate::prop_cells(self.editor.tool);
        if cells.is_empty() {
            return None;
        }
        let geom = geom?;
        let s = scale.max(0.01) as f64;
        // toolbar_geom is physical; the element tree is logical.
        let bar = Rect::new(geom.x / s, geom.y / s, geom.width / s, geom.height / s);
        let mon = Rect::new(mon.x / s, mon.y / s, mon.width / s, mon.height / s);
        let (w, h) = toolbar::props_size(&cells);
        let below = bar.y + bar.height / 2.0 >= self.selection.y + self.selection.height / 2.0;
        let pos = toolbar::place_second_row(bar, w, h, mon, below, toolbar::ANCHOR_GAP);
        Some((cells, Rect::new(pos.x, pos.y, w, h), below))
    }

    /// The colour popup. Only `visible_items` knows where the COLOR button ended
    /// up once the user hides tools, so the index comes from the same list the
    /// bar is drawn and hit-tested from.
    fn swatches_element(
        &self,
        scale: f32,
        cx: &mut Context<Self>,
        items: &[&'static str],
        geom: Option<Rect>,
        mon: Rect,
    ) -> Option<impl IntoElement> {
        if !self.color_open {
            return None;
        }
        let index = items.iter().position(|id| *id == toolbar::COLOR)?;
        // toolbar_geom is physical; the element tree is logical.
        let s = scale.max(0.01) as f64;
        let bar = geom?;
        let bar = Rect::new(bar.x / s, bar.y / s, bar.width / s, bar.height / s);
        let mon = Rect::new(mon.x / s, mon.y / s, mon.width / s, mon.height / s);
        let (w, h) = toolbar::swatches_size(theme::SWATCHES.len());
        let pos = toolbar::place_swatches(
            toolbar::item_center_x(bar, index),
            bar,
            w,
            h,
            mon,
            toolbar::ANCHOR_GAP,
        );
        Some(annotate::swatches_element(
            SwatchesModel {
                origin: (pos.x as f32, pos.y as f32),
                current: self.editor.stroke,
                alpha: self.editor.alpha,
            },
            Rc::new(|argb, this: &mut Self, _, cx| {
                this.editor.stroke = argb;
                this.color_open = false;
                cx.notify();
            }),
            Rc::new(|f, this: &mut Self, _, cx| {
                this.editor.set_alpha(toolbar::alpha_for_fraction(f) as f64);
                cx.notify();
            }),
            cx,
        ))
    }

    /// The rect / ellipse picker, centred on the shape button and placed like
    /// the colour popup. Its icon sub-buttons route through `handle_toolbar`
    /// exactly like bar items.
    fn shape_popup_element(
        &self,
        scale: f32,
        cx: &mut Context<Self>,
        items: &[&'static str],
        geom: Option<Rect>,
        mon: Rect,
    ) -> Option<impl IntoElement> {
        if !self.editor.shape_open {
            return None;
        }
        let index = items.iter().position(|id| *id == toolbar::SHAPE)?;
        // toolbar_geom is physical; the element tree is logical.
        let s = scale.max(0.01) as f64;
        let bar = geom?;
        let bar = Rect::new(bar.x / s, bar.y / s, bar.width / s, bar.height / s);
        let mon = Rect::new(mon.x / s, mon.y / s, mon.width / s, mon.height / s);
        let (w, h) = toolbar::bar_size(2);
        let pos = toolbar::place_swatches(
            toolbar::item_center_x(bar, index),
            bar,
            w,
            h,
            mon,
            toolbar::ANCHOR_GAP,
        );
        Some(annotate::shape_popup_element(
            annotate::ShapePopupModel {
                origin: (pos.x as f32, pos.y as f32),
                current: if self.editor.tool == Tool::Ellipse {
                    Tool::Ellipse
                } else {
                    Tool::Rect
                },
            },
            Rc::new(|id, this: &mut Self, window, cx| this.handle_toolbar(id, window, cx)),
            cx,
        ))
    }

    /// The bar that replaces the toolbar mid-capture (the C# LongCaptureBar):
    /// status text plus 完成/取消 chips, placed where the toolbar would sit.
    /// It must stay outside the selection -- that area is click-through.
    /// Where the long-capture bar lands, in capture coordinates: top-left plus
    /// height. `est_w` only shifts x, so the passthrough hole can ask for the
    /// band with a zero width and still get the y the bar will be drawn at.
    fn long_bar_placement(&self, scale: f32, est_w: f64, mon: Rect) -> (Point, f64) {
        let s = scale.max(0.01) as f64;
        let h = toolbar::H * s;
        let remembered = self
            .toolbar_remember_pos
            .map(|(x, y)| (x as f64 * s, y as f64 * s));
        let pos = toolbar::place_toolbar_with_policy(
            self.selection,
            est_w,
            h,
            mon,
            toolbar::ANCHOR_GAP * s,
            self.toolbar_position,
            remembered,
        );
        (pos, h)
    }

    fn long_bar_element(
        &self,
        scale: f32,
        cx: &mut Context<Self>,
        mon: Rect,
    ) -> Option<impl IntoElement> {
        if self.phase != Phase::LongCapturing {
            return None;
        }
        let state = self.long.as_ref()?;
        let s = scale.max(0.01) as f64;
        // place_toolbar centers on the selection and clamps into the monitor;
        // the width estimate only affects centering -- the bar sizes to
        // content once laid out.
        let est_w = (state.status.chars().count() as f64 * 13.0 + 4.0 * 88.0 + 72.0) * s;
        let (pos, _) = self.long_bar_placement(scale, est_w, mon);
        let status: SharedString = state.status.clone().into();
        let axis_label: SharedString = match state.axis {
            Axis::Vertical => "↕ 垂直".into(),
            Axis::Horizontal => "↔ 水平".into(),
        };
        let auto_on = state.auto_scroll.enabled();
        let auto_label: SharedString = if auto_on {
            "自动滚动：开".into()
        } else {
            "自动滚动：关".into()
        };

        // Live preview: a downsampled snapshot of the growing canvas, sent from
        // the worker on each progress frame. It is cached as a RenderImage on
        // the session, so drawing costs nothing. Kept small so the capture bar
        // stays unobtrusive; the full image is shown in the preview window on
        // 完成.
        //
        // The thumbnail doubles as the trim editor (the snow-shot scrolling
        // thumbnail widget): the rows outside the selection are dimmed, and two
        // handles drag the top and bottom edges. GPUI does not hand a mouse
        // handler its element's bounds, so the handlers work in thumbnail
        // coordinates obtained from the pointer minus the position the bar was
        // laid out at -- the same placement the element uses.
        let preview_el: gpui::AnyElement = if let Some((image, pw, ph)) = &state.preview {
            const MAX_W: f32 = 240.0;
            const MAX_H: f32 = 170.0;
            let pw = *pw;
            let ph = *ph;
            let k = (MAX_W / pw.max(1) as f32)
                .min(MAX_H / ph.max(1) as f32)
                .min(1.0);
            let dw = (pw.max(1) as f32 * k).max(1.0);
            let dh = (ph.max(1) as f32 * k).max(1.0);
            // Thumbnail geometry, in the same logical pixels GPUI reports mouse
            // positions in. The bar is placed at `pos` (capture coordinates,
            // divided by the scale factor to get logical pixels) and pads its
            // content by 12px horizontally and 6px vertically.
            const BAR_PAD_X: f32 = 12.0;
            const BAR_PAD_Y: f32 = 6.0;
            let bar_left = pos.x as f32 / scale;
            let thumb_top = pos.y as f32 / scale + BAR_PAD_Y;
            // 横向把缩略图靠左放置，左边缘因此是确定的，命中测试才能用 x。
            // 纵向维持居中，只需要 top。
            let horizontal = state.axis == Axis::Horizontal;
            let thumb_left = bar_left + BAR_PAD_X;
            // stitch 轴纵向是缩略图的高，横向是宽。total_height 在横向已经是
            // 增长长边 L，必须映射到 dw；若映射到 dh（常量屏幕高），[0,L] 会
            // 被压成顶部一条细缝，手柄既画不出也点不中。
            let thumb_extent = if horizontal { dw } else { dh };
            let canvas = state.total_height.max(1) as u32;
            let trim = state.trim;
            let trim_scale = thumb_extent / canvas as f32;
            let near = trim.top as f32 * trim_scale;
            let far = trim.bottom as f32 * trim_scale;
            div()
                .flex()
                .when(!horizontal, |el| el.justify_center())
                .when(horizontal, |el| el.justify_start())
                .w_full()
                .mb(px(6.))
                .child(
                    div()
                        .id("long-thumb")
                        .relative()
                        .rounded_md()
                        .border_1()
                        .border_color(theme::border())
                        .overflow_hidden()
                        .cursor(if state.trim_drag.is_some() {
                            CursorStyle::ClosedHand
                        } else {
                            CursorStyle::Arrow
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                let pointer = f32::from(if horizontal {
                                    event.position.x
                                } else {
                                    event.position.y
                                }) / window.scale_factor();
                                let along =
                                    pointer - (if horizontal { thumb_left } else { thumb_top });
                                let Some(state) = this.long.as_mut() else {
                                    return;
                                };
                                let canvas = state.total_height.max(1) as u32;
                                let hit = if horizontal {
                                    crate::longtrim::hit_test_x(
                                        along,
                                        thumb_extent,
                                        state.trim,
                                        canvas,
                                    )
                                } else {
                                    crate::longtrim::hit_test(
                                        along,
                                        thumb_extent,
                                        state.trim,
                                        canvas,
                                    )
                                };
                                match hit {
                                    crate::longtrim::TrimHit::None => {}
                                    hit => {
                                        cx.stop_propagation();
                                        state.trim_drag = Some(TrimDrag {
                                            axis: state.axis,
                                            edge: hit,
                                            start_range: state.trim,
                                            start_ptr: pointer,
                                            thumb_extent,
                                        });
                                        // A trim drag and the hover preview want
                                        // the same pointer: cropping wins, so
                                        // the capture resumes while dragging.
                                        state.hover.set_hover(None);
                                        state.hover_image = None;
                                        state.hover_rows = None;
                                        cx.notify();
                                    }
                                }
                            }),
                        )
                        // Hover preview: the pointer picks a band of canvas
                        // rows and the capture pauses while it is rendered at
                        // full resolution. Tracking only while no trim drag is
                        // active keeps one pointer from doing two jobs.
                        .on_mouse_move(cx.listener(
                            move |this, event: &MouseMoveEvent, window, cx| {
                                let Some(state) = this.long.as_mut() else {
                                    return;
                                };
                                if state.trim_drag.is_some() {
                                    return;
                                }
                                let pointer = f32::from(if horizontal {
                                    event.position.x
                                } else {
                                    event.position.y
                                }) / window.scale_factor();
                                let canvas = state.total_height.max(1) as u32;
                                // One viewport-tall band, along the stitch axis:
                                // the height of a vertical capture, the width of a
                                // horizontal one.
                                let viewport = match state.axis {
                                    Axis::Vertical => state.h,
                                    Axis::Horizontal => state.w,
                                };
                                let rows = hover_rows_at(
                                    pointer - (if horizontal { thumb_left } else { thumb_top }),
                                    thumb_extent,
                                    canvas,
                                    viewport,
                                );
                                if state.hover.hover() != rows {
                                    state.hover.set_cropping(false);
                                    state.hover.set_hover(rows);
                                    cx.notify();
                                }
                            },
                        ))
                        .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                            if *hovered {
                                return;
                            }
                            let Some(state) = this.long.as_mut() else {
                                return;
                            };
                            state.hover.set_hover(None);
                            state.hover_image = None;
                            state.hover_rows = None;
                            cx.notify();
                        }))
                        .child(img(ImageSource::Render(image.clone())).w(px(dw)).h(px(dh)))
                        // Dim the rows / columns the selection excludes: two
                        // translucent bands on either side of the range, laid
                        // out along the stitch axis.
                        .when(near > 0.5, |el| {
                            let band = div().absolute().bg(theme::base().opacity(0.55));
                            let band = if horizontal {
                                band.left(px(0.)).top(px(0.)).w(px(near)).h(px(dh))
                            } else {
                                band.left(px(0.)).top(px(0.)).w(px(dw)).h(px(near))
                            };
                            el.child(band)
                        })
                        .when(far < thumb_extent - 0.5, |el| {
                            let band = div().absolute().bg(theme::base().opacity(0.55));
                            let band = if horizontal {
                                band.left(px(far))
                                    .top(px(0.))
                                    .w(px((thumb_extent - far).max(0.)))
                                    .h(px(dh))
                            } else {
                                band.left(px(0.))
                                    .top(px(far))
                                    .w(px(dw))
                                    .h(px((thumb_extent - far).max(0.)))
                            };
                            el.child(band)
                        })
                        // The two edges of the kept range, drawn as thin accent
                        // rules so the boundary is visible without a handle
                        // glyph on so small a thumbnail: horizontal rules for a
                        // vertical capture, vertical ones for a horizontal one.
                        .child({
                            let edge = div().absolute().bg(theme::accent());
                            if horizontal {
                                edge.left(px(near)).top(px(0.)).w(px(1.)).h(px(dh))
                            } else {
                                edge.left(px(0.)).top(px(near)).w(px(dw)).h(px(1.))
                            }
                        })
                        .child({
                            let edge = div().absolute().bg(theme::accent());
                            if horizontal {
                                edge.left(px((far - 1.).max(0.)))
                                    .top(px(0.))
                                    .w(px(1.))
                                    .h(px(dh))
                            } else {
                                edge.left(px(0.))
                                    .top(px((far - 1.).max(0.)))
                                    .w(px(dw))
                                    .h(px(1.))
                            }
                        })
                        // Hover band: the canvas interval the pointer picked,
                        // outlined along the same stitch axis on the thumbnail.
                        .when_some(state.hover_rows, |el, (h_top, h_bottom)| {
                            let h_near = h_top as f32 * trim_scale;
                            let h_len = ((h_bottom - h_top) as f32 * trim_scale).max(1.0);
                            let band = div()
                                .absolute()
                                .border_1()
                                .border_color(theme::primary_fg());
                            let band = if horizontal {
                                band.left(px(h_near)).top(px(0.)).w(px(h_len)).h(px(dh))
                            } else {
                                band.left(px(0.)).top(px(h_near)).w(px(dw)).h(px(h_len))
                            };
                            el.child(band)
                        }),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };

        // Hover preview: the band under the pointer, rendered from the canvas
        // at capture resolution while the capture is held. It sits under the
        // thumbnail rather than over it so neither occludes the other, and it
        // is sized to the same width the thumbnail is capped at.
        let hover_el: gpui::AnyElement = match (&state.hover_image, state.hover_rows) {
            (Some((image, hw, hh)), Some((top, bottom))) => {
                const MAX_W: f32 = 240.0;
                const MAX_H: f32 = 120.0;
                let k = (MAX_W / (*hw).max(1) as f32)
                    .min(MAX_H / (*hh).max(1) as f32)
                    .min(1.0);
                let dw = ((*hw).max(1) as f32 * k).max(1.0);
                let dh = ((*hh).max(1) as f32 * k).max(1.0);
                let label: SharedString = format!("第 {top}–{bottom} 行（采集已暂停）").into();
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(2.))
                    .w_full()
                    .mb(px(6.))
                    .child(
                        div()
                            .relative()
                            .rounded_md()
                            .border_1()
                            .border_color(theme::primary_fg())
                            .overflow_hidden()
                            .child(img(ImageSource::Render(image.clone())).w(px(dw)).h(px(dh))),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme::secondary_fg())
                            .child(label),
                    )
                    .into_any_element()
            }
            _ => div().into_any_element(),
        };

        Some(
            div()
                .absolute()
                .left(px(pos.x as f32 / scale))
                .top(px(pos.y as f32 / scale))
                .flex()
                .flex_col()
                .items_center()
                .px(px(12.))
                .py(px(6.))
                .rounded_xl()
                .bg(theme::surface())
                .border_1()
                .border_color(theme::border())
                .shadow(vec![BoxShadow {
                    color: theme::base().opacity(0.12),
                    offset: point(px(0.), px(4.)),
                    blur_radius: px(16.),
                    spread_radius: px(0.),
                    inset: false,
                }])
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(preview_el)
                .child(hover_el)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        // Direction toggle: wide pages are captured left/right.
                        // Toggling resets the session so the two axes never mix.
                        .child(
                            div()
                                .id("long-axis")
                                .px(px(10.))
                                .py(px(5.))
                                .rounded_md()
                                .text_size(px(12.))
                                .text_color(theme::primary_fg())
                                .bg(theme::active_background())
                                .hover(|s| s.bg(theme::elevated_surface()))
                                .cursor(CursorStyle::PointingHand)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _window, cx| {
                                        cx.stop_propagation();
                                        this.switch_long_axis(cx);
                                    }),
                                )
                                .child(axis_label),
                        )
                        // Auto-scroll switch: opt-in, session-only, and
                        // independent of the axis and the buffer. Off is the
                        // manual model exactly as it was.
                        .child(
                            div()
                                .id("long-auto")
                                .px(px(10.))
                                .py(px(5.))
                                .rounded_md()
                                .text_size(px(12.))
                                .text_color(if auto_on {
                                    theme::on_accent()
                                } else {
                                    theme::primary_fg()
                                })
                                .bg(if auto_on {
                                    theme::accent()
                                } else {
                                    theme::active_background()
                                })
                                .hover(|s| s.bg(theme::elevated_surface()))
                                .cursor(CursorStyle::PointingHand)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _window, cx| {
                                        cx.stop_propagation();
                                        this.toggle_auto_scroll(cx);
                                    }),
                                )
                                .child(auto_label),
                        )
                        // Single-step nudge: posts one configured step on
                        // demand, whether auto-scroll is running or not. The
                        // snow-shot `scrollOnce` equivalent, handy to push
                        // past a stall or to step a page by hand.
                        .child(
                            div()
                                .id("long-step")
                                .px(px(10.))
                                .py(px(5.))
                                .rounded_md()
                                .text_size(px(12.))
                                .text_color(theme::primary_fg())
                                .bg(theme::active_background())
                                .hover(|s| s.bg(theme::elevated_surface()))
                                .cursor(CursorStyle::PointingHand)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _window, cx| {
                                        cx.stop_propagation();
                                        this.step_scroll_once(cx);
                                    }),
                                )
                                .child("单步"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::primary_fg())
                                .child(status),
                        )
                        .child(
                            div()
                                .id("long-finish")
                                .px(px(10.))
                                .py(px(5.))
                                .rounded_md()
                                .text_size(px(12.))
                                .text_color(theme::on_accent())
                                .bg(theme::accent())
                                .hover(|s| s.bg(theme::accent_hover()))
                                .cursor(CursorStyle::PointingHand)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.finish_long_capture(window, cx);
                                    }),
                                )
                                .child("完成"),
                        )
                        .child(
                            div()
                                .id("long-cancel")
                                .px(px(10.))
                                .py(px(5.))
                                .rounded_md()
                                .text_size(px(12.))
                                .text_color(theme::primary_fg())
                                .bg(theme::active_background())
                                .hover(|s| s.bg(theme::elevated_surface()))
                                .cursor(CursorStyle::PointingHand)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.cancel_long_capture(window, cx);
                                    }),
                                )
                                .child("取消"),
                        ),
                ),
        )
    }

    fn panel_element(
        &self,
        scale: f32,
        cx: &mut Context<Self>,
        geom: Option<Rect>,
        mon: Rect,
    ) -> Option<impl IntoElement> {
        let panel = self.panel.as_ref()?;
        let bar = geom?;
        let copyable = !panel.busy && !panel.body.is_empty();
        // toolbar_geom and active_monitor_rect are in physical pixels; the
        // element tree is logical, so convert before placing.
        let s = scale.max(0.01) as f64;
        let bar = Rect::new(bar.x / s, bar.y / s, bar.width / s, bar.height / s);
        let sel = self.selection;
        let sel = Rect::new(sel.x / s, sel.y / s, sel.width / s, sel.height / s);
        let mon = Rect::new(mon.x / s, mon.y / s, mon.width / s, mon.height / s);
        let chrome = (PANEL_CHROME + if copyable { PANEL_COPY_H } else { 0. }) as f64;
        let place = toolbar::place_panel_with_policy(
            bar,
            sel,
            PANEL_W as f64,
            chrome,
            (PANEL_MIN_BODY_H as f64, PANEL_MAX_H as f64),
            mon,
            toolbar::ANCHOR_GAP,
            PANEL_MARGIN as f64,
            self.panel_position,
            self.panel_remember_pos.map(|(x, y)| (x as f64, y as f64)),
        );
        let el = div()
            .id("overlay-panel")
            .absolute()
            .left(px(place.x as f32))
            .w(px(PANEL_W));
        // AboveBar bottom-anchors because the content height is unknown before
        // layout: a top edge cannot be precomputed, while the bottom edge hugs
        // the toolbar regardless of how tall the body is.
        let el = match place.y {
            toolbar::PanelY::BelowBar => {
                el.top(px((bar.y + bar.height + toolbar::ANCHOR_GAP) as f32))
            }
            toolbar::PanelY::AboveBar => {
                let parent_h = self.freeze_size.1 as f64 / s;
                el.bottom(px((parent_h - bar.y + toolbar::ANCHOR_GAP) as f32))
            }
            toolbar::PanelY::Top(y) => el.top(px(y as f32)),
        };
        Some(
            el.flex()
                .flex_col()
                .gap(px(6.))
                .p(px(10.))
                .rounded_xl()
                .bg(theme::surface())
                .border_1()
                .border_color(theme::border())
                .shadow(vec![BoxShadow {
                    color: theme::base().opacity(0.12),
                    offset: point(px(0.), px(4.)),
                    blur_radius: px(16.),
                    spread_radius: px(0.),
                    inset: false,
                }])
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::secondary_fg())
                        .child(panel.title.clone()),
                )
                .child(
                    div()
                        .id("overlay-panel-body")
                        .max_h(px(place.body_max_h as f32))
                        .overflow_y_scroll()
                        .text_size(px(13.))
                        .text_color(theme::primary_fg())
                        .child(panel.body.clone()),
                )
                .children(copyable.then(|| {
                    div().flex().flex_row().justify_end().child(
                        div()
                            .id("overlay-panel-copy")
                            .px(px(10.))
                            .py(px(4.))
                            .rounded_md()
                            .text_size(px(12.))
                            .text_color(theme::primary_fg())
                            .bg(theme::active_background())
                            .hover(|s| s.bg(theme::accent_hover()))
                            .cursor(CursorStyle::PointingHand)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.copy_panel(cx);
                                }),
                            )
                            .child("复制"),
                    )
                })),
        )
    }

    /// Text annotations live in the element tree, not the canvas, so the paint-time
    /// content mask does not reach them. Same clip, done with a container: positioned
    /// on the selection, children placed relative to it, overflow hidden.
    fn text_layer(&self, scale: f32, marks: &[Element]) -> Option<impl IntoElement> {
        if !self.has_sel {
            return None;
        }
        let sel = self.selection;
        let s = scale as f64;
        // `marks` is `editor.visible()` (committed elements + the held draft),
        // cloned once in `render` and shared with the canvas paint. Both layers
        // filter by shape, so the draft is already folded into that one clone:
        // no second clone and no temporary Vec, even while a badge is held.
        let frame = Rect::new(sel.x / s, sel.y / s, sel.width / s, sel.height / s);
        let offset = Point { x: sel.x, y: sel.y };
        // Badge and text layers position themselves with absolute window-local
        // coordinates, so their container must be an absolute full-window box.
        // A plain flow div sits below the full-size canvas and pushes every
        // bubble and caret off-screen -- which is why badges looked as though
        // they were never drawn even though the element was in the document.
        // Anchor the origin explicitly: GPUI/taffy positions an inset-auto
        // abspos box at its static flow slot (or the flex start, depending on
        // the container), neither of which is guaranteed to be (0,0). Pinning
        // left/top=0 makes the full-window box unambiguous on the real tree.
        Some(
            div()
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                .children([
                    annotate::badge_layer(frame, offset, scale, marks).into_any_element(),
                    annotate::text_layer(TextLayer {
                        frame,
                        offset,
                        scale,
                        elements: marks,
                        draft: self.text_edit.as_ref(),
                        stroke: self.editor.stroke_argb(),
                    })
                    .into_any_element(),
                ]),
        )
    }
}

impl Focusable for OverlayView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Text input for the draft: the caret is always at the end, so this is the
/// minimum the platform needs -- append committed text, keep the IME's
/// composition string marked so the next update replaces it, and tell the IME
/// where the draft sits so the candidate window opens next to it.
impl EntityInputHandler for OverlayView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        Some(
            self.text_edit
                .as_ref()?
                .text_for_range(range_utf16, actual_range),
        )
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(self.text_edit.as_ref()?.selected_range())
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.text_edit.as_ref()?.marked_range()
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        if let Some(edit) = self.text_edit.as_mut() {
            edit.unmark();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(edit) = self.text_edit.as_mut() {
            edit.replace(range_utf16, new_text);
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(edit) = self.text_edit.as_mut() {
            edit.replace_and_mark(range_utf16, new_text);
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let edit = self.text_edit.as_ref()?;
        let scale = window.scale_factor().max(0.01);
        Some(edit.ime_bounds(element_bounds, Point { x: 0.0, y: 0.0 }, scale))
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

impl Render for OverlayView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.maybe_recover_gpu(cx);
        // UIA 查询在专用 STA 线程上跑；这里只非阻塞取回结果，绝不在渲染帧里
        // 等待跨进程 COM（提供程序无响应时曾把截图窗口整条 UI 线程卡死）。
        if self.visible && self.phase == Phase::Selecting {
            self.poll_uia_result(window);
        }
        if self.visible {
            window.request_animation_frame();
        }
        // One magnifier readback per painted frame, not one per WM_MOUSEMOVE:
        // `refresh_magnifier_gpu` also short-circuits when the cursor has not
        // moved to a new source pixel since the last readback.
        // 放大镜关闭时连 GPU 回读一起省掉，不只是不画。
        if self.visible && self.phase == Phase::Selecting && self.show_magnifier {
            self.refresh_magnifier_gpu(cx);
        }
        let scale = window.scale_factor().max(0.01);
        // One pass per frame for the values every bar/popup placement reuses.
        let visible_items = self.visible_items();
        let toolbar_geom = self.toolbar_geom(scale);
        let active_monitor = self.active_monitor_rect();
        let shown = self.current_rect();
        let mouse = self.mouse;
        let mag = self.mag.clone();
        let phase = self.phase;
        let start = self.start;
        let has_sel = self.has_sel;
        // The one deep clone per frame: committed elements + held draft, shared
        // by the canvas paint and the text/badge layers via `Rc`.
        let marks: Rc<Vec<Element>> = Rc::new(self.editor.visible().cloned().collect());
        let masks = self.mark_masks(&marks);
        let dim = shown.and_then(|r| self.spotlight_dim_runs(r, &marks));
        let mosaic = self.mosaic_tiles(&marks, cx);
        let tool = self.editor.tool;
        // Registered from the canvas paint so the IME talks to this view while
        // a text draft is open; the handler is a no-op otherwise.
        let input = self
            .text_edit
            .is_some()
            .then(|| (self.focus_handle.clone(), cx.entity()));
        let cursor = match (phase, tool, self.text_edit.is_some()) {
            (_, _, true) => CursorStyle::IBeam,
            (Phase::Selecting, _, _) => CursorStyle::Crosshair,
            (Phase::Active, Tool::Select, _) => CursorStyle::Arrow,
            (Phase::Active, Tool::Text, _) => CursorStyle::IBeam,
            _ => CursorStyle::Crosshair,
        };
        let candidates = self.candidates.clone();
        let picked_color = self.picked_color;
        let color_format = self.color_format;
        let color_notice = self.last_color_notice.clone();
        let show_magnifier = self.show_magnifier;
        let show_guides = self.show_guides;
        let show_info_bar = self.show_info_bar;
        let show_shortcut_hints = self.show_shortcut_hints;
        // The canvas paint closure is `move`: give it its own `Rc` handle so the
        // original stays available for `text_layer` later in this same frame.
        let paint_marks = marks.clone();

        div()
            .id("overlay-root")
            .size_full()
            .key_context("Overlay")
            .track_focus(&self.focus_handle)
            .cursor(cursor)
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::confirm_action))
            .on_action(cx.listener(Self::cancel_action))
            .on_action(cx.listener(Self::undo_action))
            .on_action(cx.listener(Self::redo_action))
            .on_action(cx.listener(Self::copy_picked_color))
            .on_action(cx.listener(Self::cycle_picked_color_format))
            // Everything visible hangs off `visible`, so the frame drawn on the way
            // out is empty. gpui's layer is a separate swap chain from the freeze
            // frame and keeps its last frame while the window is hidden; leaving the
            // finished overlay in it means the next capture shows those pixels until
            // the first repaint lands.
            .when(self.visible, |el| {
                el.child(
                    canvas(
                        move |_, _, _| (),
                        move |bounds, _, window, cx| {
                            if let Some((focus, entity)) = input {
                                window.handle_input(
                                    &focus,
                                    ElementInputHandler::new(bounds, entity),
                                    cx,
                                );
                            }
                            paint_overlay(
                                window,
                                bounds,
                                OverlayPaint {
                                    shown,
                                    mouse,
                                    mag,
                                    phase,
                                    start,
                                    has_sel,
                                    scale,
                                    marks: paint_marks.clone(),
                                    masks,
                                    dim,
                                    mosaic,
                                    candidates,
                                    show_magnifier,
                                    show_guides,
                                },
                            );
                        },
                    )
                    .size_full(),
                )
                .when(show_info_bar, |el| {
                    el.child(coord_label(shown, mouse, phase, scale))
                        .child(color_picker_label(
                            picked_color,
                            color_format,
                            color_notice,
                            mouse,
                            phase,
                            scale,
                        ))
                })
                .when(show_shortcut_hints, |el| {
                    el.child(shortcut_hints_bar(phase))
                })
                .children(self.toolbar_element(scale, cx, &visible_items, toolbar_geom))
                .children(self.props_element(scale, cx, toolbar_geom, active_monitor))
                .children(self.long_bar_element(scale, cx, active_monitor))
                .children(self.swatches_element(
                    scale,
                    cx,
                    &visible_items,
                    toolbar_geom,
                    active_monitor,
                ))
                .children(self.shape_popup_element(
                    scale,
                    cx,
                    &visible_items,
                    toolbar_geom,
                    active_monitor,
                ))
                .children(self.panel_element(scale, cx, toolbar_geom, active_monitor))
                .children(self.text_layer(scale, &marks))
            })
    }
}

fn coord_label(shown: Option<Rect>, mouse: Point, phase: Phase, scale: f32) -> impl IntoElement {
    let (text, x, y) = if let Some(r) = shown {
        (
            format!(
                "X: {:.0} Y: {:.0} W: {:.0} H: {:.0}",
                r.x, r.y, r.width, r.height
            ),
            r.x as f32 / scale + 4.0,
            r.y as f32 / scale + 4.0,
        )
    } else if phase == Phase::Selecting {
        (
            format!("X: {:.0} Y: {:.0}", mouse.x, mouse.y),
            mouse.x as f32 / scale + 12.0,
            mouse.y as f32 / scale + 12.0,
        )
    } else {
        return div();
    };
    div()
        .absolute()
        .left(px(x.max(0.0)))
        .top(px(y.max(0.0)))
        .bg(hsla(0., 0., 0., 0.8))
        .text_color(white())
        .text_xs()
        .px(px(4.))
        .py(px(2.))
        .child(text)
}

/// Colour swatch + value next to the magnifier while selecting. `C` copies the
/// value, `F` cycles the format; a transient notice confirms the last action.
fn color_picker_label(
    color: Option<crate::colorpicker::Color>,
    format: crate::colorpicker::ColorFormat,
    notice: Option<SharedString>,
    mouse: Point,
    phase: Phase,
    scale: f32,
) -> impl IntoElement {
    if phase != Phase::Selecting || color.is_none() && notice.is_none() {
        return div();
    }
    // Sits just below the coordinate label, to the right of the cursor.
    let x = mouse.x as f32 / scale + 12.0;
    let y = mouse.y as f32 / scale + 30.0;
    let text = match color {
        Some(c) => crate::colorpicker::format_color(c, format),
        None => String::new(),
    };
    let notice = notice.unwrap_or_default();
    let swatch = color
        .map(|c| rgb((u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)))
        .unwrap_or_else(|| hsla(0., 0., 0., 1.).into());
    div()
        .absolute()
        .left(px(x.max(0.0)))
        .top(px(y.max(0.0)))
        .flex()
        .items_center()
        .gap(px(4.0))
        .bg(hsla(0., 0., 0., 0.8))
        .text_color(white())
        .text_xs()
        .px(px(4.))
        .py(px(2.))
        .child(
            div()
                .w(px(10.0))
                .h(px(10.0))
                .bg(swatch)
                .border_1()
                .border_color(white()),
        )
        .child(
            div()
                .child(text)
                .when(!notice.is_empty(), |el| el.child(div().child(notice))),
        )
}

/// 覆盖层底部的一行快捷键提示；长截图阶段换成对应文案。
/// 与信息栏一样是纯 UI，不参与导出图像。
fn shortcut_hints_bar(phase: Phase) -> impl IntoElement {
    let text = if phase == Phase::LongCapturing {
        "Enter 完成 · Esc 取消 · 滚动栏切换方向/自动"
    } else {
        "Enter 完成 · Esc 取消 · 右键菜单"
    };
    div()
        .absolute()
        .bottom(px(10.0))
        .left(px(0.0))
        .w_full()
        .flex()
        .justify_center()
        .child(
            div()
                .bg(hsla(0., 0., 0., 0.7))
                .text_color(white())
                .text_xs()
                .px(px(8.0))
                .py(px(4.0))
                .child(text),
        )
}

struct OverlayPaint {
    shown: Option<Rect>,
    mouse: Point,
    mag: Option<(Vec<u8>, u32, u32, u32)>,
    phase: Phase,
    start: Instant,
    has_sel: bool,
    scale: f32,
    marks: Rc<Vec<Element>>,
    masks: Vec<Option<Rc<ElementRuns>>>,
    dim: Option<Rc<Vec<CoverageRun>>>,
    mosaic: Rc<MosaicTiles>,
    candidates: Vec<crate::smart_selection::Candidate>,
    show_magnifier: bool,
    show_guides: bool,
}

fn paint_overlay(window: &mut Window, bounds: Bounds<Pixels>, paint: OverlayPaint) {
    let OverlayPaint {
        shown,
        mouse,
        mag,
        phase,
        start,
        has_sel,
        scale,
        marks,
        masks,
        dim,
        mosaic,
        candidates,
        show_magnifier,
        show_guides,
    } = paint;
    let origin = Point { x: 0.0, y: 0.0 };
    let hole = shown.map(|r| annotate::rect_to_bounds(r, scale));
    for dim in dim_around(bounds, hole) {
        window.paint_quad(fill(dim, hsla(0., 0., 0., 0.2)));
    }
    if let Some(r) = shown {
        let dest = annotate::rect_to_bounds(r, scale);
        window.paint_quad(outline(dest, black(), BorderStyle::Solid));
        paint_ants(window, dest, start.elapsed().as_secs_f32());
        if phase == Phase::Active && has_sel {
            paint_handles(window, dest, scale);
        }
    }
    // Smart selection candidates: draw a subtle outline around the candidate
    // boundaries when in selecting mode and no selection is made yet.
    if phase == Phase::Selecting && shown.is_none() {
        // Candidates are in screen-space pixels (capture-relative). We need to
        // draw them with the same scale as the overlay.
        if let Some(cand) = candidates.first() {
            let bounds = annotate::rect_to_bounds(cand.rect, scale);
            window.paint_quad(outline(bounds, hsla(0., 0.8, 1., 0.8), BorderStyle::Solid));
        }
        // 未选区时的辅助线：一对过光标的半透明细线。鼠标是截图像素坐标，
        // 这里按 scale 换回逻辑像素；一旦按下拖出选区（shown）就不再画。
        if show_guides {
            let mx = mouse.x as f32 / scale;
            let my = mouse.y as f32 / scale;
            let thin = px(1.0 / scale);
            let guide = hsla(0., 0., 1., 0.35);
            window.paint_quad(fill(
                Bounds {
                    origin: point(bounds.origin.x, px(my)),
                    size: size(bounds.size.width, thin),
                },
                guide,
            ));
            window.paint_quad(fill(
                Bounds {
                    origin: point(px(mx), bounds.origin.y),
                    size: size(thin, bounds.size.height),
                },
                guide,
            ));
        }
    }
    // Annotations are clipped to the selection: the export composites onto a buffer
    // that IS the crop, so anything outside it never reaches the saved image. Without
    // the same clip here the overlay would promise strokes the file does not have.
    match shown {
        Some(r) => {
            let mask = ContentMask {
                bounds: annotate::rect_to_bounds(r, scale),
            };
            window.with_content_mask(Some(mask), |window| {
                if let Some(runs) = &dim {
                    annotate::paint_spotlight(window, runs, origin, scale);
                }
                annotate::paint_marks(window, &marks, &masks, &mosaic, origin, scale);
            });
        }
        None => {
            if let Some(runs) = &dim {
                annotate::paint_spotlight(window, runs, origin, scale);
            }
            annotate::paint_marks(window, &marks, &masks, &mosaic, origin, scale);
        }
    }
    if phase == Phase::Selecting && show_magnifier {
        if let Some(pixels) = mag {
            paint_magnifier(window, bounds, mouse, pixels, scale);
        }
    }
}

fn dim_around(full: Bounds<Pixels>, hole: Option<Bounds<Pixels>>) -> Vec<Bounds<Pixels>> {
    let Some(hole) = hole else {
        return vec![full];
    };
    let mut out = Vec::new();
    if hole.origin.y > full.origin.y {
        out.push(Bounds {
            origin: full.origin,
            size: size(full.size.width, hole.origin.y - full.origin.y),
        });
    }
    let hole_bottom = hole.origin.y + hole.size.height;
    let full_bottom = full.origin.y + full.size.height;
    if hole_bottom < full_bottom {
        out.push(Bounds {
            origin: point(full.origin.x, hole_bottom),
            size: size(full.size.width, full_bottom - hole_bottom),
        });
    }
    if hole.origin.x > full.origin.x {
        out.push(Bounds {
            origin: point(full.origin.x, hole.origin.y),
            size: size(hole.origin.x - full.origin.x, hole.size.height),
        });
    }
    let hole_right = hole.origin.x + hole.size.width;
    let full_right = full.origin.x + full.size.width;
    if hole_right < full_right {
        out.push(Bounds {
            origin: point(hole_right, hole.origin.y),
            size: size(full_right - hole_right, hole.size.height),
        });
    }
    out
}

fn ants_dash_array(elapsed: f32) -> Vec<Pixels> {
    let offset = marching_ants_dash_offset(elapsed);
    let period = ANTS_DASH[0] + ANTS_DASH[1];
    let o = ((-offset) % period + period) % period;
    if o < 0.05 {
        vec![px(ANTS_DASH[0]), px(ANTS_DASH[1])]
    } else {
        vec![px(0.01), px(o), px(ANTS_DASH[0]), px(ANTS_DASH[1])]
    }
}

fn paint_ants(window: &mut Window, dest: Bounds<Pixels>, elapsed: f32) {
    let mut path = PathBuilder::stroke(px(1.)).dash_array(&ants_dash_array(elapsed));
    let origin = dest.origin;
    let tr = point(dest.origin.x + dest.size.width, dest.origin.y);
    let br = point(
        dest.origin.x + dest.size.width,
        dest.origin.y + dest.size.height,
    );
    let bl = point(dest.origin.x, dest.origin.y + dest.size.height);
    path.move_to(origin);
    path.line_to(tr);
    path.line_to(br);
    path.line_to(bl);
    path.close();
    if let Ok(p) = path.build() {
        window.paint_path(p, white());
    }
}

fn paint_handles(window: &mut Window, dest: Bounds<Pixels>, scale: f32) {
    let half = px(3.0 / scale);
    let pts = [
        dest.origin,
        point(dest.origin.x + dest.size.width, dest.origin.y),
        point(dest.origin.x, dest.origin.y + dest.size.height),
        point(
            dest.origin.x + dest.size.width,
            dest.origin.y + dest.size.height,
        ),
        point(dest.origin.x + dest.size.width / 2., dest.origin.y),
        point(
            dest.origin.x + dest.size.width / 2.,
            dest.origin.y + dest.size.height,
        ),
        point(dest.origin.x, dest.origin.y + dest.size.height / 2.),
        point(
            dest.origin.x + dest.size.width,
            dest.origin.y + dest.size.height / 2.,
        ),
    ];
    for p in pts {
        window.paint_quad(fill(
            Bounds {
                origin: point(p.x - half, p.y - half),
                size: size(half * 2., half * 2.),
            },
            white(),
        ));
    }
}

fn paint_magnifier(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    mouse: Point,
    pixels: (Vec<u8>, u32, u32, u32),
    scale: f32,
) {
    let (px_buf, mw, mh, stride) = pixels;
    let mag = MAG_COUNT as f32 * MAG_PIXEL / scale;
    let mut dest_x = mouse.x as f32 / scale + 10.0 / scale;
    let mut dest_y = mouse.y as f32 / scale + 10.0 / scale;
    let right = f32::from(bounds.origin.x) + f32::from(bounds.size.width);
    let bottom = f32::from(bounds.origin.y) + f32::from(bounds.size.height);
    if dest_x + mag > right {
        dest_x = mouse.x as f32 / scale - 10.0 / scale - mag;
    }
    if dest_y + mag > bottom {
        dest_y = mouse.y as f32 / scale - 10.0 / scale - mag;
    }
    dest_x = dest_x.max(0.0);
    dest_y = dest_y.max(0.0);
    let cell = MAG_PIXEL / scale;
    // ponytail: 15x15 = 225 quads per frame, batched by the renderer. Upgrade to a
    // single sampled texture only if the overlay frame budget actually shows it.
    for y in 0..mh {
        for x in 0..mw {
            let i = (y * stride + x * 4) as usize;
            if i + 2 >= px_buf.len() {
                continue;
            }
            let b = px_buf[i] as u32;
            let g = px_buf[i + 1] as u32;
            let r = px_buf[i + 2] as u32;
            let color = rgb((r << 16) | (g << 8) | b);
            let origin = point(px(dest_x + x as f32 * cell), px(dest_y + y as f32 * cell));
            window.paint_quad(fill(
                Bounds {
                    origin,
                    size: size(px(cell), px(cell)),
                },
                color,
            ));
        }
    }
    window.paint_quad(outline(
        Bounds {
            origin: point(px(dest_x), px(dest_y)),
            size: size(px(mag), px(mag)),
        },
        white(),
        BorderStyle::Solid,
    ));
}

fn detect_windows(self_hwnd: HWND, ox: i32, oy: i32) -> Vec<Rect> {
    let mut ctx = WinCtx {
        self_hwnd,
        ox,
        oy,
        rects: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(enum_win), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.rects
}

/// Collects the smart-selection candidates under `pt`, in screen pixels then
/// shifted into the frozen image's coordinate space.
///
/// Two sources, matching snow-shot's split between whole windows and the panes
/// inside them:
///
/// * `Window` mode asks only for the top-level window at the point — a single
///   rect, no child enumeration.
/// * `WindowSubElement` adds the visible child HWNDs under the cursor (the way
///   snow-shot's `visible_child_window_rects` does, for classic Win32 apps) and
///   the UIA elements under it (for apps that paint panes inside one HWND).
///
/// Depth is assigned by rect containment: a pane inside another candidate is
/// one layer deeper. That puts the same scale on child HWNDs and UIA elements,
/// so the hover walk can order them without knowing where each came from.
#[allow(clippy::too_many_arguments)]
fn detect_candidates(
    mode: crate::config::SmartSelectionMode,
    self_hwnd: HWND,
    pt: POINT,
    ox: i32,
    oy: i32,
    capture_w: u32,
    capture_h: u32,
    uia_rects: &[RECT],
) -> Vec<crate::smart_selection::Candidate> {
    use crate::smart_selection::{candidates_for, Candidate};
    if !mode.enabled() {
        return Vec::new();
    }
    // The top-level window under the point: the same z-ordered walk the
    // long-capture focus hand-off uses, so both features agree on what "the
    // window under the cursor" is.
    let Some(top) = window_under_point(self_hwnd, pt) else {
        return Vec::new();
    };
    let shift = |r: RECT| -> Rect {
        Rect::new(
            (r.left - ox) as f64,
            (r.top - oy) as f64,
            (r.right - r.left) as f64,
            (r.bottom - r.top) as f64,
        )
    };
    let mut rects: Vec<Rect> = Vec::new();
    if let Some(bounds) = real_window_rect(top) {
        rects.push(shift(bounds));
        if mode.wants_sub_elements() {
            // Two sources of sub-elements, mirroring snow-shot's split:
            // classic child HWNDs ...
            let mut ctx = ChildCtx {
                bounds,
                rects: Vec::new(),
            };
            unsafe {
                let _ = EnumChildWindows(
                    Some(top),
                    Some(enum_child_window_proc),
                    LPARAM(&mut ctx as *mut _ as isize),
                );
            }
            rects.extend(ctx.rects.into_iter().map(shift));

            // ... and UIA elements for modern apps that paint their panes
            // inside a single HWND (WPF / Electron / Chromium). The query runs
            // on a dedicated STA worker; we only consume an already-returned
            // result here, so a hung provider can never stall the UI thread.
            rects.extend(uia_rects.iter().map(|r| shift(*r)));
        }
    }
    if rects.is_empty() {
        return Vec::new();
    }
    // Nesting gives depth: a rect contained by another is one layer deeper.
    let depths: Vec<u32> = rects
        .iter()
        .map(|r| {
            rects
                .iter()
                .filter(|o| o != &r && contains_rect(o, r))
                .count() as u32
        })
        .collect();
    let raw: Vec<Candidate> = rects
        .into_iter()
        .zip(depths)
        .map(|(rect, depth)| Candidate { rect, depth })
        .collect();
    // Deepest / smallest first, filter to the frozen image. Without this the
    // window rect — pushed first — would always win `.first()` and the picker
    // would never snap to a child pane.
    candidates_for(
        mode,
        raw,
        Rect::new(0.0, 0.0, capture_w as f64, capture_h as f64),
        MIN_CANDIDATE_EXTENT as f64,
    )
}

/// Whether `outer` fully contains `inner`.
fn contains_rect(outer: &Rect, inner: &Rect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.right() >= inner.right()
        && outer.bottom() >= inner.bottom()
        && (outer.width > inner.width || outer.height > inner.height)
}

/// Top-level window under a screen point, skipping this overlay.
fn window_under_point(self_hwnd: HWND, pt: POINT) -> Option<HWND> {
    struct Ctx {
        exclude: HWND,
        pt: POINT,
        found: HWND,
    }
    unsafe extern "system" fn walk(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        unsafe {
            let ctx = &mut *(lparam.0 as *mut Ctx);
            if hwnd == ctx.exclude {
                return windows::core::BOOL(1);
            }
            let Some(wr) = real_window_rect(hwnd) else {
                return windows::core::BOOL(1);
            };
            if ctx.pt.x >= wr.left
                && ctx.pt.x < wr.right
                && ctx.pt.y >= wr.top
                && ctx.pt.y < wr.bottom
            {
                ctx.found = hwnd;
                return windows::core::BOOL(0);
            }
            windows::core::BOOL(1)
        }
    }
    let mut ctx = Ctx {
        exclude: self_hwnd,
        pt,
        found: HWND::default(),
    };
    unsafe {
        let _ = EnumWindows(Some(walk), LPARAM(&mut ctx as *mut _ as isize));
    }
    if ctx.found.0.is_null() {
        None
    } else {
        Some(ctx.found)
    }
}

struct ChildCtx {
    bounds: RECT,
    rects: Vec<RECT>,
}

/// Collects one visible child window's rect, clipped to the parent window.
///
/// Mirrors snow-shot's `enum_child_window_proc`: enumerate *all* descendants,
/// not just the one under the cursor. Only invisible children, empty rects and
/// children that cover the whole parent are skipped — a child identical to its
/// parent adds no boundary and would duplicate the window rect. A sibling pane
/// on the other side of the window is still a legitimate snap boundary once the
/// user moves there, and enumerating it up front removes the per-pixel HWND
/// walk and makes classic Win32 apps expose as many candidates as snow-shot.
unsafe extern "system" fn enum_child_window_proc(
    hwnd: HWND,
    lparam: LPARAM,
) -> windows::core::BOOL {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return windows::core::BOOL(1);
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return windows::core::BOOL(1);
        }
        let ctx = &mut *(lparam.0 as *mut ChildCtx);
        let clipped = RECT {
            left: rect.left.max(ctx.bounds.left),
            top: rect.top.max(ctx.bounds.top),
            right: rect.right.min(ctx.bounds.right),
            bottom: rect.bottom.min(ctx.bounds.bottom),
        };
        let non_empty = clipped.right - clipped.left > 5 && clipped.bottom - clipped.top > 5;
        let same_as_parent = clipped.left == ctx.bounds.left
            && clipped.top == ctx.bounds.top
            && clipped.right == ctx.bounds.right
            && clipped.bottom == ctx.bounds.bottom;
        // 不再要求 under_cursor：snow-shot 的 window.rs 枚举全部后代，
        // 兄弟窗口也是真实边界，交给上层按面积/深度排序即可。
        if non_empty && !same_as_parent {
            ctx.rects.push(clipped);
        }
        windows::core::BOOL(1)
    }
}

/// Frame bounds of a window a user could actually be looking at: visible, not
/// minimised, not a cloaked ghost (suspended UWP frames stay "visible" and
/// full-screen), not a passive tool window, not a sliver. Both the snap
/// detection and the long-capture focus hand-off need exactly this set.
fn real_window_rect(hwnd: HWND) -> Option<RECT> {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
        let mut cloaked: i32 = 0;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
        if cloaked != 0 {
            return None;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_NOACTIVATE.0 != 0 {
            return None;
        }
        let mut wr = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut wr as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
            && GetWindowRect(hwnd, &mut wr).is_err()
        {
            return None;
        }
        if wr.right - wr.left <= 5 || wr.bottom - wr.top <= 5 {
            return None;
        }
        Some(wr)
    }
}

unsafe extern "system" fn enum_win(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    unsafe {
        let ctx = &mut *(lparam.0 as *mut WinCtx);
        if hwnd == ctx.self_hwnd {
            return windows::core::BOOL(1);
        }
        let Some(wr) = real_window_rect(hwnd) else {
            return windows::core::BOOL(1);
        };
        ctx.rects.push(Rect::new(
            (wr.left - ctx.ox) as f64,
            (wr.top - ctx.oy) as f64,
            (wr.right - wr.left) as f64,
            (wr.bottom - wr.top) as f64,
        ));
        windows::core::BOOL(1)
    }
}

struct WinCtx {
    self_hwnd: HWND,
    ox: i32,
    oy: i32,
    rects: Vec<Rect>,
}

fn hit_window(windows: &[Rect], p: Point) -> Option<Rect> {
    windows.iter().copied().find(|r| r.contains(p))
}

/// Outer HWND rect whose client area equals `desired` (screen coordinates).
/// GPUI PopUp `WM_NCCALCSIZE` insets would otherwise shift the freeze visual right.
fn window_rect_for_client_area(hwnd: HWND, desired: RECT) -> RECT {
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
    apply_nc_borders(window, origin, client, desired)
}

fn apply_nc_borders(window: RECT, client_origin: POINT, client: RECT, desired: RECT) -> RECT {
    let left = client_origin.x - window.left;
    let top = client_origin.y - window.top;
    let right = window.right - (client_origin.x + client.right - client.left);
    let bottom = window.bottom - (client_origin.y + client.bottom - client.top);
    RECT {
        left: desired.left - left,
        top: desired.top - top,
        right: desired.right + right,
        bottom: desired.bottom + bottom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dash phase must stay in [0, period) for every elapsed value, including
    /// large ones: `offset` is negative and grows without bound, so a single `%`
    /// would hand a negative leading gap to PathBuilder.
    #[test]
    fn hole_gives_up_the_band_under_the_bar() {
        let hole = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        // Bar below the selection: the hole is untouched.
        assert_eq!(hole_clear_of_bar(hole, 1088, 1132).top, 0);
        assert_eq!(hole_clear_of_bar(hole, 1088, 1132).bottom, 1080);
        // Full-screen selection clamps the bar to the top edge, inside the hole.
        assert_eq!(hole_clear_of_bar(hole, 0, 44).top, 44);
        // Same at the bottom edge.
        assert_eq!(hole_clear_of_bar(hole, 1036, 1080).bottom, 1036);
        // A selection shorter than the bar cannot end up with an inverted hole.
        let tiny = RECT {
            left: 0,
            top: 100,
            right: 200,
            bottom: 120,
        };
        let cut = hole_clear_of_bar(tiny, 90, 134);
        assert!(cut.top <= cut.bottom, "{cut:?}");
    }

    /// 设备复位后的延迟清洞任务不能误清新会话刚设置的 passthrough 洞：
    /// 序号前进或已有新会话时都必须跳过。
    #[test]
    fn delayed_passthrough_clear_skips_a_new_long_session() {
        assert!(should_clear_long_passthrough(7, 7, false));
        assert!(!should_clear_long_passthrough(7, 8, false));
        assert!(!should_clear_long_passthrough(7, 7, true));
        assert!(!should_clear_long_passthrough(7, 8, true));
    }

    /// A stall, a pause or a static edge is a state the bar reflects; the poll
    /// predicate has no input for any of them, so no status the worker reports
    /// can end the loop (the P1-05 regression this fix removes).
    #[test]
    fn a_stall_never_stops_the_long_poll_loop() {
        assert!(should_keep_polling(true, false, true));
    }

    /// Only the hard stops end the loop: a superseded session, `maxed` (height
    /// cap, image budget, GPU reset, worker gone) and leaving the phase.
    #[test]
    fn only_hard_stops_end_the_long_poll_loop() {
        assert!(
            !should_keep_polling(false, false, true),
            "a stale generation must stop the old loop"
        );
        assert!(
            !should_keep_polling(true, true, true),
            "maxed keeps the bar but stops the polling"
        );
        assert!(
            !should_keep_polling(true, false, false),
            "完成/取消 leaves the capture phase"
        );
    }

    #[test]
    fn ants_dash_phase_is_never_negative() {
        for ms in 0..2000 {
            let arr = ants_dash_array(ms as f32 / 100.0);
            for p in &arr {
                assert!(f32::from(*p) >= 0.0, "elapsed={ms} produced {arr:?}");
            }
        }
    }

    #[test]
    fn ants_dash_wraps_at_one_period() {
        let period = ANTS_DASH[0] + ANTS_DASH[1];
        // phase 0 => plain {5,5}, no leading gap.
        assert_eq!(ants_dash_array(0.0).len(), 2);
        // one full period later the pattern is back to phase 0.
        let one_period = period / ANTS_SPEED.abs();
        assert_eq!(ants_dash_array(one_period).len(), 2);
        // mid-period inserts a leading gap equal to the phase.
        let mid = ants_dash_array(0.2);
        assert_eq!(mid.len(), 4);
        assert!((f32::from(mid[1]) - 3.0).abs() < 1e-3, "{mid:?}");
    }

    #[test]
    fn nc_insets_expand_window_so_client_matches_capture() {
        // Win11-style: 8px left/right/bottom, 1px top.
        let window = RECT {
            left: 100,
            top: 50,
            right: 100 + 1920 + 16,
            bottom: 50 + 1080 + 9,
        };
        let client_origin = POINT { x: 108, y: 51 };
        let client = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let desired = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let outer = apply_nc_borders(window, client_origin, client, desired);
        assert_eq!(outer.left, -8);
        assert_eq!(outer.top, -1);
        assert_eq!(outer.right, 1928);
        assert_eq!(outer.bottom, 1088);
    }

    #[test]
    fn zero_nc_insets_keep_window_on_capture_bounds() {
        let window = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let client_origin = POINT { x: 0, y: 0 };
        let client = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let desired = RECT {
            left: -1920,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let outer = apply_nc_borders(window, client_origin, client, desired);
        assert_eq!(outer.left, desired.left);
        assert_eq!(outer.top, desired.top);
        assert_eq!(outer.right, desired.right);
        assert_eq!(outer.bottom, desired.bottom);
    }

    #[test]
    fn complete_defaults_to_clipboard_not_file() {
        // 标志现在来自配置；同一次读取做参照，避免本机 settings.json 的非缺省值
        // 让测试变得不确定。缺省语义由 config 的 export 单测固定。
        let export = crate::config::get().screenshot.export;
        let (write_file, copy) = complete_export_flags();
        assert_eq!(write_file, export.auto_save);
        assert_eq!(copy, export.auto_copy);

        let default = crate::config::Export::default();
        assert!(!default.auto_save);
        assert!(default.auto_copy);
    }

    #[test]
    fn copy_only_always_copies_and_never_writes_a_file() {
        // 快捷截图（Ctrl+Alt+W）的结果不能被「输出」页开关改写：关掉 auto_copy
        // 后热键必须仍然复制到剪贴板，开启 auto_save 后也不能写文件。
        assert_eq!(export_flags_for(true), (false, true));
        // 普通截图仍完全遵循配置。
        let export = crate::config::get().screenshot.export;
        assert_eq!(
            export_flags_for(false),
            (export.auto_save, export.auto_copy)
        );
    }

    #[test]
    fn toolbar_item_at_picks_slot_under_pointer() {
        let items = ["select", "rect", "arrow", "pen"];
        let (w, h) = toolbar::bar_size(items.len());
        let geom = Rect::new(100.0, 200.0, w, h);
        // Button i spans [100 + PAD + i*38, +BTN] = [104,140] [142,178] [180,216] [218,254].
        let at = |x: f64| annotate::toolbar_item_at(Point { x, y: 210.0 }, geom, &items);
        assert_eq!(at(104.0), Some("select"));
        assert_eq!(at(139.0), Some("select"));
        assert_eq!(at(142.0), Some("rect"));
        assert_eq!(at(215.0), Some("arrow"));
        assert_eq!(at(253.0), Some("pen"));
        assert_eq!(at(101.0), None, "left padding is not a button");
        assert_eq!(at(141.0), None, "gap between buttons is not a button");
        assert_eq!(at(50.0), None);
        assert_eq!(
            annotate::toolbar_item_at(Point { x: 210.0, y: 10.0 }, geom, &items),
            None
        );
    }

    /// The whole point of the rewrite: the last slot must not steal Cancel's clicks.
    #[test]
    fn toolbar_item_at_matches_rendered_buttons_for_the_full_bar() {
        let items: Vec<&'static str> = PHASE1_ITEMS.to_vec();
        let (w, h) = toolbar::bar_size(items.len());
        let geom = Rect::new(0.0, 0.0, w, h);
        for (i, id) in items.iter().enumerate() {
            let left = toolbar::PAD + i as f64 * (toolbar::BTN + toolbar::ITEM_GAP);
            for x in [left, left + toolbar::BTN / 2.0, left + toolbar::BTN - 0.1] {
                assert_eq!(
                    annotate::toolbar_item_at(Point { x, y: 20.0 }, geom, &items),
                    Some(*id),
                    "x={x} should hit {id}"
                );
            }
        }
    }

    // ---- opt-in auto scroll -------------------------------------------------

    /// The beat the tests advance by: the configured default, so a change to
    /// the policy in `scroll_step` shows up here as a test failure rather than
    /// as a silently different cadence.
    const BEAT: Duration = crate::scroll_step::DEFAULT_INTERVAL;

    #[test]
    fn auto_scroll_off_by_default() {
        let mut auto = AutoScroll::default();
        assert!(!auto.enabled(), "every session starts with auto off");
        // Off means off: even a due beat posts nothing.
        assert_eq!(auto.plan(Instant::now(), false), AutoScrollStep::Wait);
        assert!(!auto.observe_post(true), "no post may keep it on");
        assert!(!auto.enabled());
    }

    #[test]
    fn auto_scroll_waits_for_the_previous_frame() {
        let mut auto = AutoScroll::default();
        auto.set_enabled(true);
        let t0 = Instant::now();
        // Due (nothing posted yet), but the worker still owns a frame.
        assert_eq!(auto.plan(t0, true), AutoScrollStep::Wait);
        assert_eq!(
            auto.plan(t0 + BEAT * 3, true),
            AutoScrollStep::Wait,
            "a long in-flight frame must not accumulate posts"
        );
        // The frame landed: the notch goes out.
        assert_eq!(auto.plan(t0, false), AutoScrollStep::Post);
        assert_eq!(
            auto.plan(t0 + Duration::from_millis(100), false),
            AutoScrollStep::Wait,
            "the beat has to elapse before the next notch"
        );
        assert_eq!(auto.plan(t0 + BEAT, false), AutoScrollStep::Post);
    }

    /// A fresh session must carry the persisted step into its beat. Without
    /// this, the settings dropdown would write a value the capture never reads.
    #[test]
    fn auto_scroll_session_starts_from_the_configured_step() {
        let configured = crate::config::AutoScroll {
            interval_ms: 250,
            notches: 4,
        }
        .step();
        let mut auto = AutoScroll::with_step(configured);
        auto.set_enabled(true);
        let t0 = Instant::now();
        assert_eq!(auto.plan(t0, false), AutoScrollStep::Post);
        assert_eq!(
            auto.plan(t0 + Duration::from_millis(249), false),
            AutoScrollStep::Wait
        );
        assert_eq!(
            auto.plan(t0 + Duration::from_millis(250), false),
            AutoScrollStep::Post
        );
        assert_eq!(auto.step.notch_multiplier(), 4);
    }

    /// The beat comes from the configuration, not from a constant buried in
    /// this file, so a slower step really does slow the session down.
    #[test]
    fn auto_scroll_honours_the_configured_interval() {
        let mut auto = AutoScroll::default();
        auto.set_enabled(true);
        auto.step.set_interval(Duration::from_millis(200));
        let t0 = Instant::now();
        assert_eq!(auto.plan(t0, false), AutoScrollStep::Post);
        assert_eq!(
            auto.plan(t0 + Duration::from_millis(199), false),
            AutoScrollStep::Wait
        );
        assert_eq!(
            auto.plan(t0 + Duration::from_millis(200), false),
            AutoScrollStep::Post,
            "a 200 ms step must beat at 200 ms, not at the old 600 ms"
        );
    }

    #[test]
    fn auto_scroll_stops_after_repeated_no_progress() {
        let mut auto = AutoScroll::default();
        auto.set_enabled(true);
        auto.observe_height(1000);
        let mut now = Instant::now();
        for i in 0..AUTO_SCROLL_MAX_IDLE_POSTS {
            assert_eq!(auto.plan(now, false), AutoScrollStep::Post, "beat {i}");
            assert!(auto.enabled());
            // Nothing new arrived from this notch.
            auto.observe_height(1000);
            now += BEAT;
        }
        assert_eq!(auto.plan(now, false), AutoScrollStep::StopNoProgress);
        assert!(
            !auto.enabled(),
            "a switch that scrolls nothing must not stay on"
        );
        assert_eq!(
            auto.plan(now + BEAT, false),
            AutoScrollStep::Wait,
            "stopped, not paused: nothing more is posted"
        );
    }

    #[test]
    fn auto_scroll_stops_when_posting_fails() {
        let mut auto = AutoScroll::default();
        auto.set_enabled(true);
        assert!(auto.observe_post(true), "a delivered notch keeps it on");
        assert!(!auto.observe_post(false), "a refused post reports stop");
        assert!(!auto.enabled());
        assert_eq!(auto.plan(Instant::now(), false), AutoScrollStep::Wait);
    }

    /// A growth in the stitched image is the only thing that proves a notch
    /// worked, so it restarts the give-up counter.
    #[test]
    fn auto_scroll_progress_restarts_the_no_progress_counter() {
        let mut auto = AutoScroll::default();
        auto.set_enabled(true);
        auto.observe_height(1000);
        let mut now = Instant::now();
        for _ in 0..AUTO_SCROLL_MAX_IDLE_POSTS - 1 {
            assert_eq!(auto.plan(now, false), AutoScrollStep::Post);
            auto.observe_height(1000);
            now += BEAT;
        }
        // One real scroll arrives: the next notches start from zero again.
        auto.observe_height(1400);
        assert_eq!(auto.plan(now, false), AutoScrollStep::Post);
        assert!(auto.enabled());
    }

    /// Forward wheel (away from the user) scrolls most apps further down;
    /// `WM_MOUSEHWHEEL` is positive to the right. Both continue the capture.
    #[test]
    fn wheel_notch_continues_the_capture_direction() {
        assert_eq!(
            wheel_notch(Axis::Vertical, 1),
            (WM_MOUSEWHEEL, WHEEL_DELTA as i32)
        );
        assert_eq!(
            wheel_notch(Axis::Horizontal, 1),
            (WM_MOUSEHWHEEL, WHEEL_DELTA as i32)
        );
        // MAKELPARAM packs x in the low word and y in the high word.
        assert_eq!(wheel_lparam(0x1234, 0x5678), 0x5678_1234);
        assert_eq!(wheel_lparam(-1, 0), 0x0000_ffff);
    }

    /// A wider step multiplies the delta and is never negative, so a step can
    /// only ever move the page the way the capture is growing.
    #[test]
    fn a_wider_step_posts_proportionally_more() {
        assert_eq!(
            wheel_notch(Axis::Vertical, 3),
            (WM_MOUSEWHEEL, (WHEEL_DELTA as i32) * 3)
        );
        assert_eq!(
            wheel_notch(Axis::Vertical, 0),
            (WM_MOUSEWHEEL, WHEEL_DELTA as i32),
            "a zero multiplier still posts one notch, never nothing"
        );
        assert_eq!(
            wheel_notch(Axis::Vertical, -5),
            (WM_MOUSEWHEEL, WHEEL_DELTA as i32),
            "a step never reverses direction"
        );
    }
    // ---- axis: vertical by default, horizontal only on the button ----------

    #[test]
    fn a_new_session_starts_vertical() {
        assert_eq!(INITIAL_LONG_AXIS, Axis::Vertical);
        // Starting again after a horizontal session must not inherit it: the
        // start path reads the constant, never the previous session's axis.
        let mut axis = INITIAL_LONG_AXIS;
        for _ in 0..3 {
            axis = axis_after_bar_click(axis, true);
            assert_eq!(axis, Axis::Horizontal);
            axis = INITIAL_LONG_AXIS;
            assert_eq!(axis, Axis::Vertical, "a horizontal session leaked");
        }
    }

    #[test]
    fn axis_changes_only_when_the_button_is_used() {
        // No click: the axis is whatever it was, in both directions.
        assert_eq!(axis_after_bar_click(Axis::Vertical, false), Axis::Vertical);
        assert_eq!(
            axis_after_bar_click(Axis::Horizontal, false),
            Axis::Horizontal
        );
        // Every click flips, so the same button goes back and forth.
        assert_eq!(axis_after_bar_click(Axis::Vertical, true), Axis::Horizontal);
        assert_eq!(axis_after_bar_click(Axis::Horizontal, true), Axis::Vertical);
    }

    // ---- 智能选区：UIA 缓存复用判定与按点重新验证 --------------------------

    fn pt(x: i32, y: i32) -> POINT {
        POINT { x, y }
    }

    fn hwnd(raw: usize) -> HWND {
        HWND(raw as *mut core::ffi::c_void)
    }

    #[test]
    fn uia_cache_reuse_within_tolerance_and_same_window() {
        let target = hwnd(0x1234);
        // 24px 容差内（含边界）且目标窗口不变时继续复用。
        assert!(uia_cache_usable(
            Some(target),
            Some(target),
            pt(100, 100),
            pt(124, 124)
        ));
        assert!(uia_cache_usable(
            Some(target),
            Some(target),
            pt(100, 100),
            pt(76, 76)
        ));
        // 超过 24px 后缓存失效，必须重新查询。
        assert!(!uia_cache_usable(
            Some(target),
            Some(target),
            pt(100, 100),
            pt(125, 100)
        ));
        assert!(!uia_cache_usable(
            Some(target),
            Some(target),
            pt(100, 100),
            pt(100, 125)
        ));
    }

    #[test]
    fn uia_cache_invalid_on_window_change_or_missing_last() {
        let a = hwnd(0x1234);
        let b = hwnd(0x5678);
        // HWND 变更（包括当前点找不到窗口）都不能复用旧缓存。
        assert!(!uia_cache_usable(
            Some(a),
            Some(b),
            pt(100, 100),
            pt(100, 100)
        ));
        assert!(!uia_cache_usable(Some(a), None, pt(100, 100), pt(100, 100)));
        // 从未成功应用过 UIA 结果，没有缓存可复用。
        assert!(!uia_cache_usable(None, Some(a), pt(100, 100), pt(100, 100)));
        assert!(!uia_cache_usable(None, None, pt(100, 100), pt(100, 100)));
    }

    #[test]
    fn filter_uia_for_point_keeps_only_rects_near_the_cursor() {
        let rect = |l, t, r, b| RECT {
            left: l,
            top: t,
            right: r,
            bottom: b,
        };
        let rects = [
            rect(100, 100, 200, 200), // 当前点 (150,150) 在内
            rect(300, 300, 400, 400), // 旧位置，远离当前点
            rect(140, 140, 160, 160), // 在内
            rect(158, 158, 200, 200), // 点在外 8px：pad=8 时保留，pad=0 时丢弃
        ];
        let kept = filter_uia_for_point(&rects, pt(150, 150), 8);
        assert_eq!(kept.len(), 3);
        assert!(kept.contains(&rect(100, 100, 200, 200)));
        assert!(kept.contains(&rect(140, 140, 160, 160)));
        assert!(kept.contains(&rect(158, 158, 200, 200)));
        assert!(!kept.contains(&rect(300, 300, 400, 400)));

        // pad=0 时按半开区间严格 contains：右/下边界不算在内。
        let strict = filter_uia_for_point(&rects, pt(150, 150), 0);
        assert_eq!(strict.len(), 2);
        assert!(!strict.contains(&rect(158, 158, 200, 200)));
        assert!(filter_uia_for_point(&rects, pt(200, 150), 0).is_empty());
    }
}
