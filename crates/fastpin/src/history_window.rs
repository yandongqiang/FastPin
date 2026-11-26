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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
        RefreshHistory,
        SelectAllHistory
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

/// 空历史提示（宫格 / 列表共用）。索引读取失败时不能假装是空历史——图片
/// 其实还在磁盘上，必须明确告知用户原因和下一步。
fn empty_hint(index_error: Option<&str>) -> impl IntoElement {
    let (title, detail): (SharedString, SharedString) = match index_error {
        Some(e) => (
            "历史索引损坏，图片仍在磁盘。".into(),
            format!("请从「打开文件夹」查看原图，或检查 index.json 后刷新。索引错误：{e}").into(),
        ),
        None => (
            "还没有截图历史。".into(),
            "保存截图后会自动记录到这里。".into(),
        ),
    };
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
                .child(title),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme::secondary_fg())
                .child(detail),
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
    bind_keys(cx);
    prune_closed(cx);
    // 重复点击入口时只激活已打开的窗口，避免每个实例各起一个 400ms
    // revision 轮询任务相互叠加；失效 handle 已由 prune_closed 清理。
    let last = HISTORY_WINDOWS.with(|slot| slot.borrow().last().map(|(_, handle)| *handle));
    let active = last
        .and_then(|handle| {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .ok()
        })
        .is_some();
    if active {
        return;
    }

    let settings = crate::config::get();
    let dir = match settings.history.store_dir() {
        Ok(d) => d,
        Err(e) => {
            crate::perf::log_app(&format!("history window: no store dir: {e}"));
            return;
        }
    };
    let store = HistoryStore::new(dir);
    let mut index_error = None;
    let records = store.list().unwrap_or_else(|e| {
        crate::perf::log_app(&format!("history window: index read failed: {e}"));
        index_error = Some(e.to_string());
        Vec::new()
    });

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
        |_, cx| cx.new(|cx| HistoryWindowView::new(cx, store, records, index_error)),
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
        KeyBinding::new("ctrl-a", SelectAllHistory, context),
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
pub(crate) fn load_thumb(path: &Path) -> Option<Arc<RenderImage>> {
    // 三重检查（文件大小 / 头部尺寸 / 解码后尺寸）统一由
    // `codec::decode_image_budgeted` 完成，和水印、剪贴板解码共用一套规则。
    let decoded = crate::codec::decode_image_budgeted(path)?;
    let thumb = image::DynamicImage::ImageRgba8(decoded).thumbnail(THUMB_SIZE, THUMB_SIZE);
    let (w, h) = thumb.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    BUDGET.checked_dims(w as u64, h as u64).ok()?;
    let src = thumb.to_rgba8();
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

/// 缓存命中判定（纯函数，便于单测）：必须同时有占位、记录过成功解码时的
/// mtime、且文件 mtime 未变。失败留下的 `None` 没有 mtime，必然返回 false，
/// 下一次刷新会重新尝试。
fn thumb_cache_hit(
    has_placeholder: bool,
    cached_mtime: Option<u64>,
    current_mtime: Option<u64>,
) -> bool {
    has_placeholder && current_mtime.is_some() && cached_mtime == current_mtime
}

/// 写入一次后台解码结果。失败时只保留 `None` 占位（避免同一批次里反复排队），
/// 但**不写** `thumbs_mtime`：文件可能正被写入，瞬时失败不该靠一个"有效"
/// 的 mtime 变成永久空白，直到用户等到 mtime 变化才恢复。
fn apply_thumb_result(
    thumbs: &mut HashMap<String, Option<Arc<RenderImage>>>,
    mtimes: &mut HashMap<String, u64>,
    id: String,
    mtime: Option<u64>,
    thumb: Option<Arc<RenderImage>>,
) {
    match thumb {
        Some(image) => {
            if let Some(m) = mtime {
                mtimes.insert(id.clone(), m);
            }
            thumbs.insert(id, Some(image));
        }
        None => {
            mtimes.remove(&id);
            thumbs.insert(id, None);
        }
    }
}

/// Total bytes across all records, formatted like "27.8 MB".
fn total_size(records: &[HistoryRecord]) -> SharedString {
    let bytes: u64 = records.iter().map(|r| r.bytes).sum();
    crate::settings::model::human_size(bytes).into()
}

/// Contiguous slice of `ids` between the anchor and the clicked row, inclusive.
/// Works in both directions; an unknown anchor degrades to the clicked row.
fn range_ids(ids: &[String], anchor: Option<&str>, clicked: &str) -> Vec<String> {
    let Some(anchor) = anchor else {
        return vec![clicked.to_string()];
    };
    let (Some(start), Some(end)) = (
        ids.iter().position(|id| id == anchor),
        ids.iter().position(|id| id == clicked),
    ) else {
        return vec![clicked.to_string()];
    };
    let (lo, hi) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    ids[lo..=hi].to_vec()
}

/// Pure selection transition for one left-click. Plain click replaces the
/// selection, Ctrl+click toggles one item, Shift+click extends from the anchor.
/// Anchors are ids (not indices) so a refresh that inserts newer records at the
/// top does not silently move the range.
fn apply_pick(
    ids: &[String],
    clicked: &str,
    anchor: Option<&str>,
    ctrl: bool,
    shift: bool,
    current: &HashSet<String>,
) -> (HashSet<String>, Option<String>) {
    if shift && anchor.is_some() {
        return (
            range_ids(ids, anchor, clicked).into_iter().collect(),
            anchor.map(str::to_string),
        );
    }
    if ctrl {
        let mut next = current.clone();
        if !next.remove(clicked) {
            next.insert(clicked.to_string());
        }
        return (next, Some(clicked.to_string()));
    }
    let mut next = HashSet::new();
    next.insert(clicked.to_string());
    (next, Some(clicked.to_string()))
}

/// Status text always reports the multi-selection count, then any operation
/// result ("已复制到剪贴板", "删除失败：…") so feedback never hides the count.
fn selection_status_text(count: usize, status: &str) -> SharedString {
    match (count, status.is_empty()) {
        (0, _) => status.into(),
        (_, true) => format!("已选 {count} 张").into(),
        (_, false) => format!("已选 {count} 张 · {status}").into(),
    }
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
    /// All currently selected record ids, in no particular order. Display
    /// order always comes from `records`, so range selection is deterministic.
    selected: HashSet<String>,
    /// Id the last plain/Ctrl/Shift pick started from, used as the fixed end of
    /// a Shift+click range.
    selection_anchor: Option<String>,
    /// Re-entrancy guard: a copy is still running in a background thread, or a
    /// batch delete is committing index changes. Both actions are ignored
    /// while true so Ctrl+C / Delete cannot race each other.
    busy: bool,
    status: SharedString,
    /// index.json 读取失败的原因。非空表示历史可能仍在磁盘上，UI 不能把它
    /// 显示成「还没有截图历史」，复制 / 删除 / 清空也因此不可用。
    index_error: Option<String>,
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
    fn new(
        cx: &mut Context<Self>,
        store: HistoryStore,
        records: Vec<HistoryRecord>,
        index_error: Option<String>,
    ) -> Self {
        let status = match &index_error {
            Some(e) => format!("历史索引损坏，图片仍在磁盘：{e}").into(),
            None => SharedString::default(),
        };
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            store,
            records,
            thumbs: HashMap::new(),
            thumbs_mtime: HashMap::new(),
            thumb_seq: 0,
            selected: HashSet::new(),
            selection_anchor: None,
            busy: false,
            status,
            index_error,
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
            let cached = thumb_cache_hit(
                self.thumbs.contains_key(&r.id),
                self.thumbs_mtime.get(&r.id).copied(),
                mtime,
            );
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
                        apply_thumb_result(
                            &mut this.thumbs,
                            &mut this.thumbs_mtime,
                            id,
                            mtime,
                            thumb,
                        );
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        match self.store.list() {
            Ok(records) => {
                let was_corrupt = self.index_error.is_some();
                self.records = records;
                self.index_error = None;
                if was_corrupt {
                    self.status = SharedString::default();
                }
            }
            Err(e) => {
                self.records.clear();
                self.index_error = Some(e.to_string());
                self.status = format!("历史索引损坏，图片仍在磁盘：{e}").into();
            }
        }
        // Records may have been removed (or a corrupt index replaced) while
        // the window was open: drop stale ids from both the selection and the
        // Shift anchor so subsequent picks never reference a vanished row.
        self.selected
            .retain(|id| self.records.iter().any(|r| &r.id == id));
        // Shift 锚点只需还存在于列表里；Ctrl 反选掉锚点后它仍可作为范围端点。
        if self
            .selection_anchor
            .as_ref()
            .is_some_and(|id| !self.records.iter().any(|r| &r.id == id))
        {
            self.selection_anchor = None;
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

    /// Selected records in display order (newest first), never in click order.
    fn selected_records(&self) -> Vec<&HistoryRecord> {
        self.records
            .iter()
            .filter(|r| self.selected.contains(&r.id))
            .collect()
    }

    /// First selected row, used by "打开文件夹" where only one folder makes sense.
    fn first_selected_record(&self) -> Option<&HistoryRecord> {
        self.records.iter().find(|r| self.selected.contains(&r.id))
    }

    fn on_pick(&mut self, clicked: &str, ctrl: bool, shift: bool, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.records.iter().map(|r| r.id.clone()).collect();
        let (selected, anchor) = apply_pick(
            &ids,
            clicked,
            self.selection_anchor.as_deref(),
            ctrl,
            shift,
            &self.selected,
        );
        self.selected = selected;
        self.selection_anchor = anchor;
        self.status = SharedString::default();
        cx.notify();
    }

    fn on_select_all(&mut self, _: &SelectAllHistory, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = self.records.iter().map(|r| r.id.clone()).collect();
        self.selection_anchor = self.records.first().map(|r| r.id.clone());
        self.status = SharedString::default();
        cx.notify();
    }

    fn on_refresh(&mut self, _: &RefreshHistory, _: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    fn on_close(&mut self, _: &CloseHistory, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    fn on_copy(&mut self, _: &CopySelectedHistory, _: &mut Window, cx: &mut Context<Self>) {
        // A copy keeps a worker thread alive; ignore repeats until it reports.
        if self.busy {
            return;
        }
        if self.selected.is_empty() {
            self.status = "请先选择一张图片".into();
            cx.notify();
            return;
        }

        // 多选：复制图片文件本身（CF_HDROP），像资源管理器复制多文件一样，
        // 粘贴到聊天/文档时会成为多张图片而不是一张扁平位图。
        if self.selected.len() > 1 {
            let paths: Vec<PathBuf> = self
                .selected_records()
                .into_iter()
                .map(|r| self.store.image_path(r))
                .filter(|p| p.is_file())
                .collect();
            let wanted = self.selected.len();
            if paths.is_empty() {
                self.status = "选中的图片文件不存在".into();
                cx.notify();
                return;
            }
            let count = paths.len();
            let missing = wanted.saturating_sub(count);
            self.busy = true;
            self.status = "正在复制…".into();
            cx.notify();
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                unsafe {
                    let _ = windows::Win32::System::Com::CoInitializeEx(
                        None,
                        windows::Win32::System::Com::COINIT_MULTITHREADED,
                    );
                }
                let status = match crate::codec::set_clipboard_files(&paths) {
                    Ok(()) if missing == 0 => format!("已复制 {count} 张图片到剪贴板"),
                    Ok(()) => format!("已复制 {count} 张图片到剪贴板（{missing} 张文件已不存在）"),
                    Err(e) => format!("复制失败：{e}"),
                };
                let _ = tx.send(status);
            });
            cx.spawn(async move |this, cx: &mut AsyncApp| {
                let status = loop {
                    match rx.try_recv() {
                        Ok(status) => break status,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            break "复制失败：后台任务中断".to_string()
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            cx.background_executor()
                                .timer(Duration::from_millis(50))
                                .await;
                        }
                    }
                };
                // `this.update` fails once the window/entity is gone, so a
                // late callback after close is simply dropped.
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    this.status = status.into();
                    cx.notify();
                });
            })
            .detach();
            return;
        }

        // 单张保留原行为：解码为 DIB 后复制图片内容。
        let Some(record) = self.selected_records().into_iter().next() else {
            self.status = "请先选择一张图片".into();
            cx.notify();
            return;
        };
        let path = self.store.image_path(record);
        self.busy = true;
        self.status = "正在复制…".into();
        cx.notify();

        // 完整解码 + DIB 组装都在后台线程：大图在 UI 线程做这两步会明显卡住
        // 整个窗口。状态在线程回报后更新。
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                );
            }
            let status = match crate::codec::decode_bgra(&path) {
                Some((bgra, w, h)) => match crate::codec::set_clipboard_dib(&bgra, w, h, w * 4) {
                    Ok(()) => "已复制到剪贴板".to_string(),
                    Err(e) => format!("复制失败：{e}"),
                },
                None => "图片解码失败".to_string(),
            };
            let _ = tx.send(status);
        });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let status = loop {
                match rx.try_recv() {
                    Ok(status) => break status,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        break "复制失败：后台任务中断".to_string()
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        cx.background_executor()
                            .timer(Duration::from_millis(50))
                            .await;
                    }
                }
            };
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.status = status.into();
                cx.notify();
            });
        })
        .detach();
    }

    fn on_delete(&mut self, _: &DeleteSelectedHistory, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.selected.is_empty() {
            self.status = "请先选择一张图片".into();
            cx.notify();
            return;
        }
        self.busy = true;
        let ids: Vec<String> = self
            .selected_records()
            .into_iter()
            .map(|r| r.id.clone())
            .collect();
        let mut removed = Vec::new();
        let mut failures = Vec::new();
        // Reuse the store's two-phase delete for every id: commit the index
        // first, then unlink the record directory. One bad row must not hide
        // the others, so failures are collected and reported together.
        for id in &ids {
            match self.store.remove(id) {
                Ok(()) => removed.push(id.clone()),
                Err(e) => failures.push(format!("{id}：{e}")),
            }
        }
        for id in &removed {
            self.selected.remove(id);
        }
        self.busy = false;
        self.reload(cx);
        self.status = if failures.is_empty() {
            format!("已删除 {} 张", removed.len()).into()
        } else {
            format!(
                "已删除 {} 张，失败 {} 张：{}",
                removed.len(),
                failures.len(),
                failures.join("；")
            )
            .into()
        };
        cx.notify();
    }

    fn on_clear(&mut self, cx: &mut Context<Self>) {
        match self.store.clear() {
            Ok(()) => {
                self.status = "已清空历史".into();
                self.selected.clear();
                self.selection_anchor = None;
                self.reload(cx);
            }
            Err(e) => {
                self.status = format!("清空失败：{e}").into();
                cx.notify();
            }
        }
    }

    fn on_open_folder(&mut self, cx: &mut Context<Self>) {
        let dir = match self.first_selected_record() {
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
        let selected = self.selected.contains(&record.id);
        let id = record.id.clone();
        let thumb_h = thumb_height(record.width, record.height);
        let mut card = div()
            .id(("history-card", index))
            .relative()
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
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.on_pick(&id, event.modifiers.control, event.modifiers.shift, cx);
                }),
            );
        if selected {
            // 多选时每张卡片都有醒目的勾选角标，一眼可辨选中集合。
            card = card.child(
                div()
                    .absolute()
                    .top(px(6.))
                    .right(px(6.))
                    .h(px(18.))
                    .min_w(px(18.))
                    .px(px(4.))
                    .rounded(px(9.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::accent())
                    .text_color(theme::on_accent())
                    .text_size(px(11.))
                    .child("✓"),
            );
        }
        card
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
        let selected = self.selected.contains(&record.id);
        let id = record.id.clone();
        const ROW_THUMB_W: f32 = 80.0;
        let thumb_h = if record.width == 0 {
            ROW_THUMB_W
        } else {
            (ROW_THUMB_W * record.height as f32 / record.width as f32).clamp(30.0, 120.0)
        };
        let mut row = div()
            .id(("history-row", index))
            .relative()
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
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.on_pick(&id, event.modifiers.control, event.modifiers.shift, cx);
                }),
            );
        if selected {
            row = row.child(
                div()
                    .absolute()
                    .top(px(4.))
                    .right(px(4.))
                    .h(px(18.))
                    .min_w(px(18.))
                    .px(px(4.))
                    .rounded(px(9.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::accent())
                    .text_color(theme::on_accent())
                    .text_size(px(11.))
                    .child("✓"),
            );
        }
        row.child(
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
            scroll = scroll.child(empty_hint(self.index_error.as_deref()));
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
        let count = self.selected.len();
        let copy_label = if count > 1 {
            format!("复制 {count} 张")
        } else {
            "复制".to_string()
        };
        let delete_label = if count > 1 {
            format!("删除 {count} 张")
        } else {
            "删除".to_string()
        };
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
                    .child(selection_status_text(count, self.status.as_ref())),
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
                    .child(
                        hist_button("history-copy", copy_label, false).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.on_copy(&CopySelectedHistory, window, cx)
                            }),
                        ),
                    )
                    .child(
                        hist_button("history-delete", delete_label, false).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.on_delete(&DeleteSelectedHistory, window, cx)
                            }),
                        ),
                    )
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

impl HistoryWindowView {
    /// 「关闭历史保留数量」对应的恢复入口：列出最近关闭的贴图，点击即恢复。
    fn closed_pins_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // 轻量快照：避免每次 render 深拷贝最多 200 条、每条约 100KB 的缩略图。
        let records = crate::pin::closed_pin_summaries();
        if records.is_empty() {
            return div();
        }
        let buttons: Vec<gpui::Stateful<gpui::Div>> = records
            .iter()
            .rev()
            .take(8)
            .map(|(id, size)| {
                let id = *id;
                let label: SharedString = format!("恢复 {}×{}", size.0, size.1).into();
                div()
                    .id(("restore-closed-pin", id))
                    .flex_none()
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .bg(theme::elevated_surface())
                    .border_1()
                    .border_color(theme::border())
                    .text_size(px(12.))
                    .cursor(CursorStyle::PointingHand)
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            match crate::pin::restore_closed_pin(cx, id) {
                                Ok(()) => this.status = "已恢复关闭的贴图".into(),
                                Err(e) => this.status = format!("恢复贴图失败：{e}").into(),
                            }
                            cx.notify();
                        }),
                    )
            })
            .collect();
        div()
            .px(px(12.))
            .py(px(6.))
            .flex()
            .flex_row()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::secondary_fg())
                    .child("最近关闭的贴图"),
            )
            .children(buttons)
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
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_close))
            .child(self.titlebar())
            .child(self.info_bar(cx))
            .child(self.closed_pins_section(cx))
            .child(self.list(cx))
            .child(self.footer(cx))
    }
}

fn hist_button(
    id: &'static str,
    label: impl Into<SharedString>,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
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
        .child(label.into());
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

    static TEST_DIR: AtomicU64 = AtomicU64::new(0);

    /// Scratch directory under target/ with an atomic counter: parallel tests
    /// never race on the same case dir (and %TEMP% is blocked by the sandbox).
    fn scratch() -> PathBuf {
        let n = TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("history-window-tests")
            .join(format!("case-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

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
        let dir = scratch();
        let path = dir.join("image.png");
        image::RgbaImage::from_pixel(240, 160, image::Rgba([10, 20, 200, 255]))
            .save(&path)
            .unwrap();

        let thumb = load_thumb(&path).expect("thumbnail decodes");
        let bytes = thumb.as_bytes(0).expect("RenderImage carries the frame");
        assert_eq!(&bytes[..4], &[200, 20, 10, 255]);
    }

    /// 文件正被写入时的解码失败是瞬时的：失败分支只能留占位，不能记 mtime，
    /// 否则下一次刷新会把它当成有效缓存，缩略图永久空白。
    #[test]
    fn a_failed_thumbnail_decode_stays_retryable() {
        let dir = scratch();
        let path = dir.join("image.png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let mtime = history_file_mtime(&path).expect("mtime");
        let id = "retry-me".to_string();

        let mut thumbs: HashMap<String, Option<Arc<RenderImage>>> = HashMap::new();
        let mut mtimes = HashMap::new();

        // 第一次失败：占位在，mtime 不在，缓存判定必须为假。
        apply_thumb_result(&mut thumbs, &mut mtimes, id.clone(), Some(mtime), None);
        assert!(thumbs.contains_key(&id), "占位保留");
        assert!(thumbs[&id].is_none());
        assert!(!mtimes.contains_key(&id), "失败不能记 mtime");
        assert!(
            !thumb_cache_hit(
                thumbs.contains_key(&id),
                mtimes.get(&id).copied(),
                Some(mtime)
            ),
            "失败后必须重试"
        );

        // 重试成功：这次才写 mtime，下次刷新才能命中缓存。
        let decoded = load_thumb(&path).expect("decodes");
        apply_thumb_result(
            &mut thumbs,
            &mut mtimes,
            id.clone(),
            Some(mtime),
            Some(decoded),
        );
        assert!(thumbs[&id].is_some());
        assert_eq!(mtimes.get(&id).copied(), Some(mtime), "成功才记 mtime");
        assert!(thumb_cache_hit(
            thumbs.contains_key(&id),
            mtimes.get(&id).copied(),
            Some(mtime)
        ));
    }

    /// 头部声明超大画布但文件只有几十字节的图片必须在解码前被拒绝：
    /// `image::open` 会按头部尺寸分配，panic=abort 下会直接结束进程。
    #[test]
    fn huge_header_image_is_rejected_before_decoding() {
        let dir = scratch();
        let path = dir.join("bomb.bmp");
        // 54 字节 BMP：BITMAPINFOHEADER 声明 20000×1（超过 MAX_SIDE），
        // 但完全没有像素数据。load_thumb 只该读头部就拒绝。
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&54u32.to_le_bytes()); // file size
        bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
        bytes.extend_from_slice(&54u32.to_le_bytes()); // pixel offset
        bytes.extend_from_slice(&40u32.to_le_bytes()); // DIB header size
        bytes.extend_from_slice(&20_000u32.to_le_bytes()); // width
        bytes.extend_from_slice(&1u32.to_le_bytes()); // height
        bytes.extend_from_slice(&1u16.to_le_bytes()); // planes
        bytes.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        bytes.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        bytes.extend_from_slice(&0u32.to_le_bytes()); // image size
        bytes.extend_from_slice(&0u32.to_le_bytes()); // x ppm
        bytes.extend_from_slice(&0u32.to_le_bytes()); // y ppm
        bytes.extend_from_slice(&0u32.to_le_bytes()); // colors used
        bytes.extend_from_slice(&0u32.to_le_bytes()); // important colors
        assert_eq!(bytes.len(), 54);
        std::fs::write(&path, &bytes).unwrap();

        assert!(BUDGET.checked_dims(20_000, 1).is_err());
        assert!(load_thumb(&path).is_none());
    }

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("id-{i}")).collect()
    }

    #[test]
    fn plain_click_replaces_selection_and_moves_anchor() {
        let ids = ids(4);
        let initial: HashSet<String> = ["id-0", "id-1"].into_iter().map(str::to_string).collect();
        let (next, anchor) = apply_pick(&ids, "id-3", Some("id-0"), false, false, &initial);
        assert_eq!(next, HashSet::from(["id-3".to_string()]));
        assert_eq!(anchor.as_deref(), Some("id-3"));
    }

    #[test]
    fn ctrl_click_toggles_one_item_and_keeps_the_rest() {
        let ids = ids(4);
        let initial: HashSet<String> = ["id-0", "id-1"].into_iter().map(str::to_string).collect();
        let (off, anchor) = apply_pick(&ids, "id-0", Some("id-1"), true, false, &initial);
        assert_eq!(off, HashSet::from(["id-1".to_string()]));
        assert_eq!(anchor.as_deref(), Some("id-0"));
        let (on, _) = apply_pick(&ids, "id-2", anchor.as_deref(), true, false, &off);
        assert_eq!(on, HashSet::from(["id-1".to_string(), "id-2".to_string()]));
    }

    #[test]
    fn shift_click_selects_inclusive_range_in_either_direction() {
        let ids = ids(5);
        assert_eq!(
            range_ids(&ids, Some("id-1"), "id-3"),
            vec!["id-1".to_string(), "id-2".to_string(), "id-3".to_string()]
        );
        // Either direction yields the same contiguous slice in display order.
        assert_eq!(
            range_ids(&ids, Some("id-3"), "id-1"),
            vec!["id-1".to_string(), "id-2".to_string(), "id-3".to_string()]
        );
        // Ctrl+Shift keeps the range semantics: the anchor stays put.
        let (selected, anchor) =
            apply_pick(&ids, "id-3", Some("id-1"), true, true, &HashSet::new());
        assert_eq!(selected.len(), 3);
        assert_eq!(anchor.as_deref(), Some("id-1"));
    }

    #[test]
    fn shift_click_without_a_live_anchor_acts_like_a_plain_click() {
        let ids = ids(3);
        assert_eq!(range_ids(&ids, None, "id-2"), vec!["id-2".to_string()]);
        assert_eq!(
            range_ids(&ids, Some("gone"), "id-1"),
            vec!["id-1".to_string()]
        );
        let (selected, anchor) = apply_pick(&ids, "id-2", None, false, true, &HashSet::new());
        assert_eq!(selected, HashSet::from(["id-2".to_string()]));
        assert_eq!(anchor.as_deref(), Some("id-2"));
    }

    #[test]
    fn status_always_shows_selection_count() {
        assert_eq!(selection_status_text(0, ""), "");
        assert_eq!(selection_status_text(0, "已删除 1 张"), "已删除 1 张");
        assert_eq!(selection_status_text(3, ""), "已选 3 张");
        assert_eq!(
            selection_status_text(2, "复制失败：x"),
            "已选 2 张 · 复制失败：x"
        );
    }
}
