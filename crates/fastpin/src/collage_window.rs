//! Image collage and PDF export window for FastPin.
//!
//! Mirrors snow-shot's ImageStitchingWindow at a deliberately small scale:
//! - Left panel: image list + settings (direction, spacing, border, grid)
//! - Right panel: preview with live layout
//! - Export: PNG collage and/or PDF (PageBased/TemplateBased)

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{Datelike, Local, Timelike};
use gpui::{
    actions, div, img, prelude::*, px, size, App, Bounds, Context, CursorStyle, FocusHandle,
    Focusable, FontWeight, ImageSource, KeyBinding, MouseButton, Render, RenderImage, SharedString,
    Window, WindowBounds, WindowHandle, WindowOptions,
};
use gpui_component::scroll::ScrollableElement;

use crate::codec::write_png_file;
use crate::collage::{
    calculate_collage_layout, calculate_pdf_composed_page, pdf_page_count, CollageItem,
    CollageLayout, CollageSettings, Direction, PdfComposedPage, PdfSettings, RectPx, Sizing,
    StitchingMethod, TemplateType,
};
use crate::overlay::theme;
use printpdf::{ColorBits, ColorSpace, Image, ImageFilter, ImageXObject, Mm, PdfDocument, Px};

actions!(
    collage_window,
    [
        CloseCollage,
        ExportCollagePng,
        ExportCollagePdf,
        OpenCollageImage,
        AddCollageImages,
        RemoveCollageImage,
        ClearCollageImages,
        PrevPdfPage,
        NextPdfPage,
        CycleCollageDirection,
        CycleCollageSizing,
        StepCollageSpacing,
        StepCollageBorder,
        StepCollageRows,
        StepCollageColumns,
        CyclePdfMethod,
        CyclePdfTemplate,
        StepPdfSpacing,
        StepPdfQuality,
    ]
);

const KEY_CONTEXT: &str = "CollageWindow";
const PREVIEW_MAX_W: f32 = 720.0;
const PREVIEW_MAX_H: f32 = 540.0;
/// A4 at ~150 DPI is ~9 MB per page. Keep only the current page plus one
/// neighbour resident instead of pre-rendering every page of a long document.
const PDF_PREVIEW_CACHE: usize = 2;

thread_local! {
    static COLLAGE_WINDOWS: RefCell<Vec<(u64, WindowHandle<CollageWindowView>)>> = const { RefCell::new(Vec::new()) };
}
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static KEYS_BOUND: Cell<bool> = const { Cell::new(false) };
}

/// Opens the collage window with no image; the user adds images from the control column.
pub fn show_collage_window(cx: &mut App) {
    open(cx, None);
}

/// Opens the collage window preloaded with a captured buffer (BGRA, tight stride).
pub fn show_collage_pixels(pixels: Arc<Vec<u8>>, width: u32, height: u32, cx: &mut App) {
    if width == 0 || height == 0 {
        return;
    }
    open(cx, Some((pixels, width, height, "screenshot".to_string())));
}

fn open(cx: &mut App, initial: Option<(Arc<Vec<u8>>, u32, u32, String)>) {
    bind_keys(cx);
    prune_closed(cx);
    // 已打开的窗口直接激活，重复点击入口不再叠加新窗口。
    let last = COLLAGE_WINDOWS.with(|slot| slot.borrow().last().map(|(_, handle)| *handle));
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

    let bounds = Bounds::centered(None, size(px(1100.), px(760.)), cx);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            focus: true,
            show: true,
            is_movable: true,
            is_resizable: true,
            is_minimizable: true,
            window_min_size: Some(size(px(900.), px(600.))),
            ..gpui_kit::component::TitleBar::window_options()
        },
        move |_, cx| cx.new(|cx| CollageWindowView::new(cx, initial)),
    );
    let handle = match opened {
        Ok(h) => h,
        Err(e) => {
            crate::perf::log_app(&format!("collage window failed: {e}"));
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
        })
        .is_err()
    {
        return;
    }
    COLLAGE_WINDOWS.with(|slot| slot.borrow_mut().push((id, handle)));
}

fn bind_keys(cx: &mut App) {
    if KEYS_BOUND.get() {
        return;
    }
    KEYS_BOUND.set(true);
    let context = Some(KEY_CONTEXT);
    cx.bind_keys(vec![
        KeyBinding::new("escape", CloseCollage, context),
        KeyBinding::new("ctrl-e", ExportCollagePng, context),
        KeyBinding::new("ctrl-p", ExportCollagePdf, context),
        KeyBinding::new("ctrl-o", OpenCollageImage, context),
        KeyBinding::new("ctrl-a", AddCollageImages, context),
        KeyBinding::new("delete", RemoveCollageImage, context),
        KeyBinding::new("ctrl-backspace", ClearCollageImages, context),
        KeyBinding::new("left", PrevPdfPage, context),
        KeyBinding::new("right", NextPdfPage, context),
        KeyBinding::new("d", CycleCollageDirection, context),
        KeyBinding::new("s", CycleCollageSizing, context),
        KeyBinding::new("up", StepCollageSpacing, context),
        KeyBinding::new("down", StepCollageBorder, context),
        KeyBinding::new("m", CyclePdfMethod, context),
        KeyBinding::new("t", CyclePdfTemplate, context),
    ]);
}

fn prune_closed(cx: &mut App) {
    COLLAGE_WINDOWS.with(|slot| {
        slot.borrow_mut()
            .retain(|(_, handle)| handle.update(cx, |_, _, _| ()).is_ok());
    });
}

fn forget(id: u64) {
    COLLAGE_WINDOWS.with(|slot| slot.borrow_mut().retain(|(open, _)| *open != id));
}

/// BGRA buffer → GPUI image. Same conversion as `longpreview::render_image`:
/// the buffer is handed over as-is and GPUI's upload handles channel order.
fn render_image(
    pixels: &Arc<Vec<u8>>,
    w: u32,
    h: u32,
) -> Option<(Arc<RenderImage>, crate::image_budget::MemoryPermit)> {
    // `from_raw` copies the whole buffer into the RenderImage. Reserve that copy
    // against the process-wide ledger and keep the permit alive alongside the
    // image (on ImageEntry) so several open windows cannot sum to an OOM.
    let bytes = (w as u64).checked_mul(h as u64)?.checked_mul(4)?;
    let permit = crate::image_budget::try_reserve(bytes).ok()?;
    let rgba = image::RgbaImage::from_raw(w, h, pixels.as_ref().clone())?;
    Some((
        Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])),
        permit,
    ))
}

fn placeholder_image() -> Arc<RenderImage> {
    let rgba = image::RgbaImage::from_raw(1, 1, vec![0, 0, 0, 0]).unwrap();
    Arc::new(RenderImage::new(vec![image::Frame::new(rgba)]))
}

/// Allocates a BGRA canvas through the shared image budget. Returns `None`
/// for a collage too large to render rather than aborting on a huge `vec!`.
fn alloc_canvas(w: u32, h: u32, bg: [u8; 4]) -> Option<Vec<u8>> {
    let bytes = crate::image_budget::ImageBudget::STANDARD
        .checked_bytes(w as u64, h as u64, 4)
        .ok()?;
    let mut canvas = Vec::new();
    canvas.try_reserve_exact(bytes).ok()?;
    canvas.resize(bytes, 0);
    for chunk in canvas.as_chunks_mut::<4>().0 {
        *chunk = bg;
    }
    Some(canvas)
}

/// 解析 PNG 导出路径：同名文件已存在时依次尝试 `name (1).png`、
/// `name (2).png`…，避免同秒重复导出覆盖上一次的结果。路径探测通过
/// `exists` 注入，便于纯函数单测不碰磁盘。
fn unique_png_export_path(dir: &Path, stem: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    for suffix in 0..u32::MAX {
        let name = if suffix == 0 {
            format!("{stem}.png")
        } else {
            format!("{stem} ({suffix}).png")
        };
        let candidate = dir.join(name);
        if !exists(&candidate) {
            return candidate;
        }
    }
    // 序号耗尽（实际上不可能）时退回原名，交给写入逻辑报错。
    dir.join(format!("{stem}.png"))
}

/// Nearest-neighbour blit that scales `src` into `rect` and applies the
/// rotation/flip transform. Coordinates are normalised, so the destination
/// rect may have any size relative to the source.
fn blit_image(
    dst: &mut [u8],
    (dst_w, dst_h): (u32, u32),
    src: &[u8],
    (src_w, src_h): (u32, u32),
    rect: RectPx,
    rotation: i8,
    flipped: bool,
) {
    if rect.width == 0 || rect.height == 0 || src_w == 0 || src_h == 0 {
        return;
    }
    let src_len = src.len();
    let dst_len = dst.len();
    let rotation = rotation.rem_euclid(4);
    for y in 0..rect.height {
        let dy = rect.y + y;
        if dy >= dst_h {
            break;
        }
        let v = (y as f32 + 0.5) / rect.height as f32;
        for x in 0..rect.width {
            let dx = rect.x + x;
            if dx >= dst_w {
                break;
            }
            let mut u = (x as f32 + 0.5) / rect.width as f32;
            if flipped {
                u = 1.0 - u;
            }
            let (su, sv) = match rotation {
                1 => (v, 1.0 - u),
                2 => (1.0 - u, 1.0 - v),
                3 => (1.0 - v, u),
                _ => (u, v),
            };
            let sx = ((su * src_w as f32) as u32).min(src_w - 1);
            let sy = ((sv * src_h as f32) as u32).min(src_h - 1);
            let src_idx = (sy as usize * src_w as usize + sx as usize) * 4;
            let dst_idx = (dy as usize * dst_w as usize + dx as usize) * 4;
            if src_idx + 3 < src_len && dst_idx + 3 < dst_len {
                dst[dst_idx..dst_idx + 4].copy_from_slice(&src[src_idx..src_idx + 4]);
            }
        }
    }
}

/// PDF 导出后台任务所需的轻量图片数据：只保留 PDF 编码真正用到的字段，
/// 不携带 UI 用的 `Arc<RenderImage>`，因此可以安全地移动到后台线程。
#[derive(Clone)]
struct PdfImage {
    id: u64,
    label: String,
    pixels: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    rotation: i8,
    flipped_h: bool,
}

impl PdfImage {
    fn to_collage_item(&self) -> CollageItem {
        CollageItem {
            id: self.id,
            label: self.label.clone(),
            width: self.width,
            height: self.height,
            rotation: self.rotation,
            flipped_h: self.flipped_h,
        }
    }
}

/// Flattens an entry to the `ImageXObject` printpdf embeds, applying
/// the same rotation/flip as the on-screen preview. `quality` (clamped to
/// 1..100) is the PDF 图片质量 setting: the RGB raster is JPEG-encoded at that
/// quality and embedded with a DCTDecode filter, so lowering it actually
/// shrinks the PDF. If the encoder refuses the buffer, the lossless RGB data
/// is embedded instead rather than dropping the image.
fn pdf_image_xobject(item: &PdfImage, quality: u8) -> Option<ImageXObject> {
    let (tw, th) = item.to_collage_item().transformed_size();
    if tw == 0 || th == 0 {
        return None;
    }
    let bytes = crate::image_budget::ImageBudget::STANDARD
        .checked_bytes(tw as u64, th as u64, 3)
        .ok()?;
    let mut rgb = Vec::new();
    rgb.try_reserve_exact(bytes).ok()?;
    rgb.resize(bytes, 255);

    let src = item.pixels.as_slice();
    let (src_w, src_h) = (item.width, item.height);
    let rotation = item.rotation.rem_euclid(4);
    for y in 0..th {
        let v = (y as f32 + 0.5) / th as f32;
        for x in 0..tw {
            let mut u = (x as f32 + 0.5) / tw as f32;
            if item.flipped_h {
                u = 1.0 - u;
            }
            let (su, sv) = match rotation {
                1 => (v, 1.0 - u),
                2 => (1.0 - u, 1.0 - v),
                3 => (1.0 - v, u),
                _ => (u, v),
            };
            let sx = ((su * src_w as f32) as u32).min(src_w.saturating_sub(1));
            let sy = ((sv * src_h as f32) as u32).min(src_h.saturating_sub(1));
            let src_idx = (sy as usize * src_w as usize + sx as usize) * 4;
            let dst_idx = (y as usize * tw as usize + x as usize) * 3;
            if src_idx + 2 < src.len() && dst_idx + 2 < rgb.len() {
                rgb[dst_idx] = src[src_idx + 2];
                rgb[dst_idx + 1] = src[src_idx + 1];
                rgb[dst_idx + 2] = src[src_idx];
            }
        }
    }

    // 质量来自 config.screenshot.pdf.image_quality（已桥接到 PdfSettings），
    // 这里再 clamp 一次，防止手改配置写出 0 或 >100。
    let quality = quality.clamp(1, 100);
    let mut jpeg = Vec::new();
    let compressed = jpeg.try_reserve_exact(bytes / 2 + 1).is_ok()
        && image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality)
            .encode(&rgb, tw, th, image::ExtendedColorType::Rgb8)
            .is_ok();

    Some(ImageXObject {
        width: Px(tw as usize),
        height: Px(th as usize),
        color_space: ColorSpace::Rgb,
        bits_per_component: ColorBits::Bit8,
        interpolate: true,
        image_data: if compressed { jpeg } else { rgb },
        // printpdf 0.6 把 ImageFilter::DCT 写成 PDF 的 /DCTDecode。
        image_filter: compressed.then_some(ImageFilter::DCT),
        clipping_bbox: None,
    })
}

/// Sibling staging path used to write a PDF before publishing it atomically.
/// Writing straight to the destination would leave a truncated file behind
/// when `save` or `flush` fails.
fn pdf_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.tmp"))
}

/// User-facing note for images that could not be embedded in the PDF.
fn pdf_skipped_warning(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!("（已跳过 {count} 张无法嵌入的图片）")
    }
}

/// `generate_pdfs` 的纯数据结果，回主线程后据此更新 UI 状态。
struct PdfExportResult {
    written: Vec<PathBuf>,
    error: Option<std::io::Error>,
    skipped: Vec<usize>,
    embedded_any: bool,
}

/// 在后台线程完成 PDF 构图与写盘：只接收拥有所有权的纯数据，不接触任何
/// UI/GPUI 状态，返回的结果可跨线程送回主线程。
fn generate_pdfs(
    images: Vec<PdfImage>,
    pdf_settings: PdfSettings,
    page_count: usize,
    dir: PathBuf,
    stamp: String,
) -> PdfExportResult {
    let items: Vec<CollageItem> = images.iter().map(|e| e.to_collage_item()).collect();
    let per_page = matches!(
        pdf_settings.export_mode,
        crate::collage::ExportMode::PerPage
    );
    let page_count = page_count.max(1);
    let mut written = Vec::new();
    let mut error = None;
    // An image that fails to convert must not vanish silently: remember it
    // so the status can warn the user instead of reporting a clean export.
    let mut skipped: Vec<usize> = Vec::new();
    let mut embedded_any = false;

    // `PerPage` writes one document per page; `Combined` keeps them all in
    // a single file. Both share the same page composer.
    let groups: Vec<Vec<usize>> = if per_page {
        (0..page_count).map(|p| vec![p]).collect()
    } else {
        vec![(0..page_count).collect()]
    };

    for (group_idx, pages) in groups.iter().enumerate() {
        let (page_w, page_h) = pdf_settings.page_size_mm(&items);
        let (doc, first_page, first_layer) =
            PdfDocument::new("FastPin Collage", Mm(page_w), Mm(page_h), "Layer 1");
        let mut page_indexes = vec![(first_page, first_layer)];
        let mut group_embedded = false;

        for _ in 1..pages.len() {
            page_indexes.push(doc.add_page(Mm(page_w), Mm(page_h), "Layer 1"));
        }

        for (slot, page_idx) in pages.iter().enumerate() {
            let (page_index, layer_index) = page_indexes[slot];
            let page = calculate_pdf_composed_page(&items, &pdf_settings, *page_idx);
            for cell in &page.cells {
                let item_index = cell.item_index;
                let Some(item) = images.get(item_index) else {
                    if !skipped.contains(&item_index) {
                        skipped.push(item_index);
                    }
                    continue;
                };
                let Some(xobj) = pdf_image_xobject(item, pdf_settings.image_quality) else {
                    // Zero-sized or over-budget: never embed a blank image.
                    if !skipped.contains(&item_index) {
                        skipped.push(item_index);
                    }
                    continue;
                };
                embedded_any = true;
                group_embedded = true;
                let dpi = if cell.image_mm.width > 0.0 {
                    xobj.width.0 as f32 * 25.4 / cell.image_mm.width
                } else {
                    96.0
                };
                let transform = printpdf::ImageTransform {
                    translate_x: Some(Mm(cell.image_mm.x)),
                    translate_y: Some(Mm(page_h - cell.image_mm.y - cell.image_mm.height)),
                    dpi: Some(dpi),
                    ..Default::default()
                };
                let layer: printpdf::PdfLayerReference =
                    doc.get_page(page_index).get_layer(layer_index);
                Image::from(xobj).add_to_layer(layer, transform);
            }
        }

        // A page with no embeddable image would produce an empty PDF.
        if !group_embedded {
            continue;
        }

        let name = if per_page {
            format!("FastPin_Collage_{stamp}_p{}.pdf", group_idx + 1)
        } else {
            format!("FastPin_Collage_{stamp}.pdf")
        };
        let path = dir.join(name);
        let temp_path = pdf_temp_path(&path);
        // Write to a sibling temp file, then publish with an atomic rename.
        // A save/flush failure leaves no truncated PDF at `path`.
        let result = (|| -> std::io::Result<()> {
            let file = std::fs::File::create(&temp_path)?;
            let mut writer = std::io::BufWriter::new(file);
            doc.save(&mut writer).map_err(std::io::Error::other)?;
            std::io::Write::flush(&mut writer)?;
            drop(writer);
            // The timestamped name is normally free, but replace a stale
            // file from an earlier same-second export before publishing.
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            std::fs::rename(&temp_path, &path)
        })();
        match result {
            Ok(()) => written.push(path),
            Err(e) => {
                let _ = std::fs::remove_file(&temp_path);
                error = Some(e);
                break;
            }
        }
    }

    PdfExportResult {
        written,
        error,
        skipped,
        embedded_any,
    }
}

struct ImageEntry {
    id: u64,
    label: String,
    pixels: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    rotation: i8,
    flipped_h: bool,
    image: Arc<RenderImage>,
    /// Ledger reservation for the RenderImage copy; released when the entry
    /// (and with it the image) is removed or the window closes.
    #[allow(dead_code)]
    permit: Option<crate::image_budget::MemoryPermit>,
}

impl ImageEntry {
    fn new(id: u64, label: String, pixels: Arc<Vec<u8>>, width: u32, height: u32) -> Self {
        let (image, permit) = match render_image(&pixels, width, height) {
            Some((image, permit)) => (image, Some(permit)),
            None => (placeholder_image(), None),
        };
        Self {
            id,
            label,
            pixels,
            width,
            height,
            rotation: 0,
            flipped_h: false,
            image,
            permit,
        }
    }

    fn to_collage_item(&self) -> CollageItem {
        CollageItem {
            id: self.id,
            label: self.label.clone(),
            width: self.width,
            height: self.height,
            rotation: self.rotation,
            flipped_h: self.flipped_h,
        }
    }
}

pub struct CollageWindowView {
    focus_handle: FocusHandle,
    images: Vec<ImageEntry>,
    selected_index: Option<usize>,

    // Static collage settings
    collage_settings: CollageSettings,

    // PDF settings
    pdf_settings: PdfSettings,

    // Preview state
    collage_layout: Option<CollageLayout>,
    collage_preview: Option<Arc<RenderImage>>,
    pdf_page_count: usize,
    pdf_current_page: usize,
    /// MRU-first page-preview cache: `(page_index, preview)`, at most
    /// `PDF_PREVIEW_CACHE` entries. Generated on demand, not for all pages.
    pdf_previews: Vec<(usize, Option<Arc<RenderImage>>)>,

    status: SharedString,
    export_result: Option<ExportResult>,
    /// 导出中标志：PNG 同步导出、PDF 后台导出共用，防止双击/切格式并发写盘。
    exporting: bool,
}

#[derive(Clone)]
struct ExportResult {
    png_path: Option<PathBuf>,
    pdf_path: Option<PathBuf>,
    message: String,
}

impl CollageWindowView {
    fn new(cx: &mut Context<Self>, initial: Option<(Arc<Vec<u8>>, u32, u32, String)>) -> Self {
        // The settings file is the source of truth: the window starts from what
        // the user last chose instead of the layout crate's defaults.
        let settings = crate::config::get();
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            images: Vec::new(),
            selected_index: None,
            collage_settings: (&settings.screenshot.collage).into(),
            pdf_settings: (&settings.screenshot.pdf).into(),
            collage_layout: None,
            collage_preview: None,
            pdf_page_count: 0,
            pdf_current_page: 0,
            pdf_previews: Vec::new(),
            status: "添加图片开始拼图".into(),
            export_result: None,
            exporting: false,
        };
        if let Some((pixels, w, h, name)) = initial {
            this.add_image(pixels, w, h, &name);
        }
        this.recalc_all(cx);
        this
    }

    /// 对最新配置应用一次增量修改并立即落盘，再把落盘后的值回写本地副本。
    ///
    /// 旧实现把整份本地 collage / pdf 覆盖回配置，窗口开启期间其它界面保存的、
    /// 本窗口没有改动的字段会被这份陈旧副本回滚。现在每次调用都从 `config::get()`
    /// 的最新值出发，闭包只修改本次涉及的那个字段，其余字段原样保留；保存成功
    /// （或尝试保存）后再回写本地副本，使它不再陈旧。11 处调用点语义保持不变。
    fn persist_settings(&mut self, edit: impl FnOnce(&mut CollageSettings, &mut PdfSettings)) {
        let mut settings = crate::config::get();
        let mut collage: CollageSettings = (&settings.screenshot.collage).into();
        let mut pdf: PdfSettings = (&settings.screenshot.pdf).into();
        edit(&mut collage, &mut pdf);
        settings.screenshot.collage = (&collage).into();
        settings.screenshot.pdf = (&pdf).into();
        if let Err(e) = crate::config::save(&settings) {
            self.status = format!("设置保存失败：{e}").into();
        }
        self.collage_settings = collage;
        self.pdf_settings = pdf;
    }

    fn add_image(&mut self, pixels: Arc<Vec<u8>>, width: u32, height: u32, name: &str) {
        let id = self.images.len() as u64 + 1;
        let entry = ImageEntry::new(id, name.into(), pixels, width, height);
        self.images.push(entry);
        self.selected_index = Some(self.images.len() - 1);
    }

    fn recalc_all(&mut self, cx: &mut Context<Self>) {
        // Static collage
        if !self.images.is_empty() {
            let items: Vec<CollageItem> = self.images.iter().map(|e| e.to_collage_item()).collect();
            self.collage_layout = Some(calculate_collage_layout(&items, &self.collage_settings));
            self.collage_preview = self.render_collage_preview(&items);
        } else {
            self.collage_layout = None;
            self.collage_preview = None;
        }

        // PDF: render only the visible page. Pre-generating every page made a
        // long document hold hundreds of MB; page turns now render on demand
        // with a two-entry LRU (see `ensure_pdf_preview`).
        if !self.images.is_empty() {
            let items: Vec<CollageItem> = self.images.iter().map(|e| e.to_collage_item()).collect();
            self.pdf_page_count = pdf_page_count(items.len(), &self.pdf_settings);
            self.pdf_current_page = self
                .pdf_current_page
                .min(self.pdf_page_count.saturating_sub(1));
            // Settings or the image list may have changed: previews are stale.
            self.pdf_previews.clear();
            self.ensure_pdf_preview(&items);
        } else {
            self.pdf_page_count = 0;
            self.pdf_current_page = 0;
            self.pdf_previews.clear();
        }
        cx.notify();
    }

    fn render_collage_preview(&self, items: &[CollageItem]) -> Option<Arc<RenderImage>> {
        let layout = self.collage_layout.as_ref()?;
        let (w, h) = layout.size;
        if w == 0 || h == 0 {
            return None;
        }

        // Create canvas
        let mut canvas = alloc_canvas(w, h, self.collage_settings.background_rgba)?;

        // Blit each image
        for (idx, rect) in layout.rects.iter().enumerate() {
            if idx >= items.len() {
                break;
            }
            let item = &self.images[idx];
            let (src_w, src_h) = item.to_collage_item().transformed_size();
            blit_image(
                &mut canvas,
                (w, h),
                &item.pixels,
                (src_w, src_h),
                *rect,
                item.rotation,
                item.flipped_h,
            );
        }

        // Scale down if too large for preview
        let max_w = PREVIEW_MAX_W;
        let max_h = PREVIEW_MAX_H;
        let scale = (max_w / w as f32).min(max_h / h as f32).min(1.0);

        let (out_w, out_h) = if scale < 1.0 {
            (
                (w as f32 * scale).round() as u32,
                (h as f32 * scale).round() as u32,
            )
        } else {
            (w, h)
        };

        let mut out = alloc_canvas(out_w, out_h, [0, 0, 0, 0])?;
        if scale < 1.0 {
            // Simple nearest-neighbor downsample
            for y in 0..out_h {
                for x in 0..out_w {
                    let src_x = ((x as f32 / scale).floor() as u32).min(w - 1);
                    let src_y = ((y as f32 / scale).floor() as u32).min(h - 1);
                    let src_idx = (src_y * w + src_x) as usize * 4;
                    let dst_idx = (y * out_w + x) as usize * 4;
                    out[dst_idx..dst_idx + 4].copy_from_slice(&canvas[src_idx..src_idx + 4]);
                }
            }
        } else {
            out.copy_from_slice(&canvas);
        }

        let rgba = image::RgbaImage::from_raw(out_w, out_h, out)?;
        Some(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
    }

    fn render_pdf_page_preview(&self, page: &PdfComposedPage) -> Option<Arc<RenderImage>> {
        let (page_w, page_h) = page.page_mm;
        if page_w == 0.0 || page_h == 0.0 || page.cells.is_empty() {
            return None;
        }

        // Render at ~150 DPI: 1mm = ~6 pixels
        let scale = 6.0;
        let out_w = (page_w * scale).round() as u32;
        let out_h = (page_h * scale).round() as u32;

        // White background
        let mut canvas = alloc_canvas(out_w, out_h, [255, 255, 255, 255])?;

        // Draw page border
        let border_px = 2;
        for x in 0..out_w {
            for y in 0..out_h {
                let idx = (y * out_w + x) as usize * 4;
                if x < border_px
                    || y < border_px
                    || x >= out_w - border_px
                    || y >= out_h - border_px
                {
                    canvas[idx] = 200;
                    canvas[idx + 1] = 200;
                    canvas[idx + 2] = 200;
                }
            }
        }

        // Draw each cell
        for cell in &page.cells {
            if cell.item_index >= self.images.len() {
                continue;
            }
            let item = &self.images[cell.item_index];
            let (src_w, src_h) = item.to_collage_item().transformed_size();

            // Convert mm to pixels
            let rect = RectPx {
                x: (cell.image_mm.x * scale).round() as u32,
                y: (cell.image_mm.y * scale).round() as u32,
                width: (cell.image_mm.width * scale).round().max(1.0) as u32,
                height: (cell.image_mm.height * scale).round().max(1.0) as u32,
            };
            blit_image(
                &mut canvas,
                (out_w, out_h),
                &item.pixels,
                (src_w, src_h),
                rect,
                item.rotation,
                item.flipped_h,
            );
        }

        let rgba = image::RgbaImage::from_raw(out_w, out_h, canvas)?;
        Some(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
    }

    /// Makes sure the current page has a preview, rendering just that page on
    /// demand. The cache is MRU-first; when full, the least recently used page
    /// is evicted, so at most `PDF_PREVIEW_CACHE` page bitmaps stay in memory.
    fn ensure_pdf_preview(&mut self, items: &[CollageItem]) {
        let page_idx = self.pdf_current_page;
        if let Some(pos) = self
            .pdf_previews
            .iter()
            .position(|(idx, _)| *idx == page_idx)
        {
            let entry = self.pdf_previews.remove(pos);
            self.pdf_previews.insert(0, entry); // refresh recency
            return;
        }
        let page = calculate_pdf_composed_page(items, &self.pdf_settings, page_idx);
        let preview = self.render_pdf_page_preview(&page);
        if self.pdf_previews.len() >= PDF_PREVIEW_CACHE {
            self.pdf_previews.pop(); // evict LRU
        }
        self.pdf_previews.insert(0, (page_idx, preview));
    }

    /// Borrows the current page's preview, already ensured by `recalc_all` and
    /// the page-turn handlers, for the render path.
    fn pdf_preview(&self) -> Option<&Arc<RenderImage>> {
        self.pdf_previews
            .iter()
            .find(|(idx, _)| *idx == self.pdf_current_page)
            .and_then(|(_, img)| img.as_ref())
    }

    fn on_open_image(&mut self, _: &OpenCollageImage, _: &mut Window, cx: &mut Context<Self>) {
        let start_dir = crate::config::screenshot_dir().unwrap_or_else(|_| std::env::temp_dir());
        if let Ok(Some(path)) = crate::settings::system::pick_image(&start_dir) {
            if let Some((bgra, w, h)) = crate::codec::decode_bgra(&path) {
                let pixels = Arc::new(bgra);
                let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
                self.add_image(pixels, w, h, name);
                self.status = SharedString::from(format!("已添加: {}", name));
                self.recalc_all(cx);
            } else {
                self.status = SharedString::from("解码图片失败");
                cx.notify();
            }
        }
    }

    fn on_add_images(&mut self, _: &AddCollageImages, cx: &mut Context<Self>) {
        let start_dir = crate::config::screenshot_dir().unwrap_or_else(|_| std::env::temp_dir());
        if let Ok(Some(path)) = crate::settings::system::pick_image(&start_dir) {
            if let Some((bgra, w, h)) = crate::codec::decode_bgra(&path) {
                let pixels = Arc::new(bgra);
                let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
                self.add_image(pixels, w, h, name);
                self.status = SharedString::from(format!("已添加: {}", name));
                self.recalc_all(cx);
            } else {
                self.status = SharedString::from("解码图片失败");
                cx.notify();
            }
        }
    }

    fn on_remove_selected(&mut self, _: &RemoveCollageImage, cx: &mut Context<Self>) {
        if let Some(idx) = self.selected_index {
            if idx < self.images.len() {
                self.images.remove(idx);
                if self.images.is_empty() {
                    self.selected_index = None;
                } else if idx >= self.images.len() {
                    self.selected_index = Some(self.images.len() - 1);
                }
                self.status = SharedString::from("已移除选中图片");
                self.recalc_all(cx);
            }
        }
    }

    fn on_clear(&mut self, _: &ClearCollageImages, cx: &mut Context<Self>) {
        if !self.images.is_empty() {
            self.images.clear();
            self.selected_index = None;
            self.status = SharedString::from("已清空所有图片");
            self.recalc_all(cx);
        }
    }

    fn on_export_png(&mut self, _: &ExportCollagePng, cx: &mut Context<Self>) {
        // 同一窗口的 PNG/PDF 导出共用一个忙标志：导出中再次点击或按快捷键
        // 直接早退，避免重复写盘（PDF 后台任务尚未结束尤其明显）。
        if self.exporting {
            return;
        }
        self.exporting = true;

        if self.images.is_empty() {
            self.exporting = false;
            self.status = SharedString::from("没有图片可导出");
            cx.notify();
            return;
        }

        let layout = self.collage_layout.as_ref();
        if layout.is_none() || !layout.unwrap().is_valid() {
            self.exporting = false;
            self.status = SharedString::from("布局无效");
            cx.notify();
            return;
        }

        let layout = layout.unwrap();
        let (w, h) = layout.size;
        let Some(mut canvas) = alloc_canvas(w, h, self.collage_settings.background_rgba) else {
            self.exporting = false;
            self.status = SharedString::from("拼图尺寸超出内存预算");
            cx.notify();
            return;
        };

        for (idx, rect) in layout.rects.iter().enumerate() {
            if idx >= self.images.len() {
                break;
            }
            let item = &self.images[idx];
            let (src_w, src_h) = item.to_collage_item().transformed_size();
            blit_image(
                &mut canvas,
                (w, h),
                &item.pixels,
                (src_w, src_h),
                *rect,
                item.rotation,
                item.flipped_h,
            );
        }

        let dir = match crate::config::screenshot_dir() {
            Ok(dir) => dir,
            Err(_) => {
                self.exporting = false;
                self.status = SharedString::from("无法获取截图目录");
                cx.notify();
                return;
            }
        };
        let time = Local::now();
        let name = format!(
            "FastPin_Collage_{:04}{:02}{:02}_{:02}{:02}{:02}",
            time.year(),
            time.month(),
            time.day(),
            time.hour(),
            time.minute(),
            time.second()
        );
        // 同秒再导出改用 `name (n).png`，不覆盖前一次结果。
        let path = unique_png_export_path(&dir, &name, |p| p.exists());

        if let Err(e) = write_png_file(&path, &canvas, w, h, w * 4) {
            self.exporting = false;
            self.status = SharedString::from(format!("导出失败: {}", e));
            cx.notify();
            return;
        }

        self.exporting = false;
        self.export_result = Some(ExportResult {
            png_path: Some(path.clone()),
            pdf_path: None,
            message: format!("已导出 PNG: {}", path.display()),
        });
        self.status = SharedString::from(format!("已导出 PNG: {}", path.display()));
        cx.notify();
    }

    fn on_export_pdf(&mut self, _: &ExportCollagePdf, cx: &mut Context<Self>) {
        // 与 PNG 共用忙标志：后台任务未结束前再次触发直接早退。关窗后任务
        // 不强杀，但完成回调用的是弱窗口句柄，窗口已关时 update 会失败，
        // 不会往已关窗口写状态。
        if self.exporting {
            return;
        }
        self.exporting = true;

        if self.images.is_empty() {
            self.exporting = false;
            self.status = SharedString::from("没有图片可导出");
            cx.notify();
            return;
        }
        if !crate::collage::validate_pdf_settings(&self.pdf_settings).is_empty() {
            self.exporting = false;
            self.status = SharedString::from("PDF 设置无效，请检查边距/纸张大小");
            cx.notify();
            return;
        }

        let dir = match crate::config::screenshot_dir() {
            Ok(dir) => dir,
            Err(_) => {
                self.exporting = false;
                self.status = SharedString::from("无法获取截图目录");
                cx.notify();
                return;
            }
        };
        let now = Local::now();
        let stamp = format!(
            "{:04}{:02}{:02}_{:02}{:02}{:02}",
            now.year(),
            now.month(),
            now.day(),
            now.hour(),
            now.minute(),
            now.second()
        );

        // 只克隆 PDF 编码真正需要的字段到轻量结构，避免把 UI 用的
        // `Arc<RenderImage>` 带进后台线程；构图与写盘由 `generate_pdfs` 完成。
        let images: Vec<PdfImage> = self
            .images
            .iter()
            .map(|e| PdfImage {
                id: e.id,
                label: e.label.clone(),
                pixels: e.pixels.clone(),
                width: e.width,
                height: e.height,
                rotation: e.rotation,
                flipped_h: e.flipped_h,
            })
            .collect();
        let pdf_settings = self.pdf_settings.clone();
        let page_count = self.pdf_page_count;
        self.status = SharedString::from("导出中…");
        self.export_result = None;
        cx.notify();

        let task = cx.background_spawn(async move {
            generate_pdfs(images, pdf_settings, page_count, dir, stamp)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.exporting = false;
                let PdfExportResult {
                    written,
                    error,
                    skipped,
                    embedded_any,
                } = result;
                let warning = pdf_skipped_warning(skipped.len());
                if !embedded_any {
                    // Every image was unusable: an empty PDF would be worse than no
                    // export, so refuse to write anything.
                    this.export_result = None;
                    this.status =
                        SharedString::from(format!("所有图片均无法嵌入 PDF，未导出{warning}"));
                    cx.notify();
                    return;
                }
                match error {
                    Some(e) => {
                        // PerPage may have written earlier pages before failing. Keep
                        // them visible so the user can open the folder and inspect them.
                        if written.is_empty() {
                            this.export_result = None;
                            this.status =
                                SharedString::from(format!("导出 PDF 失败: {e}{warning}"));
                        } else {
                            this.export_result = Some(ExportResult {
                                png_path: None,
                                pdf_path: written.first().cloned(),
                                message: format!("已导出 {} 个 PDF{warning}", written.len()),
                            });
                            this.status = SharedString::from(format!(
                                "部分 PDF 导出失败: {e}；已导出 {} 个{warning}",
                                written.len()
                            ));
                        }
                    }
                    None => {
                        this.export_result = Some(ExportResult {
                            png_path: None,
                            pdf_path: written.first().cloned(),
                            message: format!("已导出 {} 个 PDF{warning}", written.len()),
                        });
                        this.status = SharedString::from(if let Some(path) = written.first() {
                            format!("已导出 PDF: {}{warning}", path.display())
                        } else {
                            format!("没有可导出的页面{warning}")
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn on_close(&mut self, _: &CloseCollage, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    fn on_image_click(&mut self, idx: usize, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_index = Some(idx);
        cx.notify();
    }

    fn prev_pdf_page(&mut self, _: &PrevPdfPage, cx: &mut Context<Self>) {
        if self.pdf_page_count > 1 && self.pdf_current_page > 0 {
            self.pdf_current_page -= 1;
            let items: Vec<CollageItem> = self.images.iter().map(|e| e.to_collage_item()).collect();
            self.ensure_pdf_preview(&items);
            cx.notify();
        }
    }

    fn next_pdf_page(&mut self, _: &NextPdfPage, cx: &mut Context<Self>) {
        if self.pdf_page_count > 1 && self.pdf_current_page + 1 < self.pdf_page_count {
            self.pdf_current_page += 1;
            let items: Vec<CollageItem> = self.images.iter().map(|e| e.to_collage_item()).collect();
            self.ensure_pdf_preview(&items);
            cx.notify();
        }
    }

    fn cycle_collage_direction(&mut self, _: &CycleCollageDirection, cx: &mut Context<Self>) {
        self.persist_settings(|collage, _| {
            collage.direction = match collage.direction {
                Direction::Vertical => Direction::Horizontal,
                Direction::Horizontal => Direction::Custom,
                Direction::Custom => Direction::Vertical,
            };
        });
        self.recalc_all(cx);
    }

    fn cycle_collage_sizing(&mut self, _: &CycleCollageSizing, cx: &mut Context<Self>) {
        self.persist_settings(|collage, _| {
            collage.sizing = match collage.sizing {
                Sizing::Original => Sizing::Uniform,
                Sizing::Uniform => Sizing::Original,
            };
        });
        self.recalc_all(cx);
    }

    fn step_collage_spacing(&mut self, _: &StepCollageSpacing, cx: &mut Context<Self>) {
        self.persist_settings(|collage, _| {
            collage.spacing = (collage.spacing as i32 + 1).clamp(0, 100) as u32;
        });
        self.recalc_all(cx);
    }

    fn step_collage_border(&mut self, _: &StepCollageBorder, cx: &mut Context<Self>) {
        self.persist_settings(|collage, _| {
            collage.border = (collage.border as i32 + 1).clamp(0, 100) as u32;
        });
        self.recalc_all(cx);
    }

    fn step_collage_rows(&mut self, _: &StepCollageRows, cx: &mut Context<Self>) {
        if self.collage_settings.direction == Direction::Custom {
            self.persist_settings(|collage, _| {
                collage.rows = (collage.rows as i32 + 1).clamp(1, 10) as u32;
            });
            self.recalc_all(cx);
        }
    }

    fn step_collage_columns(&mut self, _: &StepCollageColumns, cx: &mut Context<Self>) {
        if self.collage_settings.direction == Direction::Custom {
            self.persist_settings(|collage, _| {
                collage.columns = (collage.columns as i32 + 1).clamp(1, 10) as u32;
            });
            self.recalc_all(cx);
        }
    }

    fn cycle_pdf_method(&mut self, _: &CyclePdfMethod, cx: &mut Context<Self>) {
        self.persist_settings(|_, pdf| {
            pdf.stitching_method = match pdf.stitching_method {
                StitchingMethod::PageBased => StitchingMethod::TemplateBased,
                StitchingMethod::TemplateBased => StitchingMethod::PageBased,
            };
        });
        self.recalc_all(cx);
    }

    fn cycle_pdf_template(&mut self, _: &CyclePdfTemplate, cx: &mut Context<Self>) {
        self.persist_settings(|_, pdf| {
            pdf.template_type = match pdf.template_type {
                TemplateType::TwoImages => TemplateType::ThreeImages,
                TemplateType::ThreeImages => TemplateType::FourImages,
                TemplateType::FourImages => TemplateType::SixImages,
                TemplateType::SixImages => TemplateType::NineImages,
                TemplateType::NineImages => TemplateType::TwoImages,
            };
        });
        self.recalc_all(cx);
    }

    fn step_pdf_spacing(&mut self, _: &StepPdfSpacing, cx: &mut Context<Self>) {
        self.persist_settings(|_, pdf| {
            pdf.spacing_mm = (pdf.spacing_mm + 1.0).clamp(0.0, 50.0);
        });
        self.recalc_all(cx);
    }

    fn step_pdf_quality(&mut self, _: &StepPdfQuality, cx: &mut Context<Self>) {
        self.persist_settings(|_, pdf| {
            pdf.image_quality = (pdf.image_quality as i32 + 1).clamp(1, 100) as u8;
        });
        cx.notify();
    }

    // Wrapper methods for mouse_down listeners (don't need Window)
    fn on_add_images_click(&mut self, cx: &mut Context<Self>) {
        self.on_add_images(&AddCollageImages, cx);
    }

    fn on_clear_click(&mut self, cx: &mut Context<Self>) {
        self.on_clear(&ClearCollageImages, cx);
    }

    fn cycle_collage_direction_click(&mut self, cx: &mut Context<Self>) {
        self.cycle_collage_direction(&CycleCollageDirection, cx);
    }

    fn cycle_collage_sizing_click(&mut self, cx: &mut Context<Self>) {
        self.cycle_collage_sizing(&CycleCollageSizing, cx);
    }

    fn cycle_pdf_method_click(&mut self, cx: &mut Context<Self>) {
        self.cycle_pdf_method(&CyclePdfMethod, cx);
    }

    fn cycle_pdf_template_click(&mut self, cx: &mut Context<Self>) {
        self.cycle_pdf_template(&CyclePdfTemplate, cx);
    }

    fn step_pdf_spacing_click(&mut self, cx: &mut Context<Self>) {
        self.step_pdf_spacing(&StepPdfSpacing, cx);
    }

    fn step_pdf_spacing_dec_click(&mut self, cx: &mut Context<Self>) {
        self.persist_settings(|_, pdf| {
            pdf.spacing_mm = (pdf.spacing_mm - 1.0).clamp(0.0, 50.0);
        });
        self.recalc_all(cx);
    }

    fn on_export_png_click(&mut self, cx: &mut Context<Self>) {
        self.on_export_png(&ExportCollagePng, cx);
    }

    fn on_export_pdf_click(&mut self, cx: &mut Context<Self>) {
        self.on_export_pdf(&ExportCollagePdf, cx);
    }

    fn prev_pdf_page_click(&mut self, cx: &mut Context<Self>) {
        self.prev_pdf_page(&PrevPdfPage, cx);
    }

    fn next_pdf_page_click(&mut self, cx: &mut Context<Self>) {
        self.next_pdf_page(&NextPdfPage, cx);
    }

    fn titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.images.is_empty() {
            "图片拼图".to_string()
        } else {
            format!("图片拼图 · {} 张", self.images.len())
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .py_2()
            .bg(theme::elevated_surface())
            .border_b_1()
            .border_color(theme::border())
            .child(
                div().flex().items_center().gap_2().child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(title),
                ),
            )
            .child(
                div().flex().items_center().gap_2().child(
                    div()
                        .id("close")
                        .flex_none()
                        .h(px(32.))
                        .px(px(14.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.))
                        .bg(theme::elevated_surface())
                        .border_1()
                        .border_color(theme::border())
                        .text_size(px(12.))
                        .cursor(CursorStyle::PointingHand)
                        .hover(|s| s.bg(theme::selection_wash()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_this, _, window, _cx| {
                                window.remove_window();
                            }),
                        )
                        .child("关闭"),
                ),
            )
    }

    fn controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(320.))
            .flex_col()
            .bg(theme::elevated_surface())
            .border_r_1()
            .border_color(theme::border())
            .child(self.image_list(cx))
            .child(div().h_px().bg(theme::border()))
            .child(self.collage_settings_panel(cx))
            .child(div().h_px().bg(theme::border()))
            .child(self.pdf_settings_panel(cx))
    }

    fn image_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_col()
            .p_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .mb_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("图片列表"),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                div()
                                    .id("add")
                                    .flex_none()
                                    .h(px(32.))
                                    .px(px(14.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(6.))
                                    .bg(theme::elevated_surface())
                                    .border_1()
                                    .border_color(theme::border())
                                    .text_size(px(12.))
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(|s| s.bg(theme::selection_wash()))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _event, _window, cx| {
                                            this.on_add_images_click(cx)
                                        }),
                                    )
                                    .child("添加"),
                            )
                            .child(
                                div()
                                    .id("clear")
                                    .flex_none()
                                    .h(px(32.))
                                    .px(px(14.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(6.))
                                    .bg(theme::elevated_surface())
                                    .border_1()
                                    .border_color(theme::border())
                                    .text_size(px(12.))
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(|s| s.bg(theme::selection_wash()))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _event, _window, cx| {
                                            this.on_clear_click(cx)
                                        }),
                                    )
                                    .child("清空"),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_col()
                    .gap_1()
                    .max_h(px(200.))
                    .overflow_y_scrollbar()
                    .children(self.images.iter().enumerate().map(|(idx, entry)| {
                        let selected = self.selected_index == Some(idx);
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .p_2()
                            .rounded_md()
                            .bg(if selected {
                                theme::selection_wash()
                            } else {
                                theme::transparent()
                            })
                            .border_1()
                            .border_color(if selected {
                                theme::accent()
                            } else {
                                theme::transparent()
                            })
                            .cursor(CursorStyle::PointingHand)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _event, window, cx| {
                                    this.on_image_click(idx, window, cx)
                                }),
                            )
                            .child(
                                div()
                                    .w(px(40.))
                                    .h(px(30.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(theme::base())
                                    .rounded_sm()
                                    .child(
                                        img(ImageSource::Render(entry.image.clone()))
                                            .w(px(38.))
                                            .h(px(28.)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_col()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .truncate()
                                            .child(entry.label.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(theme::muted())
                                            .child(format!("{}×{}", entry.width, entry.height)),
                                    ),
                            )
                            .child(
                                div()
                                    .id("remove")
                                    .flex_none()
                                    .h(px(24.))
                                    .px(px(8.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.))
                                    .bg(theme::base())
                                    .border_1()
                                    .border_color(theme::border())
                                    .text_size(px(12.))
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(|s| s.bg(theme::selection_wash()))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _event, _window, cx| {
                                            this.images.remove(idx);
                                            if this.images.is_empty() {
                                                this.selected_index = None;
                                            } else if idx >= this.images.len() {
                                                this.selected_index = Some(this.images.len() - 1);
                                            }
                                            this.status = SharedString::from("已移除");
                                            this.recalc_all(cx);
                                        }),
                                    )
                                    .child("移除"),
                            )
                    })),
            )
    }

    fn collage_settings_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_3()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .child("静态拼图"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::muted())
                            .child("方向"),
                    )
                    .child(
                        div()
                            .id("cycle-direction")
                            .flex_none()
                            .h(px(28.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.cycle_collage_direction_click(cx)
                                }),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(self.collage_direction_label()),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::muted())
                            .child("尺寸"),
                    )
                    .child(
                        div()
                            .id("cycle-sizing")
                            .flex_none()
                            .h(px(28.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.cycle_collage_sizing_click(cx)
                                }),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(self.collage_sizing_label()),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::muted())
                            .child("间距"),
                    )
                    .child(
                        div()
                            .id("step-spacing-dec")
                            .flex_none()
                            .w(px(24.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.collage_settings.spacing =
                                        (this.collage_settings.spacing as i32 - 1).clamp(0, 100)
                                            as u32;
                                    this.recalc_all(cx);
                                }),
                            )
                            .child("-"),
                    )
                    .child(
                        div()
                            .w(px(50.))
                            .text_center()
                            .text_size(px(12.))
                            .child(self.collage_settings.spacing.to_string()),
                    )
                    .child(
                        div()
                            .id("step-spacing-inc")
                            .flex_none()
                            .w(px(24.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.collage_settings.spacing =
                                        (this.collage_settings.spacing as i32 + 1).clamp(0, 100)
                                            as u32;
                                    this.recalc_all(cx);
                                }),
                            )
                            .child("+"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::muted())
                            .child("边框"),
                    )
                    .child(
                        div()
                            .id("step-border-dec")
                            .flex_none()
                            .w(px(24.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.collage_settings.border =
                                        (this.collage_settings.border as i32 - 1).clamp(0, 100)
                                            as u32;
                                    this.recalc_all(cx);
                                }),
                            )
                            .child("-"),
                    )
                    .child(
                        div()
                            .w(px(50.))
                            .text_center()
                            .text_size(px(12.))
                            .child(self.collage_settings.border.to_string()),
                    )
                    .child(
                        div()
                            .id("step-border-inc")
                            .flex_none()
                            .w(px(24.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.collage_settings.border =
                                        (this.collage_settings.border as i32 + 1).clamp(0, 100)
                                            as u32;
                                    this.recalc_all(cx);
                                }),
                            )
                            .child("+"),
                    ),
            )
            .child(
                div().when(self.collage_settings.direction == Direction::Custom, |d| {
                    d.flex_col()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(theme::muted())
                                        .child("行数"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            div()
                                                .id("step-rows-dec")
                                                .flex_none()
                                                .w(px(24.))
                                                .h(px(24.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded(px(4.))
                                                .bg(theme::base())
                                                .border_1()
                                                .border_color(theme::border())
                                                .text_size(px(12.))
                                                .cursor(CursorStyle::PointingHand)
                                                .hover(|s| s.bg(theme::selection_wash()))
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(|this, _event, _window, cx| {
                                                        this.collage_settings.rows =
                                                            (this.collage_settings.rows as i32 - 1)
                                                                .clamp(1, 10)
                                                                as u32;
                                                        this.recalc_all(cx);
                                                    }),
                                                )
                                                .child("-"),
                                        )
                                        .child(
                                            div()
                                                .w(px(50.))
                                                .text_center()
                                                .text_size(px(12.))
                                                .child(self.collage_settings.rows.to_string()),
                                        )
                                        .child(
                                            div()
                                                .id("step-rows-inc")
                                                .flex_none()
                                                .w(px(24.))
                                                .h(px(24.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded(px(4.))
                                                .bg(theme::base())
                                                .border_1()
                                                .border_color(theme::border())
                                                .text_size(px(12.))
                                                .cursor(CursorStyle::PointingHand)
                                                .hover(|s| s.bg(theme::selection_wash()))
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(|this, _event, _window, cx| {
                                                        this.collage_settings.rows =
                                                            (this.collage_settings.rows as i32 + 1)
                                                                .clamp(1, 10)
                                                                as u32;
                                                        this.recalc_all(cx);
                                                    }),
                                                )
                                                .child("+"),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(theme::muted())
                                        .child("列数"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            div()
                                                .id("step-cols-dec")
                                                .flex_none()
                                                .w(px(24.))
                                                .h(px(24.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded(px(4.))
                                                .bg(theme::base())
                                                .border_1()
                                                .border_color(theme::border())
                                                .text_size(px(12.))
                                                .cursor(CursorStyle::PointingHand)
                                                .hover(|s| s.bg(theme::selection_wash()))
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(|this, _event, _window, cx| {
                                                        this.collage_settings.columns =
                                                            (this.collage_settings.columns as i32
                                                                - 1)
                                                            .clamp(1, 10)
                                                                as u32;
                                                        this.recalc_all(cx);
                                                    }),
                                                )
                                                .child("-"),
                                        )
                                        .child(
                                            div()
                                                .w(px(50.))
                                                .text_center()
                                                .text_size(px(12.))
                                                .child(self.collage_settings.columns.to_string()),
                                        )
                                        .child(
                                            div()
                                                .id("step-cols-inc")
                                                .flex_none()
                                                .w(px(24.))
                                                .h(px(24.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded(px(4.))
                                                .bg(theme::base())
                                                .border_1()
                                                .border_color(theme::border())
                                                .text_size(px(12.))
                                                .cursor(CursorStyle::PointingHand)
                                                .hover(|s| s.bg(theme::selection_wash()))
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(|this, _event, _window, cx| {
                                                        this.collage_settings.columns =
                                                            (this.collage_settings.columns as i32
                                                                + 1)
                                                            .clamp(1, 10)
                                                                as u32;
                                                        this.recalc_all(cx);
                                                    }),
                                                )
                                                .child("+"),
                                        ),
                                ),
                        )
                }),
            )
            .child(div().when(self.collage_layout.is_some(), |d| {
                let layout = self.collage_layout.as_ref().unwrap();
                if layout.is_valid() {
                    d.child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::muted())
                            .child(format!("预览: {}×{}", layout.size.0, layout.size.1)),
                    )
                } else {
                    d.child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::danger())
                            .child("布局无效"),
                    )
                }
            }))
    }

    fn pdf_settings_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_3()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .child("PDF 导出"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::muted())
                            .child("模式"),
                    )
                    .child(
                        div()
                            .id("cycle-pdf-method")
                            .flex_none()
                            .h(px(28.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.cycle_pdf_method_click(cx)
                                }),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(self.pdf_method_label()),
                            ),
                    ),
            )
            .child(div().when(
                matches!(
                    self.pdf_settings.stitching_method,
                    StitchingMethod::TemplateBased
                ),
                |d| {
                    d.flex_col().gap_2().child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::muted())
                                    .child("模板"),
                            )
                            .child(
                                div()
                                    .id("cycle-template")
                                    .flex_none()
                                    .h(px(28.))
                                    .px(px(10.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.))
                                    .bg(theme::base())
                                    .border_1()
                                    .border_color(theme::border())
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(|s| s.bg(theme::selection_wash()))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _event, _window, cx| {
                                            this.cycle_pdf_template_click(cx)
                                        }),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(self.template_label()),
                                    ),
                            ),
                    )
                },
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::muted())
                            .child("间距"),
                    )
                    .child(
                        div()
                            .id("step-pdf-spacing-dec")
                            .flex_none()
                            .w(px(24.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.step_pdf_spacing_dec_click(cx)
                                }),
                            )
                            .child("-"),
                    )
                    .child(
                        div()
                            .w(px(60.))
                            .text_center()
                            .text_size(px(12.))
                            .child(format!("{:.1}", self.pdf_settings.spacing_mm)),
                    )
                    .child(
                        div()
                            .id("step-pdf-spacing-inc")
                            .flex_none()
                            .w(px(24.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.step_pdf_spacing_click(cx)
                                }),
                            )
                            .child("+"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::muted())
                            .child("质量"),
                    )
                    .child(
                        div()
                            .id("step-pdf-quality-dec")
                            .flex_none()
                            .w(px(24.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.pdf_settings.image_quality =
                                        (this.pdf_settings.image_quality as i32 - 1).clamp(1, 100)
                                            as u8;
                                    cx.notify();
                                }),
                            )
                            .child("-"),
                    )
                    .child(
                        div()
                            .w(px(50.))
                            .text_center()
                            .text_size(px(12.))
                            .child(self.pdf_settings.image_quality.to_string()),
                    )
                    .child(
                        div()
                            .id("step-pdf-quality-inc")
                            .flex_none()
                            .w(px(24.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(theme::base())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.pdf_settings.image_quality =
                                        (this.pdf_settings.image_quality as i32 + 1).clamp(1, 100)
                                            as u8;
                                    cx.notify();
                                }),
                            )
                            .child("+"),
                    ),
            )
            .child(div().when(self.pdf_page_count > 0, |d| {
                d.flex_col().gap_2().child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().text_size(px(11.)).child(format!(
                            "第 {} / {} 页",
                            self.pdf_current_page + 1,
                            self.pdf_page_count
                        )))
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .child(
                                    div()
                                        .id("prev-page")
                                        .flex_none()
                                        .h(px(28.))
                                        .px(px(10.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.))
                                        .bg(theme::elevated_surface())
                                        .border_1()
                                        .border_color(theme::border())
                                        .text_size(px(12.))
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|s| s.bg(theme::selection_wash()))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _event, _window, cx| {
                                                this.prev_pdf_page_click(cx)
                                            }),
                                        )
                                        .child("上一页"),
                                )
                                .child(
                                    div()
                                        .id("next-page")
                                        .flex_none()
                                        .h(px(28.))
                                        .px(px(10.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(4.))
                                        .bg(theme::elevated_surface())
                                        .border_1()
                                        .border_color(theme::border())
                                        .text_size(px(12.))
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|s| s.bg(theme::selection_wash()))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _event, _window, cx| {
                                                this.next_pdf_page_click(cx)
                                            }),
                                        )
                                        .child("下一页"),
                                ),
                        ),
                )
            }))
    }

    fn preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_1()
            .flex_col()
            .items_center()
            .justify_center()
            .p_4()
            .bg(theme::base())
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(self.collage_preview_panel(cx))
                    .child(self.pdf_preview_panel(cx)),
            )
    }

    fn collage_preview_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(PREVIEW_MAX_W))
            .flex_col()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .child("静态拼图预览"),
            )
            .child(
                div()
                    .w_full()
                    .aspect_ratio(1.5)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::base())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .child(
                        self.collage_preview
                            .as_ref()
                            .map(|render_img| {
                                img(ImageSource::Render(render_img.clone()))
                                    .w_full()
                                    .h_full()
                                    .object_fit(gpui::ObjectFit::Contain)
                                    .into_any_element()
                            })
                            .unwrap_or_else(|| {
                                div()
                                    .text_color(theme::muted())
                                    .child("无预览")
                                    .into_any_element()
                            }),
                    ),
            )
            .child(div().when(self.collage_layout.is_some(), |d| {
                let layout = self.collage_layout.as_ref().unwrap();
                if layout.is_valid() {
                    d.child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::muted())
                            .child(format!(
                                "{}×{} · {} 张",
                                layout.size.0,
                                layout.size.1,
                                self.images.len()
                            )),
                    )
                } else {
                    d.child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::danger())
                            .child("布局无效"),
                    )
                }
            }))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("export-collage-png")
                            .flex_none()
                            .h(px(32.))
                            .px(px(14.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .bg(theme::elevated_surface())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(12.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.on_export_png_click(cx)
                                }),
                            )
                            .child("导出 PNG"),
                    )
                    .child(
                        div()
                            .id("export-collage-pdf")
                            .flex_none()
                            .h(px(32.))
                            .px(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .bg(theme::accent())
                            .text_color(theme::primary_fg())
                            .text_size(px(12.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::accent_hover()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.on_export_pdf_click(cx)
                                }),
                            )
                            .child("导出 PDF"),
                    ),
            )
    }

    fn pdf_preview_panel(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(PREVIEW_MAX_W))
            .flex_col()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .child("PDF 页面预览"),
            )
            .child(
                div()
                    .w_full()
                    .aspect_ratio(1.414) // A4 ratio
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::base())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .child(if self.pdf_page_count > 0 {
                        self.pdf_preview()
                            .map(|render_img| {
                                img(ImageSource::Render(render_img.clone()))
                                    .w_full()
                                    .h_full()
                                    .object_fit(gpui::ObjectFit::Contain)
                                    .into_any_element()
                            })
                            .unwrap_or_else(|| {
                                div()
                                    .text_color(theme::muted())
                                    .child("渲染中...")
                                    .into_any_element()
                            })
                    } else {
                        div()
                            .text_color(theme::muted())
                            .child("添加图片生成 PDF")
                            .into_any_element()
                    }),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::muted())
                    .child(format!("{} 页", self.pdf_page_count)),
            )
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let export_result = self.export_result.clone();
        div()
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .py_2()
            .bg(theme::elevated_surface())
            .border_t_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::muted())
                    .child(self.status.clone()),
            )
            .child(div().when(export_result.is_some(), |d| {
                let res = export_result.as_ref().unwrap();
                d.flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::success())
                            .child(res.message.clone()),
                    )
                    .child(
                        div()
                            .when(res.png_path.is_some(), |d| {
                                let parent = res
                                    .png_path
                                    .as_ref()
                                    .and_then(|p| p.parent().map(|p| p.to_path_buf()));
                                d.child(
                                    div()
                                        .id("open-folder-png")
                                        .flex_none()
                                        .h(px(32.))
                                        .px(px(14.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(6.))
                                        .bg(theme::elevated_surface())
                                        .border_1()
                                        .border_color(theme::border())
                                        .text_size(px(12.))
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|s| s.bg(theme::selection_wash()))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |_, _, _window, _cx| {
                                                if let Some(ref path) = parent {
                                                    let _ =
                                                        crate::settings::system::open_folder(path);
                                                }
                                            }),
                                        )
                                        .child("打开文件夹"),
                                )
                            })
                            .when(res.pdf_path.is_some(), |d| {
                                let parent = res
                                    .pdf_path
                                    .as_ref()
                                    .and_then(|p| p.parent().map(|p| p.to_path_buf()));
                                d.child(
                                    div()
                                        .id("open-folder-pdf")
                                        .flex_none()
                                        .h(px(32.))
                                        .px(px(14.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(6.))
                                        .bg(theme::elevated_surface())
                                        .border_1()
                                        .border_color(theme::border())
                                        .text_size(px(12.))
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|s| s.bg(theme::selection_wash()))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |_, _, _window, _cx| {
                                                if let Some(ref path) = parent {
                                                    let _ =
                                                        crate::settings::system::open_folder(path);
                                                }
                                            }),
                                        )
                                        .child("打开文件夹"),
                                )
                            }),
                    )
            }))
    }

    fn collage_direction_label(&self) -> String {
        match self.collage_settings.direction {
            Direction::Vertical => "垂直".into(),
            Direction::Horizontal => "水平".into(),
            Direction::Custom => "网格".into(),
        }
    }

    fn collage_sizing_label(&self) -> String {
        match self.collage_settings.sizing {
            Sizing::Original => "原始".into(),
            Sizing::Uniform => "统一".into(),
        }
    }

    fn pdf_method_label(&self) -> String {
        match self.pdf_settings.stitching_method {
            StitchingMethod::PageBased => "自动分页".into(),
            StitchingMethod::TemplateBased => "模板".into(),
        }
    }

    fn template_label(&self) -> String {
        format!("{} 张", self.pdf_settings.template_type.capacity())
    }
}

impl Focusable for CollageWindowView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CollageWindowView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _action: &CloseCollage, window: &mut Window, cx| {
                    this.on_close(_action, window, cx)
                }),
            )
            .on_action(cx.listener(
                |this, _action: &ExportCollagePng, _window: &mut Window, cx| {
                    this.on_export_png(_action, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _action: &ExportCollagePdf, _window: &mut Window, cx| {
                    this.on_export_pdf(_action, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _action: &OpenCollageImage, window: &mut Window, cx| {
                    this.on_open_image(_action, window, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _action: &AddCollageImages, _window: &mut Window, cx| {
                    this.on_add_images(_action, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _action: &RemoveCollageImage, _window: &mut Window, cx| {
                    this.on_remove_selected(_action, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _action: &ClearCollageImages, _window: &mut Window, cx| {
                    this.on_clear(_action, cx)
                },
            ))
            .on_action(
                cx.listener(|this, _action: &PrevPdfPage, _window: &mut Window, cx| {
                    this.prev_pdf_page(_action, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _action: &NextPdfPage, _window: &mut Window, cx| {
                    this.next_pdf_page(_action, cx)
                }),
            )
            .on_action(cx.listener(
                |this, _action: &CycleCollageDirection, _window: &mut Window, cx| {
                    this.cycle_collage_direction(_action, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _action: &CycleCollageSizing, _window: &mut Window, cx| {
                    this.cycle_collage_sizing(_action, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _action: &StepCollageSpacing, _window: &mut Window, cx| {
                    this.step_collage_spacing(_action, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _action: &StepCollageBorder, _window: &mut Window, cx| {
                    this.step_collage_border(_action, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _action: &StepCollageRows, _window: &mut Window, cx| {
                    this.step_collage_rows(_action, cx)
                },
            ))
            .on_action(cx.listener(
                |this, _action: &StepCollageColumns, _window: &mut Window, cx| {
                    this.step_collage_columns(_action, cx)
                },
            ))
            .on_action(
                cx.listener(|this, _action: &CyclePdfMethod, _window: &mut Window, cx| {
                    this.cycle_pdf_method(_action, cx)
                }),
            )
            .on_action(cx.listener(
                |this, _action: &CyclePdfTemplate, _window: &mut Window, cx| {
                    this.cycle_pdf_template(_action, cx)
                },
            ))
            .on_action(
                cx.listener(|this, _action: &StepPdfSpacing, _window: &mut Window, cx| {
                    this.step_pdf_spacing(_action, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _action: &StepPdfQuality, _window: &mut Window, cx| {
                    this.step_pdf_quality(_action, cx)
                }),
            )
            .flex_col()
            .size_full()
            .bg(theme::base())
            .child(self.titlebar(cx))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .child(self.controls(cx))
                    .child(self.preview(cx)),
            )
            .child(self.footer(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_temp_path_is_a_sibling_that_keeps_the_original() {
        let target = Path::new("out").join("FastPin_Collage_1.pdf");
        let temp = pdf_temp_path(&target);
        assert_eq!(temp, Path::new("out").join("FastPin_Collage_1.pdf.tmp"));
        assert_eq!(target, Path::new("out").join("FastPin_Collage_1.pdf"));
    }

    #[test]
    fn skipped_warning_is_empty_only_when_nothing_was_skipped() {
        assert_eq!(pdf_skipped_warning(0), "");
        assert_eq!(pdf_skipped_warning(2), "（已跳过 2 张无法嵌入的图片）");
    }

    #[test]
    fn png_export_path_adds_numbered_suffix_on_collision() {
        use std::collections::HashSet;
        let dir = Path::new("out");
        let stem = "FastPin_Collage_20260101_120000";
        let none: HashSet<PathBuf> = HashSet::new();
        assert_eq!(
            unique_png_export_path(dir, stem, |p| none.contains(p)),
            dir.join(format!("{stem}.png")),
            "首次导出应保持原名"
        );
        let one: HashSet<PathBuf> = [dir.join(format!("{stem}.png"))].into_iter().collect();
        assert_eq!(
            unique_png_export_path(dir, stem, |p| one.contains(p)),
            dir.join(format!("{stem} (1).png")),
            "第一次冲突应使用 (1) 后缀"
        );
        let two: HashSet<PathBuf> = [
            dir.join(format!("{stem}.png")),
            dir.join(format!("{stem} (1).png")),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            unique_png_export_path(dir, stem, |p| two.contains(p)),
            dir.join(format!("{stem} (2).png")),
            "连续冲突应递增后缀"
        );
    }
}
