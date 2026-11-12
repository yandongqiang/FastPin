//! The settings window's pure model: the navigation catalogue, the row
//! registry the search box filters, the search itself, and per-page reset.
//!
//! Nothing here touches gpui or the filesystem, so the page/search/reset rules
//! stay unit-testable even though the window itself can only be checked by eye.

use crate::config::Settings;

/// One entry of the left navigation pane, in the order Citepix shows them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    General,
    Screenshot,
    Record,
    History,
    Output,
    Pin,
    Appearance,
    Hotkey,
    Translate,
    Resources,
    Data,
    About,
}

impl Page {
    pub const ALL: [Page; 12] = [
        Page::General,
        Page::Screenshot,
        Page::Record,
        Page::History,
        Page::Output,
        Page::Pin,
        Page::Appearance,
        Page::Hotkey,
        Page::Translate,
        Page::Resources,
        Page::Data,
        Page::About,
    ];

    /// Stable element-id suffix; also the key the tests use to prove the nav
    /// list has no duplicates.
    pub fn id(self) -> &'static str {
        match self {
            Page::General => "general",
            Page::Screenshot => "screenshot",
            Page::Record => "record",
            Page::History => "history",
            Page::Output => "output",
            Page::Pin => "pin",
            Page::Appearance => "appearance",
            Page::Hotkey => "hotkey",
            Page::Translate => "translate",
            Page::Resources => "resources",
            Page::Data => "data",
            Page::About => "about",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Page::General => "常规",
            Page::Screenshot => "截屏",
            Page::Record => "录屏",
            Page::History => "历史图片",
            Page::Output => "输出",
            Page::Pin => "贴图",
            Page::Appearance => "外观",
            Page::Hotkey => "快捷键",
            Page::Translate => "翻译",
            Page::Resources => "资源管理",
            Page::Data => "数据管理",
            Page::About => "关于",
        }
    }

    pub fn desc(self) -> &'static str {
        match self {
            Page::General => "启动与语言等基础行为。",
            Page::Screenshot => "截图内容、工具栏按钮与注释工具的默认值。",
            Page::Record => "录屏方式、编码与输出。",
            Page::History => "浏览、保留或删除截图历史。",
            Page::Output => "保存位置与导出的水印。",
            Page::Pin => "贴图的默认行为与编辑工具栏。",
            Page::Appearance => "界面主题与标注外观。",
            Page::Hotkey => "全局快捷键，在任何界面都可以触发。",
            Page::Translate => "翻译服务的接口、模型与目标语言。",
            Page::Resources => "本地识别模型的存放位置。",
            Page::Data => "数据目录、日志与存储占用。",
            Page::About => "版本信息与更新检查。",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Page::General => "settings/general.svg",
            Page::Screenshot => "settings/screenshot.svg",
            Page::Record => "settings/record.svg",
            Page::History => "settings/history.svg",
            Page::Output => "settings/output.svg",
            Page::Pin => "toolbar/pin.svg",
            Page::Appearance => "settings/appearance.svg",
            Page::Hotkey => "settings/hotkey.svg",
            Page::Translate => "toolbar/translate.svg",
            Page::Resources => "settings/resources.svg",
            Page::Data => "settings/data.svg",
            Page::About => "settings/info.svg",
        }
    }

    /// Whether this page owns fields in `config::Settings`. The reset button is
    /// only shown for pages where resetting something is possible.
    pub fn has_config(self) -> bool {
        matches!(
            self,
            Page::Screenshot
                | Page::Output
                | Page::Appearance
                | Page::Hotkey
                | Page::Translate
                | Page::Resources
        )
    }

    /// Pages whose backing features are absent from this build. They still
    /// exist in the navigation so the layout matches the reference, but they
    /// carry one disabled note instead of fake controls.
    pub fn unsupported(self) -> bool {
        matches!(self, Page::Record | Page::History)
    }
}

/// One searchable row. Mirrors the title/description rendered on the page so a
/// hit can be shown with its source page before the user jumps there.
pub struct RowMeta {
    pub page: Page,
    pub title: &'static str,
    pub desc: &'static str,
}

pub const ROWS: &[RowMeta] = &[
    // 常规
    RowMeta {
        page: Page::General,
        title: "开机启动",
        desc: "登录 Windows 后自动在托盘中运行 FastPin。",
    },
    RowMeta {
        page: Page::General,
        title: "界面语言",
        desc: "当前版本仅提供简体中文界面。",
    },
    // 截屏
    RowMeta {
        page: Page::Screenshot,
        title: "工具栏按钮",
        desc: "取消勾选的按钮不会出现在截图工具栏上，完成和取消始终显示。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "默认颜色",
        desc: "新画笔、形状和文字标注的颜色。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "线条粗细",
        desc: "画笔、形状和箭头的描边宽度。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "马赛克大小",
        desc: "马赛克涂抹的格子边长，单位是图像像素。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "截屏模式",
        desc: "当前版本固定为冻结屏幕后框选区域。",
    },
    // 录屏
    RowMeta {
        page: Page::Record,
        title: "录屏",
        desc: "当前版本暂不支持屏幕录制。",
    },
    // 历史图片
    RowMeta {
        page: Page::History,
        title: "历史图片",
        desc: "当前版本不保存截图历史，截图不会写入图片库。",
    },
    // 输出
    RowMeta {
        page: Page::Output,
        title: "保存位置",
        desc: "工具栏的保存按钮把 PNG 写到这里。",
    },
    RowMeta {
        page: Page::Output,
        title: "水印文字",
        desc: "导出 PNG 时烧录的文字，留空则仅当勾选时间或设置了图片时显示。",
    },
    RowMeta {
        page: Page::Output,
        title: "水印位置",
        desc: "水印在图片上的九宫格落点。",
    },
    RowMeta {
        page: Page::Output,
        title: "水印不透明度",
        desc: "10% 到 100%。",
    },
    RowMeta {
        page: Page::Output,
        title: "水印大小",
        desc: "水印文字或图片的缩放，0.5x 到 3x。",
    },
    RowMeta {
        page: Page::Output,
        title: "图片水印",
        desc: "优先于文字水印烧录，选择有效的图片文件才生效。",
    },
    RowMeta {
        page: Page::Output,
        title: "添加拍摄时间",
        desc: "在文字前附加 UTC 时间，格式 YYYY-MM-DD HH:MM:SS。",
    },
    RowMeta {
        page: Page::Output,
        title: "平铺水印",
        desc: "把文字水印重复铺满整张图片。",
    },
    // 贴图
    RowMeta {
        page: Page::Pin,
        title: "贴图工具栏按钮",
        desc: "贴图编辑沿用「截屏 → 工具栏按钮」的显隐设置。",
    },
    RowMeta {
        page: Page::Pin,
        title: "贴图默认行为",
        desc: "当前版本暂不支持配置贴图的默认缩放、透明度或关闭方式。",
    },
    // 外观
    RowMeta {
        page: Page::Appearance,
        title: "外观模式",
        desc: "切换界面的浅色或深色主题，立即生效并保存。",
    },
    // 快捷键
    RowMeta {
        page: Page::Hotkey,
        title: "区域截图",
        desc: "冻结屏幕后框选区域，进入标注工具栏。",
    },
    RowMeta {
        page: Page::Hotkey,
        title: "仅复制",
        desc: "框选后直接复制到剪贴板，不写文件。",
    },
    RowMeta {
        page: Page::Hotkey,
        title: "全屏截图",
        desc: "抓取当前显示器并保存 PNG。",
    },
    // 翻译
    RowMeta {
        page: Page::Translate,
        title: "接口地址",
        desc: "OpenAI 兼容接口，留空则翻译不可用。",
    },
    RowMeta {
        page: Page::Translate,
        title: "API Key",
        desc: "保存在 Windows 凭据管理器，不写入配置文件。",
    },
    RowMeta {
        page: Page::Translate,
        title: "模型",
        desc: "请求使用的模型名称。",
    },
    RowMeta {
        page: Page::Translate,
        title: "目标语言",
        desc: "翻译输出的语言。",
    },
    RowMeta {
        page: Page::Translate,
        title: "超时",
        desc: "秒，5 到 300。",
    },
    RowMeta {
        page: Page::Translate,
        title: "提示词",
        desc: "留空则使用内置的翻译提示词。",
    },
    // 资源管理
    RowMeta {
        page: Page::Resources,
        title: "识别引擎",
        desc: "本地识别，截图不会离开这台机器。",
    },
    RowMeta {
        page: Page::Resources,
        title: "模型目录",
        desc: "覆盖内置的 models\\v6 目录，留空使用默认位置。",
    },
    // 数据管理
    RowMeta {
        page: Page::Data,
        title: "数据目录",
        desc: "settings.json 所在位置。",
    },
    RowMeta {
        page: Page::Data,
        title: "日志目录",
        desc: "perf.log 等运行日志所在位置。",
    },
    RowMeta {
        page: Page::Data,
        title: "存储占用",
        desc: "数据目录与日志目录当前占用的空间。",
    },
    RowMeta {
        page: Page::Data,
        title: "清理日志",
        desc: "删除运行日志，不影响设置和截图。",
    },
    RowMeta {
        page: Page::Data,
        title: "历史图片",
        desc: "当前版本不保存截图历史，没有可清理的图片。",
    },
    // 关于
    RowMeta {
        page: Page::About,
        title: "版本",
        desc: "当前构建版本与构建类型。",
    },
    RowMeta {
        page: Page::About,
        title: "检查更新",
        desc: "从发布页获取最新版本。",
    },
];

pub fn rows_of(page: Page) -> impl Iterator<Item = &'static RowMeta> {
    ROWS.iter().filter(move |r| r.page == page)
}

fn matches(row: &RowMeta, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    row.title.to_lowercase().contains(&q) || row.desc.to_lowercase().contains(&q)
}

/// Cross-page search. An empty query matches every row; the window only calls
/// this with a non-empty query (an empty box shows the current page instead).
pub fn search(query: &str) -> Vec<&'static RowMeta> {
    let q = query.trim();
    ROWS.iter().filter(|r| matches(r, q)).collect()
}

/// Resets the fields the page owns. Returns whether anything was reset, so the
/// caller can say "本页没有可重置的配置" instead of silently doing nothing.
pub fn reset_page(settings: &mut Settings, page: Page) -> bool {
    let default = Settings::default();
    match page {
        Page::Screenshot => {
            settings.screenshot.hidden_toolbar_items =
                default.screenshot.hidden_toolbar_items.clone();
            settings.screenshot.annotation = default.screenshot.annotation.clone();
            true
        }
        Page::Output => {
            settings.screenshot.save_dir = default.screenshot.save_dir.clone();
            settings.screenshot.watermark = default.screenshot.watermark.clone();
            true
        }
        Page::Hotkey => {
            settings.hotkeys = default.hotkeys.clone();
            true
        }
        Page::Appearance => {
            settings.appearance = default.appearance.clone();
            true
        }
        Page::Translate => {
            settings.translation = default.translation.clone();
            true
        }
        Page::Resources => {
            settings.ocr = default.ocr.clone();
            true
        }
        _ => false,
    }
}

pub fn reset_all(settings: &mut Settings) {
    *settings = Settings::default();
}

/// Human-readable byte count for the storage row. Kept tiny and pure so the
/// Data page does not have to format numbers inline.
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        return format!("{bytes} B");
    }
    if b < KB * KB {
        return format!("{:.1} KB", b / KB);
    }
    if b < KB * KB * KB {
        return format!("{:.1} MB", b / (KB * KB));
    }
    format!("{:.1} GB", b / (KB * KB * KB))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_non_default() -> Settings {
        let mut s = Settings::default();
        s.ocr.models_dir = Some("D:/models/custom".into());
        s.translation.base_url = "https://api.example.com/v1".into();
        s.translation.model = "qwen-plus".into();
        s.translation.target_language = "ja".into();
        s.translation.prompt = "be terse".into();
        s.translation.timeout_seconds = 111;
        s.screenshot.hidden_toolbar_items = vec!["text".into(), "redo".into()];
        s.screenshot.save_dir = Some("D:/shots".into());
        s.screenshot.annotation.stroke_width = 9.0;
        s.screenshot.annotation.stroke_color = "#123456".into();
        s.screenshot.annotation.mosaic_block = 32;
        s.screenshot.watermark.text = "机密".into();
        s.screenshot.watermark.opacity = 75;
        s.screenshot.watermark.position = "topLeft".into();
        s.screenshot.watermark.scale = 2.0;
        s.screenshot.watermark.tiled = true;
        s.screenshot.watermark.timestamp = true;
        s.screenshot.watermark.image_path = Some("D:/wm.png".into());
        s.hotkeys.region = "Ctrl+Shift+1".into();
        s.hotkeys.copy_only = "Ctrl+Shift+2".into();
        s.hotkeys.fullscreen = "Ctrl+Shift+3".into();
        s.appearance = crate::config::APPEARANCE_DARK.into();
        s
    }

    #[test]
    fn every_page_has_a_title_description_and_icon() {
        let mut ids = Vec::new();
        for page in Page::ALL {
            assert!(!page.title().is_empty(), "{page:?}");
            assert!(!page.desc().is_empty(), "{page:?}");
            assert!(page.icon().ends_with(".svg"), "{page:?}");
            assert!(!ids.contains(&page.id()), "duplicate nav id for {page:?}");
            ids.push(page.id());
        }
        assert_eq!(Page::ALL.len(), 12);
    }

    /// A page with no rows would render as an unexplained blank pane.
    #[test]
    fn every_page_has_at_least_one_searchable_row() {
        for page in Page::ALL {
            let rows: Vec<_> = rows_of(page).collect();
            assert!(!rows.is_empty(), "{page:?} has no rows");
            // Unsupported pages exist to say so; their first row must name the
            // page so the disabled note is never generic.
            if page.unsupported() {
                assert!(
                    rows.iter().any(|r| r.title.contains(page.title())),
                    "{page:?} lacks a title-matching disabled row"
                );
            }
        }
    }

    #[test]
    fn empty_query_matches_every_row() {
        assert_eq!(search("").len(), ROWS.len());
        assert_eq!(search("   ").len(), ROWS.len());
    }

    #[test]
    fn search_matches_titles_descriptions_and_ignores_case() {
        let by_title = search("水印");
        assert!(by_title.iter().any(|r| r.title == "水印文字"));
        // "OpenAI" only appears in the description of 接口地址.
        let by_desc = search("openai");
        assert!(by_desc
            .iter()
            .any(|r| r.page == Page::Translate && r.title == "接口地址"));
        // Latin matching is case-insensitive.
        assert_eq!(search("OPENAI").len(), by_desc.len());
    }

    #[test]
    fn search_spans_pages_and_reports_the_source_page() {
        let hits = search("工具栏");
        let pages: Vec<Page> = hits.iter().map(|r| r.page).collect();
        assert!(pages.contains(&Page::Screenshot), "{pages:?}");
        assert!(pages.contains(&Page::Pin), "{pages:?}");
        // Every hit carries a page that can be navigated to.
        assert!(hits.iter().all(|r| Page::ALL.contains(&r.page)));
    }

    #[test]
    fn search_with_no_hits_is_empty() {
        assert!(search("zzz-不存在的配置-zzz").is_empty());
    }

    #[test]
    fn reset_page_only_touches_fields_the_page_owns() {
        let owned: fn(Page, &Settings) -> bool = |page, s| match page {
            Page::Screenshot => {
                s.screenshot.hidden_toolbar_items.is_empty()
                    && s.screenshot.annotation.stroke_color == "#FA3C3C"
            }
            Page::Output => {
                s.screenshot.save_dir.is_none() && s.screenshot.watermark.text.is_empty()
            }
            Page::Hotkey => s.hotkeys.region == "Ctrl+Alt+A",
            Page::Appearance => s.appearance == "light",
            Page::Translate => s.translation.model == "gpt-4o-mini",
            Page::Resources => s.ocr.models_dir.is_none(),
            _ => false,
        };

        for page in Page::ALL {
            let mut s = all_non_default();
            let changed = reset_page(&mut s, page);
            if page.has_config() {
                assert!(changed, "{page:?} claims config but reset did nothing");
                assert!(owned(page, &s), "{page:?} did not reset its own fields");
                // Everything the page does not own stays exactly as it was.
                match page {
                    Page::Screenshot => {
                        assert_eq!(s.screenshot.save_dir.as_deref(), Some("D:/shots".as_ref()));
                        assert_eq!(s.translation.timeout_seconds, 111);
                        assert_eq!(s.ocr.models_dir.as_deref(), Some("D:/models/custom".as_ref()));
                    }
                    Page::Output => {
                        assert_eq!(s.screenshot.hidden_toolbar_items, ["text", "redo"]);
                        assert_eq!(s.translation.model, "qwen-plus");
                        assert_eq!(s.hotkeys.region, "Ctrl+Shift+1");
                    }
                    Page::Hotkey => {
                        assert_eq!(s.translation.model, "qwen-plus");
                        assert_eq!(s.screenshot.watermark.text, "机密");
                    }
                    Page::Appearance => {
                        assert_eq!(s.translation.model, "qwen-plus");
                        assert_eq!(s.hotkeys.fullscreen, "Ctrl+Shift+3");
                        assert_eq!(s.screenshot.annotation.mosaic_block, 32);
                    }
                    Page::Translate => {
                        assert_eq!(s.hotkeys.fullscreen, "Ctrl+Shift+3");
                        assert_eq!(s.screenshot.annotation.mosaic_block, 32);
                    }
                    Page::Resources => {
                        assert_eq!(s.translation.base_url, "https://api.example.com/v1");
                        assert_eq!(s.screenshot.watermark.position, "topLeft");
                    }
                    _ => {}
                }
            } else {
                assert!(!changed, "{page:?} has no config but reported a reset");
                assert_eq!(
                    serde_json::to_value(&s).expect("serializes"),
                    serde_json::to_value(all_non_default()).expect("serializes"),
                    "{page:?} must not change any field"
                );
            }
        }
    }

    #[test]
    fn reset_all_restores_every_default() {
        let mut s = all_non_default();
        reset_all(&mut s);
        assert_eq!(
            serde_json::to_value(&s).expect("serializes"),
            serde_json::to_value(Settings::default()).expect("serializes")
        );
    }

    #[test]
    fn human_size_switches_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
