//! The settings window's pure model: the navigation catalogue, the row
//! registry the search box filters, the search itself, and per-page reset.
//!
//! Nothing here touches gpui or the filesystem, so the page/search/reset rules
//! stay unit-testable even though the window itself can only be checked by eye.

use std::sync::OnceLock;

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
            Page::Screenshot => "截图",
            Page::Record => "录屏",
            Page::History => "历史图片",
            Page::Output => "输出",
            Page::Pin => "贴图",
            Page::Appearance => "外观",
            Page::Hotkey => "快捷键",
            Page::Translate => "翻译",
            Page::Resources => "文本识别",
            Page::Data => "数据管理",
            Page::About => "关于",
        }
    }

    pub fn desc(self) -> &'static str {
        match self {
            Page::General => "启动与语言等基础行为。",
            Page::Screenshot => "调整截屏功能的相关设置。",
            Page::Record => "录屏方式、编码与输出。",
            Page::History => "查看、复制或删除已保存的截图历史。",
            Page::Output => "保存位置与导出的水印。",
            Page::Pin => "贴图的默认行为与编辑工具栏。",
            Page::Appearance => "界面主题与标注外观。",
            Page::Hotkey => "全局快捷键，在任何界面都可以触发。",
            Page::Translate => "翻译服务的接口、模型与目标语言。",
            Page::Resources => "本地 OCR 识别引擎与模型目录。",
            Page::Data => "数据目录、日志、存储占用与截图历史。",
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
            Page::General
                | Page::Screenshot
                | Page::Pin
                | Page::Output
                | Page::Appearance
                | Page::Hotkey
                | Page::Translate
                | Page::Resources
                | Page::Data
        )
    }

    /// Pages whose backing features are absent from this build. They still
    /// exist in the navigation so the layout matches the reference, but they
    /// carry one disabled note instead of fake controls.
    pub fn unsupported(self) -> bool {
        matches!(self, Page::Record)
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
        title: "以管理员启动",
        desc: "手动启动与开机启动均使用管理员权限；重启程序后生效。",
    },
    RowMeta {
        page: Page::General,
        title: "界面语言",
        desc: "当前版本仅提供简体中文界面。",
    },
    // 截图
    RowMeta {
        page: Page::Screenshot,
        title: "截屏模式",
        desc: "仅影响长截图是否保持持续捕获会话；WGC/DXGI 项当前均走自动选择。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "捕获鼠标光标",
        desc: "在截屏结果中显示鼠标光标。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "未选区时显示辅助线",
        desc: "开始截屏且尚未创建选区时自动显示辅助线；选区完成后自动隐藏。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "长截图自动开始",
        desc: "选择进入长截图工具时的开始策略。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "左键双击行为",
        desc: "在截屏选区完成后双击左键时执行的操作。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "中键单击行为",
        desc: "在截屏选区完成后单击中键时执行的操作。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "点击选区外行为",
        desc: "选择点击选区外时扩展现有选区，或重新创建新选区。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "元素检测",
        desc: "选择框选时吸附到窗口与元素、仅窗口，或完全不检测。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "记录截屏历史",
        desc: "选择自动记录到历史图片的截屏类型；成功复制或保存后才会记录。",
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
        title: "自动滚动间隔",
        desc: "长截图自动滚动时两次滚动之间的等待时间，间隔越短滚动越快。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "自动滚动步长",
        desc: "每次滚动发送的原生滚轮格数，格数越大一步跨过的页面越多。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "自定义工具栏",
        desc: "选择截屏工具栏上显示的按钮；完成与取消始终显示。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "工具栏位置",
        desc: "选择跟随选区，或固定在选区所在屏幕的顶部或底部。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "参数面板位置",
        desc: "选择跟随选区，或固定在选区所在屏幕的左侧或右侧。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "放大镜",
        desc: "在鼠标附近显示像素放大预览。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "信息栏",
        desc: "在选区附近显示坐标、尺寸与取色值。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "快捷键提示",
        desc: "每次开始截屏时显示当前阶段可用的快捷键提示。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "选区边框线类型",
        desc: "选择选区边框线使用虚线或实线。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "选区边框线粗细",
        desc: "设置选区边框线粗细，范围为 1~10 像素，边框向选区外侧扩展。",
    },
    RowMeta {
        page: Page::Screenshot,
        title: "选区边框线颜色",
        desc: "设置截屏选区边框线的颜色。",
    },
    // 录屏
    RowMeta {
        page: Page::Record,
        title: "录屏",
        desc: "当前版本暂不支持屏幕录制。",
    },
    // 输出
    RowMeta {
        page: Page::Output,
        title: "保存位置",
        desc: "工具栏的保存按钮把导出的图片写到这里。",
    },
    RowMeta {
        page: Page::Output,
        title: "图片格式",
        desc: "保存和导出使用的编码格式：PNG、JPG 或 WebP。",
    },
    RowMeta {
        page: Page::Output,
        title: "图片质量",
        desc: "JPG / WebP 的编码质量，PNG 不受影响。",
    },
    RowMeta {
        page: Page::Output,
        title: "文件名称",
        desc: "文件名模板，支持 {date}、{time} 和 {datetime} 占位符。",
    },
    RowMeta {
        page: Page::Output,
        title: "自动保存",
        desc: "完成截图时自动把图片写入保存位置；关闭后「完成」不写文件。",
    },
    RowMeta {
        page: Page::Output,
        title: "自动复制",
        desc: "完成截图时自动把图片复制到剪贴板；与自动保存相互独立。",
    },
    RowMeta {
        page: Page::Output,
        title: "水印文字",
        desc: "导出时烧录的文字，留空则仅当勾选时间或设置了图片时显示。",
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
    RowMeta {
        page: Page::Output,
        title: "图片切图",
        desc: "把一张图片切成等分或精确的网格，逐块导出。",
    },
    RowMeta {
        page: Page::Output,
        title: "拼图工具",
        desc: "把多张图片排成版式，导出 PNG 或多页 PDF。",
    },
    // 输出 → 拼图默认设置
    RowMeta {
        page: Page::Output,
        title: "拼图方向",
        desc: "多张图片的排列方向：垂直、水平或网格。",
    },
    RowMeta {
        page: Page::Output,
        title: "尺寸模式",
        desc: "图片按原始尺寸排布，或统一缩放到相同尺寸。",
    },
    RowMeta {
        page: Page::Output,
        title: "拼图间距",
        desc: "图片之间的间隔，单位像素，0 到 100。",
    },
    RowMeta {
        page: Page::Output,
        title: "拼图边框",
        desc: "整张拼图外围的边框宽度，单位像素，0 到 100。",
    },
    RowMeta {
        page: Page::Output,
        title: "网格行数",
        desc: "网格方向布局的行数，1 到 10。",
    },
    RowMeta {
        page: Page::Output,
        title: "网格列数",
        desc: "网格方向布局的列数，1 到 10。",
    },
    RowMeta {
        page: Page::Output,
        title: "背景颜色",
        desc: "拼图背景色，支持 #RRGGBB 或 #RRGGBBAA。",
    },
    // 输出 → PDF 默认设置
    RowMeta {
        page: Page::Output,
        title: "PDF 拼接方式",
        desc: "按页自动拼接，或使用固定模板拼版。",
    },
    RowMeta {
        page: Page::Output,
        title: "PDF 拼版模板",
        desc: "模板每页容纳的图片数量：2、3、4、6 或 9 张。",
    },
    RowMeta {
        page: Page::Output,
        title: "PDF 分页模式",
        desc: "合并为一个 PDF，或每页单独导出一个文件。",
    },
    RowMeta {
        page: Page::Output,
        title: "纸张大小",
        desc: "A3、A4、A5、B5、Letter、Legal、自定义或图片尺寸。",
    },
    RowMeta {
        page: Page::Output,
        title: "页面方向",
        desc: "PDF 页面使用纵向还是横向。",
    },
    RowMeta {
        page: Page::Output,
        title: "PDF 图片间距",
        desc: "同一页图片之间的间距，单位毫米，不小于 0。",
    },
    RowMeta {
        page: Page::Output,
        title: "自定义纸张宽度",
        desc: "纸张大小选择自定义时的页宽，单位毫米，大于 0。",
    },
    RowMeta {
        page: Page::Output,
        title: "自定义纸张高度",
        desc: "纸张大小选择自定义时的页高，单位毫米，大于 0。",
    },
    RowMeta {
        page: Page::Output,
        title: "上边距",
        desc: "PDF 页面上边缘留白，单位毫米，不小于 0。",
    },
    RowMeta {
        page: Page::Output,
        title: "下边距",
        desc: "PDF 页面下边缘留白，单位毫米，不小于 0。",
    },
    RowMeta {
        page: Page::Output,
        title: "左边距",
        desc: "PDF 页面左边缘留白，单位毫米，不小于 0。",
    },
    RowMeta {
        page: Page::Output,
        title: "右边距",
        desc: "PDF 页面右边缘留白，单位毫米，不小于 0。",
    },
    RowMeta {
        page: Page::Output,
        title: "PDF 图片质量",
        desc: "导出 JPG / WebP 页面时的编码质量，1 到 100。",
    },
    // 贴图
    RowMeta {
        page: Page::Pin,
        title: "自定义工具栏",
        desc: "贴图编辑沿用「截屏 → 工具栏按钮」的显隐设置。",
    },
    RowMeta {
        page: Page::Pin,
        title: "贴图默认行为",
        desc: "贴图默认缩放锚点、关闭确认与关闭历史可配置；透明度暂不支持。",
    },
    // 外观
    RowMeta {
        page: Page::Appearance,
        title: "外观模式",
        desc: "选择界面主题（gpui-kit 内置主题），立即生效并保存。",
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
    // 翻译（只用用户自己的 API）
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
    RowMeta {
        page: Page::Translate,
        title: "明文 HTTP 警告",
        desc: "http:// 接口携带 API Key 时需两次点击确认明文传输风险。",
    },
    // 文本识别
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
        title: "记录截图历史",
        desc: "保存截图时自动留存一份副本到历史目录。",
    },
    RowMeta {
        page: Page::Data,
        title: "保留期限",
        desc: "超过该天数的历史记录会在启动时、保存后以及更改后立即清理已有记录；选择“永久”等同于开启“始终保留”。",
    },
    RowMeta {
        page: Page::Data,
        title: "数量上限",
        desc: "最多保留的记录条数，超出后删除最旧的。",
    },
    RowMeta {
        page: Page::Data,
        title: "占用上限",
        desc: "历史目录的最大体积，超出后删除最旧的记录。",
    },
    RowMeta {
        page: Page::Data,
        title: "始终保留",
        desc: "开启后忽略保留期限，只按数量与体积上限清理。",
    },
    RowMeta {
        page: Page::Data,
        title: "历史目录",
        desc: "历史图片是保存截图的独立副本，删除记录不影响原截图。",
    },
    RowMeta {
        page: Page::Data,
        title: "浏览历史",
        desc: "打开历史窗口，查看、复制或删除以往截图。",
    },
    RowMeta {
        page: Page::Data,
        title: "清空历史",
        desc: "删除所有历史记录；此操作不可撤销。",
    },
    RowMeta {
        page: Page::History,
        title: "历史图片",
        desc: "打开独立历史窗口，查看、复制或删除已保存的截图。",
    },
    // 关于
    RowMeta {
        page: Page::About,
        title: "FastPin",
        desc: "轻量截图与文字识别工具。",
    },
    RowMeta {
        page: Page::About,
        title: "检查更新",
        desc: "从发布页获取最新版本。",
    },
    RowMeta {
        page: Page::About,
        title: "应用内下载",
        desc: "流式下载官方安装包到下载目录，显示进度，不经浏览器。",
    },
];

pub fn rows_of(page: Page) -> impl Iterator<Item = &'static RowMeta> {
    ROWS.iter().filter(move |r| r.page == page)
}

/// Lowercased copies of every row's title/desc, built once. The settings window
/// searches on every frame; without this it would re-lower every row (allocating
/// two `String`s each time) on every search and again for the result count.
fn lowered_rows() -> &'static [(String, String)] {
    static LOWERED: OnceLock<Vec<(String, String)>> = OnceLock::new();
    LOWERED.get_or_init(|| {
        ROWS.iter()
            .map(|r| (r.title.to_lowercase(), r.desc.to_lowercase()))
            .collect()
    })
}

/// Cross-page search. An empty query matches every row; the window only calls
/// this with a non-empty query (an empty box shows the current page instead).
/// The query is lower-cased once and reused; row text comes from the cache above.
pub fn search(query: &str) -> Vec<&'static RowMeta> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return ROWS.iter().collect();
    }
    ROWS.iter()
        .zip(lowered_rows())
        .filter(|(_, (title, desc))| title.contains(&q) || desc.contains(&q))
        .map(|(row, _)| row)
        .collect()
}

/// Resets the fields the page owns. Returns whether anything was reset, so the
/// caller can say "本页没有可重置的配置" instead of silently doing nothing.
pub fn reset_page(settings: &mut Settings, page: Page) -> bool {
    let default = Settings::default();
    match page {
        Page::General => {
            // Autostart is registry / Task Scheduler state, not settings.json;
            // only this config flag is reset here (the view handles autostart).
            settings.run_as_admin = default.run_as_admin;
            true
        }
        Page::Screenshot => {
            settings.screenshot.hidden_toolbar_items =
                default.screenshot.hidden_toolbar_items.clone();
            settings.screenshot.annotation = default.screenshot.annotation.clone();
            settings.screenshot.smart_selection = default.screenshot.smart_selection;
            settings.screenshot.auto_scroll = default.screenshot.auto_scroll.clone();
            // 截屏页新增的配置，逐项回到默认，避免重置按钮留下半页旧值。
            settings.screenshot.capture_mode = default.screenshot.capture_mode;
            settings.screenshot.show_cursor = default.screenshot.show_cursor;
            settings.screenshot.show_guides = default.screenshot.show_guides;
            settings.screenshot.long_capture_auto_start =
                default.screenshot.long_capture_auto_start;
            settings.screenshot.left_double_click = default.screenshot.left_double_click;
            settings.screenshot.middle_click = default.screenshot.middle_click;
            settings.screenshot.click_outside = default.screenshot.click_outside;
            settings.screenshot.record_history_sources =
                default.screenshot.record_history_sources.clone();
            settings.screenshot.toolbar_position = default.screenshot.toolbar_position;
            settings.screenshot.panel_position = default.screenshot.panel_position;
            settings.screenshot.show_magnifier = default.screenshot.show_magnifier;
            settings.screenshot.show_info_bar = default.screenshot.show_info_bar;
            settings.screenshot.show_shortcut_hints = default.screenshot.show_shortcut_hints;
            settings.screenshot.selection_border_style = default.screenshot.selection_border_style;
            settings.screenshot.selection_border_width = default.screenshot.selection_border_width;
            settings.screenshot.selection_border_color =
                default.screenshot.selection_border_color.clone();
            true
        }
        Page::Pin => {
            settings.pin = default.pin.clone();
            true
        }
        Page::Output => {
            settings.screenshot.save_dir = default.screenshot.save_dir.clone();
            settings.screenshot.watermark = default.screenshot.watermark.clone();
            settings.screenshot.export = default.screenshot.export.clone();
            // 本页现在也承载拼图与 PDF 的默认设置，一并重置。
            // 图片切图不在这页（由切图窗口单独管理），不能顺手重置。
            settings.screenshot.collage = default.screenshot.collage.clone();
            settings.screenshot.pdf = default.screenshot.pdf.clone();
            true
        }
        // History settings live on the Data page now; resetting that page must
        // not silently skip the history fields it shows.
        Page::Data => {
            settings.history = default.history.clone();
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

/// 把 `src` 中 `page` 拥有的字段覆盖到 `dst`，其余字段保持不动。字段范围
/// 必须与 `reset_page` 完全一致。
///
/// 页重置时先把「重置前」的快照按页写回基线：重置后的默认值与基线产生差异，
/// `settings_persist_merge` 的 `keep_edited` 就会采用快照；而其它页 / 其它窗口
/// 共享的字段仍保留 latest，不会被开窗快照整份回滚。
pub fn copy_page_fields(dst: &mut Settings, src: &Settings, page: Page) {
    match page {
        Page::General => dst.run_as_admin = src.run_as_admin,
        Page::Screenshot => {
            let d = &mut dst.screenshot;
            let s = &src.screenshot;
            d.hidden_toolbar_items = s.hidden_toolbar_items.clone();
            d.annotation = s.annotation.clone();
            d.smart_selection = s.smart_selection;
            d.auto_scroll = s.auto_scroll.clone();
            d.capture_mode = s.capture_mode;
            d.show_cursor = s.show_cursor;
            d.show_guides = s.show_guides;
            d.long_capture_auto_start = s.long_capture_auto_start;
            d.left_double_click = s.left_double_click;
            d.middle_click = s.middle_click;
            d.click_outside = s.click_outside;
            d.record_history_sources = s.record_history_sources.clone();
            d.toolbar_position = s.toolbar_position;
            d.panel_position = s.panel_position;
            d.show_magnifier = s.show_magnifier;
            d.show_info_bar = s.show_info_bar;
            d.show_shortcut_hints = s.show_shortcut_hints;
            d.selection_border_style = s.selection_border_style;
            d.selection_border_width = s.selection_border_width;
            d.selection_border_color = s.selection_border_color.clone();
        }
        Page::Pin => dst.pin = src.pin.clone(),
        Page::Output => {
            let d = &mut dst.screenshot;
            let s = &src.screenshot;
            d.save_dir = s.save_dir.clone();
            d.watermark = s.watermark.clone();
            d.export = s.export.clone();
            d.collage = s.collage.clone();
            d.pdf = s.pdf.clone();
        }
        Page::Data => dst.history = src.history.clone(),
        Page::Hotkey => dst.hotkeys = src.hotkeys.clone(),
        Page::Appearance => dst.appearance = src.appearance.clone(),
        Page::Translate => dst.translation = src.translation.clone(),
        Page::Resources => dst.ocr = src.ocr.clone(),
        _ => {}
    }
}

/// 单页重置的强制落盘：把 `page` 拥有的字段无条件从 `snapshot`（重置后的值）
/// 覆盖到 `merged`。字段范围与 `reset_page` / `copy_page_fields` 完全一致。
///
/// 重置前的值可能恰好等于默认值（例如 collage.spacing 开窗时就是 0），此时
/// `keep_edited` 看不出差异，会误判成「没改过」而保留其它窗口写入的旧值。
/// 常规合并完成后调用本函数只强制本页字段，既让重置真正写盘，又不回滚
/// 其它页 / 其它窗口的共享值。
pub fn force_page_fields(merged: &mut Settings, snapshot: &Settings, page: Page) {
    copy_page_fields(merged, snapshot, page);
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
        let mut s = Settings {
            run_as_admin: true,
            ..Settings::default()
        };
        s.ocr.models_dir = Some("D:/models/custom".into());
        s.translation.base_url = "https://api.example.com/v1".into();
        s.translation.model = "qwen-plus".into();
        s.translation.target_language = "ja".into();
        s.translation.prompt = "be terse".into();
        s.translation.timeout_seconds = 111;
        s.translation.allow_insecure_http = true;
        s.screenshot.hidden_toolbar_items = vec!["text".into(), "redo".into()];
        s.screenshot.save_dir = Some("D:/shots".into());
        s.screenshot.smart_selection = crate::config::SmartSelectionMode::Disabled;
        // 截屏页新增字段：全部偏离各自默认值，避免重置测试出现假阳性。
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
        s.screenshot.selection_border_style = crate::config::SelectionBorderStyle::Solid;
        s.screenshot.selection_border_width = 8;
        s.screenshot.selection_border_color = "#123456".into();
        // 贴图页字段也要全部偏离默认值，否则重置测试是假阳性。
        s.pin.show_magnifier = true;
        s.pin.topmost = false;
        s.pin.shadow = false;
        s.pin.titlebar = true;
        s.pin.lock = crate::config::PinLock::Size;
        s.pin.double_click_close = false;
        s.pin.thumb_toggle = false;
        s.pin.wheel_step = 20;
        s.pin.close_confirm = false;
        s.pin.close_history_limit = 100;
        s.pin.universal_text = false;
        s.pin.universal_file = false;
        s.pin.universal_image = false;
        s.pin.auto_recognize = true;
        s.pin.auto_copy = true;
        s.pin.group_switch = crate::config::PinGroupSwitch::HideOthers;
        s.pin.auto_join_group = crate::config::PinAutoJoin::Current;
        s.pin.toolbar_position = crate::config::ToolbarPosition::Bottom;
        s.pin.panel_position = crate::config::PanelPosition::Right;
        s.pin.zoom_mode = crate::config::PinZoomMode::Center;
        s.screenshot.export.auto_save = true;
        s.screenshot.export.auto_copy = false;
        s.screenshot.export.format = crate::config::ImageFormat::Jpeg;
        s.screenshot.export.quality = 60;
        s.screenshot.export.filename_template = "shot_{date}".into();
        s.screenshot.annotation.stroke_width = 9.0;
        s.screenshot.annotation.stroke_color = "#123456".into();
        s.screenshot.annotation.mosaic_block = 32;
        s.screenshot.auto_scroll.interval_ms = 1000;
        s.screenshot.auto_scroll.notches = 8;
        s.screenshot.watermark.text = "机密".into();
        s.screenshot.watermark.opacity = 75;
        s.screenshot.watermark.position = "topLeft".into();
        s.screenshot.watermark.scale = 2.0;
        s.screenshot.watermark.tiled = true;
        s.screenshot.watermark.timestamp = true;
        s.screenshot.watermark.image_path = Some("D:/wm.png".into());
        // 拼图默认值：全部偏离 CollageSettings::default()。
        s.screenshot.collage.direction = crate::config::Direction::Horizontal;
        s.screenshot.collage.sizing = crate::config::Sizing::Original;
        s.screenshot.collage.border = 5;
        s.screenshot.collage.spacing = 12;
        s.screenshot.collage.background_rgba = [1, 2, 3, 255];
        s.screenshot.collage.rows = 3;
        s.screenshot.collage.columns = 4;
        // PDF 默认值：全部偏离 PdfSettings::default()。
        s.screenshot.pdf.stitching_method = crate::config::StitchingMethod::TemplateBased;
        s.screenshot.pdf.template_type = crate::config::TemplateType::FourImages;
        s.screenshot.pdf.spacing_mm = 7.5;
        s.screenshot.pdf.paper_size = crate::config::PaperSize::Letter;
        s.screenshot.pdf.orientation = crate::config::Orientation::Landscape;
        s.screenshot.pdf.custom_width_mm = 123.0;
        s.screenshot.pdf.custom_height_mm = 234.0;
        s.screenshot.pdf.margin_top_mm = 11.0;
        s.screenshot.pdf.margin_bottom_mm = 12.0;
        s.screenshot.pdf.margin_left_mm = 13.0;
        s.screenshot.pdf.margin_right_mm = 14.0;
        s.screenshot.pdf.export_mode = crate::config::ExportMode::PerPage;
        s.screenshot.pdf.image_quality = 77;
        // 切图默认值：偏离 SplitSettings::default()。
        s.screenshot.split.count = 7;
        s.screenshot.split.overlap_width = 6;
        s.hotkeys.region = "Ctrl+Shift+1".into();
        s.hotkeys.copy_only = "Ctrl+Shift+2".into();
        s.hotkeys.fullscreen = "Ctrl+Shift+3".into();
        s.appearance = "Catppuccin Latte".into();
        s.history.enabled = false;
        s.history.dir = Some("D:/history".into());
        s.history.retention_days = 30;
        s.history.limit = 300;
        s.history.max_disk_mib = 512;
        s.history.keep_permanently = true;
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
    fn history_settings_are_owned_by_the_data_page() {
        let titles: Vec<_> = rows_of(Page::Data).map(|r| r.title).collect();
        for title in [
            "记录截图历史",
            "保留期限",
            "数量上限",
            "占用上限",
            "始终保留",
            "历史目录",
            "浏览历史",
            "清空历史",
        ] {
            assert!(titles.contains(&title), "Data page lost '{title}'");
        }
        assert!(Page::Data.has_config());
        // A search for a history row must land on the page that renders it.
        assert!(search("保留期限").iter().all(|r| r.page == Page::Data));
    }

    /// 截图页的 15 项配置都要能被搜索到，并跳回「截图」页。
    #[test]
    fn screenshot_rows_are_searchable() {
        for title in [
            "截屏模式",
            "捕获鼠标光标",
            "未选区时显示辅助线",
            "长截图自动开始",
            "左键双击行为",
            "中键单击行为",
            "点击选区外行为",
            "元素检测",
            "记录截屏历史",
            "自定义工具栏",
            "工具栏位置",
            "参数面板位置",
            "放大镜",
            "信息栏",
            "快捷键提示",
        ] {
            assert!(
                search(title)
                    .iter()
                    .any(|r| r.title == title && r.page == Page::Screenshot),
                "search lost '{title}' on Page::Screenshot"
            );
        }
        assert_eq!(Page::Screenshot.title(), "截图");
        assert_eq!(Page::Screenshot.desc(), "调整截屏功能的相关设置。");
    }

    #[test]
    fn every_rendered_tool_launcher_and_scroll_row_is_searchable() {
        for (query, page) in [
            ("自动滚动间隔", Page::Screenshot),
            ("自动滚动步长", Page::Screenshot),
            ("图片切图", Page::Output),
            ("拼图工具", Page::Output),
        ] {
            assert!(
                search(query)
                    .iter()
                    .any(|r| r.title == query && r.page == page),
                "search lost '{query}' on {page:?}"
            );
        }
    }

    /// 输出页新增的拼图 / PDF 默认设置必须能被搜索到，并能跳回来源页面。
    #[test]
    fn collage_and_pdf_rows_are_searchable_on_output_page() {
        for title in [
            "拼图方向",
            "尺寸模式",
            "拼图间距",
            "拼图边框",
            "网格行数",
            "网格列数",
            "背景颜色",
            "PDF 拼接方式",
            "PDF 拼版模板",
            "PDF 分页模式",
            "纸张大小",
            "页面方向",
            "PDF 图片间距",
            "自定义纸张宽度",
            "自定义纸张高度",
            "上边距",
            "下边距",
            "左边距",
            "右边距",
            "PDF 图片质量",
        ] {
            assert!(
                search(title)
                    .iter()
                    .any(|r| r.title == title && r.page == Page::Output),
                "search lost '{title}' on Page::Output"
            );
        }
    }

    /// 翻译页只保留 API 配置：这些行都要能被搜索到，并跳回「翻译」页。
    #[test]
    fn api_translate_rows_are_searchable_on_translate_page() {
        for title in [
            "接口地址",
            "API Key",
            "模型",
            "目标语言",
            "超时",
            "提示词",
            "明文 HTTP 警告",
        ] {
            assert!(
                search(title)
                    .iter()
                    .any(|r| r.title == title && r.page == Page::Translate),
                "search lost '{title}' on Page::Translate"
            );
        }
        assert_eq!(Page::Translate.title(), "翻译");
        assert_eq!(Page::Translate.desc(), "翻译服务的接口、模型与目标语言。");
        // 本地翻译的旧搜索词必须彻底消失，避免搜到不存在的控件。
        for title in ["默认翻译服务", "我的服务", "安装状态", "本地推理线程数"]
        {
            assert!(
                search(title).iter().all(|r| r.page != Page::Translate),
                "stale local-translate row '{title}' on Page::Translate"
            );
        }
    }

    /// 自动保存 / 自动复制是两个独立开关，两行都要能被搜索到。
    #[test]
    fn auto_export_rows_are_searchable_on_output_page() {
        for title in ["自动保存", "自动复制"] {
            assert!(
                search(title)
                    .iter()
                    .any(|r| r.title == title && r.page == Page::Output),
                "search lost '{title}' on Page::Output"
            );
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

    /// 外观行的描述指向 gpui-kit 内置主题，搜索与重置测试都要覆盖新文案。
    #[test]
    fn appearance_row_search_and_reset_use_the_new_wording() {
        let hits = search("gpui-kit 内置主题");
        assert!(hits
            .iter()
            .any(|r| r.page == Page::Appearance && r.title == "外观模式"));

        let mut s = all_non_default();
        assert_eq!(s.appearance, "Catppuccin Latte");
        assert!(reset_page(&mut s, Page::Appearance));
        assert_eq!(s.appearance, crate::config::DEFAULT_APPEARANCE);
    }

    /// The pre-lowered cache must give exactly the same answers as lowering
    /// each row on the fly, otherwise repeated searches could silently diverge.
    #[test]
    fn search_lowered_cache_agrees_with_naive_lowering() {
        for query in ["水印", "OPENAI", "openai", "工具", "zzz-不存在"] {
            let fast: Vec<_> = search(query).iter().map(|r| r.title).collect();
            let q = query.trim().to_lowercase();
            let naive: Vec<_> = ROWS
                .iter()
                .filter(|r| {
                    r.title.to_lowercase().contains(&q) || r.desc.to_lowercase().contains(&q)
                })
                .map(|r| r.title)
                .collect();
            assert_eq!(fast, naive, "query {query}");
        }
        // The cache is built only once and never changes length.
        assert_eq!(lowered_rows().len(), ROWS.len());
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

    /// 每页重置后的期望配置：从原配置出发，只把该页拥有的字段换成默认值。
    /// 顶层字段范围与 reset_page 一致；screenshot 内部按截图页 / 输出页区分。
    fn expected_after_reset(page: Page, original: &Settings) -> Settings {
        let default = Settings::default();
        let mut expected = original.clone();
        match page {
            Page::General => expected.run_as_admin = default.run_as_admin,
            Page::Screenshot => {
                let d = &default.screenshot;
                expected.screenshot.hidden_toolbar_items = d.hidden_toolbar_items.clone();
                expected.screenshot.annotation = d.annotation.clone();
                expected.screenshot.smart_selection = d.smart_selection;
                expected.screenshot.auto_scroll = d.auto_scroll.clone();
                expected.screenshot.capture_mode = d.capture_mode;
                expected.screenshot.show_cursor = d.show_cursor;
                expected.screenshot.show_guides = d.show_guides;
                expected.screenshot.long_capture_auto_start = d.long_capture_auto_start;
                expected.screenshot.left_double_click = d.left_double_click;
                expected.screenshot.middle_click = d.middle_click;
                expected.screenshot.click_outside = d.click_outside;
                expected.screenshot.record_history_sources = d.record_history_sources.clone();
                expected.screenshot.toolbar_position = d.toolbar_position;
                expected.screenshot.panel_position = d.panel_position;
                expected.screenshot.show_magnifier = d.show_magnifier;
                expected.screenshot.show_info_bar = d.show_info_bar;
                expected.screenshot.show_shortcut_hints = d.show_shortcut_hints;
                expected.screenshot.selection_border_style = d.selection_border_style;
                expected.screenshot.selection_border_width = d.selection_border_width;
                expected.screenshot.selection_border_color = d.selection_border_color.clone();
            }
            Page::Pin => expected.pin = default.pin.clone(),
            Page::Output => {
                let d = &default.screenshot;
                expected.screenshot.save_dir = d.save_dir.clone();
                expected.screenshot.watermark = d.watermark.clone();
                expected.screenshot.export = d.export.clone();
                expected.screenshot.collage = d.collage.clone();
                expected.screenshot.pdf = d.pdf.clone();
            }
            Page::Data => expected.history = default.history.clone(),
            Page::Hotkey => expected.hotkeys = default.hotkeys.clone(),
            Page::Appearance => expected.appearance = default.appearance.clone(),
            Page::Translate => expected.translation = default.translation.clone(),
            Page::Resources => expected.ocr = default.ocr.clone(),
            _ => {}
        }
        expected
    }

    /// 全量 serde JSON 对比：任何漏重置的子字段都会被发现，不再靠抽查几个
    /// 字段（Hotkey 只看 region、annotation 只看 stroke_color）蒙混过关。
    #[test]
    fn reset_page_only_touches_fields_the_page_owns() {
        for page in Page::ALL {
            let original = all_non_default();
            let mut s = original.clone();
            let changed = reset_page(&mut s, page);
            let actual = serde_json::to_value(&s).expect("serializes");
            let expected =
                serde_json::to_value(expected_after_reset(page, &original)).expect("serializes");
            if page.has_config() {
                assert!(changed, "{page:?} claims config but reset did nothing");
                assert_eq!(
                    actual, expected,
                    "{page:?} must reset exactly its owned fields"
                );
            } else {
                assert!(!changed, "{page:?} has no config but reported a reset");
                assert_eq!(actual, expected, "{page:?} must not change any field");
            }
        }
    }

    /// 递归核对：凡是本页重置改动过的字段，`copy_page_fields` 都必须把重置前
    /// 的值写进基线，否则页重置在合并时会被误判成「没改过」而落不了盘。
    fn assert_reset_fields_marked(
        original: &serde_json::Value,
        snapshot: &serde_json::Value,
        baseline: &serde_json::Value,
        path: &str,
    ) {
        if let (serde_json::Value::Object(a), serde_json::Value::Object(b)) = (original, snapshot) {
            for (key, before) in a {
                if let Some(after) = b.get(key.as_str()) {
                    assert_reset_fields_marked(
                        before,
                        after,
                        &baseline[key.as_str()],
                        &format!("{path}.{key}"),
                    );
                }
            }
        } else if original != snapshot {
            assert_eq!(
                baseline, original,
                "reset field {path} missing from baseline"
            );
        }
    }

    #[test]
    fn copy_page_fields_marks_every_field_the_page_resets() {
        for page in Page::ALL {
            let original = all_non_default();
            let mut snapshot = original.clone();
            reset_page(&mut snapshot, page);
            let mut baseline = Settings::default();
            copy_page_fields(&mut baseline, &original, page);
            assert_reset_fields_marked(
                &serde_json::to_value(&original).expect("serializes"),
                &serde_json::to_value(&snapshot).expect("serializes"),
                &serde_json::to_value(&baseline).expect("serializes"),
                page.id(),
            );
        }
    }

    /// 开窗时 spacing 已经是默认值 0，差异判断看不出「重置」，force_page_fields
    /// 必须无条件把本页字段写回快照，同时不碰其它页的字段。
    #[test]
    fn force_page_fields_overwrites_only_owned_fields() {
        let mut snapshot = Settings {
            appearance: "Catppuccin Latte".into(),
            ..Settings::default()
        }; // spacing=0 即开窗默认值
        assert!(reset_page(&mut snapshot, Page::Output));

        // 其它窗口在开窗后改写了 spacing / pdf / appearance。
        let mut merged = snapshot.clone();
        merged.screenshot.collage.spacing = 5;
        merged.screenshot.pdf.image_quality = 77;
        merged.appearance = "Tokyo Moon".into();

        force_page_fields(&mut merged, &snapshot, Page::Output);
        assert_eq!(merged.screenshot.collage.spacing, 0, "默认值也要强制写回");
        assert_eq!(
            merged.screenshot.pdf.image_quality,
            Settings::default().screenshot.pdf.image_quality
        );
        assert_eq!(merged.appearance, "Tokyo Moon", "非本页字段保持不动");
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
