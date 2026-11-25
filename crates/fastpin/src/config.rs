//! `settings.json` in `%LOCALAPPDATA%\FastPin`. Cached after the first read;
//! `save` swaps the cache so the settings window takes effect without a restart.
//!
//! The API key is deliberately absent: it stays in the Windows credential
//! vault, shared with the C# build. See `translate::api_key`.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub ocr: Ocr,
    pub translation: Translation,
    pub screenshot: Screenshot,
    pub history: History,
    pub hotkeys: Hotkeys,
    /// UI 外观：gpui-kit 主题注册名（themes 目录里各 `themes` 条目的 `name`，
    /// 如 `"Ayu Light"`，以及默认 `"Default Light"` / `"Default Dark"`），默认
    /// `"Default Light"`。纯字符串保持配置文件可手改；未知值在启动时回退到
    /// 默认浅色主题，不会让整个配置文件失效。
    pub appearance: String,
}

/// gpui-kit 默认浅色变体在 `ThemeRegistry` 里的注册名，也是 `appearance` 的默认值。
pub const DEFAULT_APPEARANCE: &str = "Default Light";
/// gpui-kit 默认深色变体的注册名，仅供 light/dark 布尔入口与下拉回退使用。
pub const DEFAULT_DARK_APPEARANCE: &str = "Default Dark";

/// 旧版 `dark()` 的明暗启发式：真实变体的明暗由 `ThemeConfig.mode` 决定，
/// 这里只按名字猜测，供纯逻辑调用方与测试兼容，UI 不再依赖。
pub fn appearance_dark(name: &str) -> bool {
    name.trim().to_ascii_lowercase().contains("dark")
}

/// 兼容用途：主题标签就是 registry 里的注册名，下拉由 `theme::available_themes`
/// 提供；这里原样返回手改的配置值，方便在设置页发现拼写错误。
pub fn appearance_label(name: &str) -> String {
    name.trim().to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ocr: Ocr::default(),
            translation: Translation::default(),
            screenshot: Screenshot::default(),
            history: History::default(),
            hotkeys: Hotkeys::default(),
            appearance: DEFAULT_APPEARANCE.into(),
        }
    }
}

impl Settings {
    /// 仅按名字猜测明暗，供纯逻辑调用方与测试兼容；真实变体的明暗由
    /// 应用主题时的 `ThemeConfig.mode` 决定，UI 不再依赖这里。
    pub fn dark(&self) -> bool {
        appearance_dark(&self.appearance)
    }
}

/// Stored the way the settings page shows them ("Ctrl+Alt+A"), so the file
/// stays hand-editable. An unparsable entry falls back to its default rather
/// than leaving the action unbound; see `hotkey::HotkeyBindings::from_settings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Hotkeys {
    pub region: String,
    pub copy_only: String,
    pub fullscreen: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            region: "Ctrl+Alt+A".into(),
            copy_only: "Ctrl+Alt+W".into(),
            fullscreen: "PrtSc".into(),
        }
    }
}

/// Auto-start is deliberately not here: the `Run` registry value is the truth,
/// and a copy in this file would desync the moment the user removes that entry
/// from Task Manager. See `settings::autostart`.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Ocr {
    /// Overrides the default `<exe dir>\models\v6`.
    pub models_dir: Option<PathBuf>,
}

/// Defaults match the C# `AppConfig` so both builds behave the same when the
/// file is missing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Translation {
    pub base_url: String,
    pub model: String,
    pub target_language: String,
    pub prompt: String,
    pub timeout_seconds: u32,
    /// Explicit, persisted acknowledgement that the API Key may be sent over
    /// plaintext HTTP. Confirmation is a two-click action in 设置 → 翻译; the
    /// runtime refuses to send a key over `http://` while this is false.
    pub allow_insecure_http: bool,
}

impl Default for Translation {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: "gpt-4o-mini".into(),
            target_language: "zh-CN".into(),
            prompt: String::new(),
            timeout_seconds: 30,
            allow_insecure_http: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Screenshot {
    /// Toolbar ids the user turned off. Empty keeps every tool on the bar,
    /// which is what the build did before this field existed — the C# default
    /// of hiding four tools would silently remove two working ones here.
    pub hidden_toolbar_items: Vec<String>,
    /// Where 保存 writes. `None` means `Pictures\FastPin`; see `screenshot_dir`.
    pub save_dir: Option<PathBuf>,
    pub annotation: Annotation,
    /// Exported-image watermark (text and/or image). Empty by default — the
    /// export composite only touches the image when this is visible.
    pub watermark: Watermark,
    /// How the region picker snaps to windows. See [`SmartSelectionMode`].
    pub smart_selection: SmartSelectionMode,
    /// Auto-scroll cadence for long screenshots. See [`AutoScroll`].
    pub auto_scroll: AutoScroll,
    /// Encoded-file format, quality and name for 保存 / export.
    pub export: Export,
    /// Static image collage settings (direction, spacing, border, grid).
    pub collage: CollageSettings,
    /// PDF export settings (paper size, margins, template, stitching method).
    pub pdf: PdfSettings,
    /// 图片切图设置（方向、类型、单位、数量 / 网格、精确尺寸与重叠）。
    /// 与 collage / pdf 不同，这里直接复用可序列化的 `imagesplit::SplitSettings`，
    /// 由图片切图窗口的 setter 逐项写回。
    pub split: crate::imagesplit::SplitSettings,
    /// 截屏时使用的捕获方式。
    pub capture_mode: CaptureMode,
    /// 截屏结果中是否显示鼠标光标。
    pub show_cursor: bool,
    /// 开始截屏且尚未创建选区时是否自动显示辅助线。
    pub show_guides: bool,
    /// 进入长截屏工具时的开始策略。
    pub long_capture_auto_start: LongCaptureAutoStart,
    /// 选区完成后双击左键执行的操作。
    pub left_double_click: ClickAction,
    /// 选区完成后单击中键执行的操作。
    pub middle_click: ClickAction,
    /// 点击选区外时的行为。
    pub click_outside: ClickOutside,
    /// 自动记录到历史图片的截屏类型。
    pub record_history_sources: Vec<String>,
    /// 截屏工具栏的位置策略。
    pub toolbar_position: ToolbarPosition,
    /// 参数面板的位置策略。
    pub panel_position: PanelPosition,
    /// 旧版「记住位置」策略使用的工具栏左上角（截屏逻辑像素，未乘缩放）。
    /// 截屏流程从不写入该字段，策略因此已不再提供给用户；保留字段仅为兼容
    /// 旧配置文件，值永远是 `None`，布局退回「跟随选区」。
    pub toolbar_remember_pos: Option<(f32, f32)>,
    /// 旧版「记住位置」策略使用的参数面板左上角（截屏逻辑像素，未乘缩放）。
    /// 同 `toolbar_remember_pos`：没有写入点，已不在设置页提供该选项。
    pub panel_remember_pos: Option<(f32, f32)>,
    /// 是否在鼠标附近显示像素放大预览和当前颜色信息。
    pub show_magnifier: bool,
    /// 是否在选区附近显示尺寸、圆角和比例等信息。
    pub show_info_bar: bool,
    /// 是否每次开始截屏时显示当前阶段可用的快捷键提示。
    pub show_shortcut_hints: bool,
}

impl Default for Screenshot {
    fn default() -> Self {
        Self {
            hidden_toolbar_items: Vec::new(),
            save_dir: None,
            annotation: Annotation::default(),
            watermark: Watermark::default(),
            smart_selection: SmartSelectionMode::default(),
            auto_scroll: AutoScroll::default(),
            export: Export::default(),
            collage: CollageSettings::default(),
            pdf: PdfSettings::default(),
            split: crate::imagesplit::SplitSettings::default(),
            capture_mode: CaptureMode::default(),
            show_cursor: false,
            show_guides: false,
            long_capture_auto_start: LongCaptureAutoStart::default(),
            left_double_click: ClickAction::default(),
            middle_click: ClickAction::default(),
            click_outside: ClickOutside::default(),
            // 当前窗口截屏没有采集入口，选项已从设置页移除；旧配置里残留的
            // `currentWindow` 仍被历史读取逻辑识别，只是不再自动记录。
            record_history_sources: vec!["region".into(), "copyOnly".into(), "fullscreen".into()],
            toolbar_position: ToolbarPosition::default(),
            panel_position: PanelPosition::default(),
            toolbar_remember_pos: None,
            panel_remember_pos: None,
            show_magnifier: true,
            show_info_bar: true,
            show_shortcut_hints: true,
        }
    }
}

/// Long-capture auto-scroll cadence, persisted so the settings page can honour
/// the README's "interval and step are configurable" promise. Integers keep the
/// file hand-editable; [`scroll_step`](crate::scroll_step) stays the single
/// source of truth for the bounds applied when the values are used.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AutoScroll {
    /// Beat interval in milliseconds, clamped to `[128, 1000]` when used.
    pub interval_ms: u32,
    /// Native wheel notches per step, clamped to `[1, 8]` when used.
    pub notches: u32,
}

impl Default for AutoScroll {
    fn default() -> Self {
        Self {
            interval_ms: crate::scroll_step::DEFAULT_INTERVAL.as_millis() as u32,
            notches: crate::scroll_step::DEFAULT_NOTCHES,
        }
    }
}

impl AutoScroll {
    /// Builds the runtime step, applying the shared clamps so a hand-edited
    /// settings.json cannot push past the stitcher's motion window.
    pub fn step(&self) -> crate::scroll_step::ScrollStep {
        let mut step = crate::scroll_step::ScrollStep::new();
        step.set_interval(std::time::Duration::from_millis(u64::from(
            self.interval_ms,
        )));
        step.set_notches(self.notches);
        step
    }
}

/// Container format for an exported screenshot. PNG is lossless and stays the
/// default because it is the only format every paste target and every build of
/// this app has always produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImageFormat {
    #[default]
    #[serde(rename = "png")]
    Png,
    /// Lossy, smallest files for photos and long pages. Quality comes from
    /// [`Export::quality`].
    #[serde(rename = "jpeg")]
    Jpeg,
    #[serde(rename = "webp")]
    Webp,
}

impl ImageFormat {
    pub const ALL: [Self; 3] = [Self::Png, Self::Jpeg, Self::Webp];

    /// Lowercase extension without the dot, as written to disk.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }

    /// Label for the settings dropdown.
    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG（无损）",
            Self::Jpeg => "JPG（有损）",
            Self::Webp => "WebP",
        }
    }

    /// Whether [`Export::quality`] changes the output. PNG ignores it, so the
    /// settings page hides the slider for that format instead of showing a
    /// control that does nothing.
    pub fn honours_quality(self) -> bool {
        !matches!(self, Self::Png)
    }
}

/// How an exported image is encoded and named.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Export {
    pub format: ImageFormat,
    /// Encoder quality, 1..=100. Clamped on read; see [`Export::clamped_quality`].
    pub quality: u8,
    /// File stem template. Supports `{date}` (YYYYMMDD), `{time}` (HHMMSS) and
    /// `{datetime}` (YYYYMMDD_HHMMSS) placeholders. Empty falls back to
    /// [`DEFAULT_FILENAME_TEMPLATE`].
    pub filename_template: String,
    /// 完成截图时是否自动写文件。默认关闭，保持旧行为（完成只复制到剪贴板）。
    pub auto_save: bool,
    /// 完成截图时是否自动复制到剪贴板。默认开启，保持旧行为。
    pub auto_copy: bool,
}

/// The name every build produced before this setting existed, so an untouched
/// config keeps writing exactly the same files.
pub const DEFAULT_FILENAME_TEMPLATE: &str = "FastPin_{date}_{time}";

impl Default for Export {
    fn default() -> Self {
        Self {
            format: ImageFormat::Png,
            quality: 90,
            filename_template: DEFAULT_FILENAME_TEMPLATE.to_string(),
            auto_save: false,
            auto_copy: true,
        }
    }
}

impl Export {
    /// Quality the encoder is actually given. Out-of-range values come from a
    /// hand-edited config, not from any UI path.
    pub fn clamped_quality(&self) -> u8 {
        self.quality.clamp(1, 100)
    }

    /// Template actually expanded: a blank or whitespace-only entry means the
    /// default rather than a nameless file.
    pub fn effective_template(&self) -> &str {
        let t = self.filename_template.trim();
        if t.is_empty() {
            DEFAULT_FILENAME_TEMPLATE
        } else {
            t
        }
    }

    /// Expands `{date}` / `{time}` / `{datetime}` and replaces characters that
    /// cannot appear in a Windows file name. Unknown `{...}` placeholders are
    /// left verbatim so a typo is visible in the resulting file name instead of
    /// being silently dropped.
    pub fn expand_name(&self, date: &str, time: &str) -> String {
        let datetime = format!("{date}_{time}");
        let expanded = self
            .effective_template()
            .replace("{date}", date)
            .replace("{time}", time)
            .replace("{datetime}", &datetime);
        sanitize_stem(&expanded)
    }
}

/// Static image collage settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CollageSettings {
    /// Arrangement direction.
    pub direction: Direction,
    /// Sizing mode.
    pub sizing: Sizing,
    /// Border around the whole collage (pixels).
    pub border: u32,
    /// Spacing between images (pixels).
    pub spacing: u32,
    /// Background color as RGBA.
    pub background_rgba: [u8; 4],
    /// Grid rows (for custom/grid direction).
    pub rows: u32,
    /// Grid columns (for custom/grid direction).
    pub columns: u32,
}

impl Default for CollageSettings {
    fn default() -> Self {
        Self {
            direction: Direction::Vertical,
            sizing: Sizing::Uniform,
            border: 0,
            spacing: 0,
            background_rgba: [255, 255, 255, 255],
            rows: 1,
            columns: 1,
        }
    }
}

/// Collage direction for static image stitching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum Direction {
    #[default]
    Vertical,
    Horizontal,
    Custom,
}

/// Sizing mode for static image stitching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum Sizing {
    #[default]
    Original,
    Uniform,
}

/// PDF export settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PdfSettings {
    /// Stitching method: automatic per-page or template-based.
    pub stitching_method: StitchingMethod,
    /// Template type for template-based stitching.
    pub template_type: TemplateType,
    /// Spacing between images on a page (mm).
    pub spacing_mm: f32,
    /// Paper size.
    pub paper_size: PaperSize,
    /// Page orientation.
    pub orientation: Orientation,
    /// Custom paper width (mm).
    pub custom_width_mm: f32,
    /// Custom paper height (mm).
    pub custom_height_mm: f32,
    /// Top margin (mm).
    pub margin_top_mm: f32,
    /// Bottom margin (mm).
    pub margin_bottom_mm: f32,
    /// Left margin (mm).
    pub margin_left_mm: f32,
    /// Right margin (mm).
    pub margin_right_mm: f32,
    /// Export mode: combined PDF or one PDF per page.
    pub export_mode: ExportMode,
    /// JPEG/WebP quality 1..100.
    pub image_quality: u8,
}

impl Default for PdfSettings {
    fn default() -> Self {
        Self {
            stitching_method: StitchingMethod::PageBased,
            template_type: TemplateType::TwoImages,
            spacing_mm: 5.0,
            paper_size: PaperSize::A4,
            orientation: Orientation::Portrait,
            custom_width_mm: 210.0,
            custom_height_mm: 297.0,
            margin_top_mm: 10.0,
            margin_bottom_mm: 10.0,
            margin_left_mm: 10.0,
            margin_right_mm: 10.0,
            export_mode: ExportMode::Combined,
            image_quality: 90,
        }
    }
}

// The settings file and the pure layout code keep separate copies of these
// types: one is serde-facing, the other is layout-only. These conversions are
// the single bridge between them.
impl From<&CollageSettings> for crate::collage::CollageSettings {
    fn from(s: &CollageSettings) -> Self {
        Self {
            direction: match s.direction {
                Direction::Vertical => crate::collage::Direction::Vertical,
                Direction::Horizontal => crate::collage::Direction::Horizontal,
                Direction::Custom => crate::collage::Direction::Custom,
            },
            sizing: match s.sizing {
                Sizing::Original => crate::collage::Sizing::Original,
                Sizing::Uniform => crate::collage::Sizing::Uniform,
            },
            border: s.border,
            spacing: s.spacing,
            background_rgba: s.background_rgba,
            rows: s.rows,
            columns: s.columns,
        }
    }
}

impl From<&crate::collage::CollageSettings> for CollageSettings {
    fn from(s: &crate::collage::CollageSettings) -> Self {
        Self {
            direction: match s.direction {
                crate::collage::Direction::Vertical => Direction::Vertical,
                crate::collage::Direction::Horizontal => Direction::Horizontal,
                crate::collage::Direction::Custom => Direction::Custom,
            },
            sizing: match s.sizing {
                crate::collage::Sizing::Original => Sizing::Original,
                crate::collage::Sizing::Uniform => Sizing::Uniform,
            },
            border: s.border,
            spacing: s.spacing,
            background_rgba: s.background_rgba,
            rows: s.rows,
            columns: s.columns,
        }
    }
}

impl From<&PdfSettings> for crate::collage::PdfSettings {
    fn from(s: &PdfSettings) -> Self {
        Self {
            stitching_method: match s.stitching_method {
                StitchingMethod::PageBased => crate::collage::StitchingMethod::PageBased,
                StitchingMethod::TemplateBased => crate::collage::StitchingMethod::TemplateBased,
            },
            template_type: match s.template_type {
                TemplateType::TwoImages => crate::collage::TemplateType::TwoImages,
                TemplateType::ThreeImages => crate::collage::TemplateType::ThreeImages,
                TemplateType::FourImages => crate::collage::TemplateType::FourImages,
                TemplateType::SixImages => crate::collage::TemplateType::SixImages,
                TemplateType::NineImages => crate::collage::TemplateType::NineImages,
            },
            spacing_mm: s.spacing_mm,
            paper_size: match s.paper_size {
                PaperSize::A3 => crate::collage::PaperSize::A3,
                PaperSize::A4 => crate::collage::PaperSize::A4,
                PaperSize::A5 => crate::collage::PaperSize::A5,
                PaperSize::B5 => crate::collage::PaperSize::B5,
                PaperSize::Letter => crate::collage::PaperSize::Letter,
                PaperSize::Legal => crate::collage::PaperSize::Legal,
                PaperSize::Custom => crate::collage::PaperSize::Custom(
                    s.custom_width_mm.round().max(1.0) as u32,
                    s.custom_height_mm.round().max(1.0) as u32,
                ),
                PaperSize::ImageSize => crate::collage::PaperSize::ImageSize,
            },
            orientation: match s.orientation {
                Orientation::Portrait => crate::collage::Orientation::Portrait,
                Orientation::Landscape => crate::collage::Orientation::Landscape,
            },
            margin_top_mm: s.margin_top_mm,
            margin_bottom_mm: s.margin_bottom_mm,
            margin_left_mm: s.margin_left_mm,
            margin_right_mm: s.margin_right_mm,
            export_mode: match s.export_mode {
                ExportMode::Combined => crate::collage::ExportMode::Combined,
                ExportMode::PerPage => crate::collage::ExportMode::PerPage,
            },
            image_quality: s.image_quality,
        }
    }
}

impl From<&crate::collage::PdfSettings> for PdfSettings {
    fn from(s: &crate::collage::PdfSettings) -> Self {
        let mut out = Self {
            stitching_method: match s.stitching_method {
                crate::collage::StitchingMethod::PageBased => StitchingMethod::PageBased,
                crate::collage::StitchingMethod::TemplateBased => StitchingMethod::TemplateBased,
            },
            template_type: match s.template_type {
                crate::collage::TemplateType::TwoImages => TemplateType::TwoImages,
                crate::collage::TemplateType::ThreeImages => TemplateType::ThreeImages,
                crate::collage::TemplateType::FourImages => TemplateType::FourImages,
                crate::collage::TemplateType::SixImages => TemplateType::SixImages,
                crate::collage::TemplateType::NineImages => TemplateType::NineImages,
            },
            spacing_mm: s.spacing_mm,
            paper_size: match s.paper_size {
                crate::collage::PaperSize::A3 => PaperSize::A3,
                crate::collage::PaperSize::A4 => PaperSize::A4,
                crate::collage::PaperSize::A5 => PaperSize::A5,
                crate::collage::PaperSize::B5 => PaperSize::B5,
                crate::collage::PaperSize::Letter => PaperSize::Letter,
                crate::collage::PaperSize::Legal => PaperSize::Legal,
                crate::collage::PaperSize::Custom(..) => PaperSize::Custom,
                crate::collage::PaperSize::ImageSize => PaperSize::ImageSize,
            },
            orientation: match s.orientation {
                crate::collage::Orientation::Portrait => Orientation::Portrait,
                crate::collage::Orientation::Landscape => Orientation::Landscape,
            },
            margin_top_mm: s.margin_top_mm,
            margin_bottom_mm: s.margin_bottom_mm,
            margin_left_mm: s.margin_left_mm,
            margin_right_mm: s.margin_right_mm,
            export_mode: match s.export_mode {
                crate::collage::ExportMode::Combined => ExportMode::Combined,
                crate::collage::ExportMode::PerPage => ExportMode::PerPage,
            },
            image_quality: s.image_quality,
            ..Self::default()
        };
        if let crate::collage::PaperSize::Custom(w, h) = s.paper_size {
            out.custom_width_mm = w as f32;
            out.custom_height_mm = h as f32;
        }
        out
    }
}

/// PDF stitching method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum StitchingMethod {
    #[default]
    PageBased,
    TemplateBased,
}

/// Template type for template-based PDF stitching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum TemplateType {
    #[default]
    TwoImages,
    ThreeImages,
    FourImages,
    SixImages,
    NineImages,
}

impl TemplateType {
    /// Capacity of each template (max images per page).
    pub fn capacity(&self) -> usize {
        match self {
            TemplateType::TwoImages => 2,
            TemplateType::ThreeImages => 3,
            TemplateType::FourImages => 4,
            TemplateType::SixImages => 6,
            TemplateType::NineImages => 9,
        }
    }
}

/// Paper sizes in millimetres.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum PaperSize {
    A3,
    #[default]
    A4,
    A5,
    B5,
    Letter,
    Legal,
    Custom,
    ImageSize,
}

/// PDF page orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
}

/// Export mode for PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum ExportMode {
    #[default]
    Combined,
    PerPage,
}

/// 文件名主干的长度上限。模板会被手工改得很长，截到 200 个字符可以给目录、
/// 扩展名和 MAX_PATH 留出充足余量，正常模板完全不受影响。
const MAX_FILENAME_STEM_CHARS: usize = 200;

/// Windows 保留设备名。即使后面带扩展名（如 `con.txt`）也不能作为文件名，
/// `CreateFile` 会直接失败；大小写不敏感，这里统一按大写比较。
const WINDOWS_RESERVED_STEMS: [&str; 24] = [
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5",
    "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
    "LPT9",
];

/// Replaces the characters Windows refuses in a file name, and strips trailing
/// dots/spaces that `CreateFile` would otherwise reject. Returns
/// [`DEFAULT_FILENAME_TEMPLATE`] if nothing usable survives. Also caps the
/// length and escapes reserved device names so a hand-edited template cannot
/// produce an unsaveable path.
fn sanitize_stem(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return DEFAULT_FILENAME_TEMPLATE.to_string();
    }
    // 按字符截断（不是字节），避免把一个多字节汉字切成两半；截断后可能
    // 又留下尾部点/空格，再修一次。
    let mut stem: String = trimmed.chars().take(MAX_FILENAME_STEM_CHARS).collect();
    let end = stem.trim_end_matches(['.', ' ']).len();
    stem.truncate(end);
    if stem.is_empty() {
        return DEFAULT_FILENAME_TEMPLATE.to_string();
    }
    // `CON` / `NUL` / `COM1` 等即使带扩展名也不能使用；在保留名后插入下划线，
    // 例如 `con.txt` -> `con_.txt`，正常文件名一个字符都不变。
    let dot = stem.find('.').unwrap_or(stem.len());
    let (upper, insert_at) = {
        let base = stem[..dot].trim_end_matches([' ', '.']);
        (base.to_ascii_uppercase(), base.len())
    };
    if WINDOWS_RESERVED_STEMS.contains(&upper.as_str()) {
        // Drop trailing spaces/dots between the reserved name and its extension
        // (`"CON .txt"` -> `"CON_.txt"`), then insert the escaping underscore.
        stem.replace_range(insert_at..dot, "");
        stem.insert(insert_at, '_');
    }
    stem
}

/// Screenshot history retention. Mirrors snow-shot's `CaptureHistoryPolicy`
/// with three limits (age, count, disk) plus a "keep forever" escape hatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct History {
    /// Whether saving an image also records it in history.
    pub enabled: bool,
    /// Optional override for the history directory. `None` uses
    /// `%LOCALAPPDATA%\FastPin\capture_history`, the folder every build shares.
    pub dir: Option<PathBuf>,
    /// Delete records older than this many days. 0 disables age-based pruning.
    pub retention_days: u64,
    /// Maximum number of records. The oldest are pruned first.
    pub limit: usize,
    /// Maximum total size of stored images, in MiB.
    pub max_disk_mib: u64,
    /// When true, ignore `retention_days` (count/disk limits still apply).
    pub keep_permanently: bool,
}

impl Default for History {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: None,
            retention_days: 7,
            limit: 100,
            max_disk_mib: 1024,
            keep_permanently: false,
        }
    }
}

impl History {
    /// Bounds for values typed into settings.json by hand. `0` is preserved:
    /// it disables age-based pruning (see the field doc and `Retention`).
    pub fn retention_days(&self) -> u64 {
        self.retention_days.min(365)
    }
    pub fn limit(&self) -> usize {
        self.limit.clamp(1, 1000)
    }
    pub fn max_disk_bytes(&self) -> u64 {
        self.max_disk_mib
            .clamp(128, 10_240)
            .saturating_mul(1024 * 1024)
    }

    /// Directory history lives in. A configured path wins; otherwise the
    /// per-user application data folder.
    pub fn store_dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.dir {
            return Ok(dir.clone());
        }
        let base =
            std::env::var("LOCALAPPDATA").map_err(|_| Error::new("LOCALAPPDATA is not set"))?;
        Ok(PathBuf::from(base).join("FastPin").join("capture_history"))
    }

    /// Convert to the store's retention view, applying the same clamps as the
    /// settings page would.
    pub fn retention(&self) -> crate::history::Retention {
        crate::history::Retention {
            limit: self.limit(),
            retention_days: self.retention_days(),
            max_disk_bytes: self.max_disk_bytes(),
            keep_permanently: self.keep_permanently,
        }
    }
}

/// What the region picker offers under the cursor as a candidate boundary.
///
/// Mirrors snow-shot's `SmartSelectionMode`: whole windows, the accessibility
/// sub-elements inside them (a finer boundary for one pane or button), or no
/// snapping at all so the user drags freely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SmartSelectionMode {
    /// Offer the window and every pane inside it: the deepest element under the
    /// cursor first, its ancestors next. Snow-shot calls this
    /// `window_sub_element`.
    #[default]
    #[serde(rename = "windowSubElement")]
    WindowSubElement,
    /// Offer only the whole window under the cursor — the fastest way to grab
    /// one app window.
    #[serde(rename = "window")]
    Window,
    /// No snapping: a plain drag, exactly as the picker behaved before this
    /// feature existed.
    #[serde(rename = "disabled")]
    Disabled,
}

impl SmartSelectionMode {
    pub const ALL: [Self; 3] = [Self::WindowSubElement, Self::Window, Self::Disabled];

    /// Stable key persisted in the settings file (and accepted from the CLI).
    pub fn id(self) -> &'static str {
        match self {
            Self::WindowSubElement => "windowSubElement",
            Self::Window => "window",
            Self::Disabled => "disabled",
        }
    }

    /// Round-trips anything, defaulting to [`Self::WindowSubElement`] the way
    /// snow-shot's `smartSelectionModeFromId` does.
    pub fn from_id(id: &str) -> Self {
        match id {
            "window" => Self::Window,
            "disabled" => Self::Disabled,
            _ => Self::WindowSubElement,
        }
    }

    /// Short label for the picker's mode switch. Mirrors the 元素检测 options.
    pub fn label(self) -> &'static str {
        match self {
            Self::WindowSubElement => "窗口与元素",
            Self::Window => "仅窗口",
            Self::Disabled => "不检测",
        }
    }

    /// Whether the picker should look up candidate rectangles at all. A
    /// disabled mode never touches the accessibility APIs.
    pub fn enabled(self) -> bool {
        self != Self::Disabled
    }

    /// Whether candidates include the panes inside a window.
    pub fn wants_sub_elements(self) -> bool {
        self == Self::WindowSubElement
    }
}

/// 截屏时使用的捕获方式。自动模式由运行时根据系统环境挑选最佳实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureMode {
    /// 根据系统环境选择最佳方式。
    #[default]
    Auto,
    /// Windows Graphics Capture。
    Wgc,
    /// Desktop Duplication API。
    Dxgi,
}

impl CaptureMode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Wgc, Self::Dxgi];

    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Wgc => "wgc",
            Self::Dxgi => "dxgi",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动（推荐）",
            Self::Wgc => "WGC",
            Self::Dxgi => "DXGI",
        }
    }
}

/// 进入长截屏工具时的开始策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LongCaptureAutoStart {
    /// 进入工具后自动开始滚动截取。
    #[default]
    Auto,
    /// 由用户手动点击开始。
    Manual,
}

impl LongCaptureAutoStart {
    pub const ALL: [Self; 2] = [Self::Auto, Self::Manual];

    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动开始（默认）",
            Self::Manual => "手动开始",
        }
    }
}

/// 选区完成后鼠标按键触发的操作；左键双击与中键单击共用同一组选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClickAction {
    /// 不执行任何操作。
    #[default]
    None,
    /// 完成当前截屏。
    Complete,
    /// 复制图片到剪贴板。
    Copy,
    /// 保存图片到文件。
    Save,
}

impl ClickAction {
    pub const ALL: [Self; 4] = [Self::None, Self::Complete, Self::Copy, Self::Save];

    pub fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Complete => "complete",
            Self::Copy => "copy",
            Self::Save => "save",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "无",
            Self::Complete => "完成截屏",
            Self::Copy => "复制到剪贴板",
            Self::Save => "保存到文件",
        }
    }
}

/// 点击选区外时的行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClickOutside {
    /// 扩展现有选区。
    #[default]
    Expand,
    /// 丢弃现有选区并重新创建。
    Recreate,
}

impl ClickOutside {
    pub const ALL: [Self; 2] = [Self::Expand, Self::Recreate];

    pub fn id(self) -> &'static str {
        match self {
            Self::Expand => "expand",
            Self::Recreate => "recreate",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Expand => "扩展选区（默认）",
            Self::Recreate => "重新创建",
        }
    }
}

/// 截屏工具栏相对选区的位置策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolbarPosition {
    /// 跟随选区移动。
    #[default]
    FollowSelection,
    /// 记住上一次的位置。已废弃：截屏流程从不写入记住的坐标，该策略实际
    /// 等同「跟随选区」，因此不再出现在设置页的下拉中；仅保留变体以兼容
    /// 旧配置文件。
    Remember,
    /// 固定在选区所在屏幕的顶部。
    Top,
    /// 固定在选区所在屏幕的底部。
    Bottom,
}

impl ToolbarPosition {
    /// 设置页实际提供的策略；`Remember` 没有位置写入点，属于死设置。
    pub const ALL: [Self; 3] = [Self::FollowSelection, Self::Top, Self::Bottom];

    pub fn id(self) -> &'static str {
        match self {
            Self::FollowSelection => "followSelection",
            Self::Remember => "remember",
            Self::Top => "top",
            Self::Bottom => "bottom",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::FollowSelection => "跟随选区",
            // 旧配置里的值仍可读，但每次渲染都退回跟随选区。
            Self::Remember => "跟随选区（旧「记住位置」）",
            Self::Top => "顶部",
            Self::Bottom => "底部",
        }
    }
}

/// 参数面板相对选区的位置策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PanelPosition {
    /// 跟随选区移动。
    #[default]
    FollowSelection,
    /// 记住上一次的位置。已废弃：参数面板坐标从未被写入，策略实际等同
    /// 「跟随选区」，不再出现在设置页下拉中；仅保留变体兼容旧配置文件。
    Remember,
    /// 固定在选区所在屏幕的左侧。
    Left,
    /// 固定在选区所在屏幕的右侧。
    Right,
}

impl PanelPosition {
    /// 设置页实际提供的位置策略（`Remember` 无写入点，已移除）。
    pub const ALL: [Self; 3] = [Self::FollowSelection, Self::Left, Self::Right];

    pub fn id(self) -> &'static str {
        match self {
            Self::FollowSelection => "followSelection",
            Self::Remember => "remember",
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::FollowSelection => "跟随选区",
            // 同 ToolbarPosition：历史值只显示为「跟随选区」。
            Self::Remember => "跟随选区（旧「记住位置」）",
            Self::Left => "左侧",
            Self::Right => "右侧",
        }
    }
}

/// Defaults for new annotation elements. The color stays a hex string so the
/// file remains hand-editable, same rationale as the hotkey strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Annotation {
    /// Stroke width in logical px; clamped to 1..=20 when applied.
    pub stroke_width: f64,
    /// "#RRGGBB" or "#AARRGGBB".
    pub stroke_color: String,
    /// Mosaic cell size in image px; clamped to 4..=64 when applied.
    pub mosaic_block: i32,
}

/// Where a watermark sits on the exported image (9-grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WatermarkPosition {
    TopLeft,
    TopCenter,
    TopRight,
    MiddleLeft,
    Center,
    MiddleRight,
    BottomLeft,
    BottomCenter,
    #[default]
    #[serde(rename = "bottomRight")]
    BottomRight,
}

impl WatermarkPosition {
    pub const ALL: [Self; 9] = [
        Self::TopLeft,
        Self::TopCenter,
        Self::TopRight,
        Self::MiddleLeft,
        Self::Center,
        Self::MiddleRight,
        Self::BottomLeft,
        Self::BottomCenter,
        Self::BottomRight,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::TopLeft => "topLeft",
            Self::TopCenter => "topCenter",
            Self::TopRight => "topRight",
            Self::MiddleLeft => "middleLeft",
            Self::Center => "center",
            Self::MiddleRight => "middleRight",
            Self::BottomLeft => "bottomLeft",
            Self::BottomCenter => "bottomCenter",
            Self::BottomRight => "bottomRight",
        }
    }

    pub fn from_key(key: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|p| p.key() == key)
            .unwrap_or_default()
    }
}

/// Text/image watermark burned into the exported image. An empty text and no
/// image disables it; the export composite skips it entirely so a screenshot
/// tool whose settings are empty behaves exactly as before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Watermark {
    /// Watermark text; empty disables the text layer.
    pub text: String,
    /// 10–100 (%).
    pub opacity: u8,
    /// 9-grid position key (`WatermarkPosition::key`).
    pub position: String,
    /// Font/image size multiplier; 0.5–3.0.
    pub scale: f32,
    /// Repeat the text watermark across the whole image.
    pub tiled: bool,
    /// Prepend the capture time (UTC `YYYY-MM-DD HH:MM:SS`).
    pub timestamp: bool,
    /// Optional image watermark path. Only used when it is an existing file.
    pub image_path: Option<PathBuf>,
}

impl Default for Watermark {
    fn default() -> Self {
        Self {
            text: String::new(),
            opacity: 50,
            position: "bottomRight".into(),
            scale: 1.0,
            tiled: false,
            timestamp: false,
            image_path: None,
        }
    }
}

impl Watermark {
    /// Whether the watermark produces any pixels at all.
    pub fn is_visible(&self) -> bool {
        self.timestamp
            || !self.text.trim().is_empty()
            || self.image_path.as_ref().is_some_and(|p| p.is_file())
    }

    /// The text actually rasterized: the user's text, optionally prefixed with
    /// the current capture time.
    pub fn resolved_text(&self) -> String {
        let stamp = self.timestamp.then(Self::utc_stamp);
        match (stamp, self.text.trim()) {
            (Some(stamp), "") => stamp,
            (Some(stamp), text) => format!("{stamp}  {text}"),
            (None, text) => text.to_string(),
        }
    }

    /// `YYYY-MM-DD HH:MM:SS` in UTC. `chrono` handles the calendar (month
    /// lengths, leap years) that hand-rolled epoch arithmetic gets wrong.
    fn utc_stamp() -> String {
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

impl Default for Annotation {
    // Derives from the editor constants so the two can never drift apart.
    fn default() -> Self {
        Self {
            stroke_width: crate::overlay::editor::DEFAULT_STROKE_WIDTH,
            stroke_color: format!("#{:06X}", crate::overlay::theme::STROKE_ARGB & 0xFF_FFFF),
            mosaic_block: crate::overlay::editor::DEFAULT_MOSAIC_BLOCK,
        }
    }
}

impl Annotation {
    /// Opaque ARGB. A broken hex string falls back to the shipped red rather
    /// than making every annotation invisible.
    pub fn stroke_argb(&self) -> u32 {
        parse_hex_color(&self.stroke_color).unwrap_or(crate::overlay::theme::STROKE_ARGB)
    }

    pub fn clamped_stroke_width(&self) -> f64 {
        self.stroke_width.clamp(1.0, 20.0)
    }

    pub fn clamped_mosaic_block(&self) -> i32 {
        self.mosaic_block.clamp(4, 64)
    }
}

/// "#RRGGBB" is made opaque; "#AARRGGBB" keeps its alpha.
pub fn parse_hex_color(text: &str) -> Option<u32> {
    let hex = text.trim().strip_prefix('#').unwrap_or(text.trim());
    let v = u32::from_str_radix(hex, 16).ok()?;
    match hex.len() {
        6 => Some(0xFF00_0000 | v),
        8 => Some(v),
        _ => None,
    }
}

/// Where saved PNGs land. A configured directory wins; otherwise
/// `Pictures\FastPin`, the folder the build has always used. Not created here
/// — the caller writes into it and `create_dir_all` right before.
pub fn screenshot_dir() -> Result<PathBuf> {
    if let Some(dir) = get().screenshot.save_dir {
        return Ok(dir);
    }
    default_screenshot_dir()
}

pub fn default_screenshot_dir() -> Result<PathBuf> {
    Ok(pictures_dir()?.join("FastPin"))
}

/// The user's Pictures known folder. Split out so the rebrand migration and
/// the default save path cannot drift apart.
pub fn pictures_dir() -> Result<PathBuf> {
    let pictures = unsafe {
        let pw = SHGetKnownFolderPath(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None)?;
        // Freed before the `?`: an early return here would leak the COM string.
        let text = pw.to_string();
        CoTaskMemFree(Some(pw.0 as *const _));
        text.map_err(|e| Error::new(e.to_string()))?
    };
    Ok(PathBuf::from(pictures))
}

pub fn path() -> Result<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").map_err(|_| Error::new("LOCALAPPDATA is not set"))?;
    Ok(PathBuf::from(base).join("FastPin").join("settings.json"))
}

/// Cloned rather than borrowed: the settings window replaces the cache, so a
/// `&'static` would have to outlive a value that is meant to change.
pub fn get() -> Settings {
    {
        let guard = cache().read().unwrap_or_else(|e| e.into_inner());
        if let Some(settings) = guard.as_ref() {
            return settings.clone();
        }
    }
    let result = read_file();
    // 只有读到文本或文件不存在（首次启动）能按默认值继续并写缓存；其它
    // 读取失败（杀软占用、同步冲突、权限）若把默认值缓存下来，下一次 save
    // 就会把默认值整份写回原文件。先把原文件改名留档，save 写的是新文件，
    // 用户仍可从 .broken-* 备份恢复；本次也不写缓存，避免污染后续读取。
    let recoverable = load_text_is_recoverable(&result);
    if !recoverable {
        if let Ok(p) = path() {
            quarantine_broken(&p, None);
        }
    }
    let text = result.unwrap_or_default();
    // 语法错误 / 非对象根节点会让 parse 直接回默认值；若放任不管，下一次
    // 保存会把默认值整份写回，原配置永久丢失。先把损坏文件改名留档，
    // 之后的 save 写的是新文件，用户仍可从 .broken-* 备份恢复。
    if is_broken_config(&text) {
        if let Ok(p) = path() {
            quarantine_broken(&p, Some(&text));
        }
    }
    let loaded = parse(&text);
    if !recoverable {
        // 读失败时原文件可能还没隔离成功，绝不缓存默认值。
        return loaded;
    }
    let mut guard = cache().write().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert(loaded).clone()
}

/// Writes a temporary file beside the real one and renames it into place.
/// `std::fs::rename` on Windows uses `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`,
/// so a crash or a failed serialize leaves the previous settings intact rather
/// than a truncated file that silently resets everything on next start.
/// Cache-last: on failure the process keeps the settings the user can see.
pub fn save(settings: &Settings) -> Result<()> {
    let path = path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|e| Error::new(format!("{e}")))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, &text)?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    *cache().write().unwrap_or_else(|e| e.into_inner()) = Some(settings.clone());
    Ok(())
}

fn cache() -> &'static RwLock<Option<Settings>> {
    static CACHE: RwLock<Option<Settings>> = RwLock::new(None);
    &CACHE
}

/// 读配置文件，区分“文件不存在”与真正的 IO 失败。过去这里把所有错误都
/// 吞成空串，让合法配置在读取失败时被当成空配置回默认值，随后被 save 覆盖。
/// `path()` 失败（LOCALAPPDATA 未设置）也归为 NotFound，与首次启动同样处理。
fn read_file() -> std::io::Result<String> {
    let p = path().map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e.to_string()))?;
    std::fs::read_to_string(p)
}

/// 读配置的结果是否能按“空配置=默认值”继续。只有读到文本和文件不存在
/// （首次启动）成立；其它 IO 错误必须走隔离分支，否则默认值会被 save
/// 写回并覆盖原文件。抽成纯函数便于覆盖各分支，不必注入真实 IO 错误。
fn load_text_is_recoverable(result: &std::io::Result<String>) -> bool {
    match result {
        Ok(_) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// 文本非空但 JSON 语法错误（尾逗号、半截文件）或根节点不是对象时，`parse`
/// 只能回默认值。这类文件必须在加载路径上隔离，否则下一次保存会覆盖它。
fn is_broken_config(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    !matches!(serde_json::from_str::<serde_json::Value>(text), Ok(value) if value.is_object())
}

/// 把损坏配置改名成同目录下的 `settings.json.broken-<pid>-<unix秒>`，
/// 撞名再加序号，避免多进程 / 多次启动互相覆盖。`rename` 而非复制，
/// 让后续 `save` 写一份全新的 settings.json。成功记一条 app 日志，
/// 失败静默返回 false（调用方照常用默认配置继续运行）。
/// `text` 为 None 表示文件读不到（IO 错误）：跳过内容一致性校验直接改名，
/// 这样才能保住原文件，不让默认值覆盖它。
fn quarantine_broken(path: &Path, text: Option<&str>) -> bool {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let Some(file_name) = path.file_name() else {
        return false;
    };
    let mut name = file_name.to_os_string();
    name.push(format!(".broken-{}-{}", std::process::id(), secs));
    let base = path.with_file_name(name);

    // 读取与隔离之间文件可能被另一进程修好：内容变了就不要动它，
    // 更不能把一份有效配置搬走。读不到文本时无从比对，直接隔离以保住原文件。
    if let Some(text) = text {
        if std::fs::read_to_string(path).ok().as_deref() != Some(text) {
            return false;
        }
    }
    for seq in 0..1024u32 {
        let candidate = if seq == 0 {
            base.clone()
        } else {
            let mut suffixed = base.clone().into_os_string();
            suffixed.push(format!("-{seq}"));
            PathBuf::from(suffixed)
        };
        // Windows 的 rename 会替换已存在的目标，所以先查重、撞名换序号，
        // 保证先前的备份不会被这一次覆盖。
        if candidate.exists() {
            continue;
        }
        match std::fs::rename(path, &candidate) {
            Ok(()) => {
                crate::perf::log_app(&format!(
                    "settings.json 语法错误或根节点不是对象，已隔离到 {}",
                    candidate.display()
                ));
                return true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return false,
        }
    }
    false
}

/// A missing or malformed file falls back to defaults — a screenshot tool must
/// still capture when its settings are broken.
fn parse(text: &str) -> Settings {
    if text.trim().is_empty() {
        return Settings::default();
    }
    let mut settings = match serde_json::from_str::<Settings>(text) {
        Ok(settings) => settings,
        Err(e) => {
            // 一个坏值（`"captureMode":"banana"`、`"paperSize":"A3XL"`、
            // `backgroundRgba:[1,2]`）过去会让整份配置归零，连 baseUrl /
            // history / saveDir 一起丢掉，下次保存又覆盖原文件。这里改为
            // 逐字段恢复：只有坏字段回退默认，其它好字段全部保留。
            eprintln!("settings.json has invalid fields ({e}); recovering field by field");
            let input: serde_json::Value = match serde_json::from_str(text) {
                Ok(value) => value,
                Err(_) => return Settings::default(),
            };
            recover_settings(&input)
        }
    };
    // 直接反序列化也会把超大数组完整收进缓存，这里与恢复路径一样截断。
    cap_settings_vecs(&mut settings);
    settings
}

/// 读取单个字段：反序列化失败时保留默认值并记日志。各结构体都带
/// `#[serde(default)]`，因此缺失的键由字段类型自己补齐，未知键被忽略。
fn recover_field<T: serde::de::DeserializeOwned>(
    input: &serde_json::Value,
    key: &str,
    label: &str,
) -> Option<T> {
    let value = input.get(key)?;
    match serde_json::from_value::<T>(value.clone()) {
        Ok(value) => Some(value),
        Err(e) => {
            eprintln!("settings.json: 字段 {label} 无效（{e}），已回退默认值");
            None
        }
    }
}

/// 批量恢复一个结构体对象里的字段，避免逐个字段写重复的 fallback 逻辑。
macro_rules! recover_fields {
    ($input:expr, $out:expr, $prefix:literal $(, $key:literal => $field:ident)* $(,)?) => {
        $(
            if let Some(value) = recover_field($input, $key, concat!($prefix, ".", $key)) {
                $out.$field = value;
            }
        )*
    };
}

/// 只有这两个字符串数组没有 schema 上限：手改的超大数组会拖慢启动。
const MAX_STRING_VEC_LEN: usize = 256;

fn cap_string_vec(items: &mut Vec<String>, label: &str) {
    if items.len() > MAX_STRING_VEC_LEN {
        eprintln!(
            "settings.json: 字段 {label} 有 {} 项，超过 {MAX_STRING_VEC_LEN} 项，已截断",
            items.len()
        );
        items.truncate(MAX_STRING_VEC_LEN);
    }
}

fn cap_settings_vecs(settings: &mut Settings) {
    cap_string_vec(
        &mut settings.screenshot.hidden_toolbar_items,
        "screenshot.hiddenToolbarItems",
    );
    cap_string_vec(
        &mut settings.screenshot.record_history_sources,
        "screenshot.recordHistorySources",
    );
}

/// `recover_field` 之外的专用恢复：先按类型恢复，再限制数组长度。
fn recover_string_vec(input: &serde_json::Value, key: &str, label: &str) -> Option<Vec<String>> {
    let mut items = recover_field(input, key, label)?;
    cap_string_vec(&mut items, label);
    Some(items)
}

/// 顶层字段逐个恢复。`screenshot` 内部含多个枚举，交给 `recover_screenshot`
/// 再按二级字段恢复，尽量少丢用户数据。
fn recover_settings(input: &serde_json::Value) -> Settings {
    let mut out = Settings::default();
    if !input.is_object() {
        eprintln!("settings.json: 根节点不是对象，已回退默认值");
        return out;
    }
    out.ocr = recover_ocr(input.get("ocr"));
    out.translation = recover_translation(input.get("translation"));
    out.screenshot = recover_screenshot(input.get("screenshot"));
    out.history = recover_history(input.get("history"));
    out.hotkeys = recover_hotkeys(input.get("hotkeys"));
    if let Some(value) = recover_field(input, "appearance", "appearance") {
        out.appearance = value;
    }
    out
}

fn recover_ocr(input: Option<&serde_json::Value>) -> Ocr {
    let mut out = Ocr::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 ocr 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "ocr", "modelsDir" => models_dir);
    out
}

fn recover_translation(input: Option<&serde_json::Value>) -> Translation {
    let mut out = Translation::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 translation 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "translation",
        "baseUrl" => base_url,
        "model" => model,
        "targetLanguage" => target_language,
        "prompt" => prompt,
        "timeoutSeconds" => timeout_seconds,
        "allowInsecureHttp" => allow_insecure_http,
    );
    out
}

fn recover_history(input: Option<&serde_json::Value>) -> History {
    let mut out = History::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 history 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "history",
        "enabled" => enabled,
        "dir" => dir,
        "retentionDays" => retention_days,
        "limit" => limit,
        "maxDiskMib" => max_disk_mib,
        "keepPermanently" => keep_permanently,
    );
    out
}

fn recover_hotkeys(input: Option<&serde_json::Value>) -> Hotkeys {
    let mut out = Hotkeys::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 hotkeys 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "hotkeys",
        "region" => region,
        "copyOnly" => copy_only,
        "fullscreen" => fullscreen,
    );
    out
}

/// `screenshot` 是最大的字段：其中每个二级字段独立尝试，坏枚举只回退它自己。
fn recover_screenshot(input: Option<&serde_json::Value>) -> Screenshot {
    let mut out = Screenshot::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 screenshot 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "screenshot",
        "saveDir" => save_dir,
        "smartSelection" => smart_selection,
        "captureMode" => capture_mode,
        "showCursor" => show_cursor,
        "showGuides" => show_guides,
        "longCaptureAutoStart" => long_capture_auto_start,
        "leftDoubleClick" => left_double_click,
        "middleClick" => middle_click,
        "clickOutside" => click_outside,
        "toolbarPosition" => toolbar_position,
        "panelPosition" => panel_position,
        "toolbarRememberPos" => toolbar_remember_pos,
        "panelRememberPos" => panel_remember_pos,
        "showMagnifier" => show_magnifier,
        "showInfoBar" => show_info_bar,
        "showShortcutHints" => show_shortcut_hints,
    );
    // 两个数组字段需要额外的长度上限，避免损坏文件里的大数组拖慢启动。
    if let Some(value) =
        recover_string_vec(input, "hiddenToolbarItems", "screenshot.hiddenToolbarItems")
    {
        out.hidden_toolbar_items = value;
    }
    if let Some(value) = recover_string_vec(
        input,
        "recordHistorySources",
        "screenshot.recordHistorySources",
    ) {
        out.record_history_sources = value;
    }
    // 这些二级字段本身也是结构体，坏值再往下一层恢复，保住同结构里的好字段。
    out.annotation = recover_annotation(input.get("annotation"));
    out.watermark = recover_watermark(input.get("watermark"));
    out.auto_scroll = recover_auto_scroll(input.get("autoScroll"));
    out.export = recover_export(input.get("export"));
    out.collage = recover_collage(input.get("collage"));
    out.pdf = recover_pdf(input.get("pdf"));
    // split 也逐字段恢复：坏 count 不再连方向 / 单位 / 网格一起丢掉。
    out.split = recover_split(input.get("split"));
    out
}

fn recover_annotation(input: Option<&serde_json::Value>) -> Annotation {
    let mut out = Annotation::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 screenshot.annotation 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "screenshot.annotation",
        "strokeWidth" => stroke_width,
        "strokeColor" => stroke_color,
        "mosaicBlock" => mosaic_block,
    );
    out
}

fn recover_watermark(input: Option<&serde_json::Value>) -> Watermark {
    let mut out = Watermark::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 screenshot.watermark 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "screenshot.watermark",
        "text" => text,
        "opacity" => opacity,
        "position" => position,
        "scale" => scale,
        "tiled" => tiled,
        "timestamp" => timestamp,
        "imagePath" => image_path,
    );
    out
}

fn recover_auto_scroll(input: Option<&serde_json::Value>) -> AutoScroll {
    let mut out = AutoScroll::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 screenshot.autoScroll 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "screenshot.autoScroll",
        "intervalMs" => interval_ms,
        "notches" => notches,
    );
    out
}

fn recover_export(input: Option<&serde_json::Value>) -> Export {
    let mut out = Export::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 screenshot.export 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "screenshot.export",
        "format" => format,
        "quality" => quality,
        "filenameTemplate" => filename_template,
        "autoSave" => auto_save,
        "autoCopy" => auto_copy,
    );
    out
}

fn recover_collage(input: Option<&serde_json::Value>) -> CollageSettings {
    let mut out = CollageSettings::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 screenshot.collage 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "screenshot.collage",
        "direction" => direction,
        "sizing" => sizing,
        "border" => border,
        "spacing" => spacing,
        "backgroundRgba" => background_rgba,
        "rows" => rows,
        "columns" => columns,
    );
    out
}

fn recover_pdf(input: Option<&serde_json::Value>) -> PdfSettings {
    let mut out = PdfSettings::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 screenshot.pdf 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "screenshot.pdf",
        "stitchingMethod" => stitching_method,
        "templateType" => template_type,
        "spacingMm" => spacing_mm,
        "paperSize" => paper_size,
        "orientation" => orientation,
        "customWidthMm" => custom_width_mm,
        "customHeightMm" => custom_height_mm,
        "marginTopMm" => margin_top_mm,
        "marginBottomMm" => margin_bottom_mm,
        "marginLeftMm" => margin_left_mm,
        "marginRightMm" => margin_right_mm,
        "exportMode" => export_mode,
        "imageQuality" => image_quality,
    );
    out
}

/// `split` 定义在 imagesplit 模块，同样逐字段恢复，保持与 annotation /
/// collage 等结构体一致的退化方式：坏子字段回默认，好字段全部保留。
fn recover_split(input: Option<&serde_json::Value>) -> crate::imagesplit::SplitSettings {
    let mut out = crate::imagesplit::SplitSettings::default();
    let Some(input) = input else { return out };
    if !input.is_object() {
        eprintln!("settings.json: 字段 screenshot.split 不是对象，已回退默认值");
        return out;
    }
    recover_fields!(input, out, "screenshot.split",
        "direction" => direction,
        "splitType" => split_type,
        "unit" => unit,
        "count" => count,
        "rows" => rows,
        "columns" => columns,
        "pixels" => pixels,
        "percentage" => percentage,
        "overlapWidth" => overlap_width,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_broken_file_falls_back_to_defaults() {
        for text in ["", "not json", "{", "[]"] {
            let s = parse(text);
            assert_eq!(s.translation.model, "gpt-4o-mini", "{text:?}");
            assert_eq!(s.translation.target_language, "zh-CN", "{text:?}");
            assert_eq!(s.translation.timeout_seconds, 30, "{text:?}");
            assert!(s.translation.base_url.is_empty(), "{text:?}");
            assert!(s.ocr.models_dir.is_none(), "{text:?}");
            assert!(s.screenshot.hidden_toolbar_items.is_empty(), "{text:?}");
        }
    }

    #[test]
    fn absent_fields_keep_their_defaults() {
        let s = parse(r#"{"translation": {"baseUrl": "https://api.example.com/v1"}}"#);
        assert_eq!(s.translation.base_url, "https://api.example.com/v1");
        assert_eq!(s.translation.model, "gpt-4o-mini");
        assert_eq!(s.translation.timeout_seconds, 30);
    }

    /// 旧版配置文件里残留的本地翻译字段是未知键：`#[serde(default)]` 负责
    /// 补齐缺省值，serde 默认忽略未知键，逐字段恢复路径也会把它们丢掉。
    /// 用户手动升级设置文件时不能因为这些残留字段而丢失 API 配置。
    #[test]
    fn leftover_local_translate_fields_are_ignored() {
        let text = r#"{
          "translation": {
            "baseUrl": "https://api.example.com/v1",
            "localModelsDir": "D:/models",
            "localThreads": 8,
            "defaultService": "haidass-143m-fp16",
            "localServices": [{
              "id": "haidass-143m-fp16",
              "name": "Haidass Translate 143M · FP16",
              "downloadUrl": "https://example.com/model.zip",
              "archiveBytes": 224593262
            }]
          }
        }"#;
        let expected = Translation {
            base_url: "https://api.example.com/v1".into(),
            ..Translation::default()
        };
        // parse 的逐字段恢复路径与直接反序列化都不能被未知键影响。
        assert_eq!(parse(text).translation, expected);
        let direct: Settings = serde_json::from_str(text).expect("unknown keys are ignored");
        assert_eq!(direct.translation, expected);

        // 未知键即使用户改成了奇怪的类型也一样被忽略。
        let weird = r#"{"translation": {"localServices": "not-an-array", "localThreads": "8"}}"#;
        assert_eq!(parse(weird).translation, Translation::default());
        let direct: Settings =
            serde_json::from_str(weird).expect("unknown keys of any JSON type are ignored");
        assert_eq!(direct.translation, Translation::default());

        // 同时存在坏字段时走逐字段恢复路径：未知键仍被丢弃，好字段保留。
        let mixed = r#"{"translation": {
          "baseUrl": "https://api.example.com/v1",
          "timeoutSeconds": "30",
          "defaultService": "haidass-143m-fp16",
          "localServices": [{"id": "custom"}]
        }}"#;
        assert_eq!(parse(mixed).translation, expected);

        // 再次写回时这些残留字段不会复活。
        let json = serde_json::to_string(&parse(text).translation).expect("serializes");
        for key in [
            "localModelsDir",
            "localThreads",
            "defaultService",
            "localServices",
            "downloadUrl",
        ] {
            assert!(!json.contains(key), "{key} survived in {json}");
        }
    }

    #[test]
    fn a_bad_field_does_not_discard_the_rest_of_the_file() {
        let s = parse(
            r#"{
              "translation": {"baseUrl": "https://api.example.com/v1", "timeoutSeconds": "30"},
              "ocr": {"modelsDir": "D:/models/v6"}
            }"#,
        );
        assert_eq!(s.translation.base_url, "https://api.example.com/v1");
        // The bad type falls back to its own default; valid fields survive.
        assert_eq!(s.translation.timeout_seconds, 30);
        assert_eq!(
            s.ocr.models_dir.as_deref(),
            Some(std::path::Path::new("D:/models/v6"))
        );
    }

    /// 坏枚举 / 坏定长数组只能通过 JSON 种类检查，却在第二次反序列化时失败；
    /// 过去这会让 `unwrap_or_default()` 把整份配置归零。现在只回退坏字段。
    #[test]
    fn a_bad_enum_keeps_every_other_field() {
        let s = parse(
            r#"{
              "translation": {"baseUrl": "https://api.example.com/v1"},
              "history": {"dir": "D:/history"},
              "screenshot": {
                "captureMode": "banana",
                "saveDir": "D:/shots",
                "collage": {"backgroundRgba": [1, 2], "spacing": 9},
                "pdf": {"paperSize": "A3XL", "imageQuality": 55}
              }
            }"#,
        );
        // 好字段一个都不能丢。
        assert_eq!(s.translation.base_url, "https://api.example.com/v1");
        assert_eq!(
            s.history.dir.as_deref(),
            Some(std::path::Path::new("D:/history"))
        );
        assert_eq!(
            s.screenshot.save_dir.as_deref(),
            Some(std::path::Path::new("D:/shots"))
        );
        assert_eq!(s.screenshot.collage.spacing, 9);
        assert_eq!(s.screenshot.pdf.image_quality, 55);
        // 坏枚举与坏定长数组只回退各自字段的默认值。
        assert_eq!(s.screenshot.capture_mode, CaptureMode::Auto);
        assert_eq!(s.screenshot.collage.background_rgba, [255, 255, 255, 255]);
        assert_eq!(s.screenshot.pdf.paper_size, PaperSize::A4);
    }

    #[test]
    fn keys_are_camel_case() {
        let s = parse(
            r#"{
              "ocr": {"modelsDir": "D:/models/v6"},
              "screenshot": {"hiddenToolbarItems": ["text", "redo"]},
              "translation": {
                "baseUrl": "https://x/v1",
                "model": "qwen-plus",
                "targetLanguage": "en",
                "prompt": "be terse",
                "timeoutSeconds": 5
              }
            }"#,
        );
        assert_eq!(
            s.ocr.models_dir.as_deref(),
            Some(std::path::Path::new("D:/models/v6"))
        );
        assert_eq!(s.screenshot.hidden_toolbar_items, ["text", "redo"]);
        assert_eq!(s.translation.base_url, "https://x/v1");
        assert_eq!(s.translation.model, "qwen-plus");
        assert_eq!(s.translation.target_language, "en");
        assert_eq!(s.translation.prompt, "be terse");
        assert_eq!(s.translation.timeout_seconds, 5);
    }

    /// The settings window round-trips through this pair; a field that
    /// serializes under a different name than it parses would silently reset.
    /// A settings.json written before the annotation/watermark/hotkey fields
    /// existed must still load: `#[serde(default)]` fills the gaps instead of
    /// erroring out and dropping every other setting with it.
    #[test]
    fn a_legacy_file_keeps_the_fields_it_never_had() {
        let s = parse(
            r#"{
              "translation": {"targetLanguage": "en", "timeoutSeconds": 12},
              "screenshot": {"saveDir": "D:/shots"}
            }"#,
        );
        assert_eq!(s.translation.target_language, "en");
        assert_eq!(s.translation.timeout_seconds, 12);
        assert_eq!(
            s.screenshot.save_dir.as_deref(),
            Some(std::path::Path::new("D:/shots"))
        );
        // Fields absent from that old file fall back to the shipped defaults.
        assert_eq!(s.translation.model, "gpt-4o-mini");
        assert!(s.translation.base_url.is_empty());
        assert!(s.ocr.models_dir.is_none());
        assert!(s.screenshot.hidden_toolbar_items.is_empty());
        assert_eq!(
            s.screenshot.annotation.stroke_color,
            Annotation::default().stroke_color
        );
        assert_eq!(
            s.screenshot.watermark.position,
            Watermark::default().position
        );
        // 旧文件没有这两个开关时回落到默认行为：只复制、不写文件。
        assert!(!s.screenshot.export.auto_save);
        assert!(s.screenshot.export.auto_copy);
        assert_eq!(s.hotkeys.region, Hotkeys::default().region);
        // A file written before the appearance field existed starts with the
        // gpui-kit default light variant.
        assert_eq!(s.appearance, DEFAULT_APPEARANCE);
        assert!(!s.dark());
    }

    #[test]
    fn appearance_variant_names_roundtrip() {
        assert_eq!(Settings::default().appearance, DEFAULT_APPEARANCE);
        // 变体注册名（含空格）原样写入、原样读回，不依赖任何内置 tint id。
        for name in [
            DEFAULT_APPEARANCE,
            DEFAULT_DARK_APPEARANCE,
            "Catppuccin Latte",
            "Catppuccin Mocha",
            "Tokyo Moon",
            "Tokyo Storm",
        ] {
            let s = Settings {
                appearance: name.into(),
                ..Default::default()
            };
            let json = serde_json::to_string_pretty(&s).expect("serializes");
            assert!(
                json.contains(&format!("\"appearance\": \"{name}\"")),
                "{json}"
            );
            let back = parse(&json);
            assert_eq!(back.appearance, name, "{name}");
        }
        // 手改的未知值也不会让整个配置文件失效；主题切换时回退默认浅色。
        let unknown = parse(r#"{"appearance": "nonsense"}"#);
        assert_eq!(unknown.appearance, "nonsense");
        assert!(!unknown.dark());
    }

    #[test]
    fn appearance_dark_and_label_are_compatibility_helpers() {
        // 真实明暗由 ThemeConfig.mode 决定；这里只验证历史名字启发式。
        assert!(!appearance_dark(DEFAULT_APPEARANCE));
        for name in [
            DEFAULT_DARK_APPEARANCE,
            "macOS Classic Dark",
            "Ayu Dark",
            "legacy-dark",
            "DARK",
        ] {
            assert!(appearance_dark(name), "{name}");
        }
        assert!(!appearance_dark("Catppuccin Mocha"));
        assert!(!appearance_dark(""));
        assert!(!appearance_dark("nonsense"));
        // 标签保持注册名原文，空白由手工编辑产生时会被去掉。
        assert_eq!(appearance_label("  Catppuccin Latte  "), "Catppuccin Latte");
        assert_eq!(appearance_label("nonsense"), "nonsense");
        assert_eq!(appearance_label(""), "");
    }

    #[test]
    fn theme_variant_names_do_not_need_a_dark_suffix() {
        // 真实明暗由 ThemeConfig.mode 决定，与名字后缀无关。下面的名字启发式
        // 只按是否含 "dark" 猜测，因此无后缀的深色变体不会被它识别为深色——
        // 这正说明深色变体靠 mode 而非后缀决定明暗，启发式仅按名字猜。
        for name in ["Catppuccin Mocha", "Tokyo Moon", "Adventure"] {
            assert!(!appearance_dark(name), "{name}");
        }
        assert!(!appearance_dark("Ayu Light"));
        assert!(appearance_dark("Ayu Dark"));
        // 大小写不敏感，首尾空白不影响判断。
        assert!(appearance_dark("  DARK  "));
    }

    #[test]
    fn what_we_write_is_what_we_read_back() {
        let mut before = Settings::default();
        before.translation.base_url = "https://x/v1".into();
        before.translation.prompt = "be terse".into();
        before.translation.target_language = "ja".into();
        before.translation.timeout_seconds = 60;
        before.translation.allow_insecure_http = true;
        before.screenshot.hidden_toolbar_items = vec!["text".into()];
        before.ocr.models_dir = Some(PathBuf::from("D:/models/v6"));

        let json = serde_json::to_string_pretty(&before).expect("serializes");
        // 翻译字段全部走 camelCase，和老字段保持同一套命名约定。
        assert!(json.contains("\"baseUrl\""), "{json}");
        assert!(json.contains("\"targetLanguage\""), "{json}");
        assert!(json.contains("\"timeoutSeconds\""), "{json}");
        assert!(json.contains("\"allowInsecureHttp\""), "{json}");
        let after = parse(&json);

        assert_eq!(after.translation.base_url, before.translation.base_url);
        assert_eq!(after.translation.prompt, before.translation.prompt);
        assert_eq!(after.translation.model, before.translation.model);
        assert_eq!(
            after.translation.target_language,
            before.translation.target_language
        );
        assert_eq!(
            after.translation.timeout_seconds,
            before.translation.timeout_seconds
        );
        assert_eq!(
            after.translation.allow_insecure_http,
            before.translation.allow_insecure_http
        );
        assert_eq!(
            after.screenshot.hidden_toolbar_items,
            before.screenshot.hidden_toolbar_items
        );
        assert_eq!(after.ocr.models_dir, before.ocr.models_dir);
        assert_eq!(after.screenshot.annotation.stroke_color, "#FA3C3C");
    }

    /// 缺省值与旧行为一致：「完成」只复制，不写文件。
    #[test]
    fn export_auto_flags_default_to_clipboard_only() {
        let export = Export::default();
        assert!(!export.auto_save);
        assert!(export.auto_copy);

        // 从未写过这两个键的配置文件也必须保留这一行为。
        let s = parse(r#"{"screenshot": {"export": {}}}"#);
        assert!(!s.screenshot.export.auto_save);
        assert!(s.screenshot.export.auto_copy);
    }

    #[test]
    fn export_auto_flags_use_camel_case_keys_and_roundtrip() {
        let s = parse(r#"{"screenshot": {"export": {"autoSave": true, "autoCopy": false}}}"#);
        assert!(s.screenshot.export.auto_save);
        assert!(!s.screenshot.export.auto_copy);

        let json = serde_json::to_string(&s.screenshot.export).expect("serializes");
        assert!(json.contains("\"autoSave\":true"), "{json}");
        assert!(json.contains("\"autoCopy\":false"), "{json}");
        let back: Export = serde_json::from_str(&json).expect("parses");
        assert_eq!(back.auto_save, s.screenshot.export.auto_save);
        assert_eq!(back.auto_copy, s.screenshot.export.auto_copy);
    }

    #[test]
    fn auto_scroll_defaults_match_the_shared_step_policy() {
        let s = Settings::default();
        assert_eq!(s.screenshot.auto_scroll.interval_ms, 600);
        assert_eq!(s.screenshot.auto_scroll.notches, 1);
        assert_eq!(
            s.screenshot.auto_scroll.step(),
            crate::scroll_step::ScrollStep::new()
        );
    }

    #[test]
    fn auto_scroll_values_are_clamped_when_applied() {
        // A hand-edited file must not be able to push the beat outside the
        // stitcher's motion window or the step past the notch ceiling.
        let s = AutoScroll {
            interval_ms: 10,
            notches: 999,
        };
        assert_eq!(s.step().interval, crate::scroll_step::MIN_INTERVAL);
        assert_eq!(s.step().notches, crate::scroll_step::MAX_NOTCHES);

        let s = AutoScroll {
            interval_ms: 5000,
            notches: 0,
        };
        assert_eq!(s.step().interval, crate::scroll_step::MAX_INTERVAL);
        assert_eq!(s.step().notches, crate::scroll_step::DEFAULT_NOTCHES);
    }

    #[test]
    fn absent_auto_scroll_keeps_its_defaults() {
        let s = parse(r#"{"screenshot": {}}"#);
        assert_eq!(s.screenshot.auto_scroll.interval_ms, 600);
        assert_eq!(s.screenshot.auto_scroll.notches, 1);
    }

    #[test]
    fn annotation_defaults_match_the_editor_constants() {
        let a = Annotation::default();
        assert_eq!(a.stroke_argb(), crate::overlay::theme::STROKE_ARGB);
        assert_eq!(
            a.clamped_stroke_width(),
            crate::overlay::editor::DEFAULT_STROKE_WIDTH
        );
        assert_eq!(
            a.clamped_mosaic_block(),
            crate::overlay::editor::DEFAULT_MOSAIC_BLOCK
        );
    }

    #[test]
    fn hex_colors_parse_both_forms() {
        assert_eq!(parse_hex_color("#FA3C3C"), Some(0xFFFA_3C3C));
        assert_eq!(parse_hex_color("fa3c3c"), Some(0xFFFA_3C3C));
        assert_eq!(parse_hex_color("#80FA3C3C"), Some(0x80FA_3C3C));
        assert_eq!(parse_hex_color("red"), None);
        assert_eq!(parse_hex_color("#12345"), None);
    }

    /// Hand-edited extremes must not break the renderer.
    #[test]
    fn annotation_values_are_clamped_when_applied() {
        let a = Annotation {
            stroke_width: 999.0,
            stroke_color: String::new(),
            mosaic_block: 0,
        };
        assert_eq!(a.clamped_stroke_width(), 20.0);
        assert_eq!(a.clamped_mosaic_block(), 4);
        assert_eq!(a.stroke_argb(), crate::overlay::theme::STROKE_ARGB);
    }

    #[test]
    fn watermark_defaults_match_the_ui_presets() {
        let wm = Watermark::default();
        assert_eq!(wm.opacity, 50);
        assert_eq!(wm.position, "bottomRight");
        assert_eq!(wm.scale, 1.0);
        assert!(!wm.tiled);
        assert!(!wm.timestamp);
        assert!(!wm.is_visible());
    }

    #[test]
    fn watermark_is_visible_only_when_text_timestamp_or_image() {
        let mut wm = Watermark::default();
        assert!(!wm.is_visible());
        wm.text = "机密".into();
        assert!(wm.is_visible());
        wm.text.clear();
        wm.timestamp = true;
        assert!(wm.is_visible());
        wm.timestamp = false;
        wm.image_path = Some(PathBuf::from("C:/does/not/exist.png"));
        assert!(
            !wm.is_visible(),
            "a missing image must not enable the watermark"
        );
    }

    #[test]
    fn watermark_resolved_text_prepends_the_stamp() {
        let mut wm = Watermark {
            text: "内部资料".into(),
            ..Default::default()
        };
        assert_eq!(wm.resolved_text(), "内部资料");
        wm.timestamp = true;
        let out = wm.resolved_text();
        assert!(out.ends_with("内部资料"));
        // `YYYY-MM-DD HH:MM:SS  内部资料`
        assert!(out.len() >= "内部资料".len() + 19);
    }

    #[test]
    fn watermark_position_roundtrips_through_its_key() {
        for p in WatermarkPosition::ALL {
            assert_eq!(WatermarkPosition::from_key(p.key()), p);
        }
        // An unknown key falls back to the default (BottomRight).
        assert_eq!(
            WatermarkPosition::from_key("nonsense"),
            WatermarkPosition::default()
        );
    }

    #[test]
    fn watermark_serializes_and_parses_back() {
        let wm = Watermark {
            text: "机密".into(),
            position: "topLeft".into(),
            scale: 2.0,
            image_path: Some(PathBuf::from("D:/a/b.png")),
            ..Default::default()
        };
        let json = serde_json::to_string(&wm).expect("serializes");
        let back: Watermark = serde_json::from_str(&json).expect("parses");
        assert_eq!(back.text, "机密");
        assert_eq!(back.position, "topLeft");
        assert_eq!(back.scale, 2.0);
        assert_eq!(back.image_path, Some(PathBuf::from("D:/a/b.png")));
    }

    /// 手写模板可能长达上万字符（超过 MAX_PATH），也可能正好是 Windows
    /// 保留设备名。截断 + 转义后保存才不会失败，正常文件名保持不变。
    #[test]
    fn sanitize_stem_caps_length_and_escapes_reserved_names() {
        let long = "a".repeat(10_000);
        let out = sanitize_stem(&long);
        assert_eq!(out.chars().count(), MAX_FILENAME_STEM_CHARS);

        for name in ["CON", "nul", "COM1", "Lpt9", "aux", "prn", "CONIN$"] {
            let out = sanitize_stem(name);
            assert_ne!(out, name, "{name:?} 必须转义");
            assert!(out.starts_with(name), "{name:?} 不应被改写正文");
            assert!(out.ends_with('_'), "{name:?} 应加下划线后缀");
        }
        assert_eq!(sanitize_stem("con.txt"), "con_.txt");
        assert_eq!(sanitize_stem("CON .txt"), "CON_.txt");

        let export = Export {
            filename_template: long,
            ..Default::default()
        };
        assert_eq!(
            export.expand_name("20260101", "120000").chars().count(),
            MAX_FILENAME_STEM_CHARS
        );
        // 默认模板（以及任何普通名字）一个字符都不变。
        let normal = Export::default();
        assert_eq!(
            normal.expand_name("20260101", "120000"),
            "FastPin_20260101_120000"
        );
    }

    /// 语法错误不能只回默认值：下一次保存会把默认值整份写回原文件。
    /// 加载路径必须先把损坏文件改名留档，且不覆盖已有备份。
    #[test]
    fn broken_config_is_quarantined_before_defaults_are_saved() {
        let unique = format!(
            "settings-quarantine-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        );
        let dir = std::env::temp_dir();
        let path = dir.join(format!("{unique}.json"));
        let prefix = format!("{unique}.json.broken-");

        fn backups(dir: &Path, prefix: &str) -> Vec<PathBuf> {
            let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
                .expect("reads temp dir")
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(prefix))
                })
                .collect();
            found.sort();
            found
        }

        // 尾逗号是语法错误，parse 会直接回默认值，必须先隔离原文件。
        let first = "{\"appearance\": \"Default Light\",}";
        std::fs::write(&path, first).expect("writes broken file");
        assert!(quarantine_broken(&path, Some(first)));
        assert!(!path.exists(), "损坏文件应已被移走");
        let first_backups = backups(&dir, &prefix);
        assert_eq!(first_backups.len(), 1, "应留下一份备份");
        let saved = std::fs::read_to_string(&first_backups[0]).expect("reads backup");
        assert_eq!(saved, first, "备份内容必须原样保留");
        assert!(is_broken_config(first));

        // 第二次损坏要另建备份（撞名加序号），不得覆盖第一次的内容。
        let second = "{\"screenshot\":";
        std::fs::write(&path, second).expect("writes second broken file");
        assert!(quarantine_broken(&path, Some(second)));
        assert!(!path.exists());
        let all = backups(&dir, &prefix);
        assert_eq!(all.len(), 2, "第二次隔离应另建备份");
        let first_saved = std::fs::read_to_string(&all[0]).expect("reads first backup");
        let second_saved = std::fs::read_to_string(&all[1]).expect("reads second backup");
        assert_eq!(first_saved, first, "旧备份不能被覆盖");
        assert_eq!(second_saved, second);

        // 合法对象与空文件都不该被隔离。
        assert!(!is_broken_config(r#"{"appearance":"Default Light"}"#));
        assert!(!is_broken_config("   "));

        for backup in all {
            let _ = std::fs::remove_file(backup);
        }
    }

    /// 读取失败（杀软/同步占用/权限）不能被当成空配置：加载路径会用
    /// `quarantine_broken(.., None)` 把原文件改名留档，本次按默认值运行但
    /// 不写缓存，下次读盘已是 NotFound 才会缓存默认值。这里覆盖“无文本
    /// 可读”的隔离、备份内容保留，以及判定函数的各分支。
    #[test]
    fn read_failure_quarantines_original_and_keeps_backup() {
        let not_found = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked");
        assert!(load_text_is_recoverable(&Ok(
            r#"{"appearance":"Default Light"}"#.into()
        )));
        assert!(load_text_is_recoverable(&Err(not_found)));
        assert!(!load_text_is_recoverable(&Err(denied)));

        let unique = format!(
            "settings-unreadable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        );
        let dir = std::env::temp_dir();
        let path = dir.join(format!("{unique}.json"));
        let prefix = format!("{unique}.json.broken-");
        // 合法配置：读取失败时绝不能被默认值覆盖，必须先原样留档。
        let original = r#"{"translation":{"baseUrl":"https://api.example.com/v1"}}"#;
        std::fs::write(&path, original).expect("writes valid config");
        assert!(quarantine_broken(&path, None));
        assert!(!path.exists(), "读取失败时原文件应已被隔离");
        let backups: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("reads temp dir")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect();
        assert_eq!(backups.len(), 1, "应留下一份备份");
        let saved = std::fs::read_to_string(&backups[0]).expect("reads backup");
        assert_eq!(saved, original, "合法配置必须原样保留在备份中");
        // 无文本可读时加载路径返回 parse("") 的默认值，且不写缓存。
        let defaults = parse("");
        assert_eq!(
            defaults.translation.model, "gpt-4o-mini",
            "读取失败时本次仍按默认配置继续运行"
        );
        assert_eq!(
            defaults.translation.base_url, "",
            "默认值不应带用户的 baseUrl"
        );
        let _ = std::fs::remove_file(&backups[0]);
    }

    #[test]
    fn a_bad_split_field_keeps_the_other_split_fields() {
        let s = parse(
            r#"{
              "screenshot": {
                "saveDir": "D:/shots",
                "split": {
                  "direction": "grid",
                  "splitType": "precise",
                  "unit": "pixels",
                  "count": "2",
                  "rows": 3,
                  "columns": 4,
                  "pixels": 32,
                  "percentage": "70",
                  "overlapWidth": 2
                }
              }
            }"#,
        );
        assert_eq!(
            s.screenshot.save_dir.as_deref(),
            Some(std::path::Path::new("D:/shots"))
        );
        assert_eq!(
            s.screenshot.split.direction,
            crate::imagesplit::SplitDirection::Grid
        );
        assert_eq!(
            s.screenshot.split.split_type,
            crate::imagesplit::SplitType::Precise
        );
        assert_eq!(
            s.screenshot.split.unit,
            crate::imagesplit::SplitUnit::Pixels
        );
        assert_eq!(s.screenshot.split.count, 2, "坏字段回默认");
        assert_eq!(s.screenshot.split.rows, 3);
        assert_eq!(s.screenshot.split.columns, 4);
        assert_eq!(s.screenshot.split.pixels, 32);
        assert_eq!(s.screenshot.split.percentage, "70");
        assert_eq!(s.screenshot.split.overlap_width, 2);
    }

    #[test]
    fn oversized_string_vecs_are_capped() {
        let mut before = Settings::default();
        before.screenshot.hidden_toolbar_items = (0..300).map(|i| format!("tool-{i}")).collect();
        before.screenshot.record_history_sources = (0..300).map(|i| format!("src-{i}")).collect();
        let json = serde_json::to_string(&before).expect("serializes");

        // 合法但超大的数组仍会被完整反序列化，parse 出口也要截断。
        let direct = parse(&json);
        assert_eq!(direct.screenshot.hidden_toolbar_items.len(), 256);
        assert_eq!(direct.screenshot.hidden_toolbar_items[255], "tool-255");
        assert_eq!(direct.screenshot.record_history_sources.len(), 256);
        assert_eq!(direct.screenshot.record_history_sources[255], "src-255");

        // 逐字段恢复路径同样截断。
        let mut broken: serde_json::Value = serde_json::from_str(&json).expect("parses");
        broken["screenshot"]["captureMode"] = serde_json::json!("banana");
        let recovered = parse(&broken.to_string());
        assert_eq!(recovered.screenshot.capture_mode, CaptureMode::Auto);
        assert_eq!(recovered.screenshot.hidden_toolbar_items.len(), 256);
        assert_eq!(recovered.screenshot.hidden_toolbar_items[0], "tool-0");
        assert_eq!(recovered.screenshot.record_history_sources.len(), 256);
        assert_eq!(recovered.screenshot.record_history_sources[0], "src-0");
    }
}
