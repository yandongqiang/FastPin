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
}

impl Default for Translation {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: "gpt-4o-mini".into(),
            target_language: "zh-CN".into(),
            prompt: String::new(),
            timeout_seconds: 30,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(rename = "bottomRight")]
    BottomRight,
}

impl Default for WatermarkPosition {
    fn default() -> Self {
        Self::BottomRight
    }
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

    /// `YYYY-MM-DD HH:MM:SS` in UTC — `chrono` is not a dependency; the
    /// watermark carries the capture time, not a locale-aware clock.
    fn utc_stamp() -> String {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let (h, m, s) = (t / 3600 % 24, t / 60 % 60, t % 60);
        let (y, d, mo) = (t / 31_536_000 + 1970, t / 86_400 % 365, t / 2_592_000 % 12);
        format!("{y:04}-{mo:02}-{d:03} {h:02}:{m:02}:{s:02}")
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

/// Writes the file, then refreshes the cache. Cache-last: if the write fails
/// the process keeps behaving like the settings the user can actually see.
pub fn save(settings: &Settings) -> Result<()> {
    let path = path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|e| Error::new(format!("{e}")))?;
    std::fs::write(&path, text)?;
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
    serde_json::from_str(text).unwrap_or_else(|e| {
        // Falling back in silence reads as "translation is broken" with no clue why.
        eprintln!("settings.json ignored: {e}");
        Settings::default()
    })
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
        assert_eq!(s.hotkeys.region, Hotkeys::default().region);
        // A file written before the appearance field existed starts light.
        assert_eq!(s.appearance, APPEARANCE_LIGHT);
        assert!(!s.dark());
    }

    #[test]
    fn appearance_roundtrips_and_unknown_values_stay_light() {
        let mut s = Settings::default();
        s.appearance = APPEARANCE_DARK.into();
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
        let mut wm = Watermark::default();
        wm.text = "内部资料".into();
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
        let mut wm = Watermark::default();
        wm.text = "机密".into();
        wm.position = "topLeft".into();
        wm.scale = 2.0;
        wm.image_path = Some(PathBuf::from("D:/a/b.png"));
        let json = serde_json::to_string(&wm).expect("serializes");
        let back: Watermark = serde_json::from_str(&json).expect("parses");
        assert_eq!(back.text, "机密");
        assert_eq!(back.position, "topLeft");
        assert_eq!(back.scale, 2.0);
        assert_eq!(back.image_path, Some(PathBuf::from("D:/a/b.png")));
    }
}
