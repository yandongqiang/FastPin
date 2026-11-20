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
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gpui::{
    deferred, div, img, prelude::*, px, size, App, AsyncApp, Bounds, Context, CursorStyle, Entity,
    Focusable, FontWeight, ImageSource, MouseButton, RenderImage, SharedString, Subscription,
    Window, WindowBounds, WindowHandle, WindowOptions,
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
                .update(cx, |view, _, cx| view.load_history_on_open(cx))
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectId {
    Language,
    StrokeWidth,
    MosaicBlock,
    WaterPosition,
    WaterOpacity,
    WaterScale,
    ExportFormat,
    ExportQuality,
    SmartSelection,
    AutoScrollInterval,
    AutoScrollNotches,
    HistoryRetention,
    HistoryLimit,
    HistoryDisk,
    Appearance,
    HistoryFilter,
}

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
    ("60%（最小）", 60),
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

pub struct SettingsView {
    page: Page,
    settings: Settings,
    status: SharedString,
    status_error: bool,
    open_select: Option<SelectId>,
    prompt_open: bool,
    /// Which action is being bound, if any. The recording hook is live for
    /// exactly as long as this is `Some`.
    recording: Option<HotkeyAction>,
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
    search: Entity<TextInput>,
    /// The global reset asks for a second click; a single stray press must not
    /// wipe every setting.
    confirm_reset_all: bool,
    /// Two-click arming for the plaintext-HTTP API Key warning. The second click
    /// persists `allowInsecureHttp`, so the runtime guard and this banner agree.
    http_warn_confirm: bool,
    /// Sizes for the Data page, sampled when the window opens and after a
    /// cleanup — never on every frame.
    data_bytes: u64,
    log_bytes: u64,
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
    /// In-flight update check guards against double clicks.
    update_checking: bool,
    /// (version, url) of a newer release, when the last check found one.
    update_available: Option<(String, String)>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = crate::config::get();
        let t = settings.translation.clone();
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
        let search = cx.new(|cx| TextInput::new(cx, "", "搜索所有配置...", false).flat());

        let export_name = cx.new(|cx| {
            TextInput::new(
                cx,
                settings.screenshot.export.filename_template.clone(),
                crate::config::DEFAULT_FILENAME_TEMPLATE,
                false,
            )
        });

        let mut subscriptions = Vec::new();

        // Every field compares before storing: the inputs notify on cursor moves
        // too, and writing the file on a click would be pure churn.
        subscriptions.push(cx.observe(&base_url, |this, input, cx| {
            let value = input.read(cx).text().to_string();
            if this.settings.translation.base_url != value {
                // A confirmation only applies to the http:// endpoint it was
                // given for; switching to https:// must revoke it.
                if !crate::translate::is_insecure_http(&value) {
                    this.settings.translation.allow_insecure_http = false;
                }
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

        Self {
            page: Page::General,
            settings,
            status: SharedString::default(),
            status_error: false,
            open_select: None,
            prompt_open: false,
            recording: None,
            autostart: system::autostart_enabled(),
            saved_api_key,
            base_url,
            api_key,
            model,
            timeout,
            prompt,
            wm_text,
            export_name,
            search,
            confirm_reset_all: false,
            http_warn_confirm: false,
            data_bytes,
            log_bytes,
            history_records: Vec::new(),
            history_thumbs: HashMap::new(),
            history_thumb_mtime: HashMap::new(),
            history_thumb_seq: 0,
            history_status: SharedString::default(),
            history_status_seq: 0,
            history_selected: None,
            history_confirm_delete: false,
            history_confirm_clear: false,
            history_filter: "all".to_string(),
            update_checking: false,
            update_available: None,
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

    fn persist(&mut self, cx: &mut Context<Self>) {
        match crate::config::save(&self.settings) {
            Ok(()) => {
                self.status = SharedString::default();
                self.status_error = false;
            }
            Err(e) => self.fail(format!("设置保存失败：{e}")),
        }
        cx.notify();
    }

    fn commit_api_key(&mut self, cx: &mut Context<Self>) {
        let key = self.api_key.read(cx).text().to_string();
        if key == self.saved_api_key {
            return;
        }
        match crate::translate::set_api_key(&key) {
            Ok(()) => {
                if key.trim().is_empty() && self.settings.translation.allow_insecure_http {
                    self.settings.translation.allow_insecure_http = false;
                    self.persist(cx);
                }
                self.http_warn_confirm = false;
                self.saved_api_key = key;
                self.ok("API Key 已保存");
            }
            Err(e) => self.fail(format!("API Key 保存失败：{e}")),
        }
        cx.notify();
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
        self.http_warn_confirm = false;
    }

    fn go_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        self.open_select = None;
        self.confirm_reset_all = false;
        self.http_warn_confirm = false;
        // Leaving/entering a page disarms any pending destructive confirmation.
        self.history_confirm_delete = false;
        self.history_confirm_clear = false;
        self.search.update(cx, |input, cx| input.set_text("", cx));
        if self.page == Page::History {
            // Load records + decode thumbnails on entry so the grid is never a
            // blank page. Refreshing again is harmless (sequence-guarded).
            self.reload_history(cx);
        }
        cx.notify();
    }

    /// Load history as soon as the window opens. The settings window can be
    /// launched directly into the History page (or opened there before any
    /// nav click), so waiting only for `go_page` would leave a blank grid.
    fn load_history_on_open(&mut self, cx: &mut Context<Self>) {
        self.reload_history(cx);
    }

    fn reset_page(&mut self, cx: &mut Context<Self>) {
        if self.page == Page::General {
            // Autostart lives in the registry, not settings.json.
            if self.autostart {
                if let Err(e) = system::set_autostart(false) {
                    self.fail(format!("开机启动设置失败：{e}"));
                    cx.notify();
                    return;
                }
                self.autostart = system::autostart_enabled();
            }
            self.ok("已重置「常规」");
            cx.notify();
            return;
        }
        if model::reset_page(&mut self.settings, self.page) {
            if self.page == Page::Translate {
                // The key lives in the credential vault, not settings.json:
                // a page reset must remove it too, not just blank the field.
                let _ = crate::translate::set_api_key("");
                self.saved_api_key.clear();
            }
            // The appearance page resets to light; re-projecting the (possibly
            // unchanged) theme here keeps the chrome and every other window in
            // step without a second click.
            theme::set_dark(cx, self.settings.dark());
            self.sync_inputs(cx);
            self.persist(cx);
            self.ok(format!("已重置「{}」", self.page.title()));
        } else {
            self.ok("本页没有可重置的配置");
        }
        cx.notify();
    }

    fn reset_all(&mut self, cx: &mut Context<Self>) {
        model::reset_all(&mut self.settings);
        // Resetting everything includes the API Key stored in the vault.
        let _ = crate::translate::set_api_key("");
        self.saved_api_key.clear();
        let _ = system::set_autostart(false);
        theme::set_dark(cx, self.settings.dark());
        self.autostart = system::autostart_enabled();
        self.sync_inputs(cx);
        self.confirm_reset_all = false;
        self.persist(cx);
        self.ok("所有设置已恢复默认");
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
        self.history_records = self
            .history_store()
            .and_then(|s| s.list().ok())
            .unwrap_or_default();
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
        match self.history_store().and_then(|s| s.remove(&id).ok()) {
            Some(()) => {
                self.set_history_status("已删除", false, cx);
                self.history_selected = None;
                self.reload_history(cx);
            }
            None => self.set_history_status("删除失败", false, cx),
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

    /// Data-page "清空截图历史" button. Uses the Data page's status line.
    fn clear_history(&mut self, cx: &mut Context<Self>) {
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
                self.fail(format!("{e}"));
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
                self.fail(format!("{e}"));
                cx.notify();
            }
        }
    }

    fn toggle_toolbar_item(&mut self, id: &str, cx: &mut Context<Self>) {
        let hidden = &mut self.settings.screenshot.hidden_toolbar_items;
        match hidden.iter().position(|h| h == id) {
            Some(i) => {
                hidden.remove(i);
            }
            None => hidden.push(id.to_string()),
        }
        self.persist(cx);
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
        if self.settings.dark() {
            "深色".into()
        } else {
            "浅色".into()
        }
    }

    /// Dark/light switch: save first, then push the theme into gpui-kit and
    /// refresh every window so the toolbar and this window repaint together.
    fn set_appearance(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.open_select = None;
        let key = if dark {
            crate::config::APPEARANCE_DARK
        } else {
            crate::config::APPEARANCE_LIGHT
        };
        if self.settings.appearance != key {
            self.settings.appearance = key.to_string();
            theme::set_dark(cx, dark);
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
                        .child(widgets::select_list().children(rows)),
                ))
            })
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
            None => "未设置".into(),
        }
    }

    fn start_recording(&mut self, action: HotkeyAction, cx: &mut Context<Self>) {
        // Clicking the field that is already recording cancels it.
        if self.recording == Some(action) {
            self.cancel_recording(cx);
            return;
        }
        self.recording = Some(action);
        self.ok("按下新的组合键");
        crate::platform::start_recording();

        // Bounded no matter what happens to this window: the recorder swallows
        // bindable keys while it runs, so it must never outlive a stuck UI.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RECORDING_TIMEOUT).await;
            crate::platform::stop_recording();
            let _ = this.update(cx, |view, cx| {
                if view.recording == Some(action) {
                    view.recording = None;
                    view.fail("录制超时，快捷键未改动");
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        crate::platform::stop_recording();
        self.recording = None;
        self.status = SharedString::default();
        self.status_error = false;
        cx.notify();
    }

    fn hotkey_recorded(&mut self, pressed: Hotkey, cx: &mut Context<Self>) {
        let Some(action) = self.recording.take() else {
            crate::platform::stop_recording();
            return;
        };
        crate::platform::stop_recording();

        let text = hotkey::format(pressed);
        // Two actions on one combo means one of them silently never fires.
        let clash = [
            (HotkeyAction::Region, &self.settings.hotkeys.region),
            (HotkeyAction::CopyOnly, &self.settings.hotkeys.copy_only),
            (HotkeyAction::Fullscreen, &self.settings.hotkeys.fullscreen),
        ]
        .into_iter()
        .find(|(other, stored)| *other != action && hotkey::parse(stored) == Some(pressed))
        .map(|(other, _)| other);

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
        self.persist(cx);
        // Takes effect now; both the LL hook and the RegisterHotKey fallback
        // hold their own copy of the bindings.
        crate::platform::apply_bindings(HotkeyBindings::from_settings(&self.settings.hotkeys));
        if self.status.is_empty() {
            self.ok(format!("已设为 {text}"));
        }
        cx.notify();
    }

    fn set_autostart(&mut self, cx: &mut Context<Self>) {
        let want = !self.autostart;
        match system::set_autostart(want) {
            Ok(()) => {
                self.status = SharedString::default();
                self.status_error = false;
            }
            Err(e) => self.fail(format!("开机启动设置失败：{e}")),
        }
        // Re-read rather than assume: the registry is the truth.
        self.autostart = system::autostart_enabled();
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
                // A model directory only counts once the three files are there;
                // keep the choice but say so right away. The resources page
                // recomputes the status on every render, so no cached label.
                self.settings.ocr.models_dir = Some(dir);
                self.persist(cx);
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
        let task = cx.background_spawn(async move { crate::update::check_latest() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.update_checking = false;
                match result {
                    Ok(release)
                        if crate::update::is_newer(
                            &release.version,
                            crate::update::current_version(),
                        ) =>
                    {
                        view.ok(format!("发现新版本 v{}", release.version));
                        view.update_available = Some((release.version, release.url));
                    }
                    Ok(_) => {
                        view.update_available = None;
                        view.ok("当前已是最新版本");
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
        let Some((_, url)) = &self.update_available else {
            return;
        };
        if let Err(e) = crate::update::open_release_page(url) {
            self.fail(e.to_string());
            cx.notify();
        }
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
                            this.ok("再次点击「重置所有设置」以确认");
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

        let smart_mode = self.settings.screenshot.smart_selection;
        let smart_label: SharedString = smart_mode.label().into();
        let smart_rows = crate::config::SmartSelectionMode::ALL
            .into_iter()
            .map(|mode| {
                widgets::select_option(
                    SharedString::from(format!("smart-{}", mode.id())),
                    mode.label(),
                    mode == smart_mode,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.settings.screenshot.smart_selection = mode;
                        this.open_select = None;
                        this.persist(cx);
                    }),
                )
            })
            .collect();
        let smart = self.select_row(
            cx,
            SelectId::SmartSelection,
            "智能选区",
            "框选时吸附到窗口或其内部元素；「禁用」为自由拖拽。",
            smart_label,
            smart_rows,
        );

        div()
            .flex()
            .flex_col()
            .child(widgets::rows(vec![
                widgets::stack(
                    "工具栏按钮",
                    "取消勾选的按钮不会出现在截图工具栏上，完成和取消始终显示。",
                )
                .child(div().flex().flex_col().gap(px(2.)).children(grid)),
                widgets::row("默认颜色", "新画笔、形状和文字标注的颜色。").child(widgets::control(
                    div().flex().flex_row().gap(px(8.)).children(swatches),
                )),
                width,
                mosaic,
                smart,
                self.scroll_interval_row(cx),
                self.scroll_notches_row(cx),
                widgets::unsupported_row("截屏模式", "当前版本固定为冻结屏幕后框选区域。"),
            ]))
            .child(widgets::hint(
                "工具栏与注释默认值在下一次截图时生效；自动滚动的间隔与步长在下一次长截图时生效。",
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
            days.iter()
                .find(|(_, d)| *d == h.retention_days)
                .map(|(label, _)| (*label).into())
                .unwrap_or_else(|| format!("{} 天", h.retention_days).into())
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
                    !h.keep_permanently && day == h.retention_days
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
            "超过该天数的历史记录会在启动时和保存后自动清理；选择“永久”等同于开启“始终保留”。",
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
        let current: SharedString = format!("{} 张", h.limit).into();
        let rows = LIMITS
            .iter()
            .map(|(label, n)| {
                widgets::select_option(("hist-limit", *n as u32), *label, *n == h.limit)
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
        let current: SharedString = if h.max_disk_mib >= 1024 {
            format!("{} GB", h.max_disk_mib / 1024).into()
        } else {
            format!("{} MB", h.max_disk_mib).into()
        };
        let rows = SIZES
            .iter()
            .map(|(label, m)| {
                widgets::select_option(("hist-disk", *m as u32), *label, *m == h.max_disk_mib)
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
            Ok(p) => p.display().to_string().into(),
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
        let clear = widgets::row("清空历史", "删除所有历史记录；此操作不可撤销。").child(
            widgets::control(widgets::danger_button("hist-clear", "清空").on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.clear_history(cx)),
            )),
        );

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
                widgets::stack(
                    "水印文字",
                    "导出时烧录的文字，留空则仅当勾选时间或设置了图片时显示。",
                )
                .child(self.wm_text.clone()),
                self.watermark_position_row(cx),
                self.watermark_opacity_row(cx),
                self.watermark_scale_row(cx),
                image,
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
            .child(widgets::hint(
                "水印随导出一并烧录到图片，不写入原图；留空且未设图片时不添加任何水印。",
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
        let current: SharedString = current_format.label().into();
        let rows = ImageFormat::ALL
            .into_iter()
            // WebP comes from an optional Windows package; offering it where
            // the codec is missing would silently export PNG under a .webp
            // setting the user just picked on purpose.
            .filter(|f| crate::codec::format_available(*f))
            .map(|f| {
                widgets::select_option(("export-fmt", f as u32), f.label(), f == current_format)
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
            "PNG 无损体积大；JPG/WebP 有损但适合长页面与照片。",
            current,
            rows,
        )
    }

    fn export_quality_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let export = &self.settings.screenshot.export;
        // PNG ignores quality, so the row explains that instead of offering a
        // control that changes nothing.
        if !export.format.honours_quality() {
            return widgets::row(
                "图片质量",
                "PNG 为无损格式，质量设置对它不生效；选择 JPG 或 WebP 后可调。",
            );
        }
        let current: SharedString = EXPORT_QUALITIES
            .iter()
            .find(|(_, v)| *v == export.quality)
            .map(|(label, _)| (*label).into())
            .unwrap_or_else(|| format!("{}%", export.quality).into());
        let quality = export.quality;
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
        widgets::rows(vec![
            widgets::row(
                "贴图工具栏按钮",
                "贴图编辑沿用「截屏 → 工具栏按钮」的显隐设置。",
            )
            .child(widgets::control(
                widgets::button("pin-goto-toolbar", "打开截屏设置").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.go_page(Page::Screenshot, cx)),
                ),
            )),
            widgets::unsupported_row(
                "贴图默认行为",
                "当前版本暂不支持配置贴图的默认缩放、透明度或关闭方式。",
            ),
        ])
    }

    fn page_appearance(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dark = self.settings.dark();
        let rows = [("浅色", false), ("深色", true)]
            .map(|(label, value)| {
                widgets::select_option(
                    SharedString::from(format!("appearance-{value}")),
                    label,
                    dark == value,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.set_appearance(value, cx)),
                )
            })
            .into_iter()
            .collect();
        let select = self.select_row(
            cx,
            SelectId::Appearance,
            "外观模式",
            "切换整个界面的浅色 / 深色主题。",
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
        let current = self.settings.translation.target_language.clone();
        let rows = LANGUAGES
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
            rows,
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

        let mut rows: Vec<_> = vec![
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
            widgets::row("超时", "秒，5 到 300。").child(widgets::control(
                div().w(px(120.)).child(self.timeout.clone()),
            )),
            prompt,
        ];
        // A key on a plaintext endpoint is the one case where the vault is not
        // enough: show a two-click acknowledgement and gate the runtime on it.
        if !self.saved_api_key.trim().is_empty()
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
                            this.persist(cx);
                            this.ok("已撤销明文 HTTP 授权");
                        } else if this.http_warn_confirm {
                            this.settings.translation.allow_insecure_http = true;
                            this.http_warn_confirm = false;
                            this.persist(cx);
                            this.ok("已确认：允许明文 HTTP 发送 API Key");
                        } else {
                            this.http_warn_confirm = true;
                            this.ok("再次点击以确认明文传输风险");
                        }
                        cx.notify();
                    }),
                )),
            );
        }
        widgets::rows(rows)
    }

    fn page_resources(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Recomputed on every render, never cached on the view: deleting or
        // swapping model files outside the app has to move the dot and the
        // label in the same frame.
        let engine = crate::ocr::engine_status();
        let default_dir: SharedString = engine
            .dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未找到".into())
            .into();
        let models: SharedString = match &self.settings.ocr.models_dir {
            Some(p) => p.display().to_string().into(),
            None => format!("默认：{default_dir}").into(),
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
                            ),
                    ),
                ),
                widgets::stack("模型目录", "覆盖内置的 models\\v6 目录，留空使用默认位置。").child(
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
            .child(widgets::hint(
                "需要检测、识别与字典三个模型文件同时存在且非空，取字与翻译才可用。",
            ))
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

        // Source filter dropdown (全部 / 保存到文件 / 复制到剪贴板).
        const FILTERS: [(&str, &str); 3] = [
            ("all", "全部来源"),
            ("saved_to_file", "保存到文件"),
            ("copied_to_clipboard", "复制到剪贴板"),
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
                        .child(widgets::select_list().children(filter_rows)),
                ))
            });

        let mut grid = div()
            .id("settings-history-grid")
            .w_full()
            .flex()
            .flex_wrap()
            .gap(px(16.));

        if visible.is_empty() {
            let (title, hint) = if self.history_records.is_empty() {
                (
                    "还没有截图历史。",
                    "保存截图或将截图复制到剪贴板后会自动记录到这里。",
                )
            } else {
                ("当前筛选下没有图片。", "试试将来源筛选切换为“全部来源”。")
            };
            grid = grid.child(
                div()
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
                    ),
            );
        } else {
            for (i, record) in visible.iter().enumerate() {
                let selected = self.history_selected.as_ref() == Some(&record.id);
                let id = record.id.clone();
                let thumb = self.history_thumbs.get(&record.id).and_then(|t| t.clone());
                grid = grid.child(
                    div()
                        .id(("settings-history-card", i))
                        .flex_none()
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
                        .child(
                            div()
                                .w(px(THUMB - 2.0 * PAD))
                                .h(px(THUMB - 2.0 * PAD))
                                .rounded(px(6.))
                                .bg(theme::muted())
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(match thumb {
                                    Some(image) => img(ImageSource::Render(image))
                                        .w(px(THUMB - 2.0 * PAD))
                                        .h(px(THUMB - 2.0 * PAD))
                                        .rounded(px(6.))
                                        .into_any_element(),
                                    None => div()
                                        .text_size(px(11.))
                                        .text_color(theme::secondary_fg())
                                        .child("无预览")
                                        .into_any_element(),
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(3.))
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme::primary_fg())
                                        .child(crate::history::format_utc(record.created_ms)),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(theme::secondary_fg())
                                        .child(format!(
                                            "{} × {} · {}",
                                            record.width,
                                            record.height,
                                            crate::history::source_label(&record.source),
                                        )),
                                ),
                        ),
                );
            }
        }

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
            .child(grid)
            .into_any_element()
    }

    fn page_about(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // One button, two jobs: check first, then become the download link
        // once a newer release is known.
        let update_button = if let Some((version, _)) = &self.update_available {
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
        widgets::rows(vec![
            widgets::row(crate::APP_NAME, "轻量截图与文字识别工具。")
                .child(widgets::control(widgets::muted_text(version))),
            widgets::row("检查更新", "从发布页获取最新版本。")
                .child(widgets::control(update_button)),
        ])
    }

    fn search_results(&self, query: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let hits = model::search(query);
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

/// Gallery source-filter predicate. `"all"` accepts everything; other values
/// match a record's source string exactly.
fn history_matches_filter(source: &str, filter: &str) -> bool {
    filter == "all" || source == filter
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

        let header = if searching {
            let n = model::search(&query).len();
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
            self.search_results(&query, cx)
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
    fn about_page_names_the_product_fastpin() {
        assert_eq!(crate::APP_NAME, "FastPin");
        assert_eq!(format!("{} 设置", crate::APP_NAME), "FastPin 设置");
        // The About row and the title both render this constant, so pinning it
        // pins every place that names the product.
        let source = include_str!("mod.rs");
        assert!(source.contains("widgets::row(crate::APP_NAME"));
        assert!(source.contains("format!(\"{} 设置\", crate::APP_NAME)"));
    }

    /// The window title must be drawn once, in the component title bar. The
    /// left rail used to repeat the logo, the product name and "设置" above
    /// the navigation, which read as two captions for the same window.
    #[test]
    fn nav_pane_no_longer_repeats_the_brand_header() {
        let source = include_str!("mod.rs");
        let nav_start = source.find("fn nav(").expect("nav is defined");
        let nav_end = source
            .find("fn search_field")
            .expect("search_field follows nav");
        let nav = &source[nav_start..nav_end];
        assert!(
            !nav.contains("crate::APP_NAME"),
            "left rail must not draw the product name"
        );
        assert!(
            !nav.contains("logo_element"),
            "left rail must not repeat the brand mark"
        );
    }

    /// The search field lives inside the title bar now; the content area no
    /// longer spends a 56px row on it.
    #[test]
    fn search_field_is_embedded_in_the_title_bar() {
        let source = include_str!("mod.rs");
        let title_start = source.find("fn titlebar").expect("titlebar is defined");
        let title_end = source.find("fn nav(").expect("nav follows titlebar");
        let titlebar = &source[title_start..title_end];
        assert!(
            titlebar.contains("self.search_field()"),
            "title bar must own the search field"
        );
        assert!(
            !source.contains(concat!("fn search", "_bar")),
            "the standalone search row was removed"
        );
        assert!(
            titlebar.contains("format!(\"{} 设置\", crate::APP_NAME)"),
            "the single window caption stays in the title bar"
        );
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

    /// The old product name survives only in the migration constants that must
    /// find pre-rebrand user data; every other identifier is named FastPin now.
    /// The former crate directory must be gone from the whole source tree too.
    #[test]
    fn source_only_keeps_legacy_migration_literals() {
        // Built from pieces so this test's own source never contains any of
        // the literals it is scanning for.
        let needle = concat!("zsh", "ot");
        let old_path_fwd = concat!("crates/", "zsh", "ot");
        let old_path_bwd = concat!("crates\\", "zsh", "ot");
        // (file name, marker present on the exempt line). External URLs are
        // skipped: they are not internal identifiers.
        let exempt: &[(&str, &str)] = &[
            ("migrate.rs", "LEGACY_DIR"),
            ("migrate.rs", "root.join"),
            ("system.rs", "LEGACY_RUN_VALUE"),
            ("translate.rs", "LEGACY_VAULT_RESOURCE"),
            // README 里的上游来源声明 / 致谢是用户可见的历史事实（用户
            // 2026-11-12 要求保留「基于旧项目二次开发」并致谢），不是内部标识
            // 残留；用该句原文作标记只豁免这些致谢行，README 其余命中照常报错。
            ("README.md", "二次vibe coding开发"),
            ("README.md", "二次开发"),
            // Phase 1 审计文档引用历史命名（C# 命名空间、README 原文摘录）作为
            // 证据，属于说明性引用而非代码残留；标记用 concat! 拼接，避免本文件
            // 自身命中扫描 needle。
            ("PHASE1_AUDIT_REPORT.md", concat!("Zsh", "ot.Translation")),
            ("PHASE1_AUDIT_REPORT.md", "二次vibe coding开发"),
        ];
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = crate_dir
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root");
        let mut stack = vec![workspace_root.to_path_buf()];
        let mut offenders: Vec<String> = Vec::new();
        let mut old_paths: Vec<String> = Vec::new();
        while let Some(dir) = stack.pop() {
            if !dir.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&dir).expect("source directory") {
                let path = entry.expect("directory entry").path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if path.is_dir() {
                    // Build cache and VCS metadata are not source and can bake
                    // in stale paths from earlier layouts.
                    if name != "target" && name != ".git" {
                        stack.push(path);
                    }
                    continue;
                }
                if name.ends_with(".lock") {
                    continue;
                }
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // Skip binary assets (icons, models, DLLs): a NUL in the head
                // is how ripgrep itself tells binary from text.
                if bytes.len() > 8 * 1024 * 1024 || bytes[..bytes.len().min(8192)].contains(&0) {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes);
                for (i, line) in text.lines().enumerate() {
                    if line.contains("http://") || line.contains("https://") {
                        continue;
                    }
                    let lower = line.to_ascii_lowercase();
                    let where_ = format!("{}:{}", path.display(), i + 1);
                    if lower.contains(needle)
                        && !exempt.iter().any(|pair| {
                            let (f, m) = *pair;
                            f == name && line.contains(m)
                        })
                    {
                        offenders.push(format!("{}: {}", where_, line.trim()));
                    }
                    if lower.contains(old_path_fwd) || lower.contains(old_path_bwd) {
                        old_paths.push(format!("{}: {}", where_, line.trim()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "old product name outside the migration constants:\n{}",
            offenders.join("\n")
        );
        assert!(
            old_paths.is_empty(),
            "old crate path still present in the source tree:\n{}",
            old_paths.join("\n")
        );
    }
}
