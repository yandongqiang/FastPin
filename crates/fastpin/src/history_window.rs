//! Browser window for screenshot history.
//!
//! Modelled on CitePix's history gallery: a grid of thumbnail cards with a
//! count + total-size header. FastPin keeps its own store layer
//! (`crate::history`) — JSON index + per-record image directory, atomic
//! writes, two-phase delete — but the *view* now matches the reference
//! product: cards wrap in a grid instead of a single column, and the header
//! shows "N 张图片 · X MB".
//!
//! Thumbnails are decoded from the stored image files on a background
//! executor so opening or refreshing the list never blocks the UI thread;
//! full-size decode happens only on Copy.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    actions, canvas, div, img, prelude::*, px, size, App, AsyncApp, Bounds, Context, CursorStyle,
    FocusHandle, Focusable, FontWeight, ImageSource, KeyBinding, MouseButton, Render, RenderImage,
    SharedString, Window, WindowBounds, WindowHandle, WindowOptions,
};
use image::GenericImageView;

use crate::history::{HistoryRecord, HistoryStore};
use crate::image_budget::BUDGET;
use crate::overlay::theme;

actions!(
    history_window,
    [
        CloseHistory,
        CopySelectedHistory,
        DeleteSelectedHistory,
        RefreshHistory
    ]
);

const KEY_CONTEXT: &str = "HistoryWindow";
/// Thumbnail decode cap — history records are bounded to ~100 by config, so
/// decoding them all at open is cheap and keeps the grid responsive.
const THUMB_SIZE: u32 = 200;
/// Fixed card width for the grid layout.
const CARD_W: f32 = 240.0;
/// Padding inside each card.
const CARD_PAD: f32 = 12.0;
/// 宫格（瀑布流）的列间距与纵向间距。
const GRID_GAP: f32 = 16.0;

/// 历史窗口的两种浏览方式（会话内状态，不持久化）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryViewMode {
    /// 宫格：Masonry 瀑布流，卡片贪心投放到最矮列。
    Grid,
    /// 列表：小缩略图 + 纵向排列的元信息。
    List,
}

/// 缩略图显示宽度固定（THUMB_SIZE - 2 * CARD_PAD），高度按原图宽高比计算。
/// 极端比例做 clamp，避免网格等高行被撑得失衡。
fn thumb_height(width: u32, height: u32) -> f32 {
    const THUMB_W: f32 = THUMB_SIZE as f32 - 2.0 * CARD_PAD; // 176
    const MIN_H: f32 = 48.0;
    const MAX_H: f32 = THUMB_W * 2.0;
    if width == 0 || height == 0 {
        return THUMB_W;
    }
    (THUMB_W * height as f32 / width as f32).clamp(MIN_H, MAX_H)
}

/// 宫格卡片总高度估计，供瀑布流累计列高使用。宫格只显示图片，
/// 因此高度就是上下内边距 + 缩略图，留少量余量避免相邻卡片重叠。
fn card_height(record: &HistoryRecord) -> f32 {
    2.0 * CARD_PAD + thumb_height(record.width, record.height) + 2.0
}

/// 空历史提示（宫格 / 列表共用）。
fn empty_hint() -> impl IntoElement {
    div()
        .w_full()
        .py(px(80.))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .child(
            div()
                .text_size(px(13.))
                .text_color(theme::secondary_fg())
                .child("还没有截图历史。"),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme::secondary_fg())
                .child("保存截图后会自动记录到这里。"),
        )
}

thread_local! {
    /// Open windows, kept so GPUI does not drop the handles (and the windows).
    static HISTORY_WINDOWS: RefCell<Vec<(u64, WindowHandle<HistoryWindowView>)>> =
        const { RefCell::new(Vec::new()) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static KEYS_BOUND: Cell<bool> = const { Cell::new(false) };
}

/// Opens the history browser. The `enabled` setting only gates *recording*:
/// turning recording off must not hide captures already on disk, otherwise the
/// one control meant to show past images stops working exactly when a user
/// wants to inspect or clear them.
pub fn show_history_window(cx: &mut App) {
    let settings = crate::config::get();
    let dir = match settings.history.store_dir() {
        Ok(d) => d,
        Err(e) => {
            crate::perf::log_app(&format!("history window: no store dir: {e}"));
            return;
        }
    };
    let store = HistoryStore::new(dir);
    let records = store.list().unwrap_or_else(|e| {
        crate::perf::log_app(&format!("history window: index read failed: {e}"));
        Vec::new()
    });
    bind_keys(cx);
    prune_closed(cx);

    let bounds = Bounds::centered(None, size(px(1000.), px(700.)), cx);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            focus: true,
            show: true,
            is_movable: true,
            is_resizable: true,
            is_minimizable: true,
            window_min_size: Some(size(px(640.), px(480.))),
            ..gpui_kit::component::TitleBar::window_options()
        },
        |_, cx| cx.new(|cx| HistoryWindowView::new(cx, store, records)),
    );
    let handle = match opened {
        Ok(h) => h,
        Err(e) => {
            crate::perf::log_app(&format!("history window failed: {e}"));
            return;
        }
    };
    if handle
        .update(cx, |view, window, cx| {
            window.focus(&view.focus_handle, cx);
            window.on_window_should_close(cx, move |_, cx| {
                forget(id);
                cx.refresh_windows();
                true
            });
            // New captures are recorded on a bare export thread, so the open
            // gallery has to poll the revision to see them without F5.
            view.start_revision_poll(cx);
        })
        .is_err()
    {
        return;
    }
    HISTORY_WINDOWS.with(|slot| slot.borrow_mut().push((id, handle)));
}

fn bind_keys(cx: &mut App) {
    if KEYS_BOUND.get() {
        return;
    }
    KEYS_BOUND.set(true);
    let context = Some(KEY_CONTEXT);
    cx.bind_keys(vec![
        KeyBinding::new("escape", CloseHistory, context),
        KeyBinding::new("ctrl-c", CopySelectedHistory, context),
        KeyBinding::new("delete", DeleteSelectedHistory, context),
        KeyBinding::new("f5", RefreshHistory, context),
    ]);
}

fn prune_closed(cx: &mut App) {
    HISTORY_WINDOWS.with(|slot| {
        slot.borrow_mut()
            .retain(|(_, handle)| handle.update(cx, |_, _, _| ()).is_ok());
    });
}

fn forget(id: u64) {
    HISTORY_WINDOWS.with(|slot| slot.borrow_mut().retain(|(open, _)| *open != id));
}

/// Decodes a stored image into a small GPUI thumb. Stored files are real RGBA
/// images (not the BGRA screenshot buffers), so — like `settings::brand_logo` —
/// the R and B channels are swapped for GPUI's B8G8R8A8 atlas.
#[allow(clippy::chunks_exact_to_as_chunks)]
pub(crate) fn load_thumb(path: &PathBuf) -> Option<Arc<RenderImage>> {
    // 与水印 / 剪贴板解码一致的三重检查：先拒绝超大文件，再用头部尺寸挡住
    // 「小文件声明巨大画布」的图片炸弹，最后复核解码后的真实尺寸。
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    BUDGET.checked_len(meta.len()).ok()?;
    let (header_w, header_h) = image::image_dimensions(path).ok()?;
    BUDGET.checked_dims(header_w as u64, header_h as u64).ok()?;

    let decoded = image::open(path).ok()?.thumbnail(THUMB_SIZE, THUMB_SIZE);
    let (w, h) = decoded.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    BUDGET.checked_dims(w as u64, h as u64).ok()?;
    let src = decoded.to_rgba8();
    // 输出缓冲也走统一预算，缩略图路径不再使用无界 `into_raw()`。
    let mut rgba = BUDGET.alloc_rgba(w as u64, h as u64).ok()?;
    rgba.copy_from_slice(src.as_raw());
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let frame = image::RgbaImage::from_raw(w, h, rgba)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(frame)])))
}

/// Modification time of a stored image in unix millis, used to decide whether
/// a cached thumbnail is still valid. `None` (file missing/unreadable) forces a
/// fresh decode, mirroring the cache in `settings::mod`.
fn history_file_mtime(path: &PathBuf) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as u64)
}

/// Total bytes across all records, formatted like "27.8 MB".
fn total_size(records: &[HistoryRecord]) -> SharedString {
    let bytes: u64 = records.iter().map(|r| r.bytes).sum();
    crate::settings::model::human_size(bytes).into()
}

pub struct HistoryWindowView {
    focus_handle: FocusHandle,
    store: HistoryStore,
    records: Vec<HistoryRecord>,
    /// Thumbnail cache keyed by record id. Survives reloads; entries whose
    /// record disappeared are dropped and unchanged files are never re-decoded.
    thumbs: HashMap<String, Option<Arc<RenderImage>>>,
    /// Mtime the cached thumb was built from. A thumb is reused only while this
    /// still matches the file's current modification time.
    thumbs_mtime: HashMap<String, u64>,
    /// Bumped on every decode request so a slow, stale batch cannot overwrite
    /// the thumbnails of a newer refresh when it finally finishes.
    thumb_seq: u64,
    selected: Option<String>,
    status: SharedString,
    /// Last history index revision this window loaded. A 400ms poll compares
    /// it with `history_revision()` so a capture saved elsewhere shows up
    /// without the user pressing 刷新.
    revision_seen: u64,
    /// 当前浏览方式（宫格 / 列表），仅会话内有效，不写入配置。
    view_mode: HistoryViewMode,
    /// 列表容器宽度，由 canvas prepaint 测得；变化时触发瀑布流重排。
    /// 初值 1000 与窗口默认宽度一致，避免首帧列数抖动。
    container_width: Rc<Cell<f32>>,
}

impl HistoryWindowView {
    fn new(cx: &mut Context<Self>, store: HistoryStore, records: Vec<HistoryRecord>) -> Self {
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            store,
            records,
            thumbs: HashMap::new(),
            thumbs_mtime: HashMap::new(),
            thumb_seq: 0,
            selected: None,
            status: SharedString::default(),
            revision_seen: crate::history::history_revision(),
            view_mode: HistoryViewMode::Grid,
            container_width: Rc::new(Cell::new(1000.0)),
        };
        this.load_thumbs_async(cx);
        this
    }

    /// Decodes thumbnails off the UI thread, reusing cached ones whose image
    /// file is unchanged. Only new/changed records are decoded; caches of
    /// deleted records are dropped. A sequence number prevents a stale batch
    /// from overwriting a newer refresh's thumbnails.
    fn load_thumbs_async(&mut self, cx: &mut Context<Self>) {
        let seq = {
            self.thumb_seq = self.thumb_seq.wrapping_add(1);
            self.thumb_seq
        };
        // Keep only cache entries for records that still exist, then work out
        // which images actually need (re)decoding. A file is unchanged when its
        // mtime still matches the one the cached thumb was built from.
        let live_ids: std::collections::HashSet<String> =
            self.records.iter().map(|r| r.id.clone()).collect();
        self.thumbs.retain(|id, _| live_ids.contains(id));
        self.thumbs_mtime.retain(|id, _| live_ids.contains(id));

        let mut jobs: Vec<(String, PathBuf)> = Vec::new();
        for r in &self.records {
            let path = self.store.image_path(r);
            let mtime = history_file_mtime(&path);
            let cached = self.thumbs.contains_key(&r.id)
                && self.thumbs_mtime.get(&r.id).copied() == mtime
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
                            let thumb = load_thumb(&path);
                            (id, mtime, thumb)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                // Ignore a batch that was queued before a newer refresh.
                if this.thumb_seq == seq {
                    for (id, mtime, thumb) in decoded {
                        if let Some(m) = mtime {
                            this.thumbs_mtime.insert(id.clone(), m);
                        }
                        this.thumbs.insert(id, thumb);
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.records = self.store.list().unwrap_or_else(|e| {
            self.status = format!("读取历史失败：{e}").into();
            Vec::new()
        });
        if self
            .selected
            .as_ref()
            .is_some_and(|id| !self.records.iter().any(|r| &r.id == id))
        {
            self.selected = None;
        }
        self.load_thumbs_async(cx);
        cx.notify();
    }

    /// Polls the global history revision every 400ms so a copy made while the
    /// gallery is open appears without a manual refresh. At most one reload per
    /// tick even when several captures landed in between; the loop exits on its
    /// own once the view is gone (`this.update` then fails).
    fn start_revision_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx: &mut AsyncApp| loop {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let keep_polling = this
                .update(cx, |this, cx| {
                    let revision = crate::history::history_revision();
                    if revision != this.revision_seen {
                        this.revision_seen = revision;
                        this.reload(cx);
                    }
                    true
                })
                .unwrap_or(false);
            if !keep_polling {
                break;
            }
        })
        .detach();
    }

    fn selected_record(&self) -> Option<&HistoryRecord> {
        let id = self.selected.as_ref()?;
        self.records.iter().find(|r| &r.id == id)
    }

    fn on_refresh(&mut self, _: &RefreshHistory, _: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    fn on_close(&mut self, _: &CloseHistory, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    fn on_copy(&mut self, _: &CopySelectedHistory, _: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.selected_record() else {
            self.status = "请先选择一张图片".into();
            cx.notify();
            return;
        };
        let path = self.store.image_path(record);
        match crate::codec::decode_bgra(&path) {
            Some((bgra, w, h)) => match crate::codec::set_clipboard_dib(&bgra, w, h, w * 4) {
                Ok(()) => self.status = "已复制到剪贴板".into(),
                Err(e) => self.status = format!("复制失败：{e}").into(),
            },
            None => self.status = "图片解码失败".into(),
        }
        cx.notify();
    }

    fn on_delete(&mut self, _: &DeleteSelectedHistory, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            self.status = "请先选择一张图片".into();
            cx.notify();
            return;
        };
        match self.store.remove(&id) {
            Ok(()) => {
                self.status = "已删除".into();
                self.selected = None;
                self.reload(cx);
            }
            Err(e) => {
                self.status = format!("删除失败：{e}").into();
                cx.notify();
            }
        }
    }

    fn on_clear(&mut self, cx: &mut Context<Self>) {
        match self.store.clear() {
            Ok(()) => {
                self.status = "已清空历史".into();
                self.selected = None;
                self.reload(cx);
            }
            Err(e) => {
                self.status = format!("清空失败：{e}").into();
                cx.notify();
            }
        }
    }

    fn on_open_folder(&mut self, cx: &mut Context<Self>) {
        let dir = match self.selected_record() {
            Some(r) => self.store.image_path(r).parent().map(PathBuf::from),
            None => Some(self.store.root().to_path_buf()),
        };
        if let Some(dir) = dir {
            if let Err(e) = crate::settings::system::open_folder(&dir) {
                self.status = format!("打开失败：{e}").into();
                cx.notify();
            }
        }
    }

    /// Title bar: "历史图片" + count chip.
    fn titlebar(&self) -> impl IntoElement {
        gpui_kit::component::TitleBar::new().pl(px(14.)).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(9.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::primary_fg())
                        .child("历史图片"),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::secondary_fg())
                        .child(format!("{} 张", self.records.len())),
                ),
        )
    }

    /// 视图切换按钮（宫格 / 列表），当前模式高亮显示。
    fn view_toggle(
        &self,
        id: &'static str,
        label: &'static str,
        mode: HistoryViewMode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.view_mode == mode;
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
                    this.view_mode = mode;
                    cx.notify();
                }),
            )
            .child(label)
    }

    /// Info bar beneath the title: "N 张图片 · X MB" + 视图切换。
    fn info_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .h(px(32.))
            .px(px(16.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .bg(theme::muted())
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::primary_fg())
                    .child(format!("{} 张图片", self.records.len())),
            )
            .child(div().w(px(1.)).h(px(10.)).bg(theme::border()))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::secondary_fg())
                    .child(total_size(&self.records)),
            )
            .child(
                div()
                    .ml_auto()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(self.view_toggle("view-grid", "宫格", HistoryViewMode::Grid, cx))
                    .child(self.view_toggle("view-list", "列表", HistoryViewMode::List, cx)),
            )
    }

    /// One grid card: thumb on top, time + size info below.
    fn card(
        &self,
        index: usize,
        record: &HistoryRecord,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.selected.as_ref() == Some(&record.id);
        let id = record.id.clone();
        let thumb_h = thumb_height(record.width, record.height);
        div()
            .id(("history-card", index))
            .flex_none()
            .w(px(CARD_W))
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(CARD_PAD))
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
                    this.selected = Some(id.clone());
                    this.status = SharedString::default();
                    cx.notify();
                }),
            )
            // 缩略图宽度固定，高度按原图比例（record.width/height）计算。
            .child(
                div()
                    .w(px(THUMB_SIZE as f32 - 2. * CARD_PAD))
                    .h(px(thumb_h))
                    .rounded(px(6.))
                    .bg(theme::muted())
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(match self.thumbs.get(&record.id).and_then(|t| t.clone()) {
                        Some(image) => img(ImageSource::Render(image))
                            .w(px(THUMB_SIZE as f32 - 2. * CARD_PAD))
                            .h(px(thumb_h))
                            .object_fit(gpui::ObjectFit::Contain)
                            .rounded(px(6.))
                            .into_any_element(),
                        None => div()
                            .w(px(THUMB_SIZE as f32 - 2. * CARD_PAD))
                            .h(px(thumb_h))
                            .rounded(px(6.))
                            .bg(theme::muted())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme::secondary_fg())
                                    .child("无预览"),
                            )
                            .into_any_element(),
                    }),
            )
    }

    /// 列表模式的一行：左侧小缩略图（80px 宽，按原图比例定高），右侧
    /// 日期 / 宽高 / 大小 / 来源纵向排列。点击选中，交互与宫格卡片一致。
    fn list_row(
        &self,
        index: usize,
        record: &HistoryRecord,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.selected.as_ref() == Some(&record.id);
        let id = record.id.clone();
        const ROW_THUMB_W: f32 = 80.0;
        let thumb_h = if record.width == 0 {
            ROW_THUMB_W
        } else {
            (ROW_THUMB_W * record.height as f32 / record.width as f32).clamp(30.0, 120.0)
        };
        div()
            .id(("history-row", index))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .p(px(8.))
            .rounded(px(6.))
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
                    this.selected = Some(id.clone());
                    this.status = SharedString::default();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(ROW_THUMB_W))
                    .h(px(thumb_h))
                    .rounded(px(4.))
                    .bg(theme::muted())
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(match self.thumbs.get(&record.id).and_then(|t| t.clone()) {
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
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme::secondary_fg())
                                    .child("无预览"),
                            )
                            .into_any_element(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
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
                            .child(format!("{} × {}", record.width, record.height)),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme::secondary_fg())
                            .child(crate::settings::model::human_size(record.bytes)),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme::secondary_fg())
                            .child(crate::history::source_label(&record.source)),
                    ),
            )
    }

    /// 浏览区：顶部一条不可见的 canvas 测量容器宽度（宽度变化时 notify 重排），
    /// 下方是滚动区。宫格用 Masonry 瀑布流，列表用纵向行。
    fn list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity_id = cx.entity_id();
        let width_cell = self.container_width.clone();
        let measurer = canvas(
            move |bounds, _, app: &mut App| {
                let w = f32::from(bounds.size.width);
                // 容差 1px，避免滚动条出现/消失造成的抖动循环。
                if (width_cell.get() - w).abs() > 1.0 {
                    width_cell.set(w);
                    app.notify(entity_id);
                }
            },
            |_, _, _, _| (),
        )
        .w_full()
        .h(px(1.));

        let mut scroll = div()
            .id("history-grid")
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .overflow_scroll()
            .bg(theme::surface_solid())
            .p(px(16.));

        if self.records.is_empty() {
            scroll = scroll.child(empty_hint());
        } else {
            match self.view_mode {
                HistoryViewMode::Grid => {
                    // 瀑布流：按测得宽度算列数，卡片贪心投放到当前最矮列。
                    let w = self.container_width.get();
                    let cols = ((w - 2.0 * 16.0 + GRID_GAP) / (CARD_W + GRID_GAP))
                        .floor()
                        .max(1.0) as usize;
                    let mut col_h = vec![0.0f32; cols];
                    let mut placements: Vec<(usize, f32, f32)> =
                        Vec::with_capacity(self.records.len());
                    for (i, record) in self.records.iter().enumerate() {
                        let col = col_h
                            .iter()
                            .enumerate()
                            .min_by(|(_, a), (_, b)| {
                                a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                            })
                            .map(|(c, _)| c)
                            .unwrap_or(0);
                        let x = col as f32 * (CARD_W + GRID_GAP);
                        let y = col_h[col];
                        col_h[col] = y + card_height(record) + GRID_GAP;
                        placements.push((i, x, y));
                    }
                    let max_col_h = col_h.iter().copied().fold(0.0f32, f32::max);
                    let mut masonry = div()
                        .id("history-masonry")
                        .relative()
                        .w_full()
                        .h(px(max_col_h.max(1.0)));
                    for (i, x, y) in placements {
                        masonry = masonry.child(
                            div().absolute().left(px(x)).top(px(y)).child(self.card(
                                i,
                                &self.records[i],
                                cx,
                            )),
                        );
                    }
                    scroll = scroll.child(masonry);
                }
                HistoryViewMode::List => {
                    let mut rows = div().w_full().flex().flex_col().gap(px(8.));
                    for (i, record) in self.records.iter().enumerate() {
                        rows = rows.child(self.list_row(i, record, cx));
                    }
                    scroll = scroll.child(rows);
                }
            }
        }

        div()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .flex()
            .flex_col()
            .child(measurer)
            .child(scroll)
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .h(px(52.))
            .px(px(16.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .bg(theme::surface_solid())
            .border_t_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::secondary_fg())
                    .child(self.status.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(hist_button("history-refresh", "刷新", false).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.on_refresh(&RefreshHistory, window, cx)
                        }),
                    ))
                    .child(
                        hist_button("history-folder", "打开文件夹", false).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.on_open_folder(cx)),
                        ),
                    )
                    .child(hist_button("history-copy", "复制", false).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.on_copy(&CopySelectedHistory, window, cx)
                        }),
                    ))
                    .child(hist_button("history-delete", "删除", false).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.on_delete(&DeleteSelectedHistory, window, cx)
                        }),
                    ))
                    .child(hist_button("history-clear", "清空", true).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.on_clear(cx)),
                    ))
                    .child(hist_button("history-close", "关闭", false).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.on_close(&CloseHistory, window, cx)),
                    )),
            )
    }
}

impl Focusable for HistoryWindowView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HistoryWindowView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::surface_solid())
            .text_color(theme::primary_fg())
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_refresh))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_close))
            .child(self.titlebar())
            .child(self.info_bar(cx))
            .child(self.list(cx))
            .child(self.footer(cx))
    }
}

fn hist_button(id: &'static str, label: &'static str, danger: bool) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .flex_none()
        .h(px(30.))
        .px(px(12.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .text_size(px(12.))
        .cursor(CursorStyle::PointingHand)
        .child(label);
    if danger {
        base.bg(theme::danger())
            .text_color(theme::on_danger())
            .hover(|s| s.bg(theme::danger_hover()))
    } else {
        base.bg(theme::elevated_surface())
            .text_color(theme::primary_fg())
            .border_1()
            .border_color(theme::border())
            .hover(|s| s.bg(theme::selection_wash()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_height_follows_aspect_ratio_with_clamp() {
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

    /// Guards the whole "show a past image" path: a real PNG on disk must turn
    /// into a GPUI thumbnail, including the B/R swap the atlas expects.
    #[test]
    fn stored_png_decodes_into_a_bgra_thumbnail() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("history-window-tests")
            .join(format!("case-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.png");
        image::RgbaImage::from_pixel(240, 160, image::Rgba([10, 20, 200, 255]))
            .save(&path)
            .unwrap();

        let thumb = load_thumb(&path).expect("thumbnail decodes");
        let bytes = thumb.as_bytes(0).expect("RenderImage carries the frame");
        assert_eq!(&bytes[..4], &[200, 20, 10, 255]);
    }
}
