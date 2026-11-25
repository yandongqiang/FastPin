//! Image collage and PDF export, ported from snow-shot's `ImageStitching`.
//!
//! Pure layout/geometry logic — no GPU, no GPUI, no global config. All functions
//! are deterministic and unit-testable without a display.

use std::collections::HashMap;

/// Paper sizes in millimetres (width, height).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaperSize {
    A3,
    A4,
    A5,
    B5,
    Letter,
    Legal,
    Custom(u32, u32), // width_mm, height_mm
    ImageSize,        // match the composed image size
}

impl PaperSize {
    pub fn dimensions_mm(self) -> (f32, f32) {
        match self {
            PaperSize::A3 => (297.0, 420.0),
            PaperSize::A4 => (210.0, 297.0),
            PaperSize::A5 => (148.0, 210.0),
            PaperSize::B5 => (176.0, 250.0),
            PaperSize::Letter => (215.9, 279.4),
            PaperSize::Legal => (215.9, 355.6),
            PaperSize::Custom(w, h) => (w as f32, h as f32),
            PaperSize::ImageSize => (0.0, 0.0), // handled specially
        }
    }
}

/// PDF page orientation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Portrait,
    Landscape,
}

impl Orientation {
    pub fn apply(self, w: f32, h: f32) -> (f32, f32) {
        match self {
            Orientation::Portrait => (w, h),
            Orientation::Landscape => (h, w),
        }
    }
}

/// Export mode: one combined PDF or one PDF per page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportMode {
    Combined,
    PerPage,
}

/// Stitching method for PDF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StitchingMethod {
    PageBased,     // automatic grid per page
    TemplateBased, // fixed template (2/3/4/6/9 images)
}

/// Template types for TemplateBased stitching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplateType {
    TwoImages,
    ThreeImages,
    FourImages,
    SixImages,
    NineImages,
}

impl TemplateType {
    /// Capacity of each template (max images per page).
    pub fn capacity(self) -> usize {
        match self {
            TemplateType::TwoImages => 2,
            TemplateType::ThreeImages => 3,
            TemplateType::FourImages => 4,
            TemplateType::SixImages => 6,
            TemplateType::NineImages => 9,
        }
    }
}

/// PDF export settings.
#[derive(Clone, Debug, PartialEq)]
pub struct PdfSettings {
    pub stitching_method: StitchingMethod,
    pub template_type: TemplateType,
    pub spacing_mm: f32,
    pub paper_size: PaperSize,
    pub orientation: Orientation,
    pub margin_top_mm: f32,
    pub margin_bottom_mm: f32,
    pub margin_left_mm: f32,
    pub margin_right_mm: f32,
    pub export_mode: ExportMode,
    pub image_quality: u8, // 1..100
}

/// Items per page for the automatic ("page based") grid. The layout below is a
/// fixed 3×3 grid, so capacity is 9 — spelling it out keeps page counts and the
/// drawn cells in agreement.
pub const PAGE_BASED_CAPACITY: usize = 9;

impl Default for PdfSettings {
    fn default() -> Self {
        Self {
            stitching_method: StitchingMethod::PageBased,
            template_type: TemplateType::TwoImages,
            spacing_mm: 5.0,
            paper_size: PaperSize::A4,
            orientation: Orientation::Portrait,
            margin_top_mm: 10.0,
            margin_bottom_mm: 10.0,
            margin_left_mm: 10.0,
            margin_right_mm: 10.0,
            export_mode: ExportMode::Combined,
            image_quality: 90,
        }
    }
}

impl PdfSettings {
    /// Page size in mm. `ImageSize` sizes the page to a 3×3 grid of the
    /// largest items, plus margins, so it never yields a zero-sized page.
    pub fn page_size_mm(&self, items: &[CollageItem]) -> (f32, f32) {
        let (paper_w, paper_h) = match self.paper_size {
            PaperSize::ImageSize => image_size_page_mm(items, self.spacing_mm),
            other => other.dimensions_mm(),
        };
        self.orientation.apply(paper_w, paper_h)
    }
}

/// A 3×3 grid of the largest item, rendered at 96 dpi.
fn image_size_page_mm(items: &[CollageItem], spacing_mm: f32) -> (f32, f32) {
    const MM_PER_PX: f32 = 25.4 / 96.0;
    let max_w = items
        .iter()
        .map(|i| i.transformed_size().0)
        .max()
        .unwrap_or(0) as f32;
    let max_h = items
        .iter()
        .map(|i| i.transformed_size().1)
        .max()
        .unwrap_or(0) as f32;
    (
        max_w * MM_PER_PX * 3.0 + spacing_mm * 2.0,
        max_h * MM_PER_PX * 3.0 + spacing_mm * 2.0,
    )
}

/// Collage direction for static image stitching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Vertical,
    Horizontal,
    Custom,
}

/// Sizing mode for static image stitching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sizing {
    Original,
    Uniform,
}

/// A rectangle in millimetres (for PDF layout).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectMm {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl RectMm {
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

/// A rectangle in pixels (for static image layout).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectPx {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl RectPx {
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// An item in the collage: an image with optional transform and label.
#[derive(Clone, Debug)]
pub struct CollageItem {
    pub id: u64,
    pub label: String,
    pub width: u32,
    pub height: u32,
    /// Optional user transform (rotation/flip). For now we only track
    /// 90° rotations and horizontal flips as they affect dimensions.
    pub rotation: i8, // 0, 1, 2, 3 (×90° clockwise)
    pub flipped_h: bool,
}

impl CollageItem {
    /// Transformed dimensions in pixels.
    pub fn transformed_size(&self) -> (u32, u32) {
        // A horizontal flip changes no dimensions, so only the rotation
        // matters here.
        if self.rotation % 2 == 0 {
            (self.width, self.height)
        } else {
            (self.height, self.width)
        }
    }
}

/// Static collage settings.
#[derive(Clone, Debug, PartialEq)]
pub struct CollageSettings {
    pub direction: Direction,
    pub sizing: Sizing,
    pub border: u32,
    pub spacing: u32,
    pub background_rgba: [u8; 4], // RGBA
    pub rows: u32,
    pub columns: u32,
}

impl Default for CollageSettings {
    fn default() -> Self {
        Self {
            direction: Direction::Vertical,
            // Matches the settings default; the two must not drift because the
            // settings file is the source of truth for the window.
            sizing: Sizing::Uniform,
            border: 0,
            spacing: 0,
            background_rgba: [255, 255, 255, 255],
            rows: 1,
            columns: 1,
        }
    }
}

/// Static collage layout result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CollageLayout {
    pub size: (u32, u32),
    /// 实际排布行数：自定义网格会按图片数扩张，不再受配置行数上限约束。
    pub rows: u32,
    pub rects: Vec<RectPx>,
    pub error: String,
}

impl CollageLayout {
    pub fn is_valid(&self) -> bool {
        self.size.0 > 0 && self.size.1 > 0 && !self.rects.is_empty() && self.error.is_empty()
    }
}

/// A cell in a PDF page.
#[derive(Clone, Debug, PartialEq)]
pub struct PdfCell {
    pub item_id: u64,
    pub item_index: usize,
    pub clip_mm: RectMm,
    pub image_mm: RectMm,
}

/// A composed PDF page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PdfComposedPage {
    pub page_mm: (f32, f32),
    pub content_mm: RectMm,
    pub cells: Vec<PdfCell>,
    pub error: String,
}

impl PdfComposedPage {
    pub fn is_valid(&self) -> bool {
        self.page_mm.0 > 0.0
            && self.page_mm.1 > 0.0
            && !self.content_mm.is_empty()
            && !self.cells.is_empty()
            && self.error.is_empty()
    }
}

/// Page layout for PDF (page + content area in mm).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PdfPageLayout {
    pub page_mm: (f32, f32),
    pub content_mm: RectMm,
    pub image_mm: RectMm,
    pub error: String,
    pub field_errors: HashMap<String, String>,
}

impl PdfPageLayout {
    pub fn is_valid(&self) -> bool {
        self.page_mm.0 > 0.0
            && self.page_mm.1 > 0.0
            && !self.content_mm.is_empty()
            && !self.image_mm.is_empty()
            && self.error.is_empty()
            && self.field_errors.is_empty()
    }
}

/// Validate PDF settings. Returns field_errors map (empty if valid).
pub fn validate_pdf_settings(settings: &PdfSettings) -> HashMap<String, String> {
    let mut errors = HashMap::new();

    if settings.spacing_mm < 0.0 {
        errors.insert("spacing_mm".into(), "spacing must be non-negative".into());
    }
    if settings.margin_top_mm < 0.0 {
        errors.insert("margin_top_mm".into(), "margin must be non-negative".into());
    }
    if settings.margin_bottom_mm < 0.0 {
        errors.insert(
            "margin_bottom_mm".into(),
            "margin must be non-negative".into(),
        );
    }
    if settings.margin_left_mm < 0.0 {
        errors.insert(
            "margin_left_mm".into(),
            "margin must be non-negative".into(),
        );
    }
    if settings.margin_right_mm < 0.0 {
        errors.insert(
            "margin_right_mm".into(),
            "margin must be non-negative".into(),
        );
    }
    if settings.image_quality == 0 || settings.image_quality > 100 {
        errors.insert("image_quality".into(), "quality must be 1..100".into());
    }
    if matches!(settings.paper_size, PaperSize::Custom(w, h) if w == 0 || h == 0) {
        errors.insert(
            "custom_size".into(),
            "custom width/height must be > 0".into(),
        );
    }

    // Content area must be positive. `ImageSize` derives its page from the
    // items, so its size is checked when the page is composed instead.
    if !matches!(settings.paper_size, PaperSize::ImageSize) {
        let (paper_w, paper_h) = settings.paper_size.dimensions_mm();
        let (page_w, page_h) = settings.orientation.apply(paper_w, paper_h);
        let content_w = page_w - settings.margin_left_mm - settings.margin_right_mm;
        let content_h = page_h - settings.margin_top_mm - settings.margin_bottom_mm;
        if content_w <= 0.0 {
            errors.insert(
                "margins".into(),
                "horizontal margins exceed page width".into(),
            );
        }
        if content_h <= 0.0 {
            errors.insert(
                "margins".into(),
                "vertical margins exceed page height".into(),
            );
        }
    }

    errors
}

/// Compute page layout (page size, content area, image area) from settings and item count.
pub fn calculate_pdf_page_layout(items: &[CollageItem], settings: &PdfSettings) -> PdfPageLayout {
    let mut layout = PdfPageLayout::default();

    let errors = validate_pdf_settings(settings);
    if !errors.is_empty() {
        layout.error = "invalid settings".into();
        layout.field_errors = errors;
        return layout;
    }

    let (page_w, page_h) = settings.page_size_mm(items);

    layout.page_mm = (page_w, page_h);

    // Content area = page minus margins.
    layout.content_mm = RectMm {
        x: settings.margin_left_mm,
        y: settings.margin_top_mm,
        width: page_w - settings.margin_left_mm - settings.margin_right_mm,
        height: page_h - settings.margin_top_mm - settings.margin_bottom_mm,
    };

    // Image area starts equal to content area; cells will subdivide it.
    layout.image_mm = layout.content_mm;

    layout
}

/// Calculate number of PDF pages needed.
pub fn pdf_page_count(item_count: usize, settings: &PdfSettings) -> usize {
    if item_count == 0 {
        return 0;
    }
    if !validate_pdf_settings(settings).is_empty() {
        return 0;
    }

    match settings.stitching_method {
        StitchingMethod::PageBased => item_count.div_ceil(PAGE_BASED_CAPACITY),
        StitchingMethod::TemplateBased => {
            let capacity = settings.template_type.capacity();
            item_count.div_ceil(capacity)
        }
    }
}

/// Calculate composed page for a given page index.
pub fn calculate_pdf_composed_page(
    items: &[CollageItem],
    settings: &PdfSettings,
    page_index: usize,
) -> PdfComposedPage {
    let mut page = PdfComposedPage::default();

    let layout = calculate_pdf_page_layout(items, settings);
    if !layout.is_valid() {
        page.error = "invalid page layout".into();
        return page;
    }

    let (page_w, page_h) = layout.page_mm;
    page.page_mm = (page_w, page_h);
    page.content_mm = layout.content_mm;

    match settings.stitching_method {
        StitchingMethod::PageBased => {
            compute_page_based_layout(items, settings, &layout, page_index, &mut page);
        }
        StitchingMethod::TemplateBased => {
            compute_template_based_layout(items, settings, &layout, page_index, &mut page);
        }
    }

    page
}

/// 宽或高为 0（或无穷/NaN）时宽高比会变成 NaN/inf，接着 `h * img_aspect`
/// 会把 NaN 几何写进 `PdfCell`，整页布局都失效。退化成 1:1 方形排布，既
/// 不产生 NaN，也不会丢掉这张图片。
fn safe_aspect(w: f32, h: f32) -> f32 {
    if w == 0.0 || h == 0.0 || !w.is_finite() || !h.is_finite() {
        1.0
    } else {
        w / h
    }
}

fn compute_page_based_layout(
    items: &[CollageItem],
    settings: &PdfSettings,
    layout: &PdfPageLayout,
    page_index: usize,
    page: &mut PdfComposedPage,
) {
    // Fixed 3×3 grid with spacing (see `PAGE_BASED_CAPACITY`).
    let spacing = settings.spacing_mm;
    let content = layout.content_mm;
    let cols = 3;
    let rows = 3;

    let cell_w = (content.width - spacing * (cols as f32 - 1.0)) / cols as f32;
    let cell_h = (content.height - spacing * (rows as f32 - 1.0)) / rows as f32;

    let start = page_index * (cols * rows);
    let end = (start + cols * rows).min(items.len());

    for (idx, item) in items[start..end].iter().enumerate() {
        let col = idx % cols;
        let row = idx / cols;

        let x = content.x + col as f32 * (cell_w + spacing);
        let y = content.y + row as f32 * (cell_h + spacing);

        // Image fills the cell with aspect preservation.
        let (img_w, img_h) = item.transformed_size();
        let img_aspect = safe_aspect(img_w as f32, img_h as f32);
        let cell_aspect = safe_aspect(cell_w, cell_h);

        let (draw_w, draw_h, draw_x, draw_y) = if img_aspect > cell_aspect {
            // Image wider than cell: fit width.
            let w = cell_w;
            let h = w / img_aspect;
            (w, h, x, y + (cell_h - h) / 2.0)
        } else {
            // Image taller than cell: fit height.
            let h = cell_h;
            let w = h * img_aspect;
            (w, h, x + (cell_w - w) / 2.0, y)
        };

        page.cells.push(PdfCell {
            item_id: item.id,
            item_index: start + idx,
            clip_mm: RectMm {
                x,
                y,
                width: cell_w,
                height: cell_h,
            },
            image_mm: RectMm {
                x: draw_x,
                y: draw_y,
                width: draw_w,
                height: draw_h,
            },
        });
    }
}

fn compute_template_based_layout(
    items: &[CollageItem],
    settings: &PdfSettings,
    layout: &PdfPageLayout,
    page_index: usize,
    page: &mut PdfComposedPage,
) {
    // Simplified template layout: just place items in a row/column within content area.
    // Snow-shot has specific templates; we do a basic grid.
    let capacity = settings.template_type.capacity();
    let start = page_index * capacity;
    let end = (start + capacity).min(items.len());
    let count = end - start;

    if count == 0 {
        return;
    }

    let content = layout.content_mm;
    let spacing = settings.spacing_mm;

    // Arrange in a single row for 2, grid for 3+, column for 4/6/9
    let (cols, rows) = match count {
        2 => (2, 1),
        3 => (3, 1),
        4 => (2, 2),
        6 => (3, 2),
        9 => (3, 3),
        _ => (count, 1),
    };

    let cell_w = (content.width - spacing * (cols as f32 - 1.0)) / cols as f32;
    let cell_h = (content.height - spacing * (rows as f32 - 1.0)) / rows as f32;

    for (idx, item) in items[start..end].iter().enumerate() {
        let col = idx % cols;
        let row = idx / cols;

        let x = content.x + col as f32 * (cell_w + spacing);
        let y = content.y + row as f32 * (cell_h + spacing);

        let (img_w, img_h) = item.transformed_size();
        let img_aspect = safe_aspect(img_w as f32, img_h as f32);
        let cell_aspect = safe_aspect(cell_w, cell_h);

        let (draw_w, draw_h, draw_x, draw_y) = if img_aspect > cell_aspect {
            let w = cell_w;
            let h = w / img_aspect;
            (w, h, x, y + (cell_h - h) / 2.0)
        } else {
            let h = cell_h;
            let w = h * img_aspect;
            (w, h, x + (cell_w - w) / 2.0, y)
        };

        page.cells.push(PdfCell {
            item_id: item.id,
            item_index: start + idx,
            clip_mm: RectMm {
                x,
                y,
                width: cell_w,
                height: cell_h,
            },
            image_mm: RectMm {
                x: draw_x,
                y: draw_y,
                width: draw_w,
                height: draw_h,
            },
        });
    }
}

/// `value * numerator / denominator`, rounded to nearest, saturating at
/// `u32::MAX` and never below 1. Used to keep uniform rows visible.
fn scale_round(value: u32, numerator: u32, denominator: u32) -> u32 {
    if denominator == 0 {
        return value.max(1);
    }
    let scaled = (value as u64 * numerator as u64 + denominator as u64 / 2) / denominator as u64;
    scaled.clamp(1, u32::MAX as u64) as u32
}

/// Static collage layout calculation.
pub fn calculate_collage_layout(
    items: &[CollageItem],
    settings: &CollageSettings,
) -> CollageLayout {
    let mut layout = CollageLayout::default();

    if items.is_empty() {
        layout.error = "no items".into();
        return layout;
    }

    let uniform = settings.sizing == Sizing::Uniform;

    match settings.direction {
        Direction::Vertical => {
            let max_w = items
                .iter()
                .map(|i| i.transformed_size().0)
                .max()
                .unwrap_or(0);
            let sizes: Vec<(u32, u32)> = items
                .iter()
                .map(|i| {
                    let (w, h) = i.transformed_size();
                    if uniform && w > 0 {
                        (max_w, scale_round(h, max_w, w))
                    } else {
                        (w, h)
                    }
                })
                .collect();
            let total_h = sizes
                .iter()
                .fold(0u32, |acc, (_, h)| acc.saturating_add(*h));
            let spacing_total = settings
                .spacing
                .saturating_mul(items.len().saturating_sub(1) as u32);
            let border2 = settings.border.saturating_mul(2);
            let h = total_h
                .saturating_add(spacing_total)
                .saturating_add(border2);

            let mut y = settings.border;
            layout.rows = items.len() as u32;
            for (w, item_h) in sizes {
                let x = settings.border.saturating_add((max_w - w) / 2);
                layout.rects.push(RectPx {
                    x,
                    y,
                    width: w,
                    height: item_h,
                });
                y = y.saturating_add(item_h).saturating_add(settings.spacing);
            }
            layout.size = (max_w.saturating_add(border2), h);
        }
        Direction::Horizontal => {
            let max_h = items
                .iter()
                .map(|i| i.transformed_size().1)
                .max()
                .unwrap_or(0);
            let sizes: Vec<(u32, u32)> = items
                .iter()
                .map(|i| {
                    let (w, h) = i.transformed_size();
                    if uniform && h > 0 {
                        (scale_round(w, max_h, h), max_h)
                    } else {
                        (w, h)
                    }
                })
                .collect();
            let total_w = sizes
                .iter()
                .fold(0u32, |acc, (w, _)| acc.saturating_add(*w));
            let spacing_total = settings
                .spacing
                .saturating_mul(items.len().saturating_sub(1) as u32);
            let border2 = settings.border.saturating_mul(2);
            let w = total_w
                .saturating_add(spacing_total)
                .saturating_add(border2);

            let mut x = settings.border;
            layout.rows = 1;
            for (item_w, h) in sizes {
                let y = settings.border.saturating_add((max_h - h) / 2);
                layout.rects.push(RectPx {
                    x,
                    y,
                    width: item_w,
                    height: h,
                });
                x = x.saturating_add(item_w).saturating_add(settings.spacing);
            }
            layout.size = (w, max_h.saturating_add(border2));
        }
        Direction::Custom => {
            // Grid: rows × columns. 图片数超过 rows×columns 时按实际需要的
            // 行数扩张画布，否则多出来的图会落到画布之外，在预览和导出中
            // 被 blit_image 静默裁掉。
            let cols = settings.columns.max(1);
            let cols_usize = cols as usize;
            let needed_rows = u32::try_from(items.len().div_ceil(cols_usize)).unwrap_or(u32::MAX);
            let rows = settings.rows.max(1).max(needed_rows);
            layout.rows = rows;

            // Compute max cell size.
            let max_cell_w = items
                .iter()
                .map(|i| i.transformed_size().0)
                .max()
                .unwrap_or(0);
            let max_cell_h = items
                .iter()
                .map(|i| i.transformed_size().1)
                .max()
                .unwrap_or(0);

            let border2 = settings.border.saturating_mul(2);
            let w = max_cell_w
                .saturating_mul(cols)
                // spacing * (n-1) 手改配置时可能回绕成很小的值，进而让拼版
                // 越界；这里按饱和乘法算，让尺寸顶到上限而不是绕回去。
                .saturating_add(settings.spacing.saturating_mul(cols - 1))
                .saturating_add(border2);
            let h = max_cell_h
                .saturating_mul(rows)
                .saturating_add(settings.spacing.saturating_mul(rows - 1))
                .saturating_add(border2);

            for (idx, item) in items.iter().enumerate() {
                let col = (idx % cols_usize) as u32;
                let row = (idx / cols_usize) as u32;
                let (mut item_w, mut item_h) = item.transformed_size();
                if uniform && item_w > 0 && item_h > 0 {
                    // Fit inside the cell without distorting the aspect ratio.
                    let scale =
                        (max_cell_w as f32 / item_w as f32).min(max_cell_h as f32 / item_h as f32);
                    item_w = ((item_w as f32 * scale).round() as u32).max(1);
                    item_h = ((item_h as f32 * scale).round() as u32).max(1);
                }
                // 手改配置里的 spacing 可能是 u32::MAX；饱和运算让格子顶到
                // 上限而不是在 debug 下溢出 panic / release 下回绕越界。
                let x = settings
                    .border
                    .saturating_add(col.saturating_mul(max_cell_w.saturating_add(settings.spacing)))
                    .saturating_add((max_cell_w - item_w) / 2);
                let y = settings
                    .border
                    .saturating_add(row.saturating_mul(max_cell_h.saturating_add(settings.spacing)))
                    .saturating_add((max_cell_h - item_h) / 2);
                layout.rects.push(RectPx {
                    x,
                    y,
                    width: item_w,
                    height: item_h,
                });
            }
            layout.size = (w, h);
        }
    }

    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64, w: u32, h: u32, label: &str) -> CollageItem {
        CollageItem {
            id,
            label: label.into(),
            width: w,
            height: h,
            rotation: 0,
            flipped_h: false,
        }
    }

    #[test]
    fn vertical_stack_computes_correct_size() {
        let items = [item(1, 200, 100, "a"), item(2, 200, 150, "b")];
        let settings = CollageSettings {
            direction: Direction::Vertical,
            spacing: 10,
            border: 5,
            ..Default::default()
        };
        let layout = calculate_collage_layout(&items, &settings);
        assert_eq!(layout.size.0, 210); // 200 + 5*2
        assert_eq!(layout.size.1, 270); // 100 + 150 + 10 + 5*2
        assert_eq!(
            layout.rects[0],
            RectPx {
                x: 5,
                y: 5,
                width: 200,
                height: 100
            }
        );
        assert_eq!(
            layout.rects[1],
            RectPx {
                x: 5,
                y: 115,
                width: 200,
                height: 150
            }
        );
    }

    #[test]
    fn horizontal_stack_computes_correct_size() {
        let items = [item(1, 100, 200, "a"), item(2, 150, 200, "b")];
        let settings = CollageSettings {
            direction: Direction::Horizontal,
            spacing: 10,
            border: 5,
            ..Default::default()
        };
        let layout = calculate_collage_layout(&items, &settings);
        assert_eq!(layout.size.0, 270); // 100 + 150 + 10 + 5*2
        assert_eq!(layout.size.1, 210); // 200 + 5*2
        assert_eq!(
            layout.rects[0],
            RectPx {
                x: 5,
                y: 5,
                width: 100,
                height: 200
            }
        );
        assert_eq!(
            layout.rects[1],
            RectPx {
                x: 115,
                y: 5,
                width: 150,
                height: 200
            }
        );
    }

    #[test]
    fn grid_custom_arranges_in_rows_and_columns() {
        let items = [
            item(1, 100, 100, "a"),
            item(2, 100, 100, "b"),
            item(3, 100, 100, "c"),
            item(4, 100, 100, "d"),
        ];
        let settings = CollageSettings {
            direction: Direction::Custom,
            rows: 2,
            columns: 2,
            spacing: 10,
            border: 5,
            ..Default::default()
        };
        let layout = calculate_collage_layout(&items, &settings);
        // 2×2 grid: 100+10+100 + 5*2 = 220 wide, same high.
        assert_eq!(layout.size, (220, 220));
        assert_eq!(
            layout.rects[0],
            RectPx {
                x: 5,
                y: 5,
                width: 100,
                height: 100
            }
        );
        assert_eq!(
            layout.rects[1],
            RectPx {
                x: 115,
                y: 5,
                width: 100,
                height: 100
            }
        );
        assert_eq!(
            layout.rects[2],
            RectPx {
                x: 5,
                y: 115,
                width: 100,
                height: 100
            }
        );
        assert_eq!(
            layout.rects[3],
            RectPx {
                x: 115,
                y: 115,
                width: 100,
                height: 100
            }
        );
    }

    /// 图片数超过 rows×columns 时行数和画布高度必须跟着扩张，保证每张图
    /// 都落在画布内，不会被预览/导出的 blit_image 静默裁掉。
    #[test]
    fn custom_grid_expands_rows_when_items_exceed_capacity() {
        let items: Vec<CollageItem> = (0..5).map(|i| item(i, 100, 100, "x")).collect();
        let settings = CollageSettings {
            direction: Direction::Custom,
            rows: 2,
            columns: 2,
            spacing: 10,
            border: 5,
            ..Default::default()
        };
        let layout = calculate_collage_layout(&items, &settings);
        assert_eq!(layout.rows, 3, "5 张图、2 列应扩张为 3 行");
        assert_eq!(layout.rects.len(), 5, "所有图片都应参与布局");
        assert_eq!(
            layout.size,
            (220, 330),
            "3 行网格高度应为 3×100 + 2×10 + 5×2"
        );
        for (idx, rect) in layout.rects.iter().enumerate() {
            assert!(
                (rect.x as u64) + (rect.width as u64) <= layout.size.0 as u64
                    && (rect.y as u64) + (rect.height as u64) <= layout.size.1 as u64,
                "第 {} 张图越出画布: {rect:?}",
                idx + 1
            );
        }
    }

    /// 手工改配置时 spacing 可能被写成 u32::MAX。乘法必须饱和到上限，
    /// 不能回绕成一个很小的值让拼版越界（也不能在 debug 下溢出 panic）。
    #[test]
    fn extreme_spacing_saturates_instead_of_wrapping() {
        let items = [item(1, 100, 100, "a"), item(2, 100, 100, "b")];
        let settings = CollageSettings {
            direction: Direction::Custom,
            rows: 2,
            columns: 2,
            spacing: u32::MAX,
            border: 5,
            ..Default::default()
        };
        let layout = calculate_collage_layout(&items, &settings);
        assert_eq!(layout.size, (u32::MAX, u32::MAX));
        // 矩形坐标同样不能回绕成负数附近的小值。
        assert!(layout.rects.iter().all(|r| r.x >= 5 && r.y >= 5));

        let horizontal = CollageSettings {
            direction: Direction::Horizontal,
            spacing: u32::MAX,
            border: 5,
            ..Default::default()
        };
        let layout = calculate_collage_layout(&items, &horizontal);
        assert_eq!(layout.size.0, u32::MAX);
    }

    #[test]
    fn rotation_swaps_dimensions() {
        let mut item = item(1, 200, 100, "a");
        item.rotation = 1; // 90°
        assert_eq!(item.transformed_size(), (100, 200));
        item.rotation = 2; // 180°
        assert_eq!(item.transformed_size(), (200, 100));
    }

    #[test]
    fn pdf_page_count_matches_template_capacity() {
        let items: Vec<CollageItem> = (0..7).map(|i| item(i, 100, 100, "x")).collect();
        let settings = PdfSettings {
            stitching_method: StitchingMethod::TemplateBased,
            template_type: TemplateType::ThreeImages,
            ..Default::default()
        };
        assert_eq!(pdf_page_count(items.len(), &settings), 3); // 7 items, 3 per page
    }

    #[test]
    fn pdf_settings_validation_catches_bad_margins() {
        let settings = PdfSettings {
            margin_left_mm: 200.0,
            margin_right_mm: 200.0,
            ..Default::default()
        };
        let errors = validate_pdf_settings(&settings);
        assert!(errors.contains_key("margins"));
    }

    #[test]
    fn pdf_page_layout_content_area_matches_margins() {
        let settings = PdfSettings::default();
        let items: Vec<CollageItem> = (0..5).map(|i| item(i, 100, 100, "x")).collect();
        let layout = calculate_pdf_page_layout(&items, &settings);
        assert!(layout.is_valid());
        // A4 portrait: 210×297, margins 10 each side → content 190×277
        assert!((layout.content_mm.width - 190.0).abs() < 0.1);
        assert!((layout.content_mm.height - 277.0).abs() < 0.1);
    }

    #[test]
    fn uniform_vertical_scales_to_the_widest_item() {
        let items = [item(1, 200, 100, "a"), item(2, 100, 100, "b")];
        let settings = CollageSettings {
            direction: Direction::Vertical,
            sizing: Sizing::Uniform,
            spacing: 0,
            border: 0,
            ..Default::default()
        };
        let layout = calculate_collage_layout(&items, &settings);
        assert_eq!(
            layout.rects[0],
            RectPx {
                x: 0,
                y: 0,
                width: 200,
                height: 100
            }
        );
        assert_eq!(
            layout.rects[1],
            RectPx {
                x: 0,
                y: 100,
                width: 200,
                height: 200
            }
        );
        assert_eq!(layout.size, (200, 300));
    }

    #[test]
    fn original_size_keeps_each_item_as_is() {
        let items = [item(1, 200, 100, "a"), item(2, 100, 100, "b")];
        let settings = CollageSettings {
            direction: Direction::Vertical,
            sizing: Sizing::Original,
            spacing: 0,
            border: 0,
            ..Default::default()
        };
        let layout = calculate_collage_layout(&items, &settings);
        assert_eq!(
            layout.rects[1],
            RectPx {
                x: 50,
                y: 100,
                width: 100,
                height: 100
            }
        );
        assert_eq!(layout.size, (200, 200));
    }

    #[test]
    fn image_size_paper_derives_a_non_zero_page() {
        let items = [item(1, 960, 540, "a")];
        let settings = PdfSettings {
            paper_size: PaperSize::ImageSize,
            spacing_mm: 5.0,
            ..Default::default()
        };
        assert!(validate_pdf_settings(&settings).is_empty());
        assert_eq!(pdf_page_count(1, &settings), 1);
        let page = calculate_pdf_composed_page(&items, &settings, 0);
        assert!(page.is_valid());
        assert!(page.page_mm.0 > 0.0 && page.page_mm.1 > 0.0);
    }

    /// 0×0 的图片若直接做除法会得到 NaN 宽高比，`h * img_aspect` 把 NaN 写进
    /// PdfCell。两种排版都必须产生有限坐标，不能把整页几何污染掉。
    #[test]
    fn zero_sized_items_do_not_produce_nan_geometry() {
        let items = [item(1, 0, 0, "empty"), item(2, 100, 100, "ok")];
        for method in [StitchingMethod::PageBased, StitchingMethod::TemplateBased] {
            let settings = PdfSettings {
                stitching_method: method,
                template_type: TemplateType::TwoImages,
                paper_size: PaperSize::A4,
                ..Default::default()
            };
            let page = calculate_pdf_composed_page(&items, &settings, 0);
            assert!(page.is_valid(), "layout must stay valid: {:?}", page.error);
            assert_eq!(page.cells.len(), 2);
            for cell in &page.cells {
                let values = [
                    cell.clip_mm.x,
                    cell.clip_mm.y,
                    cell.clip_mm.width,
                    cell.clip_mm.height,
                    cell.image_mm.x,
                    cell.image_mm.y,
                    cell.image_mm.width,
                    cell.image_mm.height,
                ];
                assert!(values.iter().all(|v| v.is_finite()), "NaN 几何: {cell:?}");
            }
        }
    }
}
