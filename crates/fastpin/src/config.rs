//! `settings.json` in `%LOCALAPPDATA%\FastPin`. Cached after the first read;
//! `save` swaps the cache so the settings window takes effect without a restart.
//!
//! The API key is deliberately absent: it stays in the Windows credential
//! vault, shared with the C# build. See `translate::api_key`.

use std::path::PathBuf;
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
    /// UI appearance: `"light"` (default) or `"dark"`. A plain string keeps the
    /// file hand-editable, same rationale as the hotkey strings; an unknown
    /// value falls back to light instead of failing the whole file.
    pub appearance: String,
}

/// Stored values of `Settings::appearance`.
pub const APPEARANCE_LIGHT: &str = "light";
pub const APPEARANCE_DARK: &str = "dark";

impl Default for Settings {
    fn default() -> Self {
        Self {
            ocr: Ocr::default(),
            translation: Translation::default(),
            screenshot: Screenshot::default(),
            history: History::default(),
            hotkeys: Hotkeys::default(),
            appearance: APPEARANCE_LIGHT.into(),
        }
    }
}

impl Settings {
    /// Whether the persisted appearance is the dark one. Anything else
    /// (including an empty or hand-edited value) reads as light.
    pub fn dark(&self) -> bool {
        self.appearance.trim().eq_ignore_ascii_case(APPEARANCE_DARK)
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// How the region picker snaps to windows. See [`SmartSelection`].
    pub smart_selection: SmartSelectionMode,
    /// Auto-scroll cadence for long screenshots. See [`AutoScroll`].
    pub auto_scroll: AutoScroll,
    /// Encoded-file format, quality and name for 保存 / export.
    pub export: Export,
    /// Static image collage settings (direction, spacing, border, grid).
    pub collage: CollageSettings,
    /// PDF export settings (paper size, margins, template, stitching method).
    pub pdf: PdfSettings,
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

/// Replaces the characters Windows refuses in a file name, and strips trailing
/// dots/spaces that `CreateFile` would otherwise reject. Returns
/// [`DEFAULT_FILENAME_TEMPLATE`] if nothing usable survives.
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
        DEFAULT_FILENAME_TEMPLATE.to_string()
    } else {
        trimmed.to_string()
    }
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

    /// Short label for the picker's mode switch.
    pub fn label(self) -> &'static str {
        match self {
            Self::WindowSubElement => "子元素",
            Self::Window => "整窗",
            Self::Disabled => "禁用",
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
    /// Reserved for a user `strftime`-style pattern; ignored today.
    pub timestamp_format: String,
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
            timestamp_format: String::new(),
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
    let loaded = parse(&read_file());
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

fn read_file() -> String {
    path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// A missing or malformed file falls back to defaults — a screenshot tool must
/// still capture when its settings are broken.
fn parse(text: &str) -> Settings {
    if text.trim().is_empty() {
        return Settings::default();
    }
    match serde_json::from_str::<Settings>(text) {
        Ok(settings) => settings,
        Err(e) => {
            // One wrong field type (say `"timeoutSeconds": "30"`) used to throw
            // away the whole file: baseUrl, saveDir and history were lost and
            // the next save overwrote the original. Keep every field whose JSON
            // type still matches the default shape and drop only the bad ones.
            eprintln!("settings.json has invalid fields ({e}); keeping the well-typed ones");
            let default = match serde_json::to_value(Settings::default()) {
                Ok(value) => value,
                Err(_) => return Settings::default(),
            };
            let input: serde_json::Value = match serde_json::from_str(text) {
                Ok(value) => value,
                Err(_) => return Settings::default(),
            };
            serde_json::from_value(merge_typed(&default, &input)).unwrap_or_default()
        }
    }
}

/// Merges `input` into the default-shaped tree, keeping only values whose JSON
/// kind matches the default at that path. Unknown keys are dropped; a bad leaf
/// falls back to its default instead of poisoning the whole document.
fn merge_typed(default: &serde_json::Value, input: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match (default, input) {
        (Value::Object(base), Value::Object(over)) => {
            let mut out = base.clone();
            for (key, value) in over {
                if let Some(expected) = base.get(key) {
                    out.insert(key.clone(), merge_typed(expected, value));
                }
            }
            Value::Object(out)
        }
        (Value::Null, other) => other.clone(),
        (expected, Value::Null) => expected.clone(),
        (expected, other) if same_kind(expected, other) => other.clone(),
        (expected, _) => expected.clone(),
    }
}

fn same_kind(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value;
    matches!(
        (a, b),
        (Value::Bool(_), Value::Bool(_))
            | (Value::Number(_), Value::Number(_))
            | (Value::String(_), Value::String(_))
            | (Value::Array(_), Value::Array(_))
    )
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
        // A file written before the appearance field existed starts light.
        assert_eq!(s.appearance, APPEARANCE_LIGHT);
        assert!(!s.dark());
    }

    #[test]
    fn appearance_roundtrips_and_unknown_values_stay_light() {
        let s = Settings {
            appearance: APPEARANCE_DARK.into(),
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&s).expect("serializes");
        assert!(json.contains("\"appearance\": \"dark\""), "{json}");
        assert!(parse(&json).dark());

        let back = parse(r#"{"appearance": "dark"}"#);
        assert!(back.dark());
        assert!(!parse(r#"{"appearance": "nonsense"}"#).dark());
        assert!(!parse(r#"{"appearance": ""}"#).dark());
        assert_eq!(Settings::default().appearance, APPEARANCE_LIGHT);
    }

    #[test]
    fn what_we_write_is_what_we_read_back() {
        let mut before = Settings::default();
        before.translation.base_url = "https://x/v1".into();
        before.translation.prompt = "be terse".into();
        before.screenshot.hidden_toolbar_items = vec!["text".into()];
        before.ocr.models_dir = Some(PathBuf::from("D:/models/v6"));

        let after = parse(&serde_json::to_string_pretty(&before).expect("serializes"));

        assert_eq!(after.translation.base_url, before.translation.base_url);
        assert_eq!(after.translation.prompt, before.translation.prompt);
        assert_eq!(after.translation.model, before.translation.model);
        assert_eq!(
            after.translation.timeout_seconds,
            before.translation.timeout_seconds
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
}
