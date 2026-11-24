//! Overlay chrome tokens, projected from the gpui-component theme.
//!
//! The free functions below keep their historical names and signatures so the
//! toolbar, settings window and pin editor do not have to know where the
//! colours come from. They read a process-wide snapshot that
//! [`apply_theme`] fills from `gpui_component::Theme`; before the first apply
//! the snapshot falls back to the gpui-component light theme.
//!
//! Everything is a snapshot rather than a `cx` lookup because most of these
//! helpers are called from pure element builders that never see the app.
//!
//! themes 目录的主题由 gpui-kit 的 `ThemeRegistry` 加载与监听：启动时同步读入
//! 目录里的所有 `*.json`（默认浅色 / 深色之外还有 20+ 变体），再交给 registry
//! 的 `watch_dir` 做热重载。查找主题时用各 JSON `themes` 数组条目的 `name`
//! （如 `Ayu Light`）——registry 正是以这个名字作为键。

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use gpui::{hsla, rgb, App, Hsla, Pixels, SharedString};
use gpui_kit::component::{Theme, ThemeColor, ThemeMode, ThemeRegistry};

/// Colours the chrome needs, already resolved from the theme's colour tokens.
///
/// `Copy` so readers can take it out of the lock and drop the guard before
/// building elements (no lock is held while rendering).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub surface: Hsla,
    pub secondary: Hsla,
    pub muted: Hsla,
    pub foreground: Hsla,
    pub muted_foreground: Hsla,
    pub primary: Hsla,
    pub primary_foreground: Hsla,
    pub selection: Hsla,
    pub border: Hsla,
    pub destructive: Hsla,
    pub input: Hsla,
    /// The component theme's `primary_hover` / `primary_active`.
    pub accent_hover: Hsla,
    pub accent_pressed: Hsla,
    pub danger_hover: Hsla,
    /// Text/icon colour painted on a `primary` fill.
    pub on_accent: Hsla,
    /// Text painted on a `destructive` fill.
    pub on_danger: Hsla,
    /// Darkest available ink, for shadows.
    pub shadow: Hsla,
    pub title_bar: Hsla,
    pub title_bar_border: Hsla,
    pub radius: Pixels,
    pub font_size: Pixels,
    pub dark: bool,
}

/// Snapshot storage. `OnceLock` cannot be refreshed when the user flips the
/// theme, so a plain `RwLock<Option<_>>` is the right shape here.
fn store() -> &'static RwLock<Option<Palette>> {
    static SNAPSHOT: RwLock<Option<Palette>> = RwLock::new(None);
    &SNAPSHOT
}

fn darker(a: Hsla, b: Hsla) -> Hsla {
    if a.l <= b.l {
        a
    } else {
        b
    }
}

/// Maps the gpui-component theme onto the chrome palette.
///
/// Light and dark differ only in which token values come in; the rules are
/// shared so a future palette cannot make one appearance unreadable while the
/// other stays fine. Component tokens that carry a hover/active companion
/// (`primary_hover`, `primary_active`, `danger_hover`) are used as-is instead
/// of being derived from lightness, so the chrome matches the component
/// library's own states.
fn palette_from(theme: &Theme) -> Palette {
    let colors = &theme.colors;
    // Shadows want the darkest available ink; in dark mode that is the
    // background, in light mode it is the foreground.
    let base = darker(colors.background, colors.foreground);
    Palette {
        surface: colors.background,
        secondary: colors.secondary,
        muted: colors.muted,
        foreground: colors.foreground,
        muted_foreground: colors.muted_foreground,
        primary: colors.primary,
        primary_foreground: colors.primary_foreground,
        selection: colors.selection,
        border: colors.border,
        destructive: colors.danger,
        input: colors.input,
        accent_hover: colors.primary_hover,
        accent_pressed: colors.primary_active,
        danger_hover: colors.danger_hover,
        on_accent: colors.primary_foreground,
        // `danger_foreground` is a mid-red in the dark palette (red-600 on
        // red-400), which disappears on the fill; the background ink is
        // readable in both appearances.
        on_danger: colors.background,
        shadow: base,
        title_bar: colors.title_bar,
        title_bar_border: colors.title_bar_border,
        radius: theme.radius,
        font_size: theme.font_size,
        dark: theme.is_dark(),
    }
}

fn light_theme() -> Theme {
    Theme::from(&*ThemeColor::light())
}

fn light_palette() -> Palette {
    palette_from(&light_theme())
}

fn current() -> Palette {
    let guard = store().read().unwrap_or_else(|e| e.into_inner());
    (*guard).unwrap_or_else(light_palette)
}

/// Replaces the snapshot from the initialized gpui-component theme.
/// Idempotent: applying the same theme twice leaves the same palette.
pub fn apply_theme(theme: &Theme) {
    let palette = palette_from(theme);
    let mut guard = store().write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(palette);
}

/// registry 里的主题名快照。`available_themes()` 是静态函数拿不到 `cx`，
/// 只能在 `init_themes` 时从 registry 收集一次；默认两项在前，其余按名字排序。
fn theme_keys() -> &'static RwLock<Vec<String>> {
    static KEYS: RwLock<Vec<String>> = RwLock::new(Vec::new());
    &KEYS
}

/// 默认两项固定排在最前，其余按名字排序。纯函数，便于无 `cx` 的测试。
fn theme_name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn rank(name: &str) -> u8 {
        if name == crate::config::DEFAULT_APPEARANCE {
            0
        } else if name == crate::config::DEFAULT_DARK_APPEARANCE {
            1
        } else {
            2
        }
    }
    rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
}

fn sorted_theme_names(mut names: Vec<String>) -> Vec<String> {
    names.sort_by(|a, b| theme_name_cmp(a, b));
    names
}

/// 下拉选项直接以 registry 注册名作为 id 和标签；空快照回退 gpui-kit 默认
/// 两项，保证下拉永远不为空。纯函数，便于无 `cx` 的测试。
fn theme_options_from_keys(names: &[String]) -> Vec<(String, String)> {
    if names.is_empty() {
        return vec![
            (
                crate::config::DEFAULT_APPEARANCE.to_string(),
                crate::config::DEFAULT_APPEARANCE.to_string(),
            ),
            (
                crate::config::DEFAULT_DARK_APPEARANCE.to_string(),
                crate::config::DEFAULT_DARK_APPEARANCE.to_string(),
            ),
        ];
    }
    names
        .iter()
        .map(|name| (name.clone(), name.clone()))
        .collect()
}

/// 设置页下拉内容；列表在 `init_themes` 后不再变化，开窗时取一次即可。
pub fn available_themes() -> Vec<(String, String)> {
    let guard = theme_keys().read().unwrap_or_else(|e| e.into_inner());
    theme_options_from_keys(&guard)
}

/// 仓库内的 themes 目录（编译期路径），供开发运行与发布包缺失时回退。
const REPO_THEMES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../themes");

/// 纯路径选择：exe 旁的 `themes/` 存在（`shipped_exists`）时优先用它（发布
/// 布局），否则回退仓库内的副本（开发运行）。把 `is_dir` 的结果作为参数传入，
/// 让这段逻辑可以脱离真实文件系统单测。
fn themes_dir_from(exe_base: Option<&Path>, shipped_exists: bool, repo: &Path) -> PathBuf {
    match exe_base {
        Some(base) if shipped_exists => base.join("themes"),
        _ => repo.to_path_buf(),
    }
}

/// 解析 themes 目录：先找 exe 旁的 `themes/`（发布布局），找不到再用仓库内的
/// 副本（开发运行）。和 ocr 的模型目录同一套策略，换目录运行也能加载主题。
fn themes_dir() -> PathBuf {
    let exe_base = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()));
    let shipped_exists = exe_base
        .as_ref()
        .map(|base| base.join("themes").is_dir())
        .unwrap_or(false);
    themes_dir_from(exe_base.as_deref(), shipped_exists, Path::new(REPO_THEMES))
}

/// 把主题名快照从 registry 刷新一次，供下拉使用。纯过程，无 `cx` 参数即可在
/// 热重载回调外复用；有 registry 时收集全部注册名（默认两项 + themes 变体）。
fn refresh_theme_keys(cx: &App) {
    let keys = sorted_theme_names(
        ThemeRegistry::global(cx)
            .themes()
            .keys()
            .map(|key| key.to_string())
            .collect(),
    );
    *theme_keys().write().unwrap_or_else(|e| e.into_inner()) = keys;
}

/// 通过 gpui-kit 的 `ThemeRegistry` 加载并监听 themes 目录：启动时先同步读入
/// 所有 `*.json`（保证首次打开设置页下拉就完整），再交给 registry 的 `watch_dir`
/// 做热重载，文件变动时刷新主题名快照并重绘窗口。找不到 registry 时清空快照，
/// 绝不 panic。
pub fn init_themes(cx: &mut gpui::App) {
    // 正常由 init() 在 gpui_kit::init 之后调用；单独调用时不 panic，静默跳过。
    if !cx.has_global::<ThemeRegistry>() {
        *theme_keys().write().unwrap_or_else(|e| e.into_inner()) = Vec::new();
        return;
    }

    let dir = themes_dir();
    // 同步读入，避免 watch_dir 的后台加载让首次下拉为空。单个文件解析失败时
    // 跳过，不阻断其余主题。
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                let _ = ThemeRegistry::global_mut(cx).load_themes_from_str(&text);
            }
        }
    }

    refresh_theme_keys(cx);

    // 后续文件变更由 registry 自己在后台监听 + 重载；回调里更新下拉快照并重绘。
    let _ = ThemeRegistry::watch_dir(dir, cx, |cx| {
        refresh_theme_keys(cx);
        cx.refresh_windows();
    });
}

/// Initializes gpui-kit (GPUI base, components and their globals), loads and
/// watches the theme directory, and applies the appearance persisted in
/// `settings.json`. Call once per `App`, before any window opens.
pub fn init(cx: &mut gpui::App) {
    gpui_kit::init(cx);
    init_themes(cx);
    set_appearance(cx, &crate::config::get().appearance);
}

/// Switches appearance at runtime through the component theme, refreshes the
/// snapshot, and asks every window (settings included) to repaint.
///
/// `name` 是 registry 里的主题注册名，即 themes 目录里各 `themes` 条目的 `name`
/// （如 `Ayu Light`）或 gpui-kit 默认浅色 / 深色。明暗与颜色一概取自 registry
/// 里的 `ThemeConfig`，不再叠加任何族映射或人造 tint。找不到的名字回退到
/// gpui-kit 默认浅色，并把修正后的名字写回配置（记日志），让配置与实际外观
/// 始终一致；绝不 panic。
pub fn set_appearance(cx: &mut gpui::App, name: &str) {
    let key = name.trim();
    let (cfg, fallback_name) = {
        let registry = ThemeRegistry::global(cx);
        if let Some(cfg) = registry.themes().get(&SharedString::from(key)).cloned() {
            (Some(cfg), None)
        } else {
            // 名字无效：回退到默认浅色变体。同时记下实际应用的注册名，
            // 写回配置，避免配置文件长期保留无效名字、UI 与外观对不上。
            let cfg = registry
                .themes()
                .get(&SharedString::from(crate::config::DEFAULT_APPEARANCE))
                .cloned()
                .or_else(|| registry.default_themes().get(&ThemeMode::Light).cloned())
                .or_else(|| registry.themes().values().next().cloned());
            let fallback_name = cfg.as_ref().map(|c| c.name.to_string());
            (cfg, fallback_name)
        }
    };
    if let Some(cfg) = cfg {
        if let Some(actual) = fallback_name {
            crate::perf::log_app(&format!(
                "appearance: 主题变体 '{name}' 无效，已回退到 '{actual}' 并写回配置"
            ));
            let mut settings = crate::config::get();
            if settings.appearance != actual {
                settings.appearance = actual;
                if let Err(e) = crate::config::save(&settings) {
                    crate::perf::log_app(&format!("appearance: 回退写回配置失败：{e}"));
                }
            }
        }
        Theme::update(cx, |t| t.apply_config(&cfg));
        let theme = Theme::global(cx).clone();
        apply_theme(&theme);
        cx.refresh_windows();
    }
}

// ---- chrome tokens ---------------------------------------------------------

/// Deepest chrome ink, used for shadows.
pub fn base() -> Hsla {
    current().shadow
}

/// Toolbar / popup surface. Slightly translucent so the frozen screenshot
/// still shows through, in both appearances.
pub fn surface() -> Hsla {
    current().surface.opacity(0.94)
}

/// Opaque surface for cards and controls that must not show what is behind.
pub fn surface_solid() -> Hsla {
    current().surface
}

/// Hover wash and secondary fill.
pub fn elevated_surface() -> Hsla {
    current().secondary
}

/// Page wash behind cards (settings window).
pub fn muted() -> Hsla {
    current().muted
}

pub fn primary_fg() -> Hsla {
    current().foreground
}

pub fn secondary_fg() -> Hsla {
    current().muted_foreground
}

pub fn disabled_fg() -> Hsla {
    current().muted_foreground.opacity(0.6)
}

/// Selected-tool / switch / primary-button fill.
pub fn accent() -> Hsla {
    current().primary
}

pub fn accent_hover() -> Hsla {
    current().accent_hover
}

// Reserved for pressed-state fills; the color test below pins the value.
#[allow(dead_code)]
pub fn accent_pressed() -> Hsla {
    current().accent_pressed
}

/// Text or icon painted on top of [`accent`].
pub fn on_accent() -> Hsla {
    current().on_accent
}

/// Selected-tool wash. Over the toolbar surface this reads as the tinted
/// square the reference paints behind the active tool.
pub fn active_background() -> Hsla {
    current().primary.opacity(0.14)
}

/// Solid wash for larger selected surfaces (settings nav pills).
pub fn selection_wash() -> Hsla {
    current().selection
}

pub fn border() -> Hsla {
    current().border
}

/// Body font size from the component theme, for consumers that live outside
/// GPUI (the owner-drawn tray menu). Corner radius is carried in [`Palette`]
/// too, but the Win32 popup menu draws its own frame, so no GDI consumer can
/// apply it.
pub fn font_size() -> Pixels {
    current().font_size
}

/// Slider rails and text-input backgrounds.
pub fn input() -> Hsla {
    current().input
}

pub fn danger() -> Hsla {
    current().destructive
}

pub fn danger_hover() -> Hsla {
    current().danger_hover
}

/// Text painted on a [`danger`] fill. The semantic `destructive_foreground`
/// is a mid-red in the dark palette, which disappears on the light red fill,
/// so the background ink (white in light, near-black in dark) is the readable
/// choice instead.
pub fn on_danger() -> Hsla {
    current().on_danger
}

pub fn success() -> Hsla {
    // No semantic success token; this green reads on both appearances.
    Hsla::from(rgb(0x35C96F))
}

pub fn transparent() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.0)
}

/// ARGB for annotation strokes.
pub const STROKE_ARGB: u32 = 0xFFFA_3C3C;

/// (hex, packed RGB) — the annotation palette, shared by the settings page's
/// default-colour row and the overlay toolbar's colour popup so the two can
/// never offer different colours. The first entry is `STROKE_ARGB`.
pub const SWATCHES: [(&str, u32); 8] = [
    ("#FA3C3C", 0xFA3C3C),
    ("#FF8C00", 0xFF8C00),
    ("#FFD700", 0xFFD700),
    ("#22B14C", 0x22B14C),
    ("#3B82F6", 0x3B82F6),
    ("#A855F7", 0xA855F7),
    ("#1E1E1E", 0x1E1E1E),
    ("#FFFFFF", 0xFFFFFF),
];

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Rgba;

    fn rgb_of(c: Hsla) -> (u8, u8, u8) {
        let r = Rgba::from(c);
        (
            (r.r * 255.0).round() as u8,
            (r.g * 255.0).round() as u8,
            (r.b * 255.0).round() as u8,
        )
    }

    fn light() -> Palette {
        palette_from(&light_theme())
    }

    fn dark() -> Palette {
        let mut theme = Theme::from(&*ThemeColor::dark());
        theme.mode = ThemeMode::Dark;
        palette_from(&theme)
    }

    #[test]
    fn light_and_dark_resolve_different_readable_palettes() {
        let light = light();
        // Light: dark ink on a bright surface.
        assert!(light.surface.l > 0.9, "{light:?}");
        assert!(light.foreground.l < 0.2, "{light:?}");
        assert!(light.muted_foreground.l > light.foreground.l);
        // Accent and its foreground are far apart, so selected tools and
        // primary buttons keep their label readable.
        assert!((light.primary.l - light.primary_foreground.l).abs() > 0.5);
        assert!((light.on_danger.l - light.destructive.l).abs() > 0.3);

        let dark = dark();
        assert!(dark.surface.l < 0.2, "{dark:?}");
        assert!(dark.foreground.l > 0.9, "{dark:?}");
        assert!(dark.muted_foreground.l < dark.foreground.l);
        assert!((dark.primary.l - dark.primary_foreground.l).abs() > 0.5);
        assert!((dark.on_danger.l - dark.destructive.l).abs() > 0.3);

        // The two appearances really differ in the key colours.
        assert_ne!(rgb_of(light.surface), rgb_of(dark.surface));
        assert_ne!(rgb_of(light.foreground), rgb_of(dark.foreground));
        assert_ne!(rgb_of(light.primary), rgb_of(dark.primary));
        assert_ne!(rgb_of(light.input), rgb_of(dark.input));
        assert_ne!(rgb_of(light.border), rgb_of(dark.border));
        // Borders and rails stay distinguishable from the surface they sit on.
        assert_ne!(rgb_of(dark.surface), rgb_of(dark.secondary));
        assert_ne!(rgb_of(dark.surface), rgb_of(dark.input));
        // Shadow ink is dark in both appearances.
        assert!(light.shadow.l < 0.2);
        assert!(dark.shadow.l < 0.2);
    }

    /// 变体配置的应用逻辑：明暗、圆角和主色都来自 `ThemeConfig`，
    /// 这正是 `set_appearance` 通过 `Theme::apply_config` 走的路径。
    #[test]
    fn applying_a_variant_config_sets_mode_and_tokens() {
        use std::rc::Rc;

        use gpui_kit::component::ThemeConfig;

        let config: ThemeConfig = serde_json::from_value(serde_json::json!({
            "name": "Catppuccin Mocha",
            "mode": "dark",
            "radius": 12,
            "colors": { "primary.background": "#A6E3A1", "background": "#1E1E2E" }
        }))
        .expect("parses variant config");
        let config = Rc::new(config);
        let mut theme = light_theme();
        assert!(!theme.is_dark());

        theme.apply_config(&config);
        assert!(theme.is_dark());
        assert_eq!(theme.radius, gpui::px(12.));
        let palette = palette_from(&theme);
        assert!(palette.dark);
        assert_eq!(rgb_of(palette.primary), rgb_of(Hsla::from(rgb(0xA6E3A1))));
        assert_eq!(rgb_of(palette.surface), rgb_of(Hsla::from(rgb(0x1E1E2E))));
    }

    #[test]
    fn apply_theme_is_idempotent() {
        // The only test that touches the process-wide snapshot: everything
        // else maps themes through `palette_from`, so parallel tests cannot
        // race each other's appearance.
        let mut t = Theme::from(&*ThemeColor::dark());
        t.mode = ThemeMode::Dark;
        apply_theme(&t);
        let once = current();
        apply_theme(&t);
        assert_eq!(once, current());
        apply_theme(&t);
        assert_eq!(once, current());

        // 切换变体后再重复应用同样不漂移。
        let mut other = Theme::from(&*ThemeColor::light());
        other.mode = ThemeMode::Light;
        apply_theme(&other);
        let light = current();
        apply_theme(&other);
        assert_eq!(light, current());
        assert_ne!(light.primary, once.primary);
    }

    #[test]
    fn uninitialized_snapshot_falls_back_to_light() {
        // The snapshot is process-wide; emulate "not yet applied" by reading
        // the fallback builder directly.
        let fallback = light_palette();
        assert!(!fallback.dark);
        assert!(fallback.surface.l > 0.9);
        assert_eq!(fallback, light());
    }

    #[test]
    fn surface_and_washes_keep_specified_alpha() {
        let light = light();
        assert!((light.surface.opacity(0.94).a - 0.94).abs() < 0.001);
        assert!((light.primary.opacity(0.14).a - 0.14).abs() < 0.001);
        assert!((light.muted_foreground.opacity(0.6).a - 0.6).abs() < 0.001);
        // Selection is the translucent token, not an opaque fill.
        assert!(light.selection.a < 1.0);
    }

    #[test]
    fn first_swatch_is_the_default_stroke() {
        assert_eq!(0xFF00_0000 | SWATCHES[0].1, STROKE_ARGB);
    }

    #[test]
    fn semantic_tokens_map_onto_the_chrome() {
        let theme = light_theme();
        let tokens = &theme.colors;
        let p = palette_from(&theme);
        assert_eq!(p.surface, tokens.background);
        assert_eq!(p.foreground, tokens.foreground);
        assert_eq!(p.muted, tokens.muted);
        assert_eq!(p.muted_foreground, tokens.muted_foreground);
        assert_eq!(p.primary, tokens.primary);
        assert_eq!(p.primary_foreground, tokens.primary_foreground);
        assert_eq!(p.border, tokens.border);
        assert_eq!(p.input, tokens.input);
        assert_eq!(p.selection, tokens.selection);
        assert_eq!(p.destructive, tokens.danger);
        assert_eq!(p.secondary, tokens.secondary);
        assert_eq!(p.title_bar, tokens.title_bar);
        assert_eq!(p.title_bar_border, tokens.title_bar_border);
        assert_eq!(p.radius, theme.radius);
        assert_eq!(p.font_size, theme.font_size);
        // The pairings the chrome actually paints with.
        assert_eq!(p.on_accent, tokens.primary_foreground);
        assert_eq!(p.on_danger, tokens.background);
        assert_ne!(p.accent_hover, p.primary);
        assert_ne!(p.danger_hover, p.destructive);
        // Component hover/active companions are consumed directly.
        assert_eq!(p.accent_hover, tokens.primary_hover);
        assert_eq!(p.accent_pressed, tokens.primary_active);
        assert_eq!(p.danger_hover, tokens.danger_hover);
    }

    /// registry 名列表的排序逻辑：默认两项在前，其余按名字排序。
    /// 不依赖 cx，也不触碰进程内的注册表。
    #[test]
    fn registry_names_are_sorted_with_defaults_first() {
        let names = sorted_theme_names(vec![
            "Tokyo Night".to_string(),
            crate::config::DEFAULT_DARK_APPEARANCE.to_string(),
            "Catppuccin Mocha".to_string(),
            crate::config::DEFAULT_APPEARANCE.to_string(),
            "Adventure".to_string(),
        ]);
        let expected: Vec<String> = [
            crate::config::DEFAULT_APPEARANCE,
            crate::config::DEFAULT_DARK_APPEARANCE,
            "Adventure",
            "Catppuccin Mocha",
            "Tokyo Night",
        ]
        .iter()
        .map(|name| name.to_string())
        .collect();
        assert_eq!(names, expected);
    }

    /// 下拉的 id 与标签都直接取 registry 注册名，默认两项也在其中。
    #[test]
    fn theme_options_come_from_registry_names() {
        let option = |name: &str| (name.to_string(), name.to_string());
        let names = sorted_theme_names(vec![
            "Catppuccin Mocha".to_string(),
            crate::config::DEFAULT_APPEARANCE.to_string(),
            crate::config::DEFAULT_DARK_APPEARANCE.to_string(),
        ]);
        assert_eq!(
            theme_options_from_keys(&names),
            vec![
                option(crate::config::DEFAULT_APPEARANCE),
                option(crate::config::DEFAULT_DARK_APPEARANCE),
                option("Catppuccin Mocha"),
            ]
        );
        // 空快照时只回退默认两项，保证下拉不为空。
        assert_eq!(
            theme_options_from_keys(&[]),
            vec![
                option(crate::config::DEFAULT_APPEARANCE),
                option(crate::config::DEFAULT_DARK_APPEARANCE),
            ]
        );
    }

    /// themes 目录选择是纯逻辑：exe 旁的目录存在时优先，否则回退仓库。
    #[test]
    fn themes_dir_prefers_shipped_directory_when_present() {
        let exe = Path::new("D:/FastPin/FastPin.exe");
        let repo = Path::new("D:/src/themes");
        let chosen = themes_dir_from(exe.parent(), true, repo);
        assert_eq!(chosen, PathBuf::from("D:/FastPin/themes"));
    }

    #[test]
    fn themes_dir_falls_back_to_repo_when_shipped_is_missing() {
        let exe = Path::new("D:/FastPin/FastPin.exe");
        let repo = Path::new("D:/src/themes");
        let chosen = themes_dir_from(exe.parent(), false, repo);
        assert_eq!(chosen.as_path(), repo);
    }

    #[test]
    fn themes_dir_falls_back_to_repo_without_exe_base() {
        let repo = Path::new("D:/src/themes");
        let chosen = themes_dir_from(None, true, repo);
        assert_eq!(chosen.as_path(), repo);
    }
}
