//! Settings window, laid out like the Citepix settings window: one caption
//! (logo + title + centred search) in the component title bar, a 200px
//! navigation pane with a global reset, and pages built from rounded row
//! sections.
//!
//! Only pages whose settings this build can actually honour carry controls;
//! the rest say so in a disabled row instead of offering dead switches.

pub(crate) mod model;
pub(crate) mod system;
pub mod text_input;
pub mod widgets;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use gpui::{
    canvas, deferred, div, img, prelude::*, px, size, App, AsyncApp, Bounds, Context, CursorStyle,
    Entity, Focusable, FontWeight, ImageSource, MouseButton, RenderImage, SharedString,
    Subscription, Window, WindowBounds, WindowHandle, WindowOptions,
};

use crate::config::Settings;
use crate::history::{HistoryRecord, HistoryStore};
use crate::hotkey::{self, Hotkey, HotkeyAction, HotkeyBindings};
use crate::overlay::{theme, toolbar};
use model::{Page, RowMeta};
use text_input::TextInput;
use widgets::{CONTENT_MAX_W, TOOLBAR_COLUMNS};

thread_local! {
    static SETTINGS_WINDOW: RefCell<Option<WindowHandle<SettingsView>>> = const { RefCell::new(None) };
    static KEYS_BOUND: Cell<bool> = const { Cell::new(false) };
}

/// Generation of the hotkey-recording session. Closing the settings window now
/// stops the recorder explicitly, but its 10s timeout task may still be in
/// flight, so it has to check its own validity even after the view that armed it
/// is gone — hence a shared counter rather than the view field alone.
/// `SettingsView::recording_seq` mirrors it.
static RECORDING_SEQ: AtomicU64 = AtomicU64::new(0);

/// Long enough to press any combo, short enough that a recorder left running by
/// a closed window cannot keep swallowing keys. See `hook::record_proc`.
const RECORDING_TIMEOUT: Duration = Duration::from_secs(10);

/// Short values sit beside their label rather than stretching the row.
const INLINE_FIELD_W: f32 = 220.;
/// Gap between the closed dropdown and its option list.
const DROPDOWN_TOP: f32 = widgets::ROW_MIN_H + 4.;

const LANGUAGES: [(&str, &str); 3] = [("zh-CN", "简体中文"), ("en", "English"), ("ja", "日本語")];

const HOTKEY_ROWS: [(HotkeyAction, &str, &str, &str); 3] = [
    (
        HotkeyAction::Region,
        "hk-region",
        "区域截图",
        "冻结屏幕后框选区域，进入标注工具栏。",
    ),
    (
        HotkeyAction::CopyOnly,
        "hk-copy",
        "仅复制",
        "框选后直接复制到剪贴板，不写文件。",
    ),
    (
        HotkeyAction::Fullscreen,
        "hk-full",
        "全屏截图",
        "抓取当前显示器并保存 PNG。",
    ),
];

/// Opens the window, or raises the one already open. Called from the tray.
pub fn open(cx: &mut App) {
    bind_keys(cx);

    if let Some(handle) = SETTINGS_WINDOW.with(|slot| *slot.borrow()) {
        let raised = handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok();
        if raised {
            return;
        }
        // The user closed it; the handle is stale.
        SETTINGS_WINDOW.with(|slot| *slot.borrow_mut() = None);
    }

    let bounds = Bounds::centered(None, size(px(1000.), px(720.)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // The component title bar draws itself over the whole client area
            // (`appears_transparent` hides the OS caption) and owns dragging
            // through its own `WindowControlArea::Drag` plus
            // `start_window_move`; the min/max/close controls are part of the
            // component. The sizing frame is kept by `is_resizable`, and the
            // minimum size is unchanged.
            focus: true,
            show: true,
            is_movable: true,
            is_resizable: true,
            is_minimizable: true,
            window_min_size: Some(size(px(860.), px(560.))),
            ..gpui_kit::component::TitleBar::window_options()
        },
        |window, cx| cx.new(|cx| SettingsView::new(window, cx)),
    );

    match opened {
        Ok(handle) => {
            // Populate the embedded history grid up front so the History page
            // shows real thumbnails even when reached without a nav click.
            handle
                .update(cx, |view, window, cx| {
                    view.load_history_on_open(cx);
                    // 关窗即通知安装包下载任务收尾：轮询任务立刻退出，worker
                    // 在每次进度回调里跳过上报。闭包持有与 view 同一个 Arc，
                    // 覆盖窗口打开之后才启动的下载。
                    let download_cancel = view.update_download_cancel.clone();
                    // 点过热键输入框后直接关窗也要立刻收钩子：否则录制钩子会
                    // 继续吞掉可绑定的按键，直到 10 秒超时才被撤下。递增代号让
                    // 旧超时任务发现失效，不再碰新窗口的录制状态。
                    window.on_window_should_close(cx, move |_, cx| {
                        download_cancel.store(true, Ordering::Release);
                        crate::platform::stop_recording();
                        RECORDING_SEQ.fetch_add(1, Ordering::SeqCst);
                        cx.refresh_windows();
                        true
                    });
                })
                .ok();
            SETTINGS_WINDOW.with(|slot| *slot.borrow_mut() = Some(handle))
        }
        Err(e) => eprintln!("settings window: {e}"),
    }
}

/// Routed from the hidden window's message loop; the recording hook posts raw
/// virtual keys because gpui never reports PrtSc as a keystroke.
pub fn on_hotkey_recorded(cx: &mut App, hotkey: Hotkey) {
    let Some(handle) = SETTINGS_WINDOW.with(|slot| *slot.borrow()) else {
        // No window to receive it — do not leave the recorder swallowing keys.
        crate::platform::stop_recording();
        return;
    };
    if handle
        .update(cx, |view, _, cx| view.hotkey_recorded(hotkey, cx))
        .is_err()
    {
        crate::platform::stop_recording();
    }
}

fn bind_keys(cx: &mut App) {
    if KEYS_BOUND.with(|b| b.get()) {
        return;
    }
    cx.bind_keys(text_input::key_bindings());
    KEYS_BOUND.with(|b| b.set(true));
}

/// The FastPin mark, decoded once per process.
///
/// `include_bytes!` keeps the logo independent of the working directory, and
/// the 1254² source is downscaled to 64² before it reaches the GPU: a 28 px
/// brand mark has no use for six megabytes of texture. The transparent variant
/// is used rather than `fastpin.png` because the titlebar sits on the window
/// surface, where the source's white card would still show as a white tile.
fn brand_logo() -> Option<Arc<RenderImage>> {
    static LOGO: OnceLock<Option<Arc<RenderImage>>> = OnceLock::new();
    LOGO.get_or_init(|| {
        let mut decoded = image::load_from_memory(include_bytes!(
            "../../../../assets/logo/fastpin-transparent.png"
        ))
        .ok()?
        .to_rgba8();
        // Both calls are no-ops on the generated asset; they keep a stale or
        // hand-edited PNG from bringing the white card back.
        crate::brand_icon::clear_edge_white(&mut decoded, crate::brand_icon::WHITE_THRESHOLD);
        crate::brand_icon::de_white_transparent(&mut decoded);
        let mut small = crate::brand_icon::resize_icon(&decoded, 64);
        // gpui's polychrome atlas is B8G8R8A8 (the layout the screenshot path
        // already feeds it) and the sprite blend is straight alpha, so the
        // decoded RGBA needs only an R/B swap — without it the blue mark
        // would render orange.
        for pixel in small.pixels_mut() {
            pixel.0.swap(0, 2);
        }
        Some(Arc::new(RenderImage::new(vec![image::Frame::new(small)])))
    })
    .clone()
}

/// Brand mark at `size`; falls back to a letter tile if the decode fails so
/// the pane never renders blank.
fn logo_element(size: f32) -> gpui::AnyElement {
    match brand_logo() {
        Some(image) => img(ImageSource::Render(image))
            .w(px(size))
            .h(px(size))
            .into_any_element(),
        None => div()
            .flex_none()
            .w(px(size))
            .h(px(size))
            .rounded(px(7.))
            .bg(widgets::accent())
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(size * 0.55))
            .font_weight(FontWeight::BOLD)
            .text_color(theme::on_accent())
            .child("F")
            .into_any_element(),
    }
}

/// The dropdown currently open, if any — only one overlays the page at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectId {
    Language,
    StrokeWidth,
    MosaicBlock,
    WaterPosition,
    WaterOpacity,
    WaterScale,
    ExportFormat,
    ExportQuality,
    // 旧「智能选区」行的 id，已被 ElementDetection 取代，保留以兼容历史引用。
    #[allow(dead_code)]
    SmartSelection,
    AutoScrollInterval,
    AutoScrollNotches,
    HistoryRetention,
    HistoryLimit,
    HistoryDisk,
    Appearance,
    HistoryFilter,
    // 输出页的拼图 / PDF 默认设置。
    CollageDirection,
    CollageSizing,
    PdfStitching,
    PdfTemplate,
    PdfExportMode,
    PdfPaperSize,
    PdfOrientation,
    // 截屏页新增的下拉；SmartSelection 保留给旧id，元素检测改用一个新id。
    CaptureMode,
    LongCaptureAutoStart,
    LeftDoubleClick,
    MiddleClick,
    ClickOutside,
    ElementDetection,
    ToolbarPosition,
    PanelPosition,
    SelectionBorderStyle,
    // 贴图页下拉。
    PinLock,
    PinGroupSwitch,
    PinAutoJoin,
    PinToolbarPosition,
    PinPanelPosition,
    PinZoomMode,
}

/// 贴图页的布尔开关，共用一套「取反后写回」逻辑。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PinToggle {
    ShowMagnifier,
    Topmost,
    Shadow,
    Titlebar,
    DoubleClickClose,
    ThumbToggle,
    CloseConfirm,
    UniversalText,
    UniversalFile,
    UniversalImage,
    AutoRecognize,
    AutoCopy,
}

/// 截屏页的布尔开关，共用一套「取反后写回」逻辑。
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
enum ScreenshotToggle {
    ShowCursor,
    ShowGuides,
    ShowMagnifier,
    ShowInfoBar,
    ShowShortcutHints,
}

/// 「记录截屏历史」的可记录截屏类型。id 与 `record_history_sources` 里存的
/// 字符串一致。当前窗口截屏没有采集入口，对应复选框已移除，避免死控件。
const RECORD_HISTORY_SOURCES: [(&str, &str); 3] = [
    ("region", "普通截屏"),
    ("copyOnly", "快速截屏"),
    ("fullscreen", "截取全屏"),
];

/// (label, position key) presets for the watermark 9-grid.
const WM_POSITIONS: [(&str, &str); 9] = [
    ("左上", "topLeft"),
    ("上中", "topCenter"),
    ("右上", "topRight"),
    ("左中", "middleLeft"),
    ("居中", "center"),
    ("右中", "middleRight"),
    ("左下", "bottomLeft"),
    ("下中", "bottomCenter"),
    ("右下", "bottomRight"),
];
const WM_OPACITIES: [(&str, u8); 5] = [
    ("10%", 10),
    ("25%", 25),
    ("50%", 50),
    ("75%", 75),
    ("100%", 100),
];
const WM_SCALES: [(&str, f32); 4] = [
    ("小 0.5x", 0.5),
    ("中 1x", 1.0),
    ("大 2x", 2.0),
    ("特大 3x", 3.0),
];

/// (label, value) presets for the JPG/WebP quality dropdown. PNG ignores it.
const EXPORT_QUALITIES: [(&str, u8); 5] = [
    ("60%（最低预设）", 60),
    ("75%", 75),
    ("85%", 85),
    ("90%（推荐）", 90),
    ("100%（最大）", 100),
];

/// (label, value) presets for the annotation dropdowns.
const STROKE_WIDTHS: [(&str, f64); 4] = [
    ("细 2 px", 2.0),
    ("中 4 px", 4.0),
    ("粗 6 px", 6.0),
    ("特粗 8 px", 8.0),
];
const MOSAIC_BLOCKS: [(&str, i32); 4] = [
    ("小 8 px", 8),
    ("中 16 px", 16),
    ("大 24 px", 24),
    ("特大 32 px", 32),
];

/// (label, milliseconds) presets for the long-capture auto-scroll beat. Values
/// stay inside scroll_step's `[128, 1000] ms` range so a persisted pick is never
/// silently clamped to something other than what the dropdown showed.
const SCROLL_INTERVALS: [(&str, u32); 4] = [
    ("快 200 ms", 200),
    ("标准 600 ms", 600),
    ("慢 800 ms", 800),
    ("最慢 1000 ms", 1000),
];
/// (label, notches) presets; same reasoning as [`SCROLL_INTERVALS`].
const SCROLL_NOTCHES: [(&str, u32); 4] = [("1 格", 1), ("2 格", 2), ("4 格", 4), ("8 格", 8)];

// ---- 拼图 / PDF 默认值：解析与中文标签 -------------------------------------
// 数字框沿用「解析失败保留上次有效值」的规则，避免把用户正在输入的半截
// 数字直接写回配置文件；范围与 collage.rs 的校验保持一致。

fn parse_u32_clamped(raw: &str, min: u32, max: u32) -> Option<u32> {
    raw.trim().parse::<u32>().ok().map(|v| v.clamp(min, max))
}

fn parse_f32_clamped(raw: &str, min: f32, max: f32) -> Option<f32> {
    raw.trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(min, max))
}

/// `#RRGGBB` / `#RRGGBBAA`（不带 # 亦可）转成配置里的 RGBA。
fn parse_rgba(raw: &str) -> Option<[u8; 4]> {
    let s = raw.trim().trim_start_matches('#');
    if !s.is_ascii() || (s.len() != 6 && s.len() != 8) {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&s[range], 16).ok();
    let r = channel(0..2)?;
    let g = channel(2..4)?;
    let b = channel(4..6)?;
    let a = if s.len() == 8 { channel(6..8)? } else { 255 };
    Some([r, g, b, a])
}

fn rgba_hex(rgba: [u8; 4]) -> String {
    let [r, g, b, a] = rgba;
    format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
}

fn collage_direction_label(d: crate::config::Direction) -> &'static str {
    match d {
        crate::config::Direction::Vertical => "垂直",
        crate::config::Direction::Horizontal => "水平",
        crate::config::Direction::Custom => "网格",
    }
}

fn collage_sizing_label(v: crate::config::Sizing) -> &'static str {
    match v {
        crate::config::Sizing::Original => "原始",
        crate::config::Sizing::Uniform => "统一",
    }
}

fn pdf_stitching_label(v: crate::config::StitchingMethod) -> &'static str {
    match v {
        crate::config::StitchingMethod::PageBased => "按页拼接",
        crate::config::StitchingMethod::TemplateBased => "按模板拼版",
    }
}

fn pdf_template_label(v: crate::config::TemplateType) -> &'static str {
    match v {
        crate::config::TemplateType::TwoImages => "2 张",
        crate::config::TemplateType::ThreeImages => "3 张",
        crate::config::TemplateType::FourImages => "4 张",
        crate::config::TemplateType::SixImages => "6 张",
        crate::config::TemplateType::NineImages => "9 张",
    }
}

fn pdf_export_mode_label(v: crate::config::ExportMode) -> &'static str {
    match v {
        crate::config::ExportMode::Combined => "合并为一个 PDF",
        crate::config::ExportMode::PerPage => "每页单独导出",
    }
}

fn pdf_paper_label(v: crate::config::PaperSize) -> &'static str {
    match v {
        crate::config::PaperSize::A3 => "A3",
        crate::config::PaperSize::A4 => "A4",
        crate::config::PaperSize::A5 => "A5",
        crate::config::PaperSize::B5 => "B5",
        crate::config::PaperSize::Letter => "Letter",
        crate::config::PaperSize::Legal => "Legal",
        crate::config::PaperSize::Custom => "自定义",
        crate::config::PaperSize::ImageSize => "图片尺寸",
    }
}

fn pdf_orientation_label(v: crate::config::Orientation) -> &'static str {
    match v {
        crate::config::Orientation::Portrait => "纵向",
        crate::config::Orientation::Landscape => "横向",
    }
}

/// Render-ready snapshot of the OCR engine status. `crate::ocr::engine_status()`
/// clones the whole Settings and walks the filesystem (exe path + three
/// `metadata()` calls), so the view samples it once and reuses the snapshot
/// across frames instead of paying that cost on every render.
struct EngineStatusSnapshot {
    available: bool,
    /// 实际生效的目录（与页面上的“默认安装位置”可能不同，例如调试回退或
    /// 自定义目录）。
    dir: SharedString,
    /// 采样一次的默认安装位置，避免每帧调用 current_exe()。
    default_dir: SharedString,
    label: &'static str,
}

impl EngineStatusSnapshot {
    fn sample() -> Self {
        Self::from(crate::ocr::engine_status())
    }
}

impl From<crate::ocr::EngineStatus> for EngineStatusSnapshot {
    fn from(engine: crate::ocr::EngineStatus) -> Self {
        Self {
            available: engine.available,
            dir: engine
                .dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "未找到".into())
                .into(),
            default_dir: crate::ocr::default_models_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "程序目录\\models\\v6".into())
                .into(),
            label: engine.label,
        }
    }
}

/// 「历史图片」内嵌页的呈现方式。会话内状态即可，不需要持久化。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum HistoryViewMode {
    /// 瀑布流宫格：卡片按最矮列投放。
    #[default]
    Grid,
    /// 单列列表：小缩略图 + 详细信息。
    List,
}

pub struct SettingsView {
    page: Page,
    settings: Settings,
    /// 上一次由本页落盘时的配置基线。`persist()` 逐字段比较 `settings` 与它，
    /// 差异即为本页真正改过的字段；相同字段保留其它窗口写入的最新值。
    /// 每次保存成功后前移，避免「改回原值」被误判成从未编辑过。
    baseline: Settings,
    /// 仅全局重置时置位：默认配置可能恰好等于开窗基线，此时逐字段差异无法
    /// 区分「重置」与「没碰过」，必须强制整份采用快照让重置落盘。单页重置
    /// 改用 `force_page`，不置这个全局开关。
    force_snapshot: bool,
    /// 单页重置时置为本页：`settings_persist_merge` 常规合并完成后，把本页
    /// 字段无条件采用快照值。重置前的值恰好等于默认值时差异判断会失效，靠
    /// 此强制让重置真正落盘；只覆盖本页字段，不回滚其它窗口的共享值。
    force_page: Option<Page>,
    /// 外观下拉的全部选项（gpui-kit registry 里的主题名，默认两项在前），开窗时取样一次。
    theme_options: Vec<(String, String)>,
    status: SharedString,
    status_error: bool,
    open_select: Option<SelectId>,
    /// 「自定义工具栏」按钮是否展开下方的勾选面板。
    customize_panel_open: bool,
    prompt_open: bool,
    /// Which action is being bound, if any. The recording hook is live for
    /// exactly as long as this is `Some`.
    recording: Option<HotkeyAction>,
    /// Generation of the active recording session, paired with every write to
    /// `recording` so an old timeout cannot kill a newer recording.
    recording_seq: u64,
    autostart: bool,
    /// What is currently in the credential vault, so blurring the field without
    /// touching it does not rewrite the entry on every focus change.
    saved_api_key: String,
    base_url: Entity<TextInput>,
    api_key: Entity<TextInput>,
    model: Entity<TextInput>,
    timeout: Entity<TextInput>,
    prompt: Entity<TextInput>,
    wm_text: Entity<TextInput>,
    /// Export file-name template. Persists as typed; a blank field falls back
    /// to `config::DEFAULT_FILENAME_TEMPLATE` when a file is actually named.
    export_name: Entity<TextInput>,
    // 输出页的拼图 / PDF 默认设置。数字框解析失败时保留上次有效值。
    collage_spacing: Entity<TextInput>,
    collage_border: Entity<TextInput>,
    collage_rows: Entity<TextInput>,
    collage_columns: Entity<TextInput>,
    collage_bg: Entity<TextInput>,
    pdf_spacing: Entity<TextInput>,
    pdf_width: Entity<TextInput>,
    pdf_height: Entity<TextInput>,
    pdf_margin_top: Entity<TextInput>,
    pdf_margin_bottom: Entity<TextInput>,
    pdf_margin_left: Entity<TextInput>,
    pdf_margin_right: Entity<TextInput>,
    pdf_quality: Entity<TextInput>,
    pin_wheel_step: Entity<TextInput>,
    pin_close_history: Entity<TextInput>,
    /// 选区边框线宽（1~10）与颜色 hex 输入框。
    selection_border_width: Entity<TextInput>,
    selection_border_color: Entity<TextInput>,
    search: Entity<TextInput>,
    /// The global reset asks for a second click; a single stray press must not
    /// wipe every setting.
    confirm_reset_all: bool,
    /// Two-click arming for the plaintext-HTTP API Key warning. The second click
    /// persists `allowInsecureHttp`, so the runtime guard and this banner agree.
    http_warn_confirm: bool,
    /// 数据页「清空历史」的两击确认。与历史页的 `history_confirm_clear`
    /// 分开，避免两个页面的状态串到一起。
    data_confirm_clear: bool,
    /// Sizes for the Data page, sampled when the window opens and after a
    /// cleanup — never on every frame.
    data_bytes: u64,
    log_bytes: u64,
    /// Cached engine status for the Resources page, refreshed on window open,
    /// on entering that page, or via the page's 刷新 button — never per frame.
    engine_status: EngineStatusSnapshot,
    /// Sampling timestamp; also throttles back-to-back refresh requests so a
    /// double click cannot repeat the expensive filesystem walk.
    engine_status_at: Instant,
    /// Embedded "历史图片" gallery state. The standalone Page::History renders
    /// the same grid as the dedicated gallery window, so the images are visible
    /// the moment the user opens the nav option instead of a near-empty page.
    history_records: Vec<HistoryRecord>,
    history_thumbs: HashMap<String, Option<Arc<RenderImage>>>,
    /// Modification time (unix millis) of the image each cached thumb was built
    /// from. A cached thumb is reused only while this still matches the file.
    history_thumb_mtime: HashMap<String, u64>,
    history_thumb_seq: u64,
    history_status: SharedString,
    /// index.json 读取失败的原因。非空时历史图片可能仍在磁盘上，页面不能
    /// 显示成空历史；状态栏和空态提示都会据此给出可见的损坏说明。
    history_index_error: Option<String>,
    /// Incremented on every history status change so an older auto-fade timer
    /// cannot clear a newer message.
    history_status_seq: u64,
    history_selected: Option<String>,
    /// Destructive history actions ask for a second click, matching the global
    /// reset pattern: the first click arms, the second performs the action.
    history_confirm_delete: bool,
    history_confirm_clear: bool,
    /// Source filter for the embedded gallery ("all" / a record source string).
    history_filter: String,
    /// 宫格 / 列表切换状态（仅本次会话有效）。
    history_view_mode: HistoryViewMode,
    /// 宫格容器的真实宽度。GPUI 此 fork 没有 `on_bounds_changed`，由 canvas
    /// 的 prepaint 回调写入；仅宽度变化时 `notify()` 触发重排。
    history_grid_width: Rc<Cell<f32>>,
    /// Last history index revision this view loaded. A 400ms poll compares it
    /// with `history_revision()` so a capture copied on the bare export thread
    /// appears without the user pressing 刷新.
    history_revision_seen: u64,
    /// Guards against spawning more than one history polling task. The task
    /// clears it when the History page (or the whole window) is left.
    history_poll_running: bool,
    /// In-flight update check guards against double clicks.
    update_checking: bool,
    /// (version, url, raw tag) of a newer release, when the last check found one.
    update_available: Option<(String, String, String)>,
    /// In-app installer download state, polled from a worker's progress channel.
    update_downloading: bool,
    /// 关窗时置位的取消标志，与下载 worker、轮询任务共享同一个 Arc：窗口
    /// 销毁后 UI 收不到进度，后台不该继续为已经消失的窗口干活。
    update_download_cancel: Arc<AtomicBool>,
    update_progress: Option<crate::update::DownloadProgress>,
    downloaded_installer: Option<PathBuf>,
    /// 下载时已验证的 SHA-256；安装前用它在磁盘上复核一次文件未被替换。
    downloaded_installer_hash: Option<[u8; 32]>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    /// A numeric/colour text field whose observer parses and clamps the typed
    /// value, then persists only when the stored setting actually changed.
    /// `apply` returns whether it changed anything.
    fn number_field<F>(
        cx: &mut Context<Self>,
        subscriptions: &mut Vec<Subscription>,
        value: String,
        placeholder: &'static str,
        apply: F,
    ) -> Entity<TextInput>
    where
        F: Fn(&mut Settings, &str) -> bool + 'static,
    {
        let input = cx.new(|cx| TextInput::new(cx, value, placeholder, false));
        subscriptions.push(cx.observe(&input, move |this, input, cx| {
            let raw = input.read(cx).text().to_string();
            if apply(&mut this.settings, &raw) {
                this.persist(cx);
            }
        }));
        input
    }

    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = crate::config::get();
        let theme_options = theme::available_themes();
        let t = settings.translation.clone();
        let pin_defaults = settings.pin.clone();
        let saved_api_key = crate::translate::api_key().unwrap_or_default();

        let base_url =
            cx.new(|cx| TextInput::new(cx, t.base_url.clone(), "https://api.openai.com/v1", false));
        let api_key = cx.new(|cx| TextInput::new(cx, saved_api_key.clone(), "sk-...", true));
        let model = cx.new(|cx| TextInput::new(cx, t.model.clone(), "gpt-4o-mini", false));
        let timeout = cx.new(|cx| TextInput::new(cx, t.timeout_seconds.to_string(), "30", false));
        let prompt = cx.new(|cx| TextInput::new(cx, t.prompt.clone(), "使用内置提示词", false));
        let wm_text = cx.new(|cx| {
            TextInput::new(
                cx,
                settings.screenshot.watermark.text.clone(),
                "水印文字，留空则不显示文字",
                false,
            )
        });
        let search = cx.new(|cx| TextInput::new(cx, "", "搜索所有配置…", false).flat());

        let export_name = cx.new(|cx| {
            TextInput::new(
                cx,
                settings.screenshot.export.filename_template.clone(),
                crate::config::DEFAULT_FILENAME_TEMPLATE,
                false,
            )
        });

        let mut subscriptions = Vec::new();

        // 拼图默认设置。间距 / 边框 0..100，行数 / 列数 1..10，颜色 #RRGGBB(AA)。
        let collage = &settings.screenshot.collage;
        let collage_spacing = Self::number_field(
            cx,
            &mut subscriptions,
            collage.spacing.to_string(),
            "0",
            |s, raw| {
                let Some(v) = parse_u32_clamped(raw, 0, 100) else {
                    return false;
                };
                let slot = &mut s.screenshot.collage.spacing;
                if *slot != v {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let collage_border = Self::number_field(
            cx,
            &mut subscriptions,
            collage.border.to_string(),
            "0",
            |s, raw| {
                let Some(v) = parse_u32_clamped(raw, 0, 100) else {
                    return false;
                };
                let slot = &mut s.screenshot.collage.border;
                if *slot != v {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let collage_rows = Self::number_field(
            cx,
            &mut subscriptions,
            collage.rows.to_string(),
            "1",
            |s, raw| {
                let Some(v) = parse_u32_clamped(raw, 1, 10) else {
                    return false;
                };
                let slot = &mut s.screenshot.collage.rows;
                if *slot != v {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let collage_columns = Self::number_field(
            cx,
            &mut subscriptions,
            collage.columns.to_string(),
            "1",
            |s, raw| {
                let Some(v) = parse_u32_clamped(raw, 1, 10) else {
                    return false;
                };
                let slot = &mut s.screenshot.collage.columns;
                if *slot != v {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let collage_bg = Self::number_field(
            cx,
            &mut subscriptions,
            rgba_hex(collage.background_rgba),
            "#FFFFFFFF",
            |s, raw| {
                let Some(v) = parse_rgba(raw) else {
                    return false;
                };
                if s.screenshot.collage.background_rgba != v {
                    s.screenshot.collage.background_rgba = v;
                    true
                } else {
                    false
                }
            },
        );

        // PDF 默认设置。间距 / 边距不小于 0，自定义宽高大于 0，质量 1..100。
        let pdf = &settings.screenshot.pdf;
        let pdf_spacing = Self::number_field(
            cx,
            &mut subscriptions,
            pdf.spacing_mm.to_string(),
            "5",
            |s, raw| {
                let Some(v) = parse_f32_clamped(raw, 0.0, 1000.0) else {
                    return false;
                };
                let slot = &mut s.screenshot.pdf.spacing_mm;
                if (*slot - v).abs() > f32::EPSILON {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let pdf_width = Self::number_field(
            cx,
            &mut subscriptions,
            pdf.custom_width_mm.to_string(),
            "210",
            |s, raw| {
                let Some(v) = parse_f32_clamped(raw, 1.0, 10_000.0) else {
                    return false;
                };
                let slot = &mut s.screenshot.pdf.custom_width_mm;
                if (*slot - v).abs() > f32::EPSILON {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let pdf_height = Self::number_field(
            cx,
            &mut subscriptions,
            pdf.custom_height_mm.to_string(),
            "297",
            |s, raw| {
                let Some(v) = parse_f32_clamped(raw, 1.0, 10_000.0) else {
                    return false;
                };
                let slot = &mut s.screenshot.pdf.custom_height_mm;
                if (*slot - v).abs() > f32::EPSILON {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let pdf_margin_top = Self::number_field(
            cx,
            &mut subscriptions,
            pdf.margin_top_mm.to_string(),
            "10",
            |s, raw| {
                let Some(v) = parse_f32_clamped(raw, 0.0, 1000.0) else {
                    return false;
                };
                let slot = &mut s.screenshot.pdf.margin_top_mm;
                if (*slot - v).abs() > f32::EPSILON {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let pdf_margin_bottom = Self::number_field(
            cx,
            &mut subscriptions,
            pdf.margin_bottom_mm.to_string(),
            "10",
            |s, raw| {
                let Some(v) = parse_f32_clamped(raw, 0.0, 1000.0) else {
                    return false;
                };
                let slot = &mut s.screenshot.pdf.margin_bottom_mm;
                if (*slot - v).abs() > f32::EPSILON {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let pdf_margin_left = Self::number_field(
            cx,
            &mut subscriptions,
            pdf.margin_left_mm.to_string(),
            "10",
            |s, raw| {
                let Some(v) = parse_f32_clamped(raw, 0.0, 1000.0) else {
                    return false;
                };
                let slot = &mut s.screenshot.pdf.margin_left_mm;
                if (*slot - v).abs() > f32::EPSILON {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let pdf_margin_right = Self::number_field(
            cx,
            &mut subscriptions,
            pdf.margin_right_mm.to_string(),
            "10",
            |s, raw| {
                let Some(v) = parse_f32_clamped(raw, 0.0, 1000.0) else {
                    return false;
                };
                let slot = &mut s.screenshot.pdf.margin_right_mm;
                if (*slot - v).abs() > f32::EPSILON {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );
        let pdf_quality = Self::number_field(
            cx,
            &mut subscriptions,
            pdf.image_quality.to_string(),
            "90",
            |s, raw| {
                let Some(v) = parse_u32_clamped(raw, 1, 100) else {
                    return false;
                };
                let v = v as u8;
                let slot = &mut s.screenshot.pdf.image_quality;
                if *slot != v {
                    *slot = v;
                    true
                } else {
                    false
                }
            },
        );

        let pin_wheel_step = Self::number_field(
            cx,
            &mut subscriptions,
            pin_defaults.wheel_step.to_string(),
            "10",
            |s, raw| {
                let Some(v) = parse_u32_clamped(raw, 1, 50) else {
                    return false;
                };
                if s.pin.wheel_step != v {
                    s.pin.wheel_step = v;
                    true
                } else {
                    false
                }
            },
        );
        let pin_close_history = Self::number_field(
            cx,
            &mut subscriptions,
            pin_defaults.close_history_limit.to_string(),
            "25",
            |s, raw| {
                let Some(v) = parse_u32_clamped(raw, 0, 200) else {
                    return false;
                };
                if s.pin.close_history_limit != v {
                    s.pin.close_history_limit = v;
                    true
                } else {
                    false
                }
            },
        );
        let border_width_value = crate::config::clamped_selection_border_width(
            settings.screenshot.selection_border_width,
        );
        let selection_border_width = Self::number_field(
            cx,
            &mut subscriptions,
            border_width_value.to_string(),
            "1",
            |s, raw| {
                let Some(v) = parse_u32_clamped(raw, 1, 10) else {
                    return false;
                };
                if s.screenshot.selection_border_width != v {
                    s.screenshot.selection_border_width = v;
                    true
                } else {
                    false
                }
            },
        );
        let border_color_hex = settings.screenshot.selection_border_color.clone();
        let selection_border_color = Self::number_field(
            cx,
            &mut subscriptions,
            border_color_hex,
            "#F4F4F4",
            |s, raw| {
                if crate::config::parse_hex_color(raw).is_none() {
                    return false;
                }
                if s.screenshot.selection_border_color != raw {
                    s.screenshot.selection_border_color = raw.into();
                    true
                } else {
                    false
                }
            },
        );

        // Every field compares before storing: the inputs notify on cursor moves
        // too, and writing the file on a click would be pure churn.
        subscriptions.push(cx.observe(&base_url, |this, input, cx| {
            let value = input.read(cx).text().to_string();
            if this.settings.translation.base_url != value {
                // 授权只针对用户确认过的那个地址：任何改动（包括 http → 另一个
                // http）都要重新确认，否则一个被改过的明文端点仍能拿到 API Key。
                this.settings.translation.allow_insecure_http = false;
                this.http_warn_confirm = false;
                this.settings.translation.base_url = value;
                this.persist(cx);
            }
        }));
        subscriptions.push(cx.observe(&model, |this, input, cx| {
            let value = input.read(cx).text().to_string();
            if this.settings.translation.model != value {
                this.settings.translation.model = value;
                this.persist(cx);
            }
        }));
        subscriptions.push(cx.observe(&prompt, |this, input, cx| {
            let value = input.read(cx).text().to_string();
            if this.settings.translation.prompt != value {
                this.settings.translation.prompt = value;
                this.persist(cx);
            }
        }));
        // The watermark text persists as typed; an empty field disables the
        // text layer without touching the timestamp/image layers.
        subscriptions.push(cx.observe(&wm_text, |this, input, cx| {
            let value = input.read(cx).text().to_string();
            if this.settings.screenshot.watermark.text != value {
                this.settings.screenshot.watermark.text = value;
                this.persist(cx);
            }
        }));

        // The name template persists verbatim, blank included: `expand_name`
        // substitutes the default at naming time, so clearing the field
        // restores the shipped name instead of writing a nameless file.
        subscriptions.push(cx.observe(&export_name, |this, input, cx| {
            let value = input.read(cx).text().to_string();
            if this.settings.screenshot.export.filename_template != value {
                this.settings.screenshot.export.filename_template = value;
                this.persist(cx);
            }
        }));
        // Unparsable input keeps the last good value rather than resetting to a
        // default the user never chose; the field itself keeps what they typed.
        subscriptions.push(cx.observe(&timeout, |this, input, cx| {
            let Ok(parsed) = input.read(cx).text().trim().parse::<u32>() else {
                return;
            };
            let clamped = parsed.clamp(5, 300);
            if this.settings.translation.timeout_seconds != clamped {
                this.settings.translation.timeout_seconds = clamped;
                this.persist(cx);
            }
        }));
        // Search only filters what is drawn; nothing is written.
        subscriptions.push(cx.observe(&search, |this, _, cx| {
            this.confirm_reset_all = false;
            cx.notify();
        }));

        // The key goes to the credential vault, not the config file, so it is
        // written on blur rather than per keystroke. Same trigger as the C#
        // `ApiKeyBox_LostFocus`.
        let key_focus = api_key.read(cx).focus_handle(cx);
        let this = cx.weak_entity();
        subscriptions.push(window.on_focus_out(&key_focus, cx, move |_, _, cx| {
            if let Some(view) = this.upgrade() {
                view.update(cx, |view, cx| view.commit_api_key(cx));
            }
        }));

        let (data_bytes, log_bytes) = Self::storage_sizes();
        let engine_status = EngineStatusSnapshot::sample();

        Self {
            page: Page::General,
            baseline: settings.clone(),
            settings,
            force_snapshot: false,
            force_page: None,
            theme_options,
            status: SharedString::default(),
            status_error: false,
            open_select: None,
            customize_panel_open: false,
            prompt_open: false,
            recording: None,
            recording_seq: 0,
            autostart: system::autostart_enabled(),
            saved_api_key,
            base_url,
            api_key,
            model,
            timeout,
            prompt,
            wm_text,
            export_name,
            collage_spacing,
            collage_border,
            collage_rows,
            collage_columns,
            collage_bg,
            pdf_spacing,
            pdf_width,
            pdf_height,
            pdf_margin_top,
            pdf_margin_bottom,
            pdf_margin_left,
            pdf_margin_right,
            pdf_quality,
            pin_wheel_step,
            pin_close_history,
            selection_border_width,
            selection_border_color,
            search,
            confirm_reset_all: false,
            http_warn_confirm: false,
            data_confirm_clear: false,
            data_bytes,
            log_bytes,
            engine_status,
            engine_status_at: Instant::now(),
            history_records: Vec::new(),
            history_thumbs: HashMap::new(),
            history_thumb_mtime: HashMap::new(),
            history_thumb_seq: 0,
            history_status: SharedString::default(),
            history_index_error: None,
            history_status_seq: 0,
            history_selected: None,
            history_confirm_delete: false,
            history_confirm_clear: false,
            history_filter: "all".to_string(),
            history_view_mode: HistoryViewMode::Grid,
            // 近似值：页面内容区宽度 ≈ min(视口宽 - 导航(200) - 内边距(24*2), 内容最大宽)。
            // 首帧由 canvas prepaint 用真实宽度覆盖。
            history_grid_width: Rc::new(Cell::new(
                (f32::from(window.viewport_size().width) - 248.0).clamp(240.0, CONTENT_MAX_W),
            )),
            history_revision_seen: crate::history::history_revision(),
            history_poll_running: false,
            update_checking: false,
            update_available: None,
            update_downloading: false,
            update_download_cancel: Arc::new(AtomicBool::new(false)),
            update_progress: None,
            downloaded_installer: None,
            downloaded_installer_hash: None,
            _subscriptions: subscriptions,
        }
    }

    fn storage_sizes() -> (u64, u64) {
        let data = system::data_dir()
            .map(|p| system::dir_size(&p))
            .unwrap_or(0);
        let logs = crate::perf::log_dir()
            .map(|p| system::dir_size(&p))
            .unwrap_or(0);
        (data, logs)
    }

    fn ok(&mut self, text: impl Into<SharedString>) {
        self.status = text.into();
        self.status_error = false;
    }

    fn fail(&mut self, text: impl Into<SharedString>) {
        self.status = text.into();
        self.status_error = true;
    }

    /// The LL hook and the RegisterHotKey fallback each hold their own copy of
    /// the bindings; push every reset/recording change to both so it takes
    /// effect without a restart.
    fn reapply_hotkeys(&self) {
        crate::platform::apply_bindings(HotkeyBindings::from_settings(&self.settings.hotkeys));
    }

    /// Opens a new recording generation and returns its token. Timers from an
    /// earlier generation become no-ops once the counter moves on.
    fn next_recording_seq(&mut self) -> u64 {
        let seq = RECORDING_SEQ.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        self.recording_seq = seq;
        seq
    }

    /// 返回本次是否真的写盘成功。重置流程据此决定能否显示「已重置」；其它
    /// 调用点忽略即可（bool 不是 must_use，不会触发未使用返回值警告）。
    fn persist(&mut self, cx: &mut Context<Self>) -> bool {
        // self.settings 是开窗时的快照，拼图 / 切图窗口期间可能已各自保存过
        // 自己的字段。从最新配置出发，只把本页改过的字段换成快照值；没改过的
        // 共享字段（collage / pdf / export / appearance / split）一律保留 latest。
        let merged = settings_persist_merge(
            &crate::config::get(),
            &self.settings,
            &self.baseline,
            self.force_snapshot,
            self.force_page,
        );
        let saved = match crate::config::save(&merged) {
            Ok(()) => {
                // 保存成功才前移基线并撤销重置强制：失败时保留现场，下次
                // 重试仍应把重置的默认值写下去。force_page 同样只在成功时清除。
                self.baseline = self.settings.clone();
                self.force_snapshot = false;
                self.force_page = None;
                self.status = SharedString::default();
                self.status_error = false;
                true
            }
            Err(e) => {
                // 失败文案留在这里，调用方（尤其是重置）不得再用成功提示覆盖。
                self.fail(format!("设置保存失败：{e}"));
                false
            }
        };
        cx.notify();
        saved
    }

    fn commit_api_key(&mut self, cx: &mut Context<Self>) {
        let key = self.api_key.read(cx).text().to_string();
        if key == self.saved_api_key {
            return;
        }
        match crate::translate::set_api_key(&key) {
            Ok(()) => {
                let mut saved = true;
                if key.trim().is_empty() && self.settings.translation.allow_insecure_http {
                    self.settings.translation.allow_insecure_http = false;
                    saved = self.persist(cx);
                }
                self.http_warn_confirm = false;
                self.saved_api_key = key;
                if saved {
                    self.ok("API Key 已保存");
                }
                // 失败时 persist 已写入「设置保存失败」，不能再覆盖成成功文案。
            }
            Err(e) => self.fail(format!("API Key 保存失败：{e}")),
        }
        cx.notify();
    }

    /// 清除密钥库成功后，同步凭据缓存与输入框文本。返回的正是要写进
    /// `TextInput` 的文本；清空它是核心不变量——否则失焦时的 `commit_api_key`
    /// 会把旧文本当成新 key 再写回密钥库。抽成纯函数便于直接单测。
    fn clear_api_key_text(saved_api_key: &mut String) -> String {
        saved_api_key.clear();
        saved_api_key.clone()
    }

    /// Re-fills the text fields after a reset, so the page and the file agree.
    fn sync_inputs(&mut self, cx: &mut Context<Self>) {
        let t = self.settings.translation.clone();
        let wm = self.settings.screenshot.watermark.text.clone();
        let template = self.settings.screenshot.export.filename_template.clone();
        let key = self.saved_api_key.clone();
        self.base_url
            .update(cx, |input, cx| input.set_text(t.base_url, cx));
        self.api_key.update(cx, |input, cx| input.set_text(key, cx));
        self.export_name
            .update(cx, |input, cx| input.set_text(template, cx));
        self.model
            .update(cx, |input, cx| input.set_text(t.model, cx));
        self.timeout.update(cx, |input, cx| {
            input.set_text(t.timeout_seconds.to_string(), cx)
        });
        self.prompt
            .update(cx, |input, cx| input.set_text(t.prompt, cx));
        self.wm_text.update(cx, |input, cx| input.set_text(wm, cx));
        // 重置本页 / 全部设置后，拼图与 PDF 的输入框也要跟着配置回退。
        let c = &self.settings.screenshot.collage;
        let p = &self.settings.screenshot.pdf;
        self.collage_spacing
            .update(cx, |input, cx| input.set_text(c.spacing.to_string(), cx));
        self.collage_border
            .update(cx, |input, cx| input.set_text(c.border.to_string(), cx));
        self.collage_rows
            .update(cx, |input, cx| input.set_text(c.rows.to_string(), cx));
        self.collage_columns
            .update(cx, |input, cx| input.set_text(c.columns.to_string(), cx));
        self.collage_bg.update(cx, |input, cx| {
            input.set_text(rgba_hex(c.background_rgba), cx)
        });
        self.pdf_spacing
            .update(cx, |input, cx| input.set_text(p.spacing_mm.to_string(), cx));
        self.pdf_width.update(cx, |input, cx| {
            input.set_text(p.custom_width_mm.to_string(), cx)
        });
        self.pdf_height.update(cx, |input, cx| {
            input.set_text(p.custom_height_mm.to_string(), cx)
        });
        self.pdf_margin_top.update(cx, |input, cx| {
            input.set_text(p.margin_top_mm.to_string(), cx)
        });
        self.pdf_margin_bottom.update(cx, |input, cx| {
            input.set_text(p.margin_bottom_mm.to_string(), cx)
        });
        self.pdf_margin_left.update(cx, |input, cx| {
            input.set_text(p.margin_left_mm.to_string(), cx)
        });
        self.pdf_margin_right.update(cx, |input, cx| {
            input.set_text(p.margin_right_mm.to_string(), cx)
        });
        self.pdf_quality.update(cx, |input, cx| {
            input.set_text(p.image_quality.to_string(), cx)
        });
        let wheel = self.settings.pin.wheel_step.to_string();
        let history = self.settings.pin.close_history_limit.to_string();
        self.pin_wheel_step
            .update(cx, |input, cx| input.set_text(wheel, cx));
        self.pin_close_history
            .update(cx, |input, cx| input.set_text(history, cx));
        let border = &self.settings.screenshot;
        let border_width =
            crate::config::clamped_selection_border_width(border.selection_border_width)
                .to_string();
        self.selection_border_width
            .update(cx, |input, cx| input.set_text(border_width, cx));
        self.selection_border_color.update(cx, |input, cx| {
            input.set_text(border.selection_border_color.clone(), cx)
        });
        self.http_warn_confirm = false;
    }

    fn go_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        self.open_select = None;
        self.customize_panel_open = false;
        self.confirm_reset_all = false;
        self.http_warn_confirm = false;
        // Leaving/entering a page disarms any pending destructive confirmation.
        self.history_confirm_delete = false;
        self.history_confirm_clear = false;
        self.data_confirm_clear = false;
        self.search.update(cx, |input, cx| input.set_text("", cx));
        if self.page == Page::History {
            // Load records + decode thumbnails on entry so the grid is never a
            // blank page. Refreshing again is harmless (sequence-guarded).
            self.reload_history(cx);
            // Keep the grid in sync with captures saved elsewhere while the
            // page stays open; the task stops itself when we leave the page.
            self.start_history_poll(cx);
        }
        if self.page == Page::Resources {
            // Model files may have changed since the window opened; sample once
            // on entry instead of gating every frame on a filesystem walk.
            self.refresh_engine_status(true, cx);
        }
        cx.notify();
    }

    /// Load history as soon as the window opens. The settings window can be
    /// launched directly into the History page (or opened there before any
    /// nav click), so waiting only for `go_page` would leave a blank grid.
    fn load_history_on_open(&mut self, cx: &mut Context<Self>) {
        self.reload_history(cx);
    }

    /// Re-samples the OCR engine status. Entering the page (`force`) always
    /// refreshes; a 刷新 button click within a second of the last sample is
    /// ignored so a double click cannot repeat the expensive filesystem walk.
    fn refresh_engine_status(&mut self, force: bool, cx: &mut Context<Self>) {
        if !force && self.engine_status_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.engine_status = EngineStatusSnapshot::sample();
        self.engine_status_at = Instant::now();
        cx.notify();
    }

    fn reset_page(&mut self, cx: &mut Context<Self>) {
        if self.page == Page::General {
            // Autostart lives in the registry / Task Scheduler, not settings.json.
            // `run_as_admin` is the one General field that does live in
            // settings.json: reset it through the same model path as every other
            // page so the "reset value == default" edge case is handled by
            // force_page_fields.
            if self.autostart {
                if let Err(e) = system::set_autostart(false, self.settings.run_as_admin) {
                    self.fail(format!("开机启动设置失败：{e}"));
                    cx.notify();
                    return;
                }
                self.autostart = system::autostart_enabled();
            }
            let before_reset = self.settings.clone();
            model::reset_page(&mut self.settings, Page::General);
            model::copy_page_fields(&mut self.baseline, &before_reset, Page::General);
            self.force_page = Some(Page::General);
            if !self.persist(cx) {
                cx.notify();
                return;
            }
            self.ok("已重置「常规」");
            cx.notify();
            return;
        }
        // 先留一份重置前的快照 / 基线：只把本页字段在基线中标成「已编辑」，
        // 保存失败时还能原样回滚，不污染下次重试的判断。
        let before_reset = self.settings.clone();
        let baseline_before = self.baseline.clone();
        if model::reset_page(&mut self.settings, self.page) {
            // 外观 / 输入框 / 热键 / OCR 引擎都是可见的运行时副作用，必须等
            // persist 成功后才应用；保存失败时 UI 与运行时要继续反映磁盘配置。
            // 不能置全局 force：那会让 collage / pdf / export / appearance /
            // split 整份取开窗快照，回滚其它窗口共享的最新值。这里把本页字段
            // 的基线改成重置前的值，并置本页强制：常规合并后由 force_page_fields
            // 无条件采用本页快照，即使「重置前值 == 默认值」差异判断失效也能落盘。
            model::copy_page_fields(&mut self.baseline, &before_reset, self.page);
            self.force_page = Some(self.page);
            let saved = self.persist(cx);
            if !saved {
                // persist 已写入「设置保存失败」且未前移基线；撤销刚才的本页
                // 标记，下次重试仍按原始基线判断，也不显示「已重置」成功提示。
                // force_page 故意保留，下次保存仍会把本页重置值写下去。
                self.baseline = baseline_before;
            } else {
                // 配置真正落盘后才应用所有可见副作用，避免失败时 UI / 运行时
                // 已变成默认值、与磁盘配置不一致。
                theme::set_appearance(cx, &self.settings.appearance);
                if self.page == Page::Translate {
                    // API Key 存在密钥库而非 settings.json，删除是不可逆的外部副作用，
                    // 必须等配置成功落盘后才执行；保存失败时凭据与缓存都保持原样。
                    if let Err(e) = crate::translate::set_api_key("") {
                        // 失败提示自带「已重置」语义，不能再叠加成功提示覆盖它。
                        self.fail(format!(
                            "已重置「{}」，但清除 API Key 失败：{e}",
                            self.page.title()
                        ));
                    } else {
                        // 缓存与输入框一起清空：只清缓存的话，失焦时
                        // commit_api_key 会把旧文本再写回密钥库。
                        let text = Self::clear_api_key_text(&mut self.saved_api_key);
                        self.api_key
                            .update(cx, |input, cx| input.set_text(text, cx));
                        self.ok(format!("已重置「{}」", self.page.title()));
                    }
                } else {
                    self.ok(format!("已重置「{}」", self.page.title()));
                }
                // 同步放在清 key 之后：此时 saved_api_key 已为空，输入框不会被写回旧值。
                self.sync_inputs(cx);
                if self.page == Page::Hotkey {
                    self.reapply_hotkeys();
                }
                if self.page == Page::Resources {
                    // 模型目录被重置，缓存的引擎快照与已加载引擎都必须同步失效。
                    crate::ocr::invalidate_engine();
                    self.refresh_engine_status(true, cx);
                }
            }
        } else {
            self.ok("本页没有可重置的配置");
        }
        cx.notify();
    }

    fn reset_all(&mut self, cx: &mut Context<Self>) {
        model::reset_all(&mut self.settings);
        self.confirm_reset_all = false;
        // 全局重置包含 split 等由其它窗口写入的字段，必须整份落盘（这里全局
        // force 是语义所需，与单页重置不同）。清掉可能残留的单页强制，语义
        // 已由 force_snapshot 完整覆盖。
        self.force_snapshot = true;
        self.force_page = None;
        let saved = self.persist(cx);
        // 保存失败时 persist 已显示「设置保存失败」，不再用成功文案覆盖，
        // force_snapshot 也保留着，下次重试仍会整份写入默认配置。外观 / 输入框 /
        // 热键 / OCR 等可见副作用也只在成功落盘后应用，失败时 UI 与运行时
        // 必须继续反映磁盘上的旧配置。
        if saved {
            theme::set_appearance(cx, &self.settings.appearance);
            // sync_inputs 会用当时的 saved_api_key 回填输入框，因此清 key 之后
            // 还要再写一次空文本（见下方），否则旧 key 会被失焦写回。
            self.sync_inputs(cx);
            self.reapply_hotkeys();
            // 全局重置也会清掉 OCR 模型目录，刷新状态页快照并让旧引擎立即失效。
            crate::ocr::invalidate_engine();
            self.refresh_engine_status(true, cx);

            // 配置已成功落盘，此时才执行不可逆的外部副作用：清除密钥库中的
            // 凭据、关闭开机启动。保存失败时两者都保持原样，避免用户数据丢失。
            let key_error = crate::translate::set_api_key("").err();
            let input_text = if key_error.is_none() {
                Some(Self::clear_api_key_text(&mut self.saved_api_key))
            } else {
                None
            };
            let autostart_error = system::set_autostart(false, self.settings.run_as_admin).err();
            self.autostart = system::autostart_enabled();
            if let Some(text) = input_text {
                // 缓存清空后输入框也必须清空，否则失焦会把旧 key 写回。
                self.api_key
                    .update(cx, |input, cx| input.set_text(text, cx));
            }
            // 两类失败分别收集，避免一个失败掩盖另一个，也保留原有提示文案风格。
            let mut failures = Vec::new();
            if let Some(e) = key_error {
                failures.push(format!("清除 API Key 失败：{e}"));
            }
            if let Some(e) = autostart_error {
                failures.push(format!("关闭开机启动失败：{e}"));
            }
            if failures.is_empty() {
                self.ok("所有设置已恢复默认");
            } else {
                self.fail(format!("所有设置已恢复默认，但{}", failures.join("；")));
            }
        }
        cx.notify();
    }

    fn set_water_position(&mut self, key: &str, cx: &mut Context<Self>) {
        let wm = &mut self.settings.screenshot.watermark;
        self.open_select = None;
        if wm.position != key {
            wm.position = key.to_string();
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_water_opacity(&mut self, value: u8, cx: &mut Context<Self>) {
        let wm = &mut self.settings.screenshot.watermark;
        self.open_select = None;
        if wm.opacity != value {
            wm.opacity = value;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_water_scale(&mut self, value: f32, cx: &mut Context<Self>) {
        let wm = &mut self.settings.screenshot.watermark;
        self.open_select = None;
        if (wm.scale - value).abs() > f32::EPSILON {
            wm.scale = value;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_export_format(&mut self, format: crate::config::ImageFormat, cx: &mut Context<Self>) {
        let export = &mut self.settings.screenshot.export;
        self.open_select = None;
        if export.format != format {
            export.format = format;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_export_quality(&mut self, quality: u8, cx: &mut Context<Self>) {
        let export = &mut self.settings.screenshot.export;
        self.open_select = None;
        if export.quality != quality {
            export.quality = quality;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    // ---- 输出页：拼图 / PDF 默认设置的下拉写回 ------------------------------

    fn set_collage_direction(&mut self, v: crate::config::Direction, cx: &mut Context<Self>) {
        self.open_select = None;
        let slot = &mut self.settings.screenshot.collage.direction;
        if *slot != v {
            *slot = v;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_collage_sizing(&mut self, v: crate::config::Sizing, cx: &mut Context<Self>) {
        self.open_select = None;
        let slot = &mut self.settings.screenshot.collage.sizing;
        if *slot != v {
            *slot = v;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_pdf_stitching(&mut self, v: crate::config::StitchingMethod, cx: &mut Context<Self>) {
        self.open_select = None;
        let slot = &mut self.settings.screenshot.pdf.stitching_method;
        if *slot != v {
            *slot = v;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_pdf_template(&mut self, v: crate::config::TemplateType, cx: &mut Context<Self>) {
        self.open_select = None;
        let slot = &mut self.settings.screenshot.pdf.template_type;
        if *slot != v {
            *slot = v;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_pdf_export_mode(&mut self, v: crate::config::ExportMode, cx: &mut Context<Self>) {
        self.open_select = None;
        let slot = &mut self.settings.screenshot.pdf.export_mode;
        if *slot != v {
            *slot = v;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_pdf_paper(&mut self, v: crate::config::PaperSize, cx: &mut Context<Self>) {
        self.open_select = None;
        let slot = &mut self.settings.screenshot.pdf.paper_size;
        if *slot != v {
            *slot = v;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_pdf_orientation(&mut self, v: crate::config::Orientation, cx: &mut Context<Self>) {
        self.open_select = None;
        let slot = &mut self.settings.screenshot.pdf.orientation;
        if *slot != v {
            *slot = v;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn toggle_export_auto_save(&mut self, cx: &mut Context<Self>) {
        let export = &mut self.settings.screenshot.export;
        export.auto_save = !export.auto_save;
        self.persist(cx);
    }

    fn toggle_export_auto_copy(&mut self, cx: &mut Context<Self>) {
        let export = &mut self.settings.screenshot.export;
        export.auto_copy = !export.auto_copy;
        self.persist(cx);
    }

    fn set_scroll_interval(&mut self, ms: u32, cx: &mut Context<Self>) {
        self.open_select = None;
        if self.settings.screenshot.auto_scroll.interval_ms != ms {
            self.settings.screenshot.auto_scroll.interval_ms = ms;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn set_scroll_notches(&mut self, notches: u32, cx: &mut Context<Self>) {
        self.open_select = None;
        if self.settings.screenshot.auto_scroll.notches != notches {
            self.settings.screenshot.auto_scroll.notches = notches;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn toggle_history_enabled(&mut self, cx: &mut Context<Self>) {
        let h = &mut self.settings.history;
        h.enabled = !h.enabled;
        self.persist(cx);
    }

    fn toggle_history_keep(&mut self, cx: &mut Context<Self>) {
        let h = &mut self.settings.history;
        h.keep_permanently = !h.keep_permanently;
        self.persist(cx);
        // Retention changes apply to records already on disk, not just future
        // ones, so the new limit takes effect the moment it is chosen.
        self.prune_history_now();
    }

    /// Sentinel value used by the retention dropdown for the "永久" option.
    /// `0` never matches a numbered day option, and maps to `keep_permanently`.
    const RETENTION_FOREVER: u64 = 0;

    fn set_history_retention(&mut self, days: u64, cx: &mut Context<Self>) {
        self.open_select = None;
        let changed = {
            let h = &mut self.settings.history;
            if days == Self::RETENTION_FOREVER {
                // "永久" is a view of the same flag as the "始终保留" toggle —
                // never delete by age, no matter what retention_days says.
                let c = !h.keep_permanently;
                h.keep_permanently = true;
                c
            } else {
                let c = h.retention_days != days || h.keep_permanently;
                h.retention_days = days;
                h.keep_permanently = false;
                c
            }
        };
        if changed {
            self.persist(cx);
            self.prune_history_now();
        } else {
            cx.notify();
        }
    }

    fn set_history_limit(&mut self, limit: usize, cx: &mut Context<Self>) {
        self.open_select = None;
        if self.settings.history.limit != limit {
            self.settings.history.limit = limit;
            self.persist(cx);
            self.prune_history_now();
        } else {
            cx.notify();
        }
    }

    fn set_history_disk(&mut self, mib: u64, cx: &mut Context<Self>) {
        self.open_select = None;
        if self.settings.history.max_disk_mib != mib {
            self.settings.history.max_disk_mib = mib;
            self.persist(cx);
            self.prune_history_now();
        } else {
            cx.notify();
        }
    }

    /// Apply the current retention policy to records already on disk. Each
    /// history setter calls this after persisting so lowering a limit removes
    /// the excess immediately instead of waiting for the next save or launch.
    fn prune_history_now(&self) {
        let h = &self.settings.history;
        if let Ok(dir) = h.store_dir() {
            let store = crate::history::HistoryStore::new(dir);
            let retention = h.retention();
            // Prune is best-effort: a failed cleanup must never make the
            // settings change look like it failed to save.
            let _ = store.prune(&retention);
        }
    }

    fn open_history(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        crate::history_window::show_history_window(cx);
    }

    /// Build the store for the effective history directory (config override or
    /// the default `capture_history` folder).
    fn history_store(&self) -> Option<HistoryStore> {
        self.settings
            .history
            .store_dir()
            .ok()
            .map(HistoryStore::new)
    }

    /// Refresh the embedded history grid from disk. Called whenever the user
    /// opens the History page or changes its contents.
    fn reload_history(&mut self, cx: &mut Context<Self>) {
        match self.history_store() {
            None => {
                self.history_records.clear();
                self.history_index_error = Some("历史目录不可用".to_string());
                self.set_history_status("历史目录不可用，无法读取历史记录", true, cx);
            }
            Some(store) => match store.list() {
                Ok(records) => {
                    let was_corrupt = self.history_index_error.is_some();
                    self.history_records = records;
                    self.history_index_error = None;
                    // 索引恢复正常时撤掉之前的损坏提示；用户刚做的删除 / 清空
                    // 提示是别的状态文案，不要误伤。
                    if was_corrupt {
                        self.history_status = SharedString::default();
                        self.history_status_seq = self.history_status_seq.wrapping_add(1);
                    }
                }
                Err(e) => {
                    self.history_records.clear();
                    self.history_index_error = Some(e.to_string());
                    self.set_history_status(
                        format!("历史索引损坏，图片仍在磁盘：{e}。可从「打开文件夹」查看原图。"),
                        true,
                        cx,
                    );
                }
            },
        }
        if self
            .history_selected
            .as_ref()
            .is_some_and(|id| !self.history_records.iter().any(|r| &r.id == id))
        {
            self.history_selected = None;
        }
        self.load_history_thumbs_async(cx);
        cx.notify();
    }

    /// Starts the single 400ms revision poll for the embedded history page.
    /// `add()` bumps the global revision from a bare export thread, so polling
    /// is the only way the view can notice a new capture. Each tick does at
    /// most one reload even if several captures landed in between. The loop
    /// ends itself when the view is dropped or the page is left; the running
    /// flag prevents duplicate loops when the page is re-entered.
    fn start_history_poll(&mut self, cx: &mut Context<Self>) {
        if self.history_poll_running {
            return;
        }
        self.history_poll_running = true;
        // Anything saved before this point is already covered by the reload
        // performed on page entry.
        self.history_revision_seen = crate::history::history_revision();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
                // `this.update` fails once the window is gone — exit promptly.
                let keep_polling = this
                    .update(cx, |this, cx| {
                        if this.page != Page::History {
                            this.history_poll_running = false;
                            return false;
                        }
                        let revision = crate::history::history_revision();
                        if revision != this.history_revision_seen {
                            this.history_revision_seen = revision;
                            this.reload_history(cx);
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_polling {
                    break;
                }
            }
        })
        .detach();
    }

    /// Decode thumbnails off the UI thread, reusing the gallery window's
    /// decoder. A sequence number prevents a stale batch from overwriting a
    /// newer refresh's thumbnails. Thumbs already decoded from an unchanged
    /// file are reused, so re-entering the page does not re-decode everything.
    fn load_history_thumbs_async(&mut self, cx: &mut Context<Self>) {
        let seq = {
            self.history_thumb_seq = self.history_thumb_seq.wrapping_add(1);
            self.history_thumb_seq
        };
        let Some(store) = self.history_store() else {
            return;
        };
        // Keep only cache entries for records that still exist, then work out
        // which images actually need (re)decoding. A file is unchanged when its
        // modification time still matches the one the cached thumb was built
        // from; a missing/unreadable mtime forces a fresh decode.
        let live_ids: std::collections::HashSet<String> =
            self.history_records.iter().map(|r| r.id.clone()).collect();
        self.history_thumbs.retain(|id, _| live_ids.contains(id));
        self.history_thumb_mtime
            .retain(|id, _| live_ids.contains(id));

        let mut jobs: Vec<(String, PathBuf)> = Vec::new();
        for r in &self.history_records {
            let path = store.image_path(r);
            let mtime = history_file_mtime(&path);
            let cached = self.history_thumbs.contains_key(&r.id)
                && self.history_thumb_mtime.get(&r.id).copied() == mtime
                && mtime.is_some();
            if !cached {
                jobs.push((r.id.clone(), path));
            }
        }
        if jobs.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let decoded = cx
                .background_executor()
                .spawn(async move {
                    jobs.into_iter()
                        .map(|(id, path)| {
                            let mtime = history_file_mtime(&path);
                            let thumb = crate::history_window::load_thumb(&path);
                            (id, mtime, thumb)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                if this.history_thumb_seq == seq {
                    for (id, mtime, thumb) in decoded {
                        if let Some(m) = mtime {
                            this.history_thumb_mtime.insert(id.clone(), m);
                        }
                        this.history_thumbs.insert(id, thumb);
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn history_selected_record(&self) -> Option<&HistoryRecord> {
        let id = self.history_selected.as_ref()?;
        self.history_records.iter().find(|r| &r.id == id)
    }

    /// Set a history status line. Transient messages fade after a few seconds;
    /// confirmation prompts (`persist`) stay until the next action. A sequence
    /// number stops an older timer from clearing a newer message.
    fn set_history_status(
        &mut self,
        text: impl Into<SharedString>,
        persist: bool,
        cx: &mut Context<Self>,
    ) {
        self.history_status = text.into();
        self.history_status_seq = self.history_status_seq.wrapping_add(1);
        let seq = self.history_status_seq;
        if !persist {
            cx.spawn(async move |this, cx: &mut AsyncApp| {
                cx.background_executor().timer(Duration::from_secs(4)).await;
                this.update(cx, |this, cx| {
                    if this.history_status_seq == seq {
                        this.history_status = SharedString::default();
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn history_copy_selected(&mut self, cx: &mut Context<Self>) {
        let Some(record) = self.history_selected_record().cloned() else {
            self.set_history_status("请先选择一张图片", false, cx);
            return;
        };
        let Some(store) = self.history_store() else {
            return;
        };
        let path = store.image_path(&record);
        match crate::codec::decode_bgra(&path) {
            Some((bgra, w, h)) => match crate::codec::set_clipboard_dib(&bgra, w, h, w * 4) {
                Ok(()) => self.set_history_status("已复制到剪贴板", false, cx),
                Err(e) => self.set_history_status(format!("复制失败：{e}"), false, cx),
            },
            None => self.set_history_status("图片解码失败", false, cx),
        }
    }

    fn history_delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.history_selected.clone() else {
            self.set_history_status("请先选择一张图片", false, cx);
            return;
        };
        // Two-click confirmation: deletion is a two-phase, irreversible commit.
        if !self.history_confirm_delete {
            self.history_confirm_delete = true;
            self.history_confirm_clear = false;
            self.set_history_status("再次点击「删除」以确认删除这张图片", true, cx);
            return;
        }
        self.history_confirm_delete = false;
        let removed = match self.history_store() {
            Some(store) => store.remove(&id).map_err(|e| e.to_string()),
            None => Err("历史目录不可用".to_string()),
        };
        match removed {
            Ok(()) => {
                self.set_history_status("已删除", false, cx);
                self.history_selected = None;
                self.reload_history(cx);
            }
            Err(e) => self.set_history_status(
                format!("删除失败：{e}。若为索引损坏，图片仍在磁盘。"),
                false,
                cx,
            ),
        }
    }

    fn history_clear(&mut self, cx: &mut Context<Self>) {
        // Two-click confirmation: clearing wipes every record irreversibly.
        if !self.history_confirm_clear {
            self.history_confirm_clear = true;
            self.history_confirm_delete = false;
            self.set_history_status("再次点击「清空」以确认清空全部历史", true, cx);
            return;
        }
        self.history_confirm_clear = false;
        match self.history_store_clear() {
            Ok(()) => {
                self.set_history_status("已清空历史", false, cx);
                self.history_selected = None;
                self.reload_history(cx);
            }
            Err(msg) => self.set_history_status(msg, false, cx),
        }
    }

    /// Source filter for the embedded gallery. Changing it also disarms any
    /// pending destructive confirmation so a hidden item cannot be deleted.
    fn set_history_filter(&mut self, filter: &str, cx: &mut Context<Self>) {
        self.history_filter = filter.to_string();
        self.open_select = None;
        self.history_confirm_delete = false;
        self.history_confirm_clear = false;
        // A filtered-out selection must not stay selected.
        if let Some(id) = self.history_selected.clone() {
            let visible = self
                .history_records
                .iter()
                .any(|r| r.id == id && history_matches_filter(&r.source, &self.history_filter));
            if !visible {
                self.history_selected = None;
            }
        }
        cx.notify();
    }

    fn history_open_folder(&mut self, cx: &mut Context<Self>) {
        // Always open the history root — consistent with the gallery window and
        // with CitePix. Per-record discovery stays in the dedicated gallery.
        let result = self
            .history_dir()
            .and_then(|dir| system::open_folder(&dir).map_err(|e| format!("打开失败：{e}")));
        match result {
            Ok(()) => self.set_history_status("已打开历史目录", false, cx),
            Err(e) => self.set_history_status(e, false, cx),
        }
    }

    fn open_image_split(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        crate::imagesplit_window::show_split_window(cx);
    }

    fn open_collage(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        crate::collage_window::show_collage_window(cx);
    }

    /// Shared low-level history operations. Both the Data page and the embedded
    /// History page call these so the actual logic lives in one place; each
    /// caller keeps its own status channel and confirmation behaviour.
    fn history_dir(&self) -> std::result::Result<PathBuf, String> {
        self.settings
            .history
            .store_dir()
            .map_err(|e| format!("历史目录不可用：{e}"))
    }

    fn history_store_clear(&self) -> std::result::Result<(), String> {
        crate::history::HistoryStore::new(self.history_dir()?)
            .clear()
            .map_err(|e| format!("清空历史失败：{e}"))
    }

    /// Data-page "清空截图历史" button. Uses the Data page's status line and
    /// the same two-click confirmation as the History page.
    fn clear_history(&mut self, cx: &mut Context<Self>) {
        if !self.data_confirm_clear {
            self.data_confirm_clear = true;
            self.ok("再次点击「清空」以确认清空全部历史记录");
            cx.notify();
            return;
        }
        self.data_confirm_clear = false;
        match self.history_store_clear() {
            Ok(()) => self.ok("截图历史已清空"),
            Err(e) => self.fail(e),
        }
        cx.notify();
    }

    /// Data-page "打开历史目录" button. Uses the Data page's status line.
    fn open_history_dir(&mut self, cx: &mut Context<Self>) {
        match self
            .history_dir()
            .and_then(|dir| system::open_folder(&dir).map_err(|e| format!("打开历史目录失败：{e}")))
        {
            Ok(()) => {}
            Err(e) => self.fail(e),
        }
        cx.notify();
    }

    fn choose_history_dir(&mut self, cx: &mut Context<Self>) {
        // Start the picker at the effective directory (configured or default).
        let current = self.settings.history.store_dir().unwrap_or_default();
        match system::pick_folder(&current) {
            Ok(None) => {}
            Ok(Some(dir)) => {
                self.settings.history.dir = Some(dir);
                self.persist(cx);
            }
            Err(e) => {
                // 系统选择器返回的是英文 HRESULT（如 0x80004005），补上中文
                // 上下文；取消走上面的 Ok(None)，不会误报。
                self.fail(format!("选择目录失败：{e}，请重试"));
                cx.notify();
            }
        }
    }

    fn toggle_water_timestamp(&mut self, cx: &mut Context<Self>) {
        let wm = &mut self.settings.screenshot.watermark;
        wm.timestamp = !wm.timestamp;
        self.persist(cx);
    }

    fn toggle_water_tiled(&mut self, cx: &mut Context<Self>) {
        let wm = &mut self.settings.screenshot.watermark;
        wm.tiled = !wm.tiled;
        self.persist(cx);
    }

    fn clear_water_image(&mut self, cx: &mut Context<Self>) {
        let wm = &mut self.settings.screenshot.watermark;
        if wm.image_path.is_some() {
            wm.image_path = None;
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn choose_water_image(&mut self, cx: &mut Context<Self>) {
        let start = self
            .settings
            .screenshot
            .watermark
            .image_path
            .clone()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        match system::pick_image(&start) {
            Ok(None) => {}
            Ok(Some(path)) => {
                self.settings.screenshot.watermark.image_path = Some(path);
                self.persist(cx);
            }
            Err(e) => {
                // 同上：补中文上下文，取消是 Ok(None)，不会进入这里。
                self.fail(format!("选择图片失败：{e}，请重试"));
                cx.notify();
            }
        }
    }

    fn toggle_toolbar_item(&mut self, id: &str, cx: &mut Context<Self>) {
        // 矩形/圆形常驻显示，任何调用入口都不允许再隐藏它们。
        if id == toolbar::RECT || id == toolbar::ELLIPSE {
            return;
        }
        let hidden = &mut self.settings.screenshot.hidden_toolbar_items;
        match hidden.iter().position(|h| h == id) {
            Some(i) => {
                hidden.remove(i);
            }
            None => hidden.push(id.to_string()),
        }
        self.persist(cx);
    }

    /// 截屏页的布尔开关统一入口：取反后写回配置并持久化。
    fn toggle_screenshot_bool(&mut self, which: ScreenshotToggle, cx: &mut Context<Self>) {
        let s = &mut self.settings.screenshot;
        let slot = match which {
            ScreenshotToggle::ShowCursor => &mut s.show_cursor,
            ScreenshotToggle::ShowGuides => &mut s.show_guides,
            ScreenshotToggle::ShowMagnifier => &mut s.show_magnifier,
            ScreenshotToggle::ShowInfoBar => &mut s.show_info_bar,
            ScreenshotToggle::ShowShortcutHints => &mut s.show_shortcut_hints,
        };
        *slot = !*slot;
        self.persist(cx);
    }

    fn toggle_pin_bool(&mut self, which: PinToggle, cx: &mut Context<Self>) {
        let slot = match which {
            PinToggle::ShowMagnifier => &mut self.settings.pin.show_magnifier,
            PinToggle::Topmost => &mut self.settings.pin.topmost,
            PinToggle::Shadow => &mut self.settings.pin.shadow,
            PinToggle::Titlebar => &mut self.settings.pin.titlebar,
            PinToggle::DoubleClickClose => &mut self.settings.pin.double_click_close,
            PinToggle::ThumbToggle => &mut self.settings.pin.thumb_toggle,
            PinToggle::CloseConfirm => &mut self.settings.pin.close_confirm,
            PinToggle::UniversalText => &mut self.settings.pin.universal_text,
            PinToggle::UniversalFile => &mut self.settings.pin.universal_file,
            PinToggle::UniversalImage => &mut self.settings.pin.universal_image,
            PinToggle::AutoRecognize => &mut self.settings.pin.auto_recognize,
            PinToggle::AutoCopy => &mut self.settings.pin.auto_copy,
        };
        *slot = !*slot;
        self.persist(cx);
    }

    fn pin_toggle_row(
        &self,
        cx: &mut Context<Self>,
        id: &'static str,
        title: &'static str,
        desc: &'static str,
        value: bool,
        which: PinToggle,
    ) -> gpui::Div {
        widgets::row(title, desc).child(widgets::control(widgets::toggle(id, value).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.toggle_pin_bool(which, cx)),
        )))
    }

    fn pin_check_box(
        &self,
        cx: &mut Context<Self>,
        id: &'static str,
        label: &'static str,
        value: bool,
        which: PinToggle,
    ) -> gpui::Stateful<gpui::Div> {
        widgets::checkbox(id, label, value).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.toggle_pin_bool(which, cx)),
        )
    }

    /// 「记录截屏历史」的四类截屏复选框；配置里的 vec 就是唯一存储。
    fn toggle_record_history_source(&mut self, id: &str, cx: &mut Context<Self>) {
        let sources = &mut self.settings.screenshot.record_history_sources;
        match sources.iter().position(|s| s == id) {
            Some(i) => {
                sources.remove(i);
            }
            None => sources.push(id.to_string()),
        }
        self.persist(cx);
    }

    /// 「自定义...」按钮展开 / 收起下方的工具栏勾选面板。
    fn toggle_customize_panel(&mut self, cx: &mut Context<Self>) {
        self.customize_panel_open = !self.customize_panel_open;
        cx.notify();
    }

    fn language_label(&self) -> SharedString {
        let code = self.settings.translation.target_language.trim();
        match LANGUAGES.iter().find(|(c, _)| *c == code) {
            Some((c, name)) => format!("{name} ({c})").into(),
            // A code typed into settings.json by hand still shows as itself.
            None if code.is_empty() => "未设置".into(),
            None => code.to_string().into(),
        }
    }

    fn set_language(&mut self, code: &str, cx: &mut Context<Self>) {
        self.open_select = None;
        if self.settings.translation.target_language != code {
            self.settings.translation.target_language = code.to_string();
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    fn appearance_label(&self) -> SharedString {
        let id = self.settings.appearance.trim();
        self.theme_options
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(id))
            .map(|(_, label)| label.clone())
            // 手工编辑的设置值没有对应选项时，原样显示，方便发现拼写错误。
            .unwrap_or_else(|| id.to_string())
            .into()
    }

    /// Appearance switch: save first, then push the theme into gpui-kit and
    /// refresh every window so the toolbar and this window repaint together.
    /// `name` 是 registry 里的主题注册名；未知值在 `theme::set_appearance` 里回退默认浅色。
    fn set_appearance(&mut self, name: &str, cx: &mut Context<Self>) {
        self.open_select = None;
        if !self.settings.appearance.eq_ignore_ascii_case(name) {
            self.settings.appearance = name.to_string();
            theme::set_appearance(cx, name);
            self.persist(cx);
        } else {
            cx.notify();
        }
    }

    /// A row whose right side is a dropdown. The open list is positioned
    /// absolutely and painted in the deferred pass, so it overlays the rows
    /// below instead of reflowing the page. Callers build the option rows
    /// themselves: each carries its own click handler.
    fn select_row(
        &self,
        cx: &mut Context<Self>,
        which: SelectId,
        header: &'static str,
        desc: &'static str,
        current: SharedString,
        rows: Vec<gpui::Stateful<gpui::Div>>,
    ) -> gpui::Div {
        let open = self.open_select == Some(which);
        let field = widgets::select(
            SharedString::from(format!("select-{}", which as u8)),
            current,
            open,
            Some(INLINE_FIELD_W),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                this.open_select = if open { None } else { Some(which) };
                cx.notify();
            }),
        );
        div()
            .relative()
            .child(widgets::row(header, desc).child(widgets::control(field)))
            .when(open, |el| {
                el.child(deferred(
                    div()
                        .absolute()
                        .top(px(DROPDOWN_TOP))
                        .right(px(16.))
                        .w(px(INLINE_FIELD_W))
                        .child(
                            widgets::select_list(SharedString::from(format!(
                                "select-list-{}",
                                which as u8
                            )))
                            .children(rows),
                        ),
                ))
            })
    }

    /// 从配置枚举的 `ALL`/`id`/`label` 生成一行下拉，截屏页新增的枚举共用它。
    /// `apply` 返回是否真的改了值，未变化时不写文件。
    #[allow(clippy::too_many_arguments)]
    fn enum_select_row<T, F>(
        &self,
        cx: &mut Context<Self>,
        which: SelectId,
        header: &'static str,
        desc: &'static str,
        current: T,
        all: &[T],
        id_of: fn(T) -> &'static str,
        label_of: fn(T) -> &'static str,
        apply: F,
    ) -> gpui::Div
    where
        T: Copy + PartialEq + 'static,
        F: Fn(&mut Settings, T) -> bool + Clone + 'static,
    {
        let current_label: SharedString = label_of(current).into();
        let rows: Vec<gpui::Stateful<gpui::Div>> = all
            .iter()
            .copied()
            .map(|value| {
                let apply = apply.clone();
                widgets::select_option(
                    SharedString::from(format!("{which:?}-{}", id_of(value))),
                    label_of(value),
                    value == current,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.open_select = None;
                        if apply(&mut this.settings, value) {
                            this.persist(cx);
                        } else {
                            cx.notify();
                        }
                    }),
                )
            })
            .collect();
        self.select_row(cx, which, header, desc, current_label, rows)
    }

    fn hotkey_text(&self, action: HotkeyAction) -> SharedString {
        if self.recording == Some(action) {
            return "按下组合键…".into();
        }
        let stored = match action {
            HotkeyAction::Region => &self.settings.hotkeys.region,
            HotkeyAction::CopyOnly => &self.settings.hotkeys.copy_only,
            HotkeyAction::Fullscreen => &self.settings.hotkeys.fullscreen,
        };
        match hotkey::parse(stored) {
            Some(h) => hotkey::format(h).into(),
            // Show the raw (bad) string so a hand-edited config can be spotted
            // and corrected here instead of masquerading as "未设置".
            None => stored.clone().into(),
        }
    }

    fn start_recording(&mut self, action: HotkeyAction, cx: &mut Context<Self>) {
        // Clicking the field that is already recording cancels it.
        if self.recording == Some(action) {
            self.cancel_recording(cx);
            return;
        }
        let seq = self.next_recording_seq();
        self.recording = Some(action);
        self.ok("按下新的组合键");
        crate::platform::start_recording();

        // Bounded no matter what happens to this window: the recorder swallows
        // bindable keys while it runs, so it must never outlive a stuck UI.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RECORDING_TIMEOUT).await;
            // A superseded timer must not tear down a newer recording — not
            // even when the view that armed it has been dropped in the meantime.
            if RECORDING_SEQ.load(Ordering::SeqCst) != seq {
                return;
            }
            crate::platform::stop_recording();
            let _ = this.update(cx, |view, cx| {
                if view.recording_seq != seq || view.recording != Some(action) {
                    return;
                }
                view.recording = None;
                view.next_recording_seq();
                view.fail("录制超时，快捷键未改动");
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        crate::platform::stop_recording();
        self.recording = None;
        self.next_recording_seq();
        self.status = SharedString::default();
        self.status_error = false;
        cx.notify();
    }

    fn hotkey_recorded(&mut self, pressed: Hotkey, cx: &mut Context<Self>) {
        let Some(action) = self.recording.take() else {
            crate::platform::stop_recording();
            self.next_recording_seq();
            return;
        };
        crate::platform::stop_recording();
        self.next_recording_seq();

        // 裸键（如单独按 A）一旦保存，全局钩子会吞掉系统里每一次该按键。
        // hook 侧解析入口也会拒绝裸键，这里在录制入口再拦一道：PrtSc
        // 可单独使用，其余组合至少要有 Ctrl / Alt / Shift / Win。
        // 这里直接退出录制，与 10 秒超时的收尾方式保持一致。
        if !recording_hotkey_acceptable(pressed) {
            self.fail("热键至少需要 Ctrl / Alt / Shift / Win 中的一个（PrtSc 除外）");
            cx.notify();
            return;
        }

        let text = hotkey::format(pressed);
        // Two actions on one combo means one of them silently never fires.
        // Compare the effective bindings, not the raw strings: a bad string
        // falls back to its default in `from_settings`, and that default can
        // still clash with the combo just recorded.
        let bindings = HotkeyBindings::from_settings(&self.settings.hotkeys);
        let clash = [
            HotkeyAction::Region,
            HotkeyAction::CopyOnly,
            HotkeyAction::Fullscreen,
        ]
        .into_iter()
        .find(|other| *other != action && bindings.binding_for(*other) == pressed);

        if let Some(other) = clash {
            let name = HOTKEY_ROWS
                .iter()
                .find(|(a, _, _, _)| *a == other)
                .map(|(_, _, title, _)| *title)
                .unwrap_or("另一项");
            self.fail(format!("{text} 已被「{name}」占用"));
            cx.notify();
            return;
        }

        match action {
            HotkeyAction::Region => self.settings.hotkeys.region = text.clone(),
            HotkeyAction::CopyOnly => self.settings.hotkeys.copy_only = text.clone(),
            HotkeyAction::Fullscreen => self.settings.hotkeys.fullscreen = text.clone(),
        }
        let saved = self.persist(cx);
        // 保存失败时 persist 已写入「设置保存失败」，磁盘上仍是旧绑定；此时若
        // 照旧 reapply，热键会即时变成新值，UI 报错但运行时已生效，重启又回退。
        // 失败时也不取走 fallback 注册失败记录——本次根本没有触发重注册。
        if saved {
            self.reapply_hotkeys();
            // fallback 模式（LL hook 装不上）下 RegisterHotKey 可能被别的程序占用：
            // 旧代码丢弃 Result 仍提示绑定成功。这里把真实失败显示出来；LL 模式
            // 没有走 RegisterHotKey，查询为 None，文案不变。
            if let Some(detail) = hotkey::take_fallback_register_failure() {
                self.fail(format!("热键被其它程序占用，未生效：{detail}"));
            } else if self.status.is_empty() {
                self.ok(format!("已设为 {text}"));
            }
        }
        cx.notify();
    }

    fn set_autostart(&mut self, cx: &mut Context<Self>) {
        let want = !self.autostart;
        match system::set_autostart(want, self.settings.run_as_admin) {
            Ok(()) => {
                self.status = SharedString::default();
                self.status_error = false;
            }
            Err(e) => self.fail(format!("开机启动设置失败：{e}")),
        }
        // Re-read rather than assume: the registry / task scheduler is the truth.
        self.autostart = system::autostart_enabled();
        cx.notify();
    }

    fn set_run_as_admin(&mut self, cx: &mut Context<Self>) {
        let want = !self.settings.run_as_admin;
        // 若开机启动已开启，必须按新模式重建注册项：普通 Run 键无法提权，
        // 管理员模式要用最高权限计划任务，两者不能同时存在。
        if self.autostart {
            if let Err(e) = system::set_autostart(true, want) {
                self.fail(format!("开机启动权限设置失败：{e}"));
                cx.notify();
                return;
            }
            self.autostart = system::autostart_enabled();
        }
        self.settings.run_as_admin = want;
        if self.persist(cx) {
            self.ok(if want {
                "已开启以管理员启动，下次启动生效"
            } else {
                "已关闭以管理员启动，下次启动生效"
            });
        }
        cx.notify();
    }

    fn choose_save_dir(&mut self, cx: &mut Context<Self>) {
        let current = crate::config::screenshot_dir().unwrap_or_default();
        match system::pick_folder(&current) {
            // Cancel is not a failure, so it leaves the status line alone.
            Ok(None) => {}
            Ok(Some(dir)) => {
                self.settings.screenshot.save_dir = Some(dir);
                self.persist(cx);
            }
            Err(e) => {
                self.fail(format!("{e}"));
                cx.notify();
            }
        }
    }

    fn choose_models_dir(&mut self, cx: &mut Context<Self>) {
        let start = self
            .settings
            .ocr
            .models_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        match system::pick_folder(&start) {
            Ok(None) => {}
            Ok(Some(dir)) => {
                // 目录里的模型文件可能还不齐，但设置本身要立刻生效。资源页
                // 用的是缓存快照（不是每次渲染重算），所以改完必须主动刷新。
                self.settings.ocr.models_dir = Some(dir);
                self.persist(cx);
                // 已加载的引擎最长会缓存 60 秒旧模型，换目录必须立刻失效。
                crate::ocr::invalidate_engine();
                self.refresh_engine_status(true, cx);
            }
            Err(e) => {
                self.fail(format!("{e}"));
                cx.notify();
            }
        }
    }

    fn clear_models_dir(&mut self, cx: &mut Context<Self>) {
        if self.settings.ocr.models_dir.take().is_some() {
            self.persist(cx);
            // 清除目录后下一次识别必须重新按默认位置加载，不能用缓存的旧模型。
            crate::ocr::invalidate_engine();
            self.refresh_engine_status(true, cx);
        } else {
            cx.notify();
        }
    }

    fn open_folder(
        &mut self,
        dir: crate::error::Result<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let result = dir.and_then(|path| system::open_folder(&path));
        if let Err(e) = result {
            self.fail(format!("{e}"));
        }
        cx.notify();
    }

    fn clear_logs(&mut self, cx: &mut Context<Self>) {
        match system::clear_logs() {
            Ok(0) => self.ok("没有可清理的日志"),
            Ok(n) => self.ok(format!("已清理 {n} 个日志文件")),
            Err(e) => self.fail(format!("清理失败：{e}")),
        }
        let (data, logs) = Self::storage_sizes();
        self.data_bytes = data;
        self.log_bytes = logs;
        cx.notify();
    }

    fn check_update(&mut self, cx: &mut Context<Self>) {
        if self.update_checking {
            return;
        }
        self.update_checking = true;
        self.ok("正在检查更新…");
        let task = cx.background_spawn(async move { crate::update::check_latest_or_none() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.update_checking = false;
                match result {
                    Ok(Some(release))
                        if crate::update::is_newer(
                            &release.version,
                            crate::update::current_version(),
                        ) =>
                    {
                        view.ok(format!("发现新版本 v{}", release.version));
                        view.update_available = Some((release.version, release.url, release.tag));
                    }
                    Ok(Some(_)) => {
                        view.update_available = None;
                        view.ok("当前已是最新版本");
                    }
                    // GitHub 在仓库还没有任何 Release 时 /releases/latest 返回 404。
                    // 这不是网络故障，给用户一个可理解的提示而不是 HTTP 错误码。
                    Ok(None) => {
                        view.update_available = None;
                        view.ok("暂无发布版本");
                    }
                    Err(e) => view.fail(format!("检查更新失败：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_update_page(&mut self, cx: &mut Context<Self>) {
        let Some((_, url, _)) = &self.update_available else {
            return;
        };
        if let Err(e) = crate::update::open_release_page(url) {
            self.fail(e.to_string());
            cx.notify();
        }
    }

    /// Streams the installer in-app instead of handing the URL to a browser.
    /// The downloader is blocking WinHTTP, so it runs on a dedicated thread and
    /// reports progress over a channel that the UI task drains every 150 ms.
    fn download_update(&mut self, cx: &mut Context<Self>) {
        if self.update_downloading {
            return;
        }
        let Some((version, _, tag)) = self.update_available.clone() else {
            return;
        };
        self.update_downloading = true;
        self.update_progress = None;
        self.downloaded_installer = None;
        self.downloaded_installer_hash = None;
        // 新一次下载重置上次的取消标记；关窗回调持有的是同一个 Arc。
        self.update_download_cancel.store(false, Ordering::Release);
        let cancel = self.update_download_cancel.clone();
        cx.notify();

        enum Msg {
            Progress(crate::update::DownloadProgress),
            Done(std::result::Result<(PathBuf, [u8; 32]), String>),
        }
        let (tx, rx) = mpsc::channel::<Msg>();
        let finished = Arc::new(AtomicBool::new(false));
        let flag = finished.clone();
        let worker_cancel = cancel.clone();
        std::thread::spawn(move || {
            if worker_cancel.load(Ordering::Acquire) {
                flag.store(true, Ordering::Release);
                return;
            }
            let outcome =
                crate::update::download_release_asset_with_hash(&version, Some(&tag), |progress| {
                    // 关窗后不再有接收方：跳过进度上报，别再堆积消息。
                    // net 层的回调是 FnMut(DownloadProgress) 且无返回值，
                    // 无法从这里直接中断 WinHTTP 读取，只能等它自己收尾。
                    if worker_cancel.load(Ordering::Acquire) {
                        return;
                    }
                    let _ = tx.send(Msg::Progress(progress));
                });
            if worker_cancel.load(Ordering::Acquire) {
                // 关窗已置位取消标记：轮询任务会立刻退出并 drop rx，Done 发到
                // 断开的通道也没人处理。worker 自己删掉已落盘的安装包，
                // 避免留下无人认领、要等下次启动才被清理的孤儿文件。
                if let Ok((path, _)) = &outcome {
                    let _ = std::fs::remove_file(path);
                }
                flag.store(true, Ordering::Release);
                return;
            }
            // 取消标记可能在上面检查之后、send 之前/之后才置位：轮询任务
            // 此时可能已经丢弃 rx。send 完再确认一次，确保已落盘的安装包
            // 不会变成无人认领的孤儿文件。
            let downloaded = outcome.as_ref().ok().map(|(path, _)| path.clone());
            let _ = tx.send(Msg::Done(outcome.map_err(|e| e.to_string())));
            if worker_cancel.load(Ordering::Acquire) {
                if let Some(path) = downloaded {
                    let _ = std::fs::remove_file(path);
                }
            }
            flag.store(true, Ordering::Release);
        });

        cx.spawn(async move |this, cx: &mut AsyncApp| {
            while !finished.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                while let Ok(msg) = rx.try_recv() {
                    if this
                        .update(cx, |this, cx| {
                            match msg {
                                Msg::Progress(p) => this.update_progress = Some(p),
                                Msg::Done(Ok((path, hash))) => {
                                    this.update_downloading = false;
                                    this.update_progress = None;
                                    this.downloaded_installer = Some(path);
                                    this.downloaded_installer_hash = Some(hash);
                                    this.ok("安装包已下载");
                                }
                                Msg::Done(Err(e)) => {
                                    this.update_downloading = false;
                                    this.update_progress = None;
                                    this.fail(format!("下载失败：{e}"));
                                }
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        // 窗口已销毁：轮询任务立刻退出，不再每 150ms 空转。
                        return;
                    }
                }
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
            }
            if cancel.load(Ordering::Acquire) {
                // 关窗竞态：Done 可能刚进通道，轮询就看到 cancel。能取到就删，
                // 取不到则由 worker 在 send 后复查取消标记负责删除。
                while let Ok(msg) = rx.try_recv() {
                    if let Msg::Done(Ok((path, _))) = msg {
                        let _ = std::fs::remove_file(path);
                    }
                }
                return;
            }
            // Drain the final Done message after the worker set its flag.
            while let Ok(msg) = rx.try_recv() {
                if this
                    .update(cx, |this, cx| {
                        if let Msg::Done(Ok((path, hash))) = msg {
                            this.update_downloading = false;
                            this.update_progress = None;
                            this.downloaded_installer = Some(path);
                            this.downloaded_installer_hash = Some(hash);
                            this.ok("安装包已下载");
                        } else if let Msg::Done(Err(e)) = msg {
                            this.update_downloading = false;
                            this.update_progress = None;
                            this.fail(format!("下载失败：{e}"));
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
    }

    fn open_downloaded_folder(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.downloaded_installer.clone() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = crate::settings::system::open_folder(parent) {
                self.fail(format!("打开下载目录失败：{e}"));
                cx.notify();
            }
        }
    }

    /// 按用户选择的范围拉起下载好的安装包。每用户模式不弹 UAC；全机器模式
    /// 由 ShellExecuteW 的 runas 触发 UAC，被拒时 launch_installer 会返回明确
    /// 错误，而不是静默无反应。启动成功后必须退出应用，否则安装器可能无法
    /// 替换正在运行的 exe。
    fn install_downloaded(&mut self, scope: crate::update::InstallScope, cx: &mut Context<Self>) {
        let Some(path) = self.downloaded_installer.clone() else {
            return;
        };
        // 下载时验过一次，这里在启动前对磁盘文件再验一次：两次点击之间
        // 文件可能被替换。整文件哈希放到后台线程，避免 UI 卡住。
        let verify_path = path.clone();
        let verify_hash = self.downloaded_installer_hash;
        let task = cx.background_spawn(async move {
            match verify_hash {
                Some(hash) => crate::update::verify_installer_hash(&verify_path, &hash),
                None => Ok(()),
            }
        });
        cx.spawn(async move |this, cx| {
            let verified = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = verified {
                    this.fail(format!("安装前校验失败：{e}"));
                    cx.notify();
                    return;
                }
                match crate::update::launch_installer(&path, scope) {
                    Ok(()) => {
                        this.ok(match scope {
                            crate::update::InstallScope::CurrentUser => {
                                "安装程序已启动，应用将退出"
                            }
                            crate::update::InstallScope::AllUsers => {
                                "安装程序已启动，应用将退出；请在 UAC 提示中确认"
                            }
                        });
                        cx.notify();
                        cx.quit();
                    }
                    Err(e) => {
                        this.fail(e.to_string());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    // ---- chrome ------------------------------------------------------------

    /// Caption drawn by the gpui-component `TitleBar`: the component owns the
    /// 34px height, the gradient and bottom hairline, the min/max/close
    /// controls, the `WindowControlArea` hit areas and `start_window_move`
    /// dragging. The app content is the brand mark, the window title, and the
    /// search field centred between them and the window buttons.
    ///
    /// This is the only place the product title is drawn; the left rail starts
    /// with its navigation items.
    fn titlebar(&self) -> impl IntoElement {
        gpui_kit::component::TitleBar::new()
            .pl(px(14.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.))
                    .child(
                        div()
                            .flex_none()
                            .w(px(24.))
                            .h(px(24.))
                            .rounded(px(6.))
                            .overflow_hidden()
                            .child(logo_element(24.)),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::primary_fg())
                            .child(format!("{} 设置", crate::APP_NAME)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_center()
                    .justify_center()
                    .child(self.search_field()),
            )
    }

    fn nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let reset_label = if self.confirm_reset_all {
            "再次点击确认重置"
        } else {
            "重置所有设置"
        };
        div()
            .flex_none()
            .w(px(200.))
            .h_full()
            .flex()
            .flex_col()
            .bg(widgets::nav_bg())
            .child(
                div()
                    .id("settings-nav")
                    .flex_1()
                    .overflow_y_scroll()
                    .pt(px(10.))
                    .pb(px(6.))
                    .children(Page::ALL.map(|page| {
                        widgets::nav_item(
                            SharedString::from(format!("nav-{}", page.id())),
                            page.icon(),
                            page.title(),
                            self.page == page,
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| this.go_page(page, cx)),
                        )
                    })),
            )
            .child(div().p(px(14.)).child(
                widgets::danger_button("reset-all", reset_label).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        if this.confirm_reset_all {
                            this.reset_all(cx);
                        } else {
                            this.confirm_reset_all = true;
                            this.ok(
                                "再次点击确认：将清除全部设置、删除已保存的 API Key 并关闭开机启动",
                            );
                            cx.notify();
                        }
                    }),
                ),
            ))
    }

    /// The global search input, drawn inside the title bar. `occlude` matters:
    /// the title bar marks its whole strip as a drag area, and without it the
    /// drag hitbox wins the OS hit test over the field, so clicks would move
    /// the window instead of focusing the input.
    fn search_field(&self) -> impl IntoElement {
        div()
            .occlude()
            .w(px(480.))
            .h(px(32.))
            .px(px(10.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .rounded(px(16.))
            .bg(widgets::input_bg())
            .border_1()
            .border_color(theme::border())
            .child(
                gpui::svg()
                    .path("settings/search.svg")
                    .w(px(14.))
                    .h(px(14.))
                    .flex_none()
                    .text_color(theme::secondary_fg()),
            )
            .child(div().flex_1().min_w(px(0.)).child(self.search.clone()))
    }

    fn heading(
        &self,
        title: impl Into<SharedString>,
        desc: impl Into<SharedString>,
        allow_reset: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let can_reset = allow_reset && (self.page.has_config() || self.page == Page::General);
        div()
            .w_full()
            .flex()
            .flex_row()
            .items_start()
            .justify_between()
            .gap(px(16.))
            .pt(px(20.))
            .pb(px(16.))
            .child(widgets::page_heading(title, desc))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .when(!self.status.is_empty(), |el| {
                        el.child(widgets::status_pill(self.status.clone(), self.status_error))
                    })
                    .when(can_reset, |el| {
                        el.child(
                            widgets::primary_button("reset-page", "重置本页设置").on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.reset_page(cx)),
                            ),
                        )
                    }),
            )
    }

    // ---- pages -------------------------------------------------------------

    fn page_general(&self, cx: &mut Context<Self>) -> impl IntoElement {
        widgets::rows(vec![
            widgets::row(
                "开机启动",
                format!(
                    "登录 Windows 后自动在托盘中运行 {}，写入当前用户的 Run 注册表项。",
                    crate::APP_NAME
                ),
            )
            .child(widgets::control(
                widgets::toggle("autostart", self.autostart).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.set_autostart(cx)),
                ),
            )),
            widgets::row(
                "以管理员启动",
                "勾选后，手动启动会弹出 UAC 提示并以管理员权限运行；开机启动改用最高权限计划任务。重启程序后生效。",
            )
            .child(widgets::control(
                widgets::toggle("run-as-admin", self.settings.run_as_admin).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.set_run_as_admin(cx)),
                ),
            )),
            widgets::unsupported_row("界面语言", "当前版本仅提供简体中文界面。"),
        ])
    }

    fn page_screenshot(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hidden = self.settings.screenshot.hidden_toolbar_items.clone();
        let items: Vec<&'static str> = toolbar::PHASE1_ITEMS
            .iter()
            .copied()
            .filter(|id| toolbar::CUSTOMIZABLE.contains(id))
            .collect();
        // Fixed rows of four, as the C# `VariableSizedWrapGrid` lays them out.
        let mut grid: Vec<gpui::Div> = Vec::new();
        for chunk in items.chunks(TOOLBAR_COLUMNS) {
            let mut cells = Vec::new();
            for id in chunk.iter().copied() {
                let checked = !hidden.iter().any(|h| h == id);
                cells.push(
                    widgets::checkbox(id, toolbar::label(id), checked).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_toolbar_item(id, cx);
                        }),
                    ),
                );
            }
            grid.push(div().flex().flex_row().gap(px(4.)).children(cells));
        }

        let ann = &self.settings.screenshot.annotation;
        let current_argb = ann.stroke_argb();
        let swatches = theme::SWATCHES.map(|(hex, rgb24)| {
            let selected = current_argb == 0xFF00_0000 | rgb24;
            div()
                .id(SharedString::from(format!("swatch-{hex}")))
                .flex_none()
                .w(px(22.))
                .h(px(22.))
                .rounded_full()
                .bg(gpui::rgb(rgb24))
                .border_2()
                .border_color(if selected {
                    widgets::accent()
                } else {
                    theme::border()
                })
                .cursor(CursorStyle::PointingHand)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        if this.settings.screenshot.annotation.stroke_color != hex {
                            this.settings.screenshot.annotation.stroke_color = hex.into();
                            this.persist(cx);
                        }
                    }),
                )
        });

        let stored_w = ann.stroke_width;
        let width_label: SharedString = STROKE_WIDTHS
            .iter()
            .find(|(_, v)| (*v - stored_w).abs() < f64::EPSILON)
            .map(|(label, _)| label.to_string().into())
            .unwrap_or_else(|| format!("{stored_w:.0} px").into());
        let width_rows = STROKE_WIDTHS
            .map(|(label, v)| {
                widgets::select_option(
                    SharedString::from(format!("width-{v}")),
                    label,
                    (v - stored_w).abs() < f64::EPSILON,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.settings.screenshot.annotation.stroke_width = v;
                        this.open_select = None;
                        this.persist(cx);
                    }),
                )
            })
            .into_iter()
            .collect();
        let width = self.select_row(
            cx,
            SelectId::StrokeWidth,
            "线条粗细",
            "画笔、形状和箭头的描边宽度。",
            width_label,
            width_rows,
        );

        let stored_b = ann.mosaic_block;
        let mosaic_label: SharedString = MOSAIC_BLOCKS
            .iter()
            .find(|(_, v)| *v == stored_b)
            .map(|(label, _)| label.to_string().into())
            .unwrap_or_else(|| format!("{stored_b} px").into());
        let mosaic_rows = MOSAIC_BLOCKS
            .map(|(label, v)| {
                widgets::select_option(
                    SharedString::from(format!("mosaic-{v}")),
                    label,
                    v == stored_b,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.settings.screenshot.annotation.mosaic_block = v;
                        this.open_select = None;
                        this.persist(cx);
                    }),
                )
            })
            .into_iter()
            .collect();
        let mosaic = self.select_row(
            cx,
            SelectId::MosaicBlock,
            "马赛克大小",
            "马赛克涂抹的格子边长，单位是图像像素。",
            mosaic_label,
            mosaic_rows,
        );

        use crate::config::{
            CaptureMode, ClickAction, ClickOutside, LongCaptureAutoStart, PanelPosition,
            SmartSelectionMode, ToolbarPosition,
        };

        // 1 截屏模式
        let capture_mode = self.enum_select_row(
            cx,
            SelectId::CaptureMode,
            "截屏模式",
            "仅影响长截图是否保持持续捕获会话；WGC/DXGI 项当前均走自动选择。",
            self.settings.screenshot.capture_mode,
            &CaptureMode::ALL,
            CaptureMode::id,
            CaptureMode::label,
            |s: &mut Settings, v: CaptureMode| {
                if s.screenshot.capture_mode != v {
                    s.screenshot.capture_mode = v;
                    true
                } else {
                    false
                }
            },
        );

        // 2 捕获鼠标光标
        let show_cursor = widgets::row(
            "捕获鼠标光标",
            "在截屏结果中显示鼠标光标；DXGI 回退时无效（平台限制）。",
        )
        .child(widgets::control(
            widgets::toggle("toggle-show-cursor", self.settings.screenshot.show_cursor)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.toggle_screenshot_bool(ScreenshotToggle::ShowCursor, cx)
                    }),
                ),
        ));

        // 3 未选区时显示辅助线
        let show_guides = widgets::row(
            "未选区时显示辅助线",
            "开始截屏且尚未创建选区时自动显示辅助线；选区完成后自动隐藏。",
        )
        .child(widgets::control(
            widgets::toggle("toggle-show-guides", self.settings.screenshot.show_guides)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.toggle_screenshot_bool(ScreenshotToggle::ShowGuides, cx)
                    }),
                ),
        ));

        // 4 长截屏自动开始
        let long_capture = self.enum_select_row(
            cx,
            SelectId::LongCaptureAutoStart,
            "长截图自动开始",
            "选择进入长截图工具时的开始策略。",
            self.settings.screenshot.long_capture_auto_start,
            &LongCaptureAutoStart::ALL,
            LongCaptureAutoStart::id,
            LongCaptureAutoStart::label,
            |s: &mut Settings, v: LongCaptureAutoStart| {
                if s.screenshot.long_capture_auto_start != v {
                    s.screenshot.long_capture_auto_start = v;
                    true
                } else {
                    false
                }
            },
        );

        // 5 左键双击行为
        let left_double_click = self.enum_select_row(
            cx,
            SelectId::LeftDoubleClick,
            "左键双击行为",
            "在截屏选区完成后双击左键时执行的操作。",
            self.settings.screenshot.left_double_click,
            &ClickAction::ALL,
            ClickAction::id,
            ClickAction::label,
            |s: &mut Settings, v: ClickAction| {
                if s.screenshot.left_double_click != v {
                    s.screenshot.left_double_click = v;
                    true
                } else {
                    false
                }
            },
        );

        // 6 中键单击行为
        let middle_click = self.enum_select_row(
            cx,
            SelectId::MiddleClick,
            "中键单击行为",
            "在截屏选区完成后单击中键时执行的操作。",
            self.settings.screenshot.middle_click,
            &ClickAction::ALL,
            ClickAction::id,
            ClickAction::label,
            |s: &mut Settings, v: ClickAction| {
                if s.screenshot.middle_click != v {
                    s.screenshot.middle_click = v;
                    true
                } else {
                    false
                }
            },
        );

        // 7 点击选区外行为
        let click_outside = self.enum_select_row(
            cx,
            SelectId::ClickOutside,
            "点击选区外行为",
            "选择点击选区外时扩展现有选区，或重新创建新选区。",
            self.settings.screenshot.click_outside,
            &ClickOutside::ALL,
            ClickOutside::id,
            ClickOutside::label,
            |s: &mut Settings, v: ClickOutside| {
                if s.screenshot.click_outside != v {
                    s.screenshot.click_outside = v;
                    true
                } else {
                    false
                }
            },
        );

        // 9 记录截屏历史：配置里的 vec 就是四个复选框的唯一存储。
        let history_sources = self.settings.screenshot.record_history_sources.clone();
        let history_checks: Vec<gpui::Stateful<gpui::Div>> = RECORD_HISTORY_SOURCES
            .iter()
            .map(|(id, label)| {
                let checked = history_sources.iter().any(|s| s == *id);
                widgets::checkbox(
                    SharedString::from(format!("history-src-{id}")),
                    *label,
                    checked,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.toggle_record_history_source(id, cx);
                    }),
                )
            })
            .collect();
        let record_history = widgets::stack(
            "记录截屏历史",
            "选择自动记录到历史图片的截屏类型；成功复制或保存后才会记录。",
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(4.))
                .children(history_checks),
        );

        // 11 工具栏位置
        let toolbar_position = self.enum_select_row(
            cx,
            SelectId::ToolbarPosition,
            "工具栏位置",
            "选择跟随选区，或固定在选区所在屏幕的顶部或底部。",
            self.settings.screenshot.toolbar_position,
            &ToolbarPosition::ALL,
            ToolbarPosition::id,
            ToolbarPosition::label,
            |s: &mut Settings, v: ToolbarPosition| {
                if s.screenshot.toolbar_position != v {
                    s.screenshot.toolbar_position = v;
                    true
                } else {
                    false
                }
            },
        );

        // 12 参数面板位置
        let panel_position = self.enum_select_row(
            cx,
            SelectId::PanelPosition,
            "参数面板位置",
            "选择跟随选区，或固定在选区所在屏幕的左侧或右侧。",
            self.settings.screenshot.panel_position,
            &PanelPosition::ALL,
            PanelPosition::id,
            PanelPosition::label,
            |s: &mut Settings, v: PanelPosition| {
                if s.screenshot.panel_position != v {
                    s.screenshot.panel_position = v;
                    true
                } else {
                    false
                }
            },
        );

        // 13 放大镜
        let show_magnifier =
            widgets::row("放大镜", "在鼠标附近显示像素放大预览。").child(widgets::control(
                widgets::toggle(
                    "toggle-show-magnifier",
                    self.settings.screenshot.show_magnifier,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.toggle_screenshot_bool(ScreenshotToggle::ShowMagnifier, cx)
                    }),
                ),
            ));

        // 14 信息栏
        let show_info_bar =
            widgets::row("信息栏", "在选区附近显示坐标、尺寸与取色值。").child(widgets::control(
                widgets::toggle(
                    "toggle-show-info-bar",
                    self.settings.screenshot.show_info_bar,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.toggle_screenshot_bool(ScreenshotToggle::ShowInfoBar, cx)
                    }),
                ),
            ));

        // 15 快捷键提示
        let show_shortcut_hints =
            widgets::row("快捷键提示", "显示常用操作提示。").child(widgets::control(
                widgets::toggle(
                    "toggle-show-shortcut-hints",
                    self.settings.screenshot.show_shortcut_hints,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.toggle_screenshot_bool(ScreenshotToggle::ShowShortcutHints, cx)
                    }),
                ),
            ));

        // 8 元素检测：复用 smart_selection 配置，只改标题与选项文案。
        let element_detection = self.enum_select_row(
            cx,
            SelectId::ElementDetection,
            "元素检测",
            "选择框选时吸附到窗口与元素、仅窗口，或完全不检测。",
            self.settings.screenshot.smart_selection,
            &SmartSelectionMode::ALL,
            SmartSelectionMode::id,
            SmartSelectionMode::label,
            |s: &mut Settings, v: SmartSelectionMode| {
                if s.screenshot.smart_selection != v {
                    s.screenshot.smart_selection = v;
                    true
                } else {
                    false
                }
            },
        );

        // 10 自定义工具栏：按钮切换下方的勾选面板，逻辑沿用 toggle_toolbar_item。
        use crate::config::SelectionBorderStyle;
        let border_style = self.enum_select_row(
            cx,
            SelectId::SelectionBorderStyle,
            "选区边框线类型",
            "选择选区边框线使用虚线或实线。",
            self.settings.screenshot.selection_border_style,
            &SelectionBorderStyle::ALL,
            SelectionBorderStyle::id,
            SelectionBorderStyle::label,
            |s: &mut Settings, v: SelectionBorderStyle| {
                if s.screenshot.selection_border_style != v {
                    s.screenshot.selection_border_style = v;
                    true
                } else {
                    false
                }
            },
        );
        let border_width = self.number_row(
            "选区边框线粗细",
            "设置选区边框线粗细，范围为 1~10 像素，边框向选区外侧扩展。",
            &self.selection_border_width,
            60.,
        );
        let border_color_row = {
            let current = self.settings.screenshot.selection_border_color.trim();
            let swatches = [
                ("#F4F4F4", 0xF4F4F4u32),
                ("#FFFFFF", 0xFFFFFFu32),
                ("#40A0FF", 0x40A0FFu32),
                ("#FA3C3C", 0xFA3C3Cu32),
            ]
            .into_iter()
            .map(|(hex, rgb24)| {
                div()
                    .id(SharedString::from(format!("border-swatch-{hex}")))
                    .w(px(18.))
                    .h(px(18.))
                    .rounded_full()
                    .bg(gpui::rgb(rgb24))
                    .border_2()
                    .border_color(if current.eq_ignore_ascii_case(hex) {
                        widgets::accent()
                    } else {
                        theme::border()
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            let input = this.selection_border_color.clone();
                            input.update(cx, |i, cx| i.set_text(hex, cx));
                            if this.settings.screenshot.selection_border_color != hex {
                                this.settings.screenshot.selection_border_color = hex.into();
                                this.persist(cx);
                            }
                        }),
                    )
            })
            .collect::<Vec<_>>();
            widgets::row("选区边框线颜色", "设置截屏选区边框线的颜色。").child(widgets::control(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .children(swatches)
                    .child(div().w(px(110.)).child(self.selection_border_color.clone())),
            ))
        };

        let customize = widgets::stack(
            "自定义工具栏",
            "选择截屏工具栏上显示的按钮；完成与取消始终显示。",
        )
        .child(
            widgets::button("customize-toolbar", "自定义…").on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.toggle_customize_panel(cx)),
            ),
        )
        .when(self.customize_panel_open, |el| {
            el.child(div().flex().flex_col().gap(px(2.)).children(grid))
        });

        div()
            .flex()
            .flex_col()
            .child(widgets::rows(vec![
                capture_mode,
                show_cursor,
                show_guides,
                long_capture,
                left_double_click,
                middle_click,
                click_outside,
                element_detection,
                border_style,
                border_width,
                border_color_row,
                record_history,
                widgets::row("默认颜色", "新画笔、形状和文字标注的颜色。").child(widgets::control(
                    div().flex().flex_row().gap(px(8.)).children(swatches),
                )),
                width,
                mosaic,
                self.scroll_interval_row(cx),
                self.scroll_notches_row(cx),
                customize,
                toolbar_position,
                panel_position,
                show_magnifier,
                show_info_bar,
                show_shortcut_hints,
            ]))
            .child(widgets::hint(
                "工具栏与标注默认值在下一次截图时生效；自动滚动的间隔与步长在下一次长截图时生效。",
            ))
    }

    /// Auto-scroll cadence for long captures. Persisted through the shared
    /// `Screenshot` config so the next capture picks the values up; the clamp
    /// lives in `AutoScroll::step`, not here, to keep one source of truth.
    fn scroll_interval_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let ms = self.settings.screenshot.auto_scroll.interval_ms;
        let current: SharedString = SCROLL_INTERVALS
            .iter()
            .find(|(_, v)| *v == ms)
            .map(|(label, _)| (*label).into())
            .unwrap_or_else(|| format!("{ms} ms").into());
        let rows = SCROLL_INTERVALS
            .iter()
            .map(|(label, v)| {
                widgets::select_option(("scroll-int", *v), *label, *v == ms).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_scroll_interval(*v, cx)),
                )
            })
            .collect();
        self.select_row(
            cx,
            SelectId::AutoScrollInterval,
            "自动滚动间隔",
            "长截图自动滚动时，两次滚动之间的等待时间；间隔越短滚动越快。",
            current,
            rows,
        )
    }

    fn scroll_notches_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let notches = self.settings.screenshot.auto_scroll.notches;
        let current: SharedString = SCROLL_NOTCHES
            .iter()
            .find(|(_, v)| *v == notches)
            .map(|(label, _)| (*label).into())
            .unwrap_or_else(|| format!("{notches} 格").into());
        let rows = SCROLL_NOTCHES
            .iter()
            .map(|(label, v)| {
                widgets::select_option(("scroll-notch", *v), *label, *v == notches).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_scroll_notches(*v, cx)),
                )
            })
            .collect();
        self.select_row(
            cx,
            SelectId::AutoScrollNotches,
            "自动滚动步长",
            "每次滚动发送的原生滚轮格数；格数越大，一步跨过的页面越多。",
            current,
            rows,
        )
    }

    fn page_unsupported(&self, page: Page) -> impl IntoElement {
        // The row text comes from the same registry the search box reads, so a
        // hit always lands on a page that says the same thing.
        let items: Vec<gpui::Div> = model::rows_of(page)
            .take(1)
            .map(|r| widgets::unsupported_row(r.title, r.desc))
            .collect();
        div()
            .flex()
            .flex_col()
            .child(widgets::rows(items))
            .child(widgets::hint(
                "该页只在界面结构上与参考产品对齐，没有可写入的配置项。",
            ))
    }

    fn history_retention_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        // `let`, not `const`: the rows below reference Self::RETENTION_FOREVER,
        // and a nested const item cannot use Self from its enclosing type.
        let days: [(&str, u64); 5] = [
            ("1 天", 1),
            ("7 天", 7),
            ("30 天", 30),
            ("90 天", 90),
            ("永久", Self::RETENTION_FOREVER),
        ];
        let h = &self.settings.history;
        // "永久" is backed by keep_permanently, not a day count (the config
        // layer clamps retention_days to 365, so a large value could never
        // actually mean "forever"). Reflect that in both the label and check.
        let current: SharedString = if h.keep_permanently {
            "永久".into()
        } else {
            // 手改配置可能写入超界天数（如 5000）：显示与实际清理保持一致，
            // 都用 clamp 后的 getter，而不是原始字段。
            let days_value = h.retention_days();
            days.iter()
                .find(|(_, d)| *d == days_value)
                .map(|(label, _)| (*label).into())
                .unwrap_or_else(|| format!("{} 天", days_value).into())
        };
        let rows = days
            .iter()
            .map(|(label, d)| {
                // Copy the day out of the local array: the listener is 'static
                // and cannot borrow from `days`, which is dropped at return.
                let day = *d;
                let selected = if day == Self::RETENTION_FOREVER {
                    h.keep_permanently
                } else {
                    !h.keep_permanently && day == h.retention_days()
                };
                widgets::select_option(("hist-ret", day as u32), *label, selected).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_history_retention(day, cx)),
                )
            })
            .collect();
        self.select_row(
            cx,
            SelectId::HistoryRetention,
            "保留期限",
            "超过该天数的历史记录会在启动时、保存后以及更改后立即清理已有记录；选择“永久”等同于开启“始终保留”。",
            current,
            rows,
        )
    }

    fn history_limit_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        const LIMITS: [(&str, usize); 5] = [
            ("20 张", 20),
            ("50 张", 50),
            ("100 张", 100),
            ("300 张", 300),
            ("1000 张", 1000),
        ];
        let h = &self.settings.history;
        // 与 config::History::limit 的 clamp 保持一致，手改的超界值不误导用户。
        let limit = h.limit();
        let current: SharedString = format!("{} 张", limit).into();
        let rows = LIMITS
            .iter()
            .map(|(label, n)| {
                widgets::select_option(("hist-limit", *n as u32), *label, *n == limit)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.set_history_limit(*n, cx)),
                    )
            })
            .collect();
        self.select_row(
            cx,
            SelectId::HistoryLimit,
            "数量上限",
            "最多保留的记录条数，超出后删除最旧的。",
            current,
            rows,
        )
    }

    fn history_disk_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        const SIZES: [(&str, u64); 5] = [
            ("128 MB", 128),
            ("512 MB", 512),
            ("1 GB", 1024),
            ("4 GB", 4096),
            ("10 GB", 10240),
        ];
        let h = &self.settings.history;
        // getter 返回字节，这里转回 MiB 显示；单位与 config 的 clamp 保持一致。
        let max_disk_mib = h.max_disk_bytes() / (1024 * 1024);
        let current: SharedString = if max_disk_mib >= 1024 {
            format!("{} GB", max_disk_mib / 1024).into()
        } else {
            format!("{} MB", max_disk_mib).into()
        };
        let rows = SIZES
            .iter()
            .map(|(label, m)| {
                widgets::select_option(("hist-disk", *m as u32), *label, *m == max_disk_mib)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.set_history_disk(*m, cx)),
                    )
            })
            .collect();
        self.select_row(
            cx,
            SelectId::HistoryDisk,
            "占用上限",
            "历史目录的最大体积，超出后删除最旧的记录。",
            current,
            rows,
        )
    }

    /// History controls, owned by the Data page. Kept as a row builder so the
    /// search index and the rendered page stay in the same place.
    fn history_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::Div> {
        let h = &self.settings.history;
        let store_dir: SharedString = match h.store_dir() {
            // 历史目录内部用 `\\?\` 长路径读写，显示时还原成普通路径。
            Ok(p) => crate::config::shell_path(&p).display().to_string().into(),
            Err(e) => format!("{e}").into(),
        };
        let location = widgets::stack(
            "历史目录",
            "历史图片是保存截图的独立副本；删除这里的记录不影响原截图文件。",
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .child(widgets::path_line(store_dir).flex_1().min_w(px(0.)))
                .child(widgets::button("hist-dir-change", "更改").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.choose_history_dir(cx)),
                ))
                .child(
                    widgets::button("hist-dir-open", "打开文件夹").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.open_history_dir(cx)),
                    ),
                ),
        );
        let actions = widgets::row("浏览历史", "打开历史窗口，查看、复制或删除以往截图。").child(
            widgets::control(
                widgets::button("hist-open-window", "查看历史").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.open_history(window, cx)),
                ),
            ),
        );
        let clear_label = if self.data_confirm_clear {
            "再次点击清空"
        } else {
            "清空"
        };
        let clear =
            widgets::row("清空历史", "删除所有历史记录；此操作不可撤销。").child(widgets::control(
                widgets::danger_button("hist-clear", clear_label).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.clear_history(cx)),
                ),
            ));

        vec![
            widgets::row("记录截图历史", "保存截图时自动留存一份副本到历史目录。").child(
                widgets::control(widgets::toggle("hist-enabled", h.enabled).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.toggle_history_enabled(cx)),
                )),
            ),
            self.history_retention_row(cx),
            self.history_limit_row(cx),
            self.history_disk_row(cx),
            widgets::row("始终保留", "开启后忽略保留期限，只按数量与体积上限清理。").child(
                widgets::control(
                    widgets::toggle("hist-keep", h.keep_permanently).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.toggle_history_keep(cx)),
                    ),
                ),
            ),
            location,
            actions,
            clear,
        ]
    }

    /// A short numeric field beside its label, as the 超时 row does.
    fn number_row(
        &self,
        title: &'static str,
        desc: &'static str,
        input: &Entity<TextInput>,
        width: f32,
    ) -> gpui::Div {
        widgets::row(title, desc)
            .child(widgets::control(div().w(px(width)).child((*input).clone())))
    }

    /// 输出页的「拼图默认设置」分组：保存后对新打开的拼图窗口生效。
    fn collage_controls(&self, cx: &mut Context<Self>) -> gpui::Div {
        let c = &self.settings.screenshot.collage;
        let directions = [
            (crate::config::Direction::Vertical, "垂直"),
            (crate::config::Direction::Horizontal, "水平"),
            (crate::config::Direction::Custom, "网格"),
        ];
        let direction_rows = directions
            .into_iter()
            .map(|(v, label)| {
                widgets::select_option(
                    SharedString::from(format!("collage-dir-{v:?}")),
                    label,
                    c.direction == v,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_collage_direction(v, cx)),
                )
            })
            .collect();
        let direction = self.select_row(
            cx,
            SelectId::CollageDirection,
            "拼图方向",
            "多张图片的排列方向：垂直、水平或网格。",
            collage_direction_label(c.direction).into(),
            direction_rows,
        );

        let sizings = [
            (crate::config::Sizing::Original, "原始"),
            (crate::config::Sizing::Uniform, "统一"),
        ];
        let sizing_rows = sizings
            .into_iter()
            .map(|(v, label)| {
                widgets::select_option(
                    SharedString::from(format!("collage-sizing-{v:?}")),
                    label,
                    c.sizing == v,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_collage_sizing(v, cx)),
                )
            })
            .collect();
        let sizing = self.select_row(
            cx,
            SelectId::CollageSizing,
            "尺寸模式",
            "图片按原始尺寸排布，或统一缩放到相同尺寸。",
            collage_sizing_label(c.sizing).into(),
            sizing_rows,
        );

        // 背景色：输入框（#RRGGBB / #RRGGBBAA）+ 常用预设。
        let swatches = [
            ("#FFFFFFFF", "白"),
            ("#000000FF", "黑"),
            ("#FF0000FF", "红"),
            ("#87CEEBFF", "天蓝"),
        ]
        .into_iter()
        .map(|(hex, _label)| {
            let [r, g, b, _] = parse_rgba(hex).unwrap_or([255; 4]);
            let rgb = gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | (b as u32));
            div()
                .id(SharedString::from(format!("collage-bg-{hex}")))
                .flex_none()
                .w(px(18.))
                .h(px(18.))
                .rounded_full()
                .bg(rgb)
                .border_1()
                .border_color(theme::border())
                .cursor(CursorStyle::PointingHand)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        let Some(rgba) = parse_rgba(hex) else {
                            return;
                        };
                        if this.settings.screenshot.collage.background_rgba == rgba {
                            return;
                        }
                        this.settings.screenshot.collage.background_rgba = rgba;
                        this.collage_bg
                            .update(cx, |input, cx| input.set_text(hex, cx));
                        this.persist(cx);
                    }),
                )
        });
        let background =
            widgets::stack("背景颜色", "支持 #RRGGBB 或 #RRGGBBAA；也可点选下方预设。").child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .children(swatches)
                    .child(div().w(px(110.)).child(self.collage_bg.clone())),
            );

        widgets::rows(vec![
            direction,
            sizing,
            self.number_row(
                "拼图间距",
                "图片之间的间隔，像素，0 到 100。",
                &self.collage_spacing,
                90.,
            ),
            self.number_row(
                "拼图边框",
                "整张拼图外围的边框宽度，像素，0 到 100。",
                &self.collage_border,
                90.,
            ),
            self.number_row(
                "网格行数",
                "网格方向布局的行数，1 到 10。",
                &self.collage_rows,
                90.,
            ),
            self.number_row(
                "网格列数",
                "网格方向布局的列数，1 到 10。",
                &self.collage_columns,
                90.,
            ),
            background,
        ])
    }

    /// 输出页的「PDF 默认设置」分组。
    fn pdf_controls(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = &self.settings.screenshot.pdf;
        let stitchings = [
            (crate::config::StitchingMethod::PageBased, "按页拼接"),
            (crate::config::StitchingMethod::TemplateBased, "按模板拼版"),
        ];
        let stitching_rows = stitchings
            .into_iter()
            .map(|(v, label)| {
                widgets::select_option(
                    SharedString::from(format!("pdf-stitching-{v:?}")),
                    label,
                    p.stitching_method == v,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_pdf_stitching(v, cx)),
                )
            })
            .collect();
        let stitching = self.select_row(
            cx,
            SelectId::PdfStitching,
            "PDF 拼接方式",
            "按页自动拼接，或使用固定模板拼版。",
            pdf_stitching_label(p.stitching_method).into(),
            stitching_rows,
        );

        let templates = [
            (crate::config::TemplateType::TwoImages, "2 张"),
            (crate::config::TemplateType::ThreeImages, "3 张"),
            (crate::config::TemplateType::FourImages, "4 张"),
            (crate::config::TemplateType::SixImages, "6 张"),
            (crate::config::TemplateType::NineImages, "9 张"),
        ];
        let template_rows = templates
            .into_iter()
            .map(|(v, label)| {
                widgets::select_option(
                    SharedString::from(format!("pdf-template-{v:?}")),
                    label,
                    p.template_type == v,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_pdf_template(v, cx)),
                )
            })
            .collect();
        let template = self.select_row(
            cx,
            SelectId::PdfTemplate,
            "PDF 拼版模板",
            "模板每页容纳的图片数量：2、3、4、6 或 9 张。",
            pdf_template_label(p.template_type).into(),
            template_rows,
        );

        let modes = [
            (crate::config::ExportMode::Combined, "合并为一个 PDF"),
            (crate::config::ExportMode::PerPage, "每页单独导出"),
        ];
        let mode_rows = modes
            .into_iter()
            .map(|(v, label)| {
                widgets::select_option(
                    SharedString::from(format!("pdf-mode-{v:?}")),
                    label,
                    p.export_mode == v,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_pdf_export_mode(v, cx)),
                )
            })
            .collect();
        let mode = self.select_row(
            cx,
            SelectId::PdfExportMode,
            "PDF 分页模式",
            "合并为一个 PDF，或每页单独导出一个文件。",
            pdf_export_mode_label(p.export_mode).into(),
            mode_rows,
        );

        let papers = [
            (crate::config::PaperSize::A3, "A3"),
            (crate::config::PaperSize::A4, "A4"),
            (crate::config::PaperSize::A5, "A5"),
            (crate::config::PaperSize::B5, "B5"),
            (crate::config::PaperSize::Letter, "Letter"),
            (crate::config::PaperSize::Legal, "Legal"),
            (crate::config::PaperSize::Custom, "自定义"),
            (crate::config::PaperSize::ImageSize, "图片尺寸"),
        ];
        let paper_rows = papers
            .into_iter()
            .map(|(v, label)| {
                widgets::select_option(
                    SharedString::from(format!("pdf-paper-{v:?}")),
                    label,
                    p.paper_size == v,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_pdf_paper(v, cx)),
                )
            })
            .collect();
        let paper = self.select_row(
            cx,
            SelectId::PdfPaperSize,
            "纸张大小",
            "A3、A4、A5、B5、Letter、Legal、自定义或图片尺寸。",
            pdf_paper_label(p.paper_size).into(),
            paper_rows,
        );

        let orientations = [
            (crate::config::Orientation::Portrait, "纵向"),
            (crate::config::Orientation::Landscape, "横向"),
        ];
        let orientation_rows = orientations
            .into_iter()
            .map(|(v, label)| {
                widgets::select_option(
                    SharedString::from(format!("pdf-orientation-{v:?}")),
                    label,
                    p.orientation == v,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_pdf_orientation(v, cx)),
                )
            })
            .collect();
        let orientation = self.select_row(
            cx,
            SelectId::PdfOrientation,
            "页面方向",
            "PDF 页面使用纵向还是横向。",
            pdf_orientation_label(p.orientation).into(),
            orientation_rows,
        );

        widgets::rows(vec![
            stitching,
            template,
            mode,
            paper,
            orientation,
            self.number_row(
                "PDF 图片间距",
                "同一页图片之间的间距，毫米，不小于 0。",
                &self.pdf_spacing,
                90.,
            ),
            self.number_row(
                "自定义纸张宽度",
                "纸张大小选择自定义时生效，毫米，大于 0。",
                &self.pdf_width,
                90.,
            ),
            self.number_row(
                "自定义纸张高度",
                "纸张大小选择自定义时生效，毫米，大于 0。",
                &self.pdf_height,
                90.,
            ),
            self.number_row(
                "上边距",
                "页面上边缘留白，毫米，不小于 0。",
                &self.pdf_margin_top,
                90.,
            ),
            self.number_row(
                "下边距",
                "页面下边缘留白，毫米，不小于 0。",
                &self.pdf_margin_bottom,
                90.,
            ),
            self.number_row(
                "左边距",
                "页面左边缘留白，毫米，不小于 0。",
                &self.pdf_margin_left,
                90.,
            ),
            self.number_row(
                "右边距",
                "页面右边缘留白，毫米，不小于 0。",
                &self.pdf_margin_right,
                90.,
            ),
            self.number_row(
                "PDF 图片质量",
                "导出 JPG / WebP 页面的编码质量，1 到 100。",
                &self.pdf_quality,
                90.,
            ),
        ])
    }

    fn page_output(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let save_dir: SharedString = match crate::config::screenshot_dir() {
            Ok(p) => p.display().to_string().into(),
            Err(e) => format!("{e}").into(),
        };
        let location = widgets::stack(
            "保存位置",
            "工具栏的保存按钮把截图写到这里，扩展名跟随下方格式。",
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .child(widgets::path_line(save_dir).flex_1().min_w(px(0.)))
                .child(widgets::button("save-dir-pick", "更改").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.choose_save_dir(cx)),
                ))
                .child(
                    widgets::icon_button("save-dir-open", "settings/folder.svg").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.open_folder(crate::config::screenshot_dir(), cx);
                        }),
                    ),
                ),
        );

        let wm = &self.settings.screenshot.watermark;
        let path_text: SharedString = match &wm.image_path {
            Some(p) => p.display().to_string().into(),
            None => "未设置（仅使用文字水印）".into(),
        };
        let image = widgets::stack("图片水印", "优先于文字水印烧录；选择有效的图片文件才生效。")
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(widgets::path_line(path_text).flex_1().min_w(px(0.)))
                    .child(widgets::button("wm-img-pick", "选择图片").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.choose_water_image(cx)),
                    ))
                    .child(
                        widgets::icon_button("wm-img-clear", "settings/close.svg").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.clear_water_image(cx)),
                        ),
                    ),
            );

        div()
            .flex()
            .flex_col()
            .child(widgets::rows(vec![
                location,
                self.export_format_row(cx),
                self.export_quality_row(cx),
                widgets::stack(
                    "文件名称",
                    "可用 {date}（年月日）、{time}（时分秒）、{datetime}（两者）。留空使用默认名。",
                )
                .child(self.export_name.clone()),
                widgets::row(
                    "自动保存",
                    "完成截图时自动把图片写入保存位置；关闭后「完成」不写文件。",
                )
                .child(widgets::control(
                    widgets::toggle(
                        "export-auto-save",
                        self.settings.screenshot.export.auto_save,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.toggle_export_auto_save(cx)),
                    ),
                )),
                widgets::row(
                    "自动复制",
                    "完成截图时自动把图片复制到剪贴板；与自动保存相互独立。",
                )
                .child(widgets::control(
                    widgets::toggle(
                        "export-auto-copy",
                        self.settings.screenshot.export.auto_copy,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.toggle_export_auto_copy(cx)),
                    ),
                )),
                widgets::stack(
                    "水印文字",
                    "导出时烧录的文字，留空则仅当勾选时间或设置了图片时显示。",
                )
                .child(self.wm_text.clone()),
                self.watermark_position_row(cx),
                self.watermark_opacity_row(cx),
                self.watermark_scale_row(cx),
                image,
                widgets::row(
                    "添加拍摄时间",
                    "在文字前附加 UTC 时间，格式 YYYY-MM-DD HH:MM:SS。",
                )
                .child(widgets::control(
                    widgets::toggle("wm-timestamp", self.settings.screenshot.watermark.timestamp)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.toggle_water_timestamp(cx)),
                        ),
                )),
                widgets::row(
                    "平铺水印",
                    "把文字水印重复铺满整张图片；关闭时只放在九宫格位置。",
                )
                .child(widgets::control(
                    widgets::toggle("wm-tiled", self.settings.screenshot.watermark.tiled)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.toggle_water_tiled(cx)),
                        ),
                )),
            ]))
            .child(widgets::group_header("拼图默认设置"))
            .child(self.collage_controls(cx))
            .child(widgets::group_header("PDF 默认设置"))
            .child(self.pdf_controls(cx))
            .child(widgets::rows(vec![
                widgets::stack(
                    "图片切图",
                    "把一张图片切成等分/精确的网格，逐块导出，适合分页打印长截图。",
                )
                .child(
                    widgets::button("split-open-window", "打开切图工具").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.open_image_split(window, cx)),
                    ),
                ),
                widgets::stack(
                    "拼图工具",
                    "把多张图片拼成一张，支持垂直/水平/网格布局，可导出 PNG 或多页 PDF。",
                )
                .child(
                    widgets::button("collage-open-window", "打开拼图工具").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.open_collage(window, cx)),
                    ),
                ),
            ]))
            .child(widgets::hint(
                "水印随导出一并烧录到图片，不写入原图；留空且未设图片时不添加任何水印。拼图与 PDF 的默认值保存后，对之后打开的工具生效。",
            ))
    }

    fn watermark_position_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let wm = &self.settings.screenshot.watermark;
        let current: SharedString = WM_POSITIONS
            .iter()
            .find(|(_, k)| wm.position == *k)
            .map(|(label, _)| (*label).into())
            .unwrap_or_else(|| "右下".into());
        let rows = WM_POSITIONS
            .map(|(label, key)| {
                widgets::select_option(key, label, wm.position == key).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_water_position(key, cx)),
                )
            })
            .into_iter()
            .collect();
        self.select_row(
            cx,
            SelectId::WaterPosition,
            "水印位置",
            "水印在图片上的九宫格落点。",
            current,
            rows,
        )
    }

    fn watermark_opacity_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let wm = &self.settings.screenshot.watermark;
        let current: SharedString = WM_OPACITIES
            .iter()
            .find(|(_, v)| *v == wm.opacity)
            .map(|(label, _)| (*label).into())
            .unwrap_or_else(|| format!("{}%", wm.opacity).into());
        let rows = WM_OPACITIES
            .map(|(label, v)| {
                widgets::select_option(("wm-op", v as u32), label, v == wm.opacity).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_water_opacity(v, cx)),
                )
            })
            .into_iter()
            .collect();
        self.select_row(
            cx,
            SelectId::WaterOpacity,
            "水印不透明度",
            "10% 到 100%。",
            current,
            rows,
        )
    }

    fn watermark_scale_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let wm = &self.settings.screenshot.watermark;
        let current: SharedString = WM_SCALES
            .iter()
            .find(|(_, v)| (*v - wm.scale).abs() < f32::EPSILON)
            .map(|(label, _)| (*label).into())
            .unwrap_or_else(|| format!("{:.1}x", wm.scale).into());
        let rows = WM_SCALES
            .map(|(label, v)| {
                widgets::select_option(
                    ("wm-sc", v as u32),
                    label,
                    (v - wm.scale).abs() < f32::EPSILON,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_water_scale(v, cx)),
                )
            })
            .into_iter()
            .collect();
        self.select_row(
            cx,
            SelectId::WaterScale,
            "水印大小",
            "水印文字或图片的缩放，0.5x 到 3x。",
            current,
            rows,
        )
    }

    fn export_format_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        use crate::config::ImageFormat;
        let current_format = self.settings.screenshot.export.format;
        // 与导出线程使用同一个 effective_format：本机没有编码器时，界面必须
        // 显示真实落盘的格式，不能再摆出一个无效的 WebP + 质量滑块。
        let effective = crate::codec::effective_format(current_format);
        let current: SharedString = if effective == current_format {
            current_format.label().into()
        } else {
            format!(
                "{}（本机不支持，实际保存为 {}）",
                current_format.label(),
                effective.label()
            )
            .into()
        };
        let rows = ImageFormat::ALL
            .into_iter()
            // WebP comes from an optional Windows package; offering it where
            // the codec is missing would silently export PNG under a .webp
            // setting the user just picked on purpose.
            .filter(|f| crate::codec::format_available(*f))
            .map(|f| {
                widgets::select_option(("export-fmt", f as u32), f.label(), f == effective)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.set_export_format(f, cx)),
                    )
            })
            .collect();
        self.select_row(
            cx,
            SelectId::ExportFormat,
            "图片格式",
            "PNG 无损体积大；JPG/WebP 有损但适合长页面与照片；本机不支持的格式自动回退为 PNG。",
            current,
            rows,
        )
    }

    fn export_quality_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let export = &self.settings.screenshot.export;
        // PNG ignores quality, so the row explains that instead of offering a
        // control that changes nothing. Use the same fallback as the export
        // thread: a selected-but-unavailable WebP really writes PNG.
        if !crate::codec::effective_format(export.format).honours_quality() {
            return widgets::row(
                "图片质量",
                "PNG 为无损格式，质量设置对它不生效；不支持的格式会自动回退为 PNG，选择 JPG（或可用的 WebP）后可调。",
            );
        }
        // 与 app.rs 导出线程一致用 clamp 后的值，手改超界配置不显示假数字。
        let quality = export.clamped_quality();
        let current: SharedString = EXPORT_QUALITIES
            .iter()
            .find(|(_, v)| *v == quality)
            .map(|(label, _)| (*label).into())
            .unwrap_or_else(|| format!("{}%", quality).into());
        let rows = EXPORT_QUALITIES
            .map(|(label, v)| {
                widgets::select_option(("export-q", v as u32), label, v == quality).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_export_quality(v, cx)),
                )
            })
            .into_iter()
            .collect();
        self.select_row(
            cx,
            SelectId::ExportQuality,
            "图片质量",
            "有损格式的编码质量，越高越清晰、文件越大。",
            current,
            rows,
        )
    }

    fn page_pin(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::config::{PinAutoJoin, PinGroupSwitch, PinLock, PinZoomMode, ToolbarPosition};

        let p = &self.settings.pin;
        let mut rows: Vec<gpui::Div> = vec![
            self.pin_toggle_row(
                cx,
                "pin-magnifier",
                "默认放大镜",
                "新建贴图时默认开启像素放大镜。",
                p.show_magnifier,
                PinToggle::ShowMagnifier,
            ),
            self.pin_toggle_row(
                cx,
                "pin-topmost",
                "默认窗口置顶",
                "新建贴图时默认保持窗口置顶。",
                p.topmost,
                PinToggle::Topmost,
            ),
            self.pin_toggle_row(
                cx,
                "pin-shadow",
                "默认窗口阴影",
                "新建贴图时默认显示窗口阴影。",
                p.shadow,
                PinToggle::Shadow,
            ),
            self.pin_toggle_row(
                cx,
                "pin-titlebar",
                "默认显示标题栏",
                "新建贴图时默认显示独立标题栏。",
                p.titlebar,
                PinToggle::Titlebar,
            ),
            self.enum_select_row(
                cx,
                SelectId::PinLock,
                "默认锁定",
                "限制贴图窗口的拖动或缩放。",
                p.lock,
                &PinLock::ALL,
                |v| match v {
                    PinLock::None => "none",
                    PinLock::Position => "position",
                    PinLock::Size => "size",
                },
                PinLock::label,
                |s: &mut Settings, v: PinLock| {
                    if s.pin.lock != v {
                        s.pin.lock = v;
                        true
                    } else {
                        false
                    }
                },
            ),
            self.pin_toggle_row(
                cx,
                "pin-double-close",
                "双击关闭贴图",
                "在贴图窗口空白区域双击时关闭当前贴图。",
                p.double_click_close,
                PinToggle::DoubleClickClose,
            ),
            self.pin_toggle_row(
                cx,
                "pin-thumb-toggle",
                "鼠标切换缩略图",
                "按住左键时点击右键，可在原图与缩略图之间切换。",
                p.thumb_toggle,
                PinToggle::ThumbToggle,
            ),
            widgets::row("滚轮缩放步长", "每次滚轮缩放调整的百分比，范围 1-50。").child(
                widgets::control(div().w(px(90.)).child(self.pin_wheel_step.clone())),
            ),
            self.pin_toggle_row(
                cx,
                "pin-close-confirm",
                "关闭时确认",
                "关闭贴图前弹出确认提示，避免误关。",
                p.close_confirm,
                PinToggle::CloseConfirm,
            ),
            widgets::row(
                "关闭历史保留数量",
                "关闭的贴图可通过快捷键逐张恢复，最多保留 200 张，0 表示不记录。",
            )
            .child(widgets::control(
                div().w(px(90.)).child(self.pin_close_history.clone()),
            )),
        ];

        rows.push(
            widgets::row("万能贴图类型", "控制万能贴图快捷键可创建的内容类型。").child(
                widgets::control(div().flex().flex_row().gap(px(8.)).children([
                    self.pin_check_box(
                        cx,
                        "pin-universal-text",
                        "文本贴图",
                        p.universal_text,
                        PinToggle::UniversalText,
                    ),
                    self.pin_check_box(
                        cx,
                        "pin-universal-file",
                        "文件贴图",
                        p.universal_file,
                        PinToggle::UniversalFile,
                    ),
                    self.pin_check_box(
                        cx,
                        "pin-universal-image",
                        "图片贴图",
                        p.universal_image,
                        PinToggle::UniversalImage,
                    ),
                ])),
            ),
        );
        rows.push(self.pin_toggle_row(
            cx,
            "pin-auto-recognize",
            "自动识别文字",
            "新建图片贴图后自动识别文字。",
            p.auto_recognize,
            PinToggle::AutoRecognize,
        ));
        rows.push(self.pin_toggle_row(
            cx,
            "pin-auto-copy",
            "自动复制到剪贴板",
            "贴图创建成功后，自动把贴图图片复制到剪贴板。",
            p.auto_copy,
            PinToggle::AutoCopy,
        ));
        rows.push(self.enum_select_row(
            cx,
            SelectId::PinGroupSwitch,
            "切换分组时",
            "切换一个分组时，其他已显示分组的处理方式。",
            p.group_switch,
            &PinGroupSwitch::ALL,
            |v| match v {
                PinGroupSwitch::KeepOthers => "keepOthers",
                PinGroupSwitch::HideOthers => "hideOthers",
            },
            PinGroupSwitch::label,
            |s: &mut Settings, v: PinGroupSwitch| {
                if s.pin.group_switch != v {
                    s.pin.group_switch = v;
                    // 切回「保留其他分组」时，之前被隐藏的窗口必须立刻恢复。
                    if v != PinGroupSwitch::HideOthers {
                        crate::pin::restore_all_hidden();
                    }
                    true
                } else {
                    false
                }
            },
        ));
        rows.push(self.enum_select_row(
            cx,
            SelectId::PinAutoJoin,
            "新贴图自动加入分组",
            "新建贴图时是否自动加入当前分组。",
            p.auto_join_group,
            &PinAutoJoin::ALL,
            |v| match v {
                PinAutoJoin::None => "none",
                PinAutoJoin::Current => "current",
            },
            PinAutoJoin::label,
            |s: &mut Settings, v: PinAutoJoin| {
                if s.pin.auto_join_group != v {
                    s.pin.auto_join_group = v;
                    true
                } else {
                    false
                }
            },
        ));
        rows.push(
            widgets::row("贴图分组管理", "创建和管理贴图分组。").child(widgets::control(
                widgets::button("pin-manage-groups", "管理分组...").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.ok("把一张贴图拖到另一张贴图上即可成组；无需单独管理窗口");
                        cx.notify();
                    }),
                ),
            )),
        );
        rows.push(
            widgets::row(
                "自定义工具栏",
                "贴图编辑沿用「截屏 → 工具栏按钮」的显隐设置。",
            )
            .child(widgets::control(
                widgets::button("pin-goto-toolbar", "打开截屏设置").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.go_page(Page::Screenshot, cx)),
                ),
            )),
        );
        rows.push(self.enum_select_row(
            cx,
            SelectId::PinToolbarPosition,
            "工具栏位置",
            "选择跟随贴图、记住位置，或固定在贴图所在屏幕的顶部/底部。",
            p.toolbar_position,
            &ToolbarPosition::ALL,
            ToolbarPosition::id,
            ToolbarPosition::label,
            |s: &mut Settings, v: ToolbarPosition| {
                if s.pin.toolbar_position != v {
                    s.pin.toolbar_position = v;
                    true
                } else {
                    false
                }
            },
        ));
        rows.push(self.enum_select_row(
            cx,
            SelectId::PinPanelPosition,
            "参数面板位置",
            "选择跟随贴图、记住位置，或固定在贴图所在屏幕的左侧/右侧。",
            p.panel_position,
            &crate::config::PanelPosition::ALL,
            crate::config::PanelPosition::id,
            crate::config::PanelPosition::label,
            |s: &mut Settings, v: crate::config::PanelPosition| {
                if s.pin.panel_position != v {
                    s.pin.panel_position = v;
                    true
                } else {
                    false
                }
            },
        ));
        rows.push(self.enum_select_row(
            cx,
            SelectId::PinZoomMode,
            "缩放模式",
            "选择贴图缩放时保持左上角或中心位置不变。",
            p.zoom_mode,
            &PinZoomMode::ALL,
            |v| match v {
                PinZoomMode::TopLeft => "topLeft",
                PinZoomMode::Center => "center",
            },
            PinZoomMode::label,
            |s: &mut Settings, v: PinZoomMode| {
                if s.pin.zoom_mode != v {
                    s.pin.zoom_mode = v;
                    true
                } else {
                    false
                }
            },
        ));
        widgets::rows(rows)
    }

    fn page_appearance(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.settings.appearance.trim();
        let rows = self
            .theme_options
            .iter()
            .map(|(id, label)| {
                // 监听器要求 'static，把借用自 self 的 id / 标签各克隆一份。
                let id = id.clone();
                let label = label.clone();
                widgets::select_option(
                    SharedString::from(format!("appearance-{id}")),
                    label,
                    current.eq_ignore_ascii_case(&id),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_appearance(&id, cx)),
                )
            })
            .collect();
        let select = self.select_row(
            cx,
            SelectId::Appearance,
            "外观模式",
            "选择界面主题（gpui-kit 内置主题），立即生效并保存。",
            self.appearance_label(),
            rows,
        );
        div()
            .flex()
            .flex_col()
            .child(widgets::rows(vec![select]))
            .child(widgets::hint(
                "主题立即生效并保存：截图工具栏、弹层、贴图工具栏和本窗口会一起切换。",
            ))
    }

    fn page_hotkey(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut items: Vec<gpui::Div> = Vec::new();
        for (action, id, title, desc) in HOTKEY_ROWS {
            let recording = self.recording == Some(action);
            let mut field = widgets::hotkey_field(id, self.hotkey_text(action), recording)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.start_recording(action, cx);
                    }),
                );
            if recording {
                field = field.child(widgets::cancel_button("hk-cancel").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.cancel_recording(cx);
                    }),
                ));
            }
            items.push(widgets::row(title, desc).child(widgets::control(field)));
        }
        div()
            .flex()
            .flex_col()
            .child(widgets::rows(items))
            .child(widgets::hint(
                "点击右侧方框后按下组合键。录制期间截图不会触发；再点一次或等待 10 秒即取消。",
            ))
    }

    fn page_translate(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // 翻译只用用户自己的 API：接口、模型、目标语言、提示词与超时。
        let current = self.settings.translation.target_language.clone();
        let language_options = LANGUAGES
            .map(|(code, name)| {
                widgets::select_option(code, format!("{name} ({code})"), current == code)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.set_language(code, cx)),
                    )
            })
            .into_iter()
            .collect();
        let language = self.select_row(
            cx,
            SelectId::Language,
            "目标语言",
            "翻译输出的语言。",
            self.language_label(),
            language_options,
        );

        let prompt_summary = if self.settings.translation.prompt.trim().is_empty() {
            "使用内置提示词"
        } else {
            "自定义提示词"
        };
        // Collapsed by default, as the C# `SettingsExpander` has it: a prompt is
        // set once and then never looked at.
        let prompt = div()
            .flex()
            .flex_col()
            .child(
                widgets::row("提示词", "留空则使用内置的翻译提示词。")
                    .child(widgets::control(widgets::expander_summary(
                        prompt_summary,
                        self.prompt_open,
                    )))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.prompt_open = !this.prompt_open;
                            cx.notify();
                        }),
                    ),
            )
            .when(self.prompt_open, |el| {
                el.child(
                    widgets::stack("提示词内容", "发送给模型的系统提示词。")
                        .child(self.prompt.clone()),
                )
            });

        let mut rows: Vec<gpui::Div> = vec![
            widgets::stack(
                "接口地址",
                "OpenAI 兼容接口，例如 https://api.openai.com/v1；留空则翻译不可用。",
            )
            .child(self.base_url.clone()),
            widgets::stack("API Key", "保存在 Windows 凭据管理器，不写入配置文件。")
                .child(self.api_key.clone()),
            widgets::row("模型", "请求使用的模型名称。").child(widgets::control(
                div().w(px(INLINE_FIELD_W)).child(self.model.clone()),
            )),
            language,
            widgets::row("请求超时", "秒，5 到 300。按四个阶段分摊，每段至少 2 秒。").child(
                widgets::control(div().w(px(120.)).child(self.timeout.clone())),
            ),
            prompt,
        ];
        // A key on a plaintext endpoint is the one case where the vault is not
        // enough: show a two-click acknowledgement and gate the runtime on it.
        // 空地址只是「翻译不可用」，不是「正在用明文 HTTP」；is_insecure_http
        // 对空串/解析失败也返回 true，必须先排除空值再显示警告。
        if !self.saved_api_key.trim().is_empty()
            && !self.settings.translation.base_url.trim().is_empty()
            && crate::translate::is_insecure_http(&self.settings.translation.base_url)
        {
            let acknowledged = self.settings.translation.allow_insecure_http;
            let label = if acknowledged {
                "已确认：点击撤销明文 HTTP 授权"
            } else if self.http_warn_confirm {
                "再次点击以确认明文传输风险"
            } else {
                "我了解风险，仍要使用 HTTP"
            };
            rows.insert(
                0,
                widgets::row(
                    "明文 HTTP 警告",
                    "当前接口使用 http://，API Key 会以明文发送。仅对本地或可信服务使用。",
                )
                .child(widgets::danger_button("http-risk", label).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        if this.settings.translation.allow_insecure_http {
                            this.settings.translation.allow_insecure_http = false;
                            this.http_warn_confirm = false;
                            // persist 失败时自己会写入「设置保存失败」，这里不能
                            // 再用成功文案覆盖它。
                            if this.persist(cx) {
                                this.ok("已撤销明文 HTTP 授权");
                            }
                        } else if this.http_warn_confirm {
                            this.settings.translation.allow_insecure_http = true;
                            this.http_warn_confirm = false;
                            if this.persist(cx) {
                                this.ok("已确认：允许明文 HTTP 发送 API Key");
                            }
                        } else {
                            this.http_warn_confirm = true;
                            this.ok("再次点击以确认明文传输风险");
                        }
                        cx.notify();
                    }),
                )),
            );
        }

        div()
            .flex()
            .flex_col()
            .child(widgets::rows(rows))
            .child(widgets::hint(
                "翻译只用你自己的 API：没有本地翻译模型。接口地址留空时翻译不可用。",
            ))
    }

    fn page_resources(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Sampled on window open, on entering this page, and via 刷新 — never
        // on every frame (engine_status does a full Settings clone + file I/O).
        let engine = &self.engine_status;
        let models: SharedString = match &self.settings.ocr.models_dir {
            Some(p) => p.display().to_string().into(),
            None => format!("默认：{}", engine.default_dir).into(),
        };
        div()
            .flex()
            .flex_col()
            .child(widgets::rows(vec![
                widgets::row("识别引擎", "本地识别，截图不会离开这台机器。").child(
                    widgets::control(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(div().w(px(7.)).h(px(7.)).rounded(px(4.)).bg(
                                if engine.available {
                                    theme::success()
                                } else {
                                    // Grey, not red: a missing optional model is
                                    // an "not installed" state, not a failure.
                                    theme::secondary_fg()
                                },
                            ))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::secondary_fg())
                                    .child(engine.label),
                            )
                            .child(widgets::button("engine-refresh", "刷新").on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.refresh_engine_status(false, cx)),
                            )),
                    ),
                ),
                widgets::stack(
                    "模型目录",
                    "默认使用程序安装位置的 models\\v6；这里可覆盖为其它目录。",
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.))
                        .child(widgets::path_line(models).flex_1().min_w(px(0.)))
                        .child(widgets::button("models-pick", "更改").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.choose_models_dir(cx)),
                        ))
                        .child(
                            widgets::icon_button("models-clear", "settings/close.svg")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| this.clear_models_dir(cx)),
                                ),
                        ),
                ),
            ]))
            .child(widgets::hint(format!(
                "当前生效目录：{}；需要检测、识别与字典三个模型文件同时存在且非空，取字与翻译才可用。",
                engine.dir
            )))
    }

    fn page_data(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let path_of = |p: crate::error::Result<std::path::PathBuf>| -> SharedString {
            match p {
                Ok(p) => p.display().to_string().into(),
                Err(_) => "LOCALAPPDATA 未设置".into(),
            }
        };
        let usage: SharedString = format!(
            "数据 {} · 日志 {}",
            model::human_size(self.data_bytes),
            model::human_size(self.log_bytes)
        )
        .into();

        let mut rows = vec![
            widgets::row("数据目录", "settings.json 所在位置。").child(widgets::control(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .w(px(360.))
                    .child(
                        widgets::path_line(path_of(system::data_dir()))
                            .flex_1()
                            .min_w(px(0.)),
                    )
                    .child(
                        widgets::icon_button("open-data", "settings/folder.svg").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.open_folder(system::data_dir(), cx);
                            }),
                        ),
                    ),
            )),
            widgets::row("日志目录", "perf.log 等运行日志所在位置。").child(widgets::control(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .w(px(360.))
                    .child(
                        widgets::path_line(path_of(crate::perf::log_dir()))
                            .flex_1()
                            .min_w(px(0.)),
                    )
                    .child(
                        widgets::icon_button("open-logs", "settings/folder.svg").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.open_folder(crate::perf::log_dir(), cx);
                            }),
                        ),
                    ),
            )),
            widgets::row("存储占用", "数据目录与日志目录当前占用的空间。")
                .child(widgets::control(widgets::muted_text(usage))),
            widgets::row("清理日志", "删除运行日志，不影响设置和截图。").child(widgets::control(
                widgets::button("clear-logs", "清理").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.clear_logs(cx)),
                ),
            )),
        ];
        // The standalone History page was folded in here: keeping its controls
        // next to the other storage data means one place to manage what the app
        // keeps on disk.
        rows.extend(self.history_rows(cx));
        widgets::rows(rows)
    }

    /// 视图切换按钮（宫格 / 列表），当前模式高亮。样式与独立历史窗口一致。
    fn history_view_toggle(
        &self,
        id: &'static str,
        label: &'static str,
        mode: HistoryViewMode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.history_view_mode == mode;
        div()
            .id(id)
            .flex_none()
            .h(px(24.))
            .px(px(10.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.))
            .text_size(px(11.))
            .cursor(CursorStyle::PointingHand)
            .bg(if active {
                theme::accent()
            } else {
                theme::elevated_surface()
            })
            .text_color(if active {
                theme::on_accent()
            } else {
                theme::primary_fg()
            })
            .border_1()
            .border_color(if active {
                theme::accent()
            } else {
                theme::border()
            })
            .hover(|s| s.border_color(theme::accent_hover()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.history_view_mode = mode;
                    cx.notify();
                }),
            )
            .child(label)
    }

    /// 列表模式下的单行历史卡片：左侧小缩略图，右侧日期 / 尺寸 / 大小 / 来源。
    /// 点击整行即选中该图片，与宫格卡片行为一致。
    fn history_list_row(
        &self,
        record: &HistoryRecord,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        const ROW_THUMB_W: f32 = 80.0;
        const ROW_PAD: f32 = 8.0;

        let selected = self.history_selected.as_ref() == Some(&record.id);
        let id = record.id.clone();
        let thumb = self.history_thumbs.get(&record.id).and_then(|t| t.clone());
        // 小缩略图按原图比例缩放，限制在合理区间避免把行撑得过高/过矮。
        let thumb_h = if record.width == 0 || record.height == 0 {
            ROW_THUMB_W
        } else {
            (ROW_THUMB_W * record.height as f32 / record.width as f32).clamp(44.0, 80.0)
        };

        div()
            .id(("settings-history-row", index))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .p(px(ROW_PAD))
            .rounded(px(8.))
            .cursor(CursorStyle::PointingHand)
            .bg(if selected {
                theme::selection_wash()
            } else {
                theme::elevated_surface()
            })
            .border_1()
            .border_color(if selected {
                theme::accent()
            } else {
                theme::border()
            })
            .hover(|s| s.border_color(theme::accent_hover()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.history_selected = Some(id.clone());
                    this.history_status = SharedString::default();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(ROW_THUMB_W))
                    .h(px(thumb_h))
                    .rounded(px(4.))
                    .bg(theme::muted())
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(match thumb {
                        Some(image) => img(ImageSource::Render(image))
                            .w(px(ROW_THUMB_W))
                            .h(px(thumb_h))
                            .object_fit(gpui::ObjectFit::Contain)
                            .rounded(px(4.))
                            .into_any_element(),
                        None => div()
                            .w(px(ROW_THUMB_W))
                            .h(px(thumb_h))
                            .rounded(px(4.))
                            .bg(theme::muted())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("无预览")
                            .into_any_element(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::primary_fg())
                            .child(crate::history::format_utc(record.created_ms)),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::secondary_fg())
                            .child(format!("{} × {}", record.width, record.height)),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::secondary_fg())
                            .child(model::human_size(record.bytes)),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::secondary_fg())
                            .child(crate::history::source_label(&record.source)),
                    ),
            )
    }

    /// Embedded history gallery. This renders the *same* grid as the standalone
    /// history window so clicking "历史图片" in the nav shows the actual captured
    /// images — not a near-empty action page that looks like "nothing loaded".
    fn page_history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const CARD_W: f32 = 240.0;
        const THUMB: f32 = 200.0;
        const PAD: f32 = 12.0;

        // Only records matching the source filter are shown and counted.
        let visible: Vec<&HistoryRecord> = self
            .history_records
            .iter()
            .filter(|r| history_matches_filter(&r.source, &self.history_filter))
            .collect();
        let total_bytes: u64 = visible.iter().map(|r| r.bytes).sum();
        let total_label = model::human_size(total_bytes);

        // Source filter dropdown. New records use concrete source keys; older
        // ones kept `saved_to_file` / `copied_to_clipboard`, both still matched.
        const FILTERS: [(&str, &str); 7] = [
            ("all", "全部来源"),
            ("region", "普通截屏"),
            ("copyOnly", "快速截屏"),
            ("fullscreen", "截取全屏"),
            ("currentWindow", "截取当前窗口"),
            ("saved_to_file", "保存到文件（旧）"),
            ("copied_to_clipboard", "复制到剪贴板（旧）"),
        ];
        let filter_open = self.open_select == Some(SelectId::HistoryFilter);
        let current_label: SharedString = FILTERS
            .iter()
            .find(|(value, _)| *value == self.history_filter)
            .map(|(_, label)| *label)
            .unwrap_or("全部来源")
            .into();
        let filter_field = widgets::select(
            SharedString::from(format!("select-{}", SelectId::HistoryFilter as u8)),
            current_label,
            filter_open,
            Some(140.),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                this.open_select = if filter_open {
                    None
                } else {
                    Some(SelectId::HistoryFilter)
                };
                cx.notify();
            }),
        );
        let filter_value = self.history_filter.clone();
        let filter_rows = FILTERS
            .iter()
            .enumerate()
            .map(|(i, (value, label))| {
                let value = value.to_string();
                let selected = value == filter_value;
                widgets::select_option(("hist-filter", i), *label, selected).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_history_filter(&value, cx)),
                )
            })
            .collect::<Vec<_>>();
        let filter_control = div()
            .relative()
            .child(filter_field)
            .when(filter_open, |el| {
                el.child(deferred(
                    div()
                        .absolute()
                        .top(px(DROPDOWN_TOP))
                        .right(px(0.))
                        .w(px(140.))
                        .child(
                            widgets::select_list("select-list-hist-filter").children(filter_rows),
                        ),
                ))
            });

        const GAP: f32 = 16.0;
        // 卡片中除缩略图以外的固定高度估算：上下内边距 + 图文间距 + 两行文字 + 边框。
        // 只用于瀑布流列高平衡，不影响卡片真实布局。
        // Grid cards show the image only, so the chrome is just the card padding.
        const CARD_CHROME_H: f32 = 2.0 * PAD + 2.0;

        let view_id = cx.entity_id();
        let width_cell = self.history_grid_width.clone();

        let gallery = if visible.is_empty() {
            let (title, hint) = if self.history_records.is_empty() {
                if self.history_index_error.is_some() {
                    (
                        "历史索引损坏，图片仍在磁盘。",
                        "请从「打开文件夹」查看原图，或检查 index.json 后刷新重试。",
                    )
                } else {
                    (
                        "还没有截图历史。",
                        "保存截图或将截图复制到剪贴板后会自动记录到这里。",
                    )
                }
            } else {
                ("当前筛选下没有图片。", "试试将来源筛选切换为“全部来源”。")
            };
            div()
                .id("settings-history-grid")
                .w_full()
                .py(px(60.))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::secondary_fg())
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::secondary_fg())
                        .child(hint),
                )
        } else if self.history_view_mode == HistoryViewMode::List {
            // 列表模式：纵向堆叠的单行卡片。
            let mut list = div()
                .id("settings-history-list")
                .w_full()
                .flex()
                .flex_col()
                .gap(px(8.));
            for (i, record) in visible.iter().enumerate() {
                list = list.child(self.history_list_row(record, i, cx));
            }
            list
        } else {
            // 宫格模式：Masonry 瀑布流。按容器宽度计算列数，每张图投放到最矮列。
            let w = width_cell.get().max(CARD_W + 2.0 * PAD);
            let cols = ((w - 2.0 * PAD + GAP) / (CARD_W + GAP)).floor().max(1.0) as usize;
            let mut col_heights = vec![0.0f32; cols];
            let mut placements: Vec<((f32, f32), &HistoryRecord, usize)> =
                Vec::with_capacity(visible.len());
            for (i, record) in visible.iter().enumerate() {
                let card_h = thumb_height(record.width, record.height) + CARD_CHROME_H + GAP;
                // 投放到当前最矮的一列；相同高度时保留靠左的优先级。
                let col = col_heights
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(c, _)| c)
                    .unwrap_or(0);
                let x = PAD + col as f32 * (CARD_W + GAP);
                let y = col_heights[col];
                col_heights[col] += card_h;
                placements.push(((x, y), *record, i));
            }
            let max_h = col_heights.iter().copied().fold(0.0f32, f32::max).max(PAD);

            let mut grid = div()
                .id("settings-history-grid")
                .relative()
                .w_full()
                .h(px(max_h))
                // 用 canvas 的 prepaint 读取真实容器宽度；只有变化超过 1px 才
                // notify 重排，避免滚动条或舍入误差引起的抖动。
                .child(
                    canvas(
                        move |bounds, _, app| {
                            let new_w: f32 = bounds.size.width.into();
                            if (new_w - width_cell.get()).abs() > 1.0 {
                                width_cell.set(new_w);
                                app.notify(view_id);
                            }
                        },
                        |_, _, _, _| (),
                    )
                    .w_full()
                    .h(px(1.)),
                );

            for ((x, y), record, i) in placements {
                let selected = self.history_selected.as_ref() == Some(&record.id);
                let id = record.id.clone();
                let thumb = self.history_thumbs.get(&record.id).and_then(|t| t.clone());
                let thumb_h = thumb_height(record.width, record.height);
                grid = grid.child(
                    div()
                        .id(("settings-history-card", i))
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(CARD_W))
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .p(px(PAD))
                        .rounded(px(8.))
                        .cursor(CursorStyle::PointingHand)
                        .bg(if selected {
                            theme::selection_wash()
                        } else {
                            theme::elevated_surface()
                        })
                        .border_1()
                        .border_color(if selected {
                            theme::accent()
                        } else {
                            theme::border()
                        })
                        .hover(|s| s.border_color(theme::accent_hover()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.history_selected = Some(id.clone());
                                this.history_status = SharedString::default();
                                cx.notify();
                            }),
                        )
                        // 缩略图宽度固定，高度按原图比例（record.width/height）计算。
                        .child(
                            div()
                                .w(px(THUMB - 2.0 * PAD))
                                .h(px(thumb_h))
                                .rounded(px(6.))
                                .bg(theme::muted())
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(match thumb {
                                    Some(image) => img(ImageSource::Render(image))
                                        .w(px(THUMB - 2.0 * PAD))
                                        .h(px(thumb_h))
                                        .object_fit(gpui::ObjectFit::Contain)
                                        .rounded(px(6.))
                                        .into_any_element(),
                                    None => div()
                                        .w(px(THUMB - 2.0 * PAD))
                                        .h(px(thumb_h))
                                        .rounded(px(6.))
                                        .bg(theme::muted())
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child("无预览")
                                        .into_any_element(),
                                }),
                        ),
                );
            }
            grid
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::primary_fg())
                            .child(format!("{} 张图片", visible.len())),
                    )
                    .child(div().w(px(1.)).h(px(10.)).bg(theme::border()))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::secondary_fg())
                            .child(total_label),
                    )
                    .child(div().flex_1())
                    .child(filter_control),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    // 宫格 / 列表切换：选中项用强调色高亮。
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.))
                            .child(self.history_view_toggle(
                                "hist-view-grid",
                                "宫格",
                                HistoryViewMode::Grid,
                                cx,
                            ))
                            .child(self.history_view_toggle(
                                "hist-view-list",
                                "列表",
                                HistoryViewMode::List,
                                cx,
                            )),
                    )
                    .child(widgets::button("hist-refresh", "刷新").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.reload_history(cx)),
                    ))
                    .child(
                        widgets::button("hist-open-window", "独立窗口打开").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| this.open_history(window, cx)),
                        ),
                    )
                    .child(widgets::button("hist-folder", "打开文件夹").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.history_open_folder(cx)),
                    ))
                    .child(widgets::button("hist-copy", "复制").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.history_copy_selected(cx)),
                    ))
                    .child(
                        widgets::button(
                            "hist-delete",
                            if self.history_confirm_delete {
                                "再次点击删除"
                            } else {
                                "删除"
                            },
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.history_delete_selected(cx)),
                        ),
                    )
                    .child(
                        widgets::button(
                            "hist-clear",
                            if self.history_confirm_clear {
                                "再次点击清空"
                            } else {
                                "清空"
                            },
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.history_clear(cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(if self.history_status.is_empty() {
                        theme::secondary_fg()
                    } else {
                        theme::accent()
                    })
                    .child(self.history_status.clone()),
            )
            .child(gallery)
            .into_any_element()
    }

    fn page_about(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // One button, two jobs: check first, then become the download link
        // once a newer release is known.
        let update_button = if let Some((version, _, _)) = &self.update_available {
            widgets::button("update-open", format!("前往下载 v{version}")).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.open_update_page(cx)),
            )
        } else {
            widgets::button(
                "update-check",
                if self.update_checking {
                    "检查中…"
                } else {
                    "检查更新"
                },
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.check_update(cx)),
            )
        };
        let download_label: SharedString = if let Some(path) = &self.downloaded_installer {
            format!(
                "打开目录（{}）",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
            .into()
        } else if self.update_downloading {
            match &self.update_progress {
                Some(p) => format!("下载中… {}%", p.percent().unwrap_or(0)).into(),
                None => "下载中…".into(),
            }
        } else if self.update_available.is_some() {
            "应用内下载".into()
        } else {
            "检查更新后可用".into()
        };
        let download_button = widgets::button("update-download", download_label).on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                if this.downloaded_installer.is_some() {
                    this.open_downloaded_folder(cx);
                } else if this.update_available.is_some() {
                    this.download_update(cx);
                } else {
                    // Never leave a visible button silently dead: explain what
                    // has to happen before it can do anything.
                    this.ok("请先检查更新");
                    cx.notify();
                }
            }),
        );
        let version: SharedString = format!(
            "v{}{}",
            crate::update::current_version(),
            if cfg!(debug_assertions) {
                " · debug"
            } else {
                ""
            }
        )
        .into();
        let mut rows = vec![
            widgets::row(crate::APP_NAME, "轻量截图与文字识别工具。")
                .child(widgets::control(widgets::muted_text(version))),
            widgets::row("检查更新", "从发布页获取最新版本。")
                .child(widgets::control(update_button)),
            widgets::row(
                "应用内下载",
                "下载官方安装包到 %LOCALAPPDATA%\\FastPin\\downloads，不经过浏览器。",
            )
            .child(widgets::control(download_button)),
        ];
        // 安装包落盘后才出现安装入口。范围由用户选：默认每用户安装不弹
        // UAC，需要全机器安装时才提权；两条路径都与安装器的权限声明一致。
        if self.downloaded_installer.is_some() {
            let install_user = widgets::button("update-install-user", "仅当前用户").on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.install_downloaded(crate::update::InstallScope::CurrentUser, cx)
                }),
            );
            let install_all = widgets::button("update-install-all", "所有用户（需管理员）")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.install_downloaded(crate::update::InstallScope::AllUsers, cx)
                    }),
                );
            rows.push(
                widgets::row(
                    "安装更新",
                    "选择安装范围；仅当前用户无需管理员权限，所有用户会弹出 UAC。",
                )
                .child(widgets::control(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(install_user)
                        .child(install_all),
                )),
            );
        }
        widgets::rows(rows)
    }

    /// Renders an already-computed hit list. The caller reuses the same search
    /// for the header count, so a frame never scans the row registry twice.
    fn search_results(
        &self,
        hits: &[&'static RowMeta],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if hits.is_empty() {
            return widgets::rows(vec![widgets::unsupported_row(
                "没有匹配的配置",
                "换个关键词试试，例如「水印」「快捷键」「日志」。",
            )])
            .into_any_element();
        }
        let mut items: Vec<gpui::AnyElement> = Vec::new();
        for (i, hit) in hits.iter().enumerate() {
            let meta: &'static RowMeta = hit;
            let mut el = widgets::row(meta.title, meta.desc)
                .child(widgets::control(widgets::page_tag(meta.page.title())))
                .id(SharedString::from(format!("hit-{i}")))
                .cursor(CursorStyle::PointingHand)
                .hover(|s| s.bg(widgets::accent_soft()));
            el = el.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.go_page(meta.page, cx)),
            );
            items.push(el.into_any_element());
        }
        widgets::rows(items).into_any_element()
    }
    fn page_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match self.page {
            Page::General => self.page_general(cx).into_any_element(),
            Page::Screenshot => self.page_screenshot(cx).into_any_element(),
            Page::Record => {
                debug_assert!(self.page.unsupported());
                self.page_unsupported(self.page).into_any_element()
            }
            Page::History => self.page_history(cx).into_any_element(),
            Page::Appearance => self.page_appearance(cx).into_any_element(),
            Page::Output => self.page_output(cx).into_any_element(),
            Page::Pin => self.page_pin(cx).into_any_element(),
            Page::Hotkey => self.page_hotkey(cx).into_any_element(),
            Page::Translate => self.page_translate(cx).into_any_element(),
            Page::Resources => self.page_resources(cx).into_any_element(),
            Page::Data => self.page_data(cx).into_any_element(),
            Page::About => self.page_about(cx).into_any_element(),
        }
    }
}

/// 共享字段的逐字段合并：本页改过（快照与上次落盘基线不同）才用快照值，
/// 没改过的一律保留其它窗口写入的最新值。`force` 仅供全局重置使用——默认值
/// 可能恰好等于基线，此时不看差异也必须整份采用快照。单页重置不置全局 force，
/// 由 `force_page` 在常规合并后强制本页字段采用快照。
fn keep_edited<T: PartialEq + Clone>(dst: &mut T, snapshot: &T, baseline: &T, force: bool) {
    if force || snapshot != baseline {
        *dst = snapshot.clone();
    }
}

/// 设置页落盘前的合并：从最新配置出发，只覆盖本页真正改过的字段。
///
/// ocr / translation / history / hotkeys 与截屏页的显示项、水印、13 个独占
/// 控件字段只有本页写，沿用快照（与旧行为一致）。collage / pdf / export /
/// appearance 同时被拼图窗口、切图窗口和主题回写修改：`baseline` 是上次本页
/// 落盘时的值，逐字段比较即可知道本页是否改过，没改过的字段保留 latest，
/// 不再被陈旧快照整体回滚。split 本页没有控件，只有全局重置才会改，因此仅
/// 在 `force` 时取快照。`force_page` 用于单页重置：常规合并完成后把该页字段
/// 无条件覆盖为快照值，解决「重置前值 == 默认值」时差异判断失效的问题。
fn settings_persist_merge(
    latest: &Settings,
    snapshot: &Settings,
    baseline: &Settings,
    force: bool,
    force_page: Option<Page>,
) -> Settings {
    let mut merged = latest.clone();
    merged.ocr = snapshot.ocr.clone();
    merged.translation = snapshot.translation.clone();
    merged.history = snapshot.history.clone();
    merged.hotkeys = snapshot.hotkeys.clone();
    // 贴图设置只有本页写入；贴图行为读取的是持久化后的配置。
    merged.pin = snapshot.pin.clone();

    // 从 latest 出发：共享字段未编辑时自然保留其它窗口的写入。本页独占的
    // 显示类字段整份用快照覆盖；共享字段再用 keep_edited 逐字段判断。
    let mut screenshot = latest.screenshot.clone();
    let (s, b) = (&snapshot.screenshot, &baseline.screenshot);
    screenshot.hidden_toolbar_items = s.hidden_toolbar_items.clone();
    screenshot.save_dir = s.save_dir.clone();
    screenshot.annotation = s.annotation.clone();
    screenshot.watermark = s.watermark.clone();
    screenshot.smart_selection = s.smart_selection;
    screenshot.auto_scroll = s.auto_scroll.clone();
    // 截屏页独占的控件字段：无条件采用快照（force 与否都一样）。否则开关改了
    // 也落不了盘，reset_page(Screenshot) / reset_all 也带不回默认值。
    screenshot.capture_mode = s.capture_mode;
    screenshot.show_cursor = s.show_cursor;
    screenshot.show_guides = s.show_guides;
    screenshot.long_capture_auto_start = s.long_capture_auto_start;
    screenshot.left_double_click = s.left_double_click;
    screenshot.middle_click = s.middle_click;
    screenshot.click_outside = s.click_outside;
    screenshot.record_history_sources = s.record_history_sources.clone();
    screenshot.toolbar_position = s.toolbar_position;
    screenshot.panel_position = s.panel_position;
    screenshot.show_magnifier = s.show_magnifier;
    screenshot.show_info_bar = s.show_info_bar;
    screenshot.show_shortcut_hints = s.show_shortcut_hints;
    screenshot.selection_border_style = s.selection_border_style;
    screenshot.selection_border_width = s.selection_border_width;
    screenshot.selection_border_color = s.selection_border_color.clone();
    keep_edited(
        &mut screenshot.collage.direction,
        &s.collage.direction,
        &b.collage.direction,
        force,
    );
    keep_edited(
        &mut screenshot.collage.sizing,
        &s.collage.sizing,
        &b.collage.sizing,
        force,
    );
    keep_edited(
        &mut screenshot.collage.border,
        &s.collage.border,
        &b.collage.border,
        force,
    );
    keep_edited(
        &mut screenshot.collage.spacing,
        &s.collage.spacing,
        &b.collage.spacing,
        force,
    );
    keep_edited(
        &mut screenshot.collage.background_rgba,
        &s.collage.background_rgba,
        &b.collage.background_rgba,
        force,
    );
    keep_edited(
        &mut screenshot.collage.rows,
        &s.collage.rows,
        &b.collage.rows,
        force,
    );
    keep_edited(
        &mut screenshot.collage.columns,
        &s.collage.columns,
        &b.collage.columns,
        force,
    );

    keep_edited(
        &mut screenshot.pdf.stitching_method,
        &s.pdf.stitching_method,
        &b.pdf.stitching_method,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.template_type,
        &s.pdf.template_type,
        &b.pdf.template_type,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.spacing_mm,
        &s.pdf.spacing_mm,
        &b.pdf.spacing_mm,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.paper_size,
        &s.pdf.paper_size,
        &b.pdf.paper_size,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.orientation,
        &s.pdf.orientation,
        &b.pdf.orientation,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.custom_width_mm,
        &s.pdf.custom_width_mm,
        &b.pdf.custom_width_mm,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.custom_height_mm,
        &s.pdf.custom_height_mm,
        &b.pdf.custom_height_mm,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.margin_top_mm,
        &s.pdf.margin_top_mm,
        &b.pdf.margin_top_mm,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.margin_bottom_mm,
        &s.pdf.margin_bottom_mm,
        &b.pdf.margin_bottom_mm,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.margin_left_mm,
        &s.pdf.margin_left_mm,
        &b.pdf.margin_left_mm,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.margin_right_mm,
        &s.pdf.margin_right_mm,
        &b.pdf.margin_right_mm,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.export_mode,
        &s.pdf.export_mode,
        &b.pdf.export_mode,
        force,
    );
    keep_edited(
        &mut screenshot.pdf.image_quality,
        &s.pdf.image_quality,
        &b.pdf.image_quality,
        force,
    );

    keep_edited(
        &mut screenshot.export.format,
        &s.export.format,
        &b.export.format,
        force,
    );
    keep_edited(
        &mut screenshot.export.quality,
        &s.export.quality,
        &b.export.quality,
        force,
    );
    keep_edited(
        &mut screenshot.export.filename_template,
        &s.export.filename_template,
        &b.export.filename_template,
        force,
    );
    keep_edited(
        &mut screenshot.export.auto_save,
        &s.export.auto_save,
        &b.export.auto_save,
        force,
    );
    keep_edited(
        &mut screenshot.export.auto_copy,
        &s.export.auto_copy,
        &b.export.auto_copy,
        force,
    );

    if force {
        // 全局重置才会动 split：此时必须落默认配置，平时保留切图窗口的最新值。
        screenshot.split = s.split.clone();
    }
    merged.screenshot = screenshot;
    keep_edited(
        &mut merged.appearance,
        &snapshot.appearance,
        &baseline.appearance,
        force,
    );
    // 「以管理员启动」是常规页独占的顶层字段，与 appearance 同构。
    keep_edited(
        &mut merged.run_as_admin,
        &snapshot.run_as_admin,
        &baseline.run_as_admin,
        force,
    );
    // 单页重置：默认值可能恰好等于基线，差异判断识别不出「已重置」，常规
    // 合并后无条件采用本页快照。只覆盖该页字段，其它页 / 窗口不受影响。
    if let Some(page) = force_page {
        model::force_page_fields(&mut merged, snapshot, page);
    }
    merged
}

/// 录制时接受的热键判定：除 PrtSc 外必须带 Ctrl / Alt / Shift / Win 中至少一个。
/// 裸键会被全局钩子一直吞掉，保存后日常输入全部失灵，必须在录制入口拦下；
/// PrtSc 沿用系统截图键的习惯，允许单独使用。
fn recording_hotkey_acceptable(pressed: Hotkey) -> bool {
    pressed.vk == hotkey::VK_SNAPSHOT || pressed.ctrl || pressed.alt || pressed.shift || pressed.win
}

/// Gallery source-filter predicate. `"all"` accepts everything; other values
/// match a record's source string exactly.
fn history_matches_filter(source: &str, filter: &str) -> bool {
    filter == "all" || source == filter
}

/// 缩略图显示宽度固定（THUMB - 2 * PAD），高度按原图宽高比计算。
/// 极端比例做 clamp，避免过高/过宽的图片把等高行撑得失衡。
fn thumb_height(width: u32, height: u32) -> f32 {
    const THUMB_W: f32 = 176.0; // THUMB(200) - 2 * PAD(12)
    const MIN_H: f32 = 48.0;
    const MAX_H: f32 = THUMB_W * 2.0;
    if width == 0 || height == 0 {
        return THUMB_W;
    }
    (THUMB_W * height as f32 / width as f32).clamp(MIN_H, MAX_H)
}

/// Modification time of a history image in unix millis, used to decide whether
/// a cached thumbnail is still valid. `None` means the file could not be
/// inspected and forces a fresh decode.
fn history_file_mtime(path: &PathBuf) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as u64)
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).text().trim().to_string();
        let searching = !query.is_empty();
        // One search per frame feeds both the result count and the list.
        let hits = if searching {
            model::search(&query)
        } else {
            Vec::new()
        };

        let header = if searching {
            let n = hits.len();
            self.heading(
                "搜索结果",
                format!("在所有页面中匹配「{query}」，共 {n} 项；点击可跳转到来源页面。"),
                false,
                cx,
            )
            .into_any_element()
        } else {
            self.heading(self.page.title(), self.page.desc(), true, cx)
                .into_any_element()
        };
        let body = if searching {
            self.search_results(&hits, cx)
        } else {
            self.page_body(cx)
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(widgets::page_bg())
            .text_color(theme::primary_fg())
            .child(self.titlebar())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.nav(cx))
                    .child(
                        div().flex_1().h_full().flex().flex_col().child(
                            div()
                                .id("settings-page")
                                .flex_1()
                                .overflow_y_scroll()
                                .px(px(24.))
                                .pb(px(24.))
                                .child(
                                    div()
                                        .w_full()
                                        .max_w(px(CONTENT_MAX_W))
                                        .flex()
                                        .flex_col()
                                        .child(header)
                                        .child(body),
                                ),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_status_snapshot_keeps_only_renderable_fields() {
        let installed = crate::ocr::EngineStatus {
            available: true,
            dir: Some(PathBuf::from("D:/models/v6")),
            label: "PP-OCRv6 Small",
        };
        let snapshot = EngineStatusSnapshot::from(installed);
        assert!(snapshot.available);
        assert_eq!(&*snapshot.dir, "D:/models/v6");
        assert_eq!(snapshot.label, "PP-OCRv6 Small");

        let missing = crate::ocr::EngineStatus {
            available: false,
            dir: None,
            label: "模型缺失，识别不可用",
        };
        let snapshot = EngineStatusSnapshot::from(missing);
        assert!(!snapshot.available);
        assert_eq!(&*snapshot.dir, "未找到");
    }

    /// 设置页保存时，其它窗口在开窗期间写入的 collage / pdf / export /
    /// appearance 必须保留；只有本页真正编辑过的字段才用快照值覆盖。
    #[test]
    fn persist_merge_keeps_untouched_shared_fields_from_latest() {
        let mut latest = Settings::default();
        latest.screenshot.collage.spacing = 33; // 拼图窗口写入
        latest.screenshot.collage.border = 7; // 拼图窗口写入
        latest.screenshot.pdf.paper_size = crate::config::PaperSize::Letter;
        latest.screenshot.pdf.image_quality = 77;
        latest.screenshot.export.format = crate::config::ImageFormat::Webp; // 切图窗口写入
        latest.screenshot.export.quality = 88; // 切图窗口写入
        latest.screenshot.export.filename_template = "Shot_{date}".into();
        latest.screenshot.split.count = 9; // 切图窗口写入
        latest.appearance = "Tokyo Moon".into(); // 主题回写

        // 基线 = 开窗 / 上次落盘时的配置，这里用默认值模拟。
        let baseline = Settings::default();
        let mut snapshot = baseline.clone();
        // 本页只改了 collage.spacing 与 pdf.paper_size。
        snapshot.screenshot.collage.spacing = 11;
        snapshot.screenshot.pdf.paper_size = crate::config::PaperSize::A5;
        // 本页独占字段不受合并影响，始终用快照。
        snapshot.translation.base_url = "https://snapshot/v1".into();
        snapshot.screenshot.watermark.text = "机密".into();
        snapshot.hotkeys.region = "Ctrl+Shift+1".into();

        let merged = settings_persist_merge(&latest, &snapshot, &baseline, false, None);
        // 本页改过的字段：用快照值，控件才能真正落盘。
        assert_eq!(merged.screenshot.collage.spacing, 11);
        assert_eq!(
            merged.screenshot.pdf.paper_size,
            crate::config::PaperSize::A5
        );
        // 未编辑的共享字段：保留 latest，不再回滚其它窗口的改动。
        assert_eq!(merged.screenshot.collage.border, 7);
        assert_eq!(merged.screenshot.pdf.image_quality, 77);
        assert_eq!(
            merged.screenshot.export.format,
            crate::config::ImageFormat::Webp
        );
        assert_eq!(merged.screenshot.export.quality, 88);
        assert_eq!(merged.screenshot.export.filename_template, "Shot_{date}");
        assert_eq!(merged.screenshot.split.count, 9);
        assert_eq!(merged.appearance, "Tokyo Moon");
        // 本页独占字段仍用快照。
        assert_eq!(merged.translation.base_url, "https://snapshot/v1");
        assert_eq!(merged.screenshot.watermark.text, "机密");
        assert_eq!(merged.hotkeys.region, "Ctrl+Shift+1");

        // 重置：默认值可能恰好等于基线，也必须整份采用快照，否则重置不落盘。
        let reset = settings_persist_merge(&latest, &baseline, &baseline, true, None);
        assert_eq!(reset.screenshot.collage.spacing, 0);
        assert_eq!(reset.screenshot.collage.border, 0);
        assert_eq!(
            reset.screenshot.export.format,
            crate::config::ImageFormat::Png
        );
        assert_eq!(reset.screenshot.export.quality, 90);
        assert_eq!(reset.appearance, crate::config::DEFAULT_APPEARANCE);
        assert_eq!(reset.screenshot.split.count, 2);
    }

    /// 截屏页 13 个独占字段的非默认样例（与 model.rs 的重置用例一致）。
    fn non_default_exclusive(s: &mut Settings) {
        s.screenshot.capture_mode = crate::config::CaptureMode::Dxgi;
        s.screenshot.show_cursor = true;
        s.screenshot.show_guides = true;
        s.screenshot.long_capture_auto_start = crate::config::LongCaptureAutoStart::Manual;
        s.screenshot.left_double_click = crate::config::ClickAction::Save;
        s.screenshot.middle_click = crate::config::ClickAction::Copy;
        s.screenshot.click_outside = crate::config::ClickOutside::Recreate;
        s.screenshot.record_history_sources = vec!["region".into()];
        s.screenshot.toolbar_position = crate::config::ToolbarPosition::Bottom;
        s.screenshot.panel_position = crate::config::PanelPosition::Right;
        s.screenshot.show_magnifier = false;
        s.screenshot.show_info_bar = false;
        s.screenshot.show_shortcut_hints = false;
    }

    fn assert_exclusive(actual: &Settings, expected: &Settings) {
        assert_eq!(
            actual.screenshot.capture_mode,
            expected.screenshot.capture_mode
        );
        assert_eq!(
            actual.screenshot.show_cursor,
            expected.screenshot.show_cursor
        );
        assert_eq!(
            actual.screenshot.show_guides,
            expected.screenshot.show_guides
        );
        assert_eq!(
            actual.screenshot.long_capture_auto_start,
            expected.screenshot.long_capture_auto_start
        );
        assert_eq!(
            actual.screenshot.left_double_click,
            expected.screenshot.left_double_click
        );
        assert_eq!(
            actual.screenshot.middle_click,
            expected.screenshot.middle_click
        );
        assert_eq!(
            actual.screenshot.click_outside,
            expected.screenshot.click_outside
        );
        assert_eq!(
            actual.screenshot.record_history_sources,
            expected.screenshot.record_history_sources
        );
        assert_eq!(
            actual.screenshot.toolbar_position,
            expected.screenshot.toolbar_position
        );
        assert_eq!(
            actual.screenshot.panel_position,
            expected.screenshot.panel_position
        );
        assert_eq!(
            actual.screenshot.show_magnifier,
            expected.screenshot.show_magnifier
        );
        assert_eq!(
            actual.screenshot.show_info_bar,
            expected.screenshot.show_info_bar
        );
        assert_eq!(
            actual.screenshot.show_shortcut_hints,
            expected.screenshot.show_shortcut_hints
        );
    }

    /// 截图页独占控件没有 keep_edited 保护：无论 force 与否、无论基线如何，
    /// 都必须取快照，否则打开「捕获鼠标光标」这类开关根本落不了盘，重置也无效。
    #[test]
    fn persist_merge_uses_screenshot_page_owned_fields_even_when_unchanged() {
        let baseline = Settings::default();
        let mut snapshot = baseline.clone();
        non_default_exclusive(&mut snapshot);
        let merged =
            settings_persist_merge(&Settings::default(), &snapshot, &baseline, false, None);
        assert_eq!(
            merged.screenshot.capture_mode,
            crate::config::CaptureMode::Dxgi
        );
        assert!(merged.screenshot.show_cursor, "捕获鼠标光标必须落盘");
        assert_exclusive(&merged, &snapshot);

        // 重置：快照已回默认，latest 里还留着非默认值也必须写回默认。
        let mut stale = Settings::default();
        non_default_exclusive(&mut stale);
        let reset = settings_persist_merge(&stale, &baseline, &baseline, false, None);
        assert_exclusive(&reset, &baseline);
        // force=true（全局重置）同理。
        let forced = settings_persist_merge(&stale, &baseline, &baseline, true, None);
        assert_exclusive(&forced, &baseline);
    }

    /// 触发回归的场景：开窗 spacing=0，拼图窗口改成 5，此时点「重置数据页」
    /// 不能把 collage / pdf / export / appearance / split 回滚成开窗值。
    #[test]
    fn page_reset_does_not_roll_back_other_windows_shared_fields() {
        let mut baseline = Settings::default();
        baseline.history.enabled = false; // 本页重置前的旧值
        let mut snapshot = baseline.clone();
        let mut latest = Settings::default();
        latest.screenshot.collage.spacing = 5;
        latest.screenshot.pdf.image_quality = 77;
        latest.screenshot.export.quality = 88;
        latest.appearance = "Tokyo Moon".into();
        latest.screenshot.split.count = 9;

        assert!(model::reset_page(&mut snapshot, Page::Data));
        let mut adjusted = baseline.clone();
        model::copy_page_fields(&mut adjusted, &baseline, Page::Data);
        let merged = settings_persist_merge(&latest, &snapshot, &adjusted, false, None);
        assert_eq!(merged.screenshot.collage.spacing, 5);
        assert_eq!(merged.screenshot.pdf.image_quality, 77);
        assert_eq!(merged.screenshot.export.quality, 88);
        assert_eq!(merged.appearance, "Tokyo Moon");
        assert_eq!(merged.screenshot.split.count, 9);
        assert_eq!(
            serde_json::to_value(&merged.history).expect("serializes"),
            serde_json::to_value(&snapshot.history).expect("serializes")
        );
    }

    /// 本页字段仍要能重置落盘；其它页的字段保持旧 baseline，继续取 latest。
    #[test]
    fn page_reset_baseline_marks_only_owned_fields_edited() {
        let mut before = Settings::default();
        before.screenshot.collage.spacing = 12; // 上次落盘的本页值
        before.appearance = "Catppuccin Latte".into(); // 开窗时本页的外观值
        let mut snapshot = before.clone();
        assert!(model::reset_page(&mut snapshot, Page::Output));

        // 真实的 baseline 包含所有字段的上次落盘值；copy_page_fields 只把
        // 本页字段改成重置前值，其它字段必须保持原样。
        let mut adjusted = before.clone();
        model::copy_page_fields(&mut adjusted, &before, Page::Output);
        assert_eq!(
            adjusted.appearance, "Catppuccin Latte",
            "外观不是输出页的字段，重置输出页不应触碰它"
        );

        let mut latest = before.clone();
        latest.screenshot.collage.spacing = 5; // 其它窗口也写过，但本页重置优先
        latest.appearance = "Tokyo Moon".into(); // 主题回写
        let merged = settings_persist_merge(&latest, &snapshot, &adjusted, false, None);
        assert_eq!(merged.screenshot.collage.spacing, 0, "本页重置值必须落盘");
        assert_eq!(merged.appearance, "Tokyo Moon", "其它页字段保留 latest");
    }

    /// 回归：开窗时 spacing 恰好就是默认值 0，拼图窗口改成 5，点「重置输出页」
    /// 必须把 0 真正写盘。旧逻辑靠基线差异判断，此时 diff 为空而保留 latest=5，
    /// 用户看到「已重置」但配置没变。force_page 只强制本页，不动其它页。
    #[test]
    fn page_reset_before_value_equals_default_still_lands() {
        let snapshot = Settings::default(); // 开窗快照：spacing=0（默认值）
        let baseline = snapshot.clone();
        // 开窗后其它窗口写入的新值。
        let mut latest = snapshot.clone();
        latest.screenshot.collage.spacing = 5;
        latest.screenshot.pdf.image_quality = 77;
        latest.screenshot.export.quality = 88;
        latest.appearance = "Tokyo Moon".into();
        latest.screenshot.split.count = 9;

        // 普通保存（force_page=None）：本页没编辑，仍保留 latest。
        let normal = settings_persist_merge(&latest, &snapshot, &baseline, false, None);
        assert_eq!(normal.screenshot.collage.spacing, 5);

        let defaults = Settings::default();
        let merged =
            settings_persist_merge(&latest, &snapshot, &baseline, false, Some(Page::Output));
        assert_eq!(
            merged.screenshot.collage.spacing, 0,
            "重置前是默认值也必须落盘"
        );
        assert_eq!(
            merged.screenshot.pdf.image_quality, defaults.screenshot.pdf.image_quality,
            "输出页字段随重置一起回默认"
        );
        assert_eq!(
            merged.screenshot.export.quality, defaults.screenshot.export.quality,
            "输出页字段随重置一起回默认"
        );
        assert_eq!(
            merged.appearance, "Tokyo Moon",
            "外观属于其它页，保留 latest"
        );
        assert_eq!(merged.screenshot.split.count, 9, "切图字段保留 latest");
    }

    /// 基线前移后，「把值改回默认值」也必须与基线产生差异，才能跨两次
    /// persist 正确落盘（回归：第二次合并拿旧基线比对，误判成没改过）。
    #[test]
    fn persist_merge_value_returned_to_default_lands_after_baseline_advanced() {
        let baseline = Settings::default();
        let mut snapshot = baseline.clone();
        snapshot.screenshot.collage.spacing = 5;
        let mut latest = Settings::default();
        let first = settings_persist_merge(&latest, &snapshot, &baseline, false, None);
        assert_eq!(first.screenshot.collage.spacing, 5);

        // 第一次保存成功后基线前移为快照；本页再把 spacing 改回默认 0。
        let advanced = snapshot;
        let snapshot2 = Settings::default();
        latest.screenshot.collage.spacing = 5;
        let second = settings_persist_merge(&latest, &snapshot2, &advanced, false, None);
        assert_eq!(second.screenshot.collage.spacing, 0);
    }

    #[test]
    fn recording_rejects_bare_keys_except_prtsc() {
        let bare_a = Hotkey::new(false, false, false, false, hotkey::VK_A);
        let prtsc = Hotkey::new(false, false, false, false, hotkey::VK_SNAPSHOT);
        let ctrl_a = Hotkey::new(true, false, false, false, hotkey::VK_A);
        let alt_a = Hotkey::new(false, true, false, false, hotkey::VK_A);
        let shift_a = Hotkey::new(false, false, true, false, hotkey::VK_A);
        let win_a = Hotkey::new(false, false, false, true, hotkey::VK_A);

        assert!(
            !recording_hotkey_acceptable(bare_a),
            "裸 A 必须拒绝，否则全局钩子会吞掉日常输入"
        );
        assert!(recording_hotkey_acceptable(prtsc), "PrtSc 是唯一允许的裸键");
        assert!(recording_hotkey_acceptable(ctrl_a), "Ctrl 组合可用");
        assert!(recording_hotkey_acceptable(alt_a), "Alt 组合可用");
        assert!(recording_hotkey_acceptable(shift_a), "Shift 组合可用");
        assert!(recording_hotkey_acceptable(win_a), "Win 组合可用");
    }

    #[test]
    fn history_thumb_height_follows_aspect_ratio_with_clamp() {
        // 16:9 → 176 * 9 / 16 = 99
        assert_eq!(thumb_height(1920, 1080), 99.0);
        // 1:1 → 176
        assert_eq!(thumb_height(1000, 1000), 176.0);
        // 极宽图 → 下限 48
        assert_eq!(thumb_height(10000, 100), 48.0);
        // 极高图 → 上限 176 * 2 = 352
        assert_eq!(thumb_height(100, 10000), 352.0);
        // 尺寸缺失时回退方形
        assert_eq!(thumb_height(0, 0), 176.0);
    }

    #[test]
    fn history_source_filter_matches_all_and_exact_sources() {
        assert!(history_matches_filter("saved_to_file", "all"));
        assert!(history_matches_filter("copied_to_clipboard", "all"));
        assert!(history_matches_filter("saved_to_file", "saved_to_file"));
        assert!(!history_matches_filter(
            "saved_to_file",
            "copied_to_clipboard"
        ));
        assert!(!history_matches_filter(
            "copied_to_clipboard",
            "saved_to_file"
        ));
        assert!(!history_matches_filter("clipboard", "copied_to_clipboard"));
    }

    #[test]
    fn embedded_brand_logo_decodes() {
        let decoded =
            image::load_from_memory(include_bytes!("../../../../assets/logo/fastpin.png"))
                .expect("embedded logo decodes");
        assert!(decoded.width() >= 64 && decoded.height() >= 64);
        let logo = brand_logo().expect("brand logo caches");
        let bytes = logo.as_bytes(0).expect("RenderImage carries the frame");
        // Straight-alpha BGRA: the titlebar tile must be transparent at the
        // corner and opaque in the middle, i.e. no white card.
        assert_eq!(bytes[3], 0, "corner alpha");
        assert!(bytes[(32 * 64 + 32) * 4 + 3] > 0, "glyph alpha");
    }
}
