//! Image splitting — the pure layout core of snow-shot's Image【Split】
//! (`snow_shot/src/presentation/imagesplit/imagesplitdocument.cpp`).
//!
//! snow-shot cuts one large image into a grid of output tiles, most often to
//! print a tall screenshot across several pages. The heavy GUI (drag handles,
//! undo history, async content-aware "Smart" analysis) lives in Qt and is out
//! of scope here. What is worth porting is the deterministic, testable part:
//!
//! - three directions (Vertical / Horizontal / Grid) and three types
//!   (Equal / Precise / Smart), where Smart shares Equal's *nominal* boundaries
//!   at the layout layer — exactly what snow-shot's
//!   `calculateImageSplitLayout` does (see its header comment: "Smart uses
//!   Equal's initial boundaries here; content-aware boundaries need
//!   prepareImageSplitLayout"). Content-aware refinement can be layered on
//!   later without changing this API.
//! - guide lines stored as arithmetic runs (we use a plain sorted `Vec`, which
//!   is equivalent for the tile counts this feature targets).
//! - half-open regions: `base_region_at` gives the unexpanded cell that anchors
//!   a guide/preview number, `region_at` grows it by `overlap_width` on each
//!   side clamped to the canvas — this is snow-shot's `baseRegionAt`/`regionAt`.
//! - export streams each region as its own PNG into a `Split Images_<name>`
//!   subfolder, numbered `1.png`, `2.png`, … (snow-shot `imagesplitexport.cpp`).
//!
//! The module is pure filesystem + slice work: no GPUI, no threads, no global
//! config. Callers feed BGRA pixels (FastPin's internal format) and encode with
//! the shared WIC codec.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which axis (or both) the image is cut along.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SplitDirection {
    Vertical,
    Horizontal,
    Grid,
}

/// How cut positions are chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SplitType {
    /// Equal-sized tiles (`count` pieces along the axis).
    Equal,
    /// Fixed pixel or percentage steps.
    Precise,
    /// Content-aware in snow-shot; nominal layout is identical to Equal here.
    Smart,
}

/// Unit used by the Precise type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SplitUnit {
    Pixels,
    Percentage,
}

/// All user-tunable split parameters. Mirrors snow-shot `ImageSplitSettings`.
/// 直接作为 `settings.json` 的 `screenshot.split` 持久化：camelCase 键，
/// 缺字段时由下面的 `Default` 补齐，旧配置文件无需迁移。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SplitSettings {
    pub direction: SplitDirection,
    pub split_type: SplitType,
    pub unit: SplitUnit,
    /// Number of tiles along the axis for Vertical/Horizontal.
    pub count: u32,
    /// Grid rows.
    pub rows: u32,
    /// Grid columns.
    pub columns: u32,
    /// Precise pixel step (snow-shot rejects values below 16).
    pub pixels: u32,
    /// Precise percentage, e.g. "50" or "50%".
    pub percentage: String,
    /// Extra pixels added to every side of each output crop.
    pub overlap_width: u32,
}

impl Default for SplitSettings {
    fn default() -> Self {
        Self {
            direction: SplitDirection::Vertical,
            split_type: SplitType::Smart,
            unit: SplitUnit::Percentage,
            count: 2,
            rows: 2,
            columns: 2,
            pixels: 16,
            percentage: "50".into(),
            overlap_width: 0,
        }
    }
}

/// An integer rectangle with half-open `[x, x+width) × [y, y+height)` bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Resolved cut lines and the derived tile geometry. Mirrors
/// snow-shot `ImageSplitLayout`; `errors` carries per-field validation
/// messages instead of throwing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplitLayout {
    pub width: u32,
    pub height: u32,
    pub vertical_lines: Vec<u32>,
    pub horizontal_lines: Vec<u32>,
    pub overlap_width: u32,
    pub errors: HashMap<String, String>,
}

fn lines_valid(lines: &[u32], dimension: u32) -> bool {
    let mut last = 0u32;
    for &line in lines {
        if line == 0 || line >= dimension || line <= last {
            return false;
        }
        last = line;
    }
    true
}

impl SplitLayout {
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.errors.is_empty()
            && lines_valid(&self.vertical_lines, self.width)
            && lines_valid(&self.horizontal_lines, self.height)
    }

    /// Number of output tiles: `(vertical+1) * (horizontal+1)`.
    pub fn output_count(&self) -> u32 {
        if self.width == 0
            || self.height == 0
            || !self.errors.is_empty()
            || !lines_valid(&self.vertical_lines, self.width)
            || !lines_valid(&self.horizontal_lines, self.height)
        {
            return 0;
        }
        (self.vertical_lines.len() as u32 + 1) * (self.horizontal_lines.len() as u32 + 1)
    }

    /// The unexpanded cell at `index`, left-to-right then top-to-bottom.
    pub fn base_region_at(&self, index: u32) -> Option<Rect> {
        if index >= self.output_count() {
            return None;
        }
        let columns = self.vertical_lines.len() as u32 + 1;
        let column = index % columns;
        let row = index / columns;
        let left = if column == 0 {
            0
        } else {
            self.vertical_lines[(column - 1) as usize]
        };
        let right = if column == self.vertical_lines.len() as u32 {
            self.width
        } else {
            self.vertical_lines[column as usize]
        };
        let top = if row == 0 {
            0
        } else {
            self.horizontal_lines[(row - 1) as usize]
        };
        let bottom = if row == self.horizontal_lines.len() as u32 {
            self.height
        } else {
            self.horizontal_lines[row as usize]
        };
        Some(Rect {
            x: left,
            y: top,
            width: right.saturating_sub(left),
            height: bottom.saturating_sub(top),
        })
    }

    /// The crop actually exported: the base cell grown by `overlap_width` on
    /// every side, clamped to the canvas (snow-shot `regionAt`).
    pub fn region_at(&self, index: u32) -> Option<Rect> {
        let base = self.base_region_at(index)?;
        if base.is_empty() || self.overlap_width == 0 {
            return Some(base);
        }
        let o = self.overlap_width as u64;
        let left = (base.x as u64).saturating_sub(o).min(self.width as u64) as u32;
        let top = (base.y as u64).saturating_sub(o).min(self.height as u64) as u32;
        let right = ((base.x as u64) + (base.width as u64) + o).min(self.width as u64) as u32;
        let bottom = ((base.y as u64) + (base.height as u64) + o).min(self.height as u64) as u32;
        Some(Rect {
            x: left,
            y: top,
            width: right.saturating_sub(left),
            height: bottom.saturating_sub(top),
        })
    }
}

fn equal_lines(dimension: u32, count: u32, field: &str, layout: &mut SplitLayout) -> Vec<u32> {
    if count == 0 {
        layout
            .errors
            .insert(field.into(), "面积必须为正整数".into());
        return Vec::new();
    }
    if count > dimension {
        layout
            .errors
            .insert(field.into(), "每块至少要有 1 像素宽和高".into());
        return Vec::new();
    }
    if count == 1 {
        return Vec::new();
    }
    let section = (dimension / count) as u64;
    let extra = (dimension % count) as u64;
    let leading = extra.min((count - 1) as u64);
    let remaining = (count - 1) as u64 - leading;
    let mut lines = Vec::with_capacity((count - 1) as usize);
    // The first `leading` cells are one pixel wider.
    for i in 0..leading {
        lines.push(((section + 1) * (i + 1)) as u32);
    }
    // The remaining cells use the base section width, offset by the extra run.
    let start = extra * (section + 1) + section;
    for i in 0..remaining {
        lines.push((start + section * i) as u32);
    }
    lines
}

fn parse_percentage(text: &str) -> Option<f64> {
    let trimmed = text.trim().trim_end_matches('%').trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: f64 = trimmed.parse().ok()?;
    if !value.is_finite() || value <= 0.0 || value > 100.0 {
        return None;
    }
    Some(value / 100.0)
}

fn precise_lines(dimension: u32, settings: &SplitSettings, layout: &mut SplitLayout) -> Vec<u32> {
    if settings.unit == SplitUnit::Pixels {
        if settings.pixels < 16 {
            layout
                .errors
                .insert("pixels".into(), "至少输入 16 像素".into());
            return Vec::new();
        }
        let step = settings.pixels as u64;
        let count = (dimension as u64 - 1) / step;
        return (0..count).map(|i| (step * (i + 1)) as u32).collect();
    }

    let Some(ratio) = parse_percentage(&settings.percentage) else {
        layout
            .errors
            .insert("percentage".into(), "请输入 5% 到 100% 之间的百分比".into());
        return Vec::new();
    };
    if f64::from(dimension) * ratio < 1.0 {
        layout
            .errors
            .insert("percentage".into(), "每块至少要有 1 像素宽和高".into());
        return Vec::new();
    }
    let mut lines = Vec::new();
    for multiple in 1..=20u64 {
        let boundary = (f64::from(dimension) * ratio * multiple as f64).round() as u64;
        if boundary >= dimension as u64 {
            break;
        }
        if boundary == 0 || lines.last().is_some_and(|&last| boundary <= last as u64) {
            layout
                .errors
                .insert("percentage".into(), "每块至少要有 1 像素宽和高".into());
            return Vec::new();
        }
        lines.push(boundary as u32);
    }
    lines
}

/// Resolve the cut geometry for a canvas of `width × height`.
pub fn calculate_layout(width: u32, height: u32, settings: &SplitSettings) -> SplitLayout {
    let mut layout = SplitLayout {
        width,
        height,
        overlap_width: settings.overlap_width,
        ..SplitLayout::default()
    };
    if width == 0 || height == 0 {
        layout.errors.insert("image".into(), "图片不可用".into());
        return layout;
    }
    match settings.direction {
        SplitDirection::Grid => {
            layout.vertical_lines = equal_lines(width, settings.columns, "columns", &mut layout);
            layout.horizontal_lines = equal_lines(height, settings.rows, "rows", &mut layout);
        }
        SplitDirection::Vertical | SplitDirection::Horizontal => {
            let vertical = settings.direction == SplitDirection::Vertical;
            let dimension = if vertical { width } else { height };
            let lines = match settings.split_type {
                SplitType::Equal | SplitType::Smart => {
                    equal_lines(dimension, settings.count, "count", &mut layout)
                }
                SplitType::Precise => precise_lines(dimension, settings, &mut layout),
            };
            if vertical {
                layout.vertical_lines = lines;
            } else {
                layout.horizontal_lines = lines;
            }
        }
    }
    layout
}

/// Crop a tight BGRA buffer for `region` out of a full BGRA canvas.
pub fn crop_bgra_region(pixels: &[u8], width: u32, height: u32, region: Rect) -> Option<Vec<u8>> {
    if region.is_empty()
        || region.x.checked_add(region.width)? > width
        || region.y.checked_add(region.height)? > height
    {
        return None;
    }
    let row_bytes = region.width.checked_mul(4)? as usize;
    let mut out = vec![0u8; row_bytes.checked_mul(region.height as usize)?];
    let src_stride = width.checked_mul(4)? as usize;
    for row in 0..region.height as usize {
        let src_row = (region.y as usize + row) * src_stride + region.x as usize * 4;
        let src = pixels.get(src_row..src_row + row_bytes)?;
        let dst = row * row_bytes;
        out[dst..dst + row_bytes].copy_from_slice(src);
    }
    Some(out)
}

/// Result of exporting a batch of split tiles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SplitExportResult {
    pub directory: PathBuf,
    pub saved_count: u32,
    pub total_count: u32,
    pub error: String,
}

impl SplitExportResult {
    pub fn succeeded(&self) -> bool {
        self.error.is_empty()
            && !self.directory.as_os_str().is_empty()
            && self.total_count > 0
            && self.saved_count == self.total_count
    }
}

/// COM apartment for the WIC encoder used by `export_split_images`. Balances
/// `CoInitializeEx` on drop; a thread already in an apartment keeps its model.
struct Apartment {
    uninit: bool,
}

impl Apartment {
    fn join() -> Self {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self { uninit: hr.is_ok() }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.uninit {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// Replace characters that are awkward in a folder name.
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches(|c| c == '.' || c == ' ');
    if cleaned.is_empty() {
        "Screenshot".into()
    } else {
        cleaned.into()
    }
}

/// Resolve an available `Split Images_<name>` path under `parent`, adding
/// ` (n)` on collisions. The directory is *not* created here: callers write
/// into a staging folder first and only rename it here once every tile
/// succeeded, so a failed export never leaves half-written tiles behind.
fn resolve_export_directory(parent: &Path, image_name: &str) -> std::io::Result<PathBuf> {
    if !parent.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "输出目录不存在",
        ));
    }
    let base = format!("Split Images_{}", sanitize_name(image_name));
    for suffix in 1..=i32::MAX {
        let name = if suffix == 1 {
            base.clone()
        } else {
            format!("{base} ({suffix})")
        };
        let candidate = parent.join(&name);
        match candidate.try_exists() {
            Ok(false) => return Ok(candidate),
            Ok(true) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "无法创建输出目录",
    ))
}

/// Claim a unique hidden staging directory next to the final one. `create_dir`
/// is atomic, so concurrent exports never share a staging folder.
fn create_staging_directory(parent: &Path) -> std::io::Result<PathBuf> {
    for suffix in 0..i32::MAX {
        let candidate = parent.join(format!(".Split Images_tmp-{}-{suffix}", std::process::id()));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "无法创建临时目录",
    ))
}

/// Encode and write a single tile into `dir`; errors name the offending tile.
fn export_one_tile(
    dir: &Path,
    pixels: &[u8],
    layout: &SplitLayout,
    format: crate::config::ImageFormat,
    quality: u8,
    index: u32,
) -> Result<(), String> {
    let region = layout
        .region_at(index)
        .ok_or_else(|| format!("第 {} 块区域无效", index + 1))?;
    let crop = crop_bgra_region(pixels, layout.width, layout.height, region)
        .ok_or_else(|| format!("第 {} 块裁剪失败", index + 1))?;
    let stride = region.width * 4;
    let encoded =
        crate::codec::encode_bgra(format, quality, &crop, region.width, region.height, stride)
            .map_err(|e| format!("第 {} 块编码失败：{e}", index + 1))?;
    let path = dir.join(format!("{}.{}", index + 1, format.extension()));
    std::fs::write(&path, encoded).map_err(|e| format!("第 {} 块写入失败：{e}", index + 1))?;
    Ok(())
}

/// Export every tile of `layout` as a PNG into a new subfolder of `parent`.
///
/// `pixels` is the full BGRA canvas (stride `width * 4`). Encoding goes through
/// the shared WIC path so it behaves identically to the app's normal saves.
/// Tiles are written to a staging folder and renamed into place only once all
/// of them succeeded: a mid-export failure removes the staging folder and
/// leaves `directory` empty, so `succeeded()` keeps reporting failure and the
/// UI never offers to open a half-populated folder.
pub fn export_split_images(
    pixels: &[u8],
    layout: &SplitLayout,
    format: crate::config::ImageFormat,
    quality: u8,
    parent: &Path,
    image_name: &str,
) -> SplitExportResult {
    let mut result = SplitExportResult {
        total_count: layout.output_count(),
        ..SplitExportResult::default()
    };
    if !layout.is_valid()
        || layout.width.checked_mul(layout.height).is_none()
        || (pixels.len() as u64) < layout.width as u64 * layout.height as u64 * 4
    {
        result.error = "图片或切图配置无效".into();
        return result;
    }
    let final_dir = match resolve_export_directory(parent, image_name) {
        Ok(d) => d,
        Err(e) => {
            result.error = format!("无法创建输出目录：{e}");
            return result;
        }
    };
    let staging = match create_staging_directory(parent) {
        Ok(d) => d,
        Err(e) => {
            result.error = format!("无法创建临时目录：{e}");
            return result;
        }
    };
    // WIC encoders require a COM apartment on this thread.
    let _apartment = Apartment::join();
    for index in 0..result.total_count {
        if let Err(e) = export_one_tile(&staging, pixels, layout, format, quality, index) {
            let _ = std::fs::remove_dir_all(&staging);
            result.error = e;
            return result;
        }
        result.saved_count += 1;
    }
    // Every tile is on disk: publish the folder atomically. `directory` is
    // assigned only here, so a rename failure also reports an empty directory.
    match std::fs::rename(&staging, &final_dir) {
        Ok(()) => result.directory = final_dir,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            result.saved_count = 0;
            result.error = format!("导出目录落盘失败：{e}");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(direction: SplitDirection, count: u32) -> SplitSettings {
        SplitSettings {
            direction,
            count,
            split_type: SplitType::Equal,
            ..SplitSettings::default()
        }
    }

    #[test]
    fn equal_split_distributes_the_remaining_pixels_to_leading_cells() {
        // 10 / 3 -> 4,3,3 ; boundaries at 4 and 7.
        let layout = calculate_layout(10, 1, &settings(SplitDirection::Vertical, 3));
        assert!(layout.is_valid(), "{:?}", layout.errors);
        assert_eq!(layout.vertical_lines, vec![4, 7]);
        assert_eq!(layout.output_count(), 3);
        assert_eq!(
            layout.base_region_at(0),
            Some(Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 1
            })
        );
        assert_eq!(
            layout.base_region_at(1),
            Some(Rect {
                x: 4,
                y: 0,
                width: 3,
                height: 1
            })
        );
        assert_eq!(
            layout.base_region_at(2),
            Some(Rect {
                x: 7,
                y: 0,
                width: 3,
                height: 1
            })
        );
    }

    #[test]
    fn equal_split_with_two_remaining_pixels() {
        // 10 / 4 -> 3,3,2,2 ; boundaries at 3, 6, 8.
        let layout = calculate_layout(10, 1, &settings(SplitDirection::Vertical, 4));
        assert_eq!(layout.vertical_lines, vec![3, 6, 8]);
    }

    #[test]
    fn grid_splits_both_axes() {
        let mut s = settings(SplitDirection::Grid, 0);
        s.rows = 2;
        s.columns = 3;
        let layout = calculate_layout(10, 10, &s);
        assert!(layout.is_valid());
        assert_eq!(layout.vertical_lines, vec![4, 7]);
        assert_eq!(layout.horizontal_lines, vec![5]);
        assert_eq!(layout.output_count(), 6);
        assert_eq!(
            layout.base_region_at(5),
            Some(Rect {
                x: 7,
                y: 5,
                width: 3,
                height: 5
            })
        );
    }

    #[test]
    fn a_single_tile_has_no_guides() {
        let layout = calculate_layout(100, 100, &settings(SplitDirection::Vertical, 1));
        assert!(layout.vertical_lines.is_empty());
        assert_eq!(layout.output_count(), 1);
    }

    #[test]
    fn invalid_counts_are_reported_not_panicked() {
        let zero = calculate_layout(10, 10, &settings(SplitDirection::Vertical, 0));
        assert!(zero.errors.contains_key("count"));
        assert_eq!(zero.output_count(), 0);
        let too_many = calculate_layout(10, 10, &settings(SplitDirection::Vertical, 11));
        assert!(too_many.errors.contains_key("count"));
    }

    #[test]
    fn precise_pixels_rejects_small_steps_and_then_steps_evenly() {
        let mut s = settings(SplitDirection::Vertical, 0);
        s.split_type = SplitType::Precise;
        s.unit = SplitUnit::Pixels;
        s.pixels = 8;
        let bad = calculate_layout(100, 10, &s);
        assert!(bad.errors.contains_key("pixels"));
        s.pixels = 16;
        let good = calculate_layout(100, 10, &s);
        assert!(good.is_valid());
        assert_eq!(good.vertical_lines, vec![16, 32, 48, 64, 80, 96]);
    }

    #[test]
    fn precise_percentage_puts_a_line_at_the_requested_fraction() {
        let mut s = settings(SplitDirection::Horizontal, 0);
        s.split_type = SplitType::Precise;
        s.unit = SplitUnit::Percentage;
        s.percentage = "50".into();
        let layout = calculate_layout(100, 100, &s);
        assert_eq!(layout.horizontal_lines, vec![50]);
        s.percentage = "25%".into();
        let layout = calculate_layout(100, 100, &s);
        assert_eq!(layout.horizontal_lines, vec![25, 50, 75]);
    }

    #[test]
    fn overlap_grows_a_region_and_clamps_to_the_canvas() {
        let mut s = settings(SplitDirection::Vertical, 2);
        s.overlap_width = 4;
        let layout = calculate_layout(100, 10, &s);
        // base [0,50) -> grown [0,54); base [50,100) -> grown [46,100).
        assert_eq!(
            layout.region_at(0),
            Some(Rect {
                x: 0,
                y: 0,
                width: 54,
                height: 10
            })
        );
        assert_eq!(
            layout.region_at(1),
            Some(Rect {
                x: 46,
                y: 0,
                width: 54,
                height: 10
            })
        );
    }

    #[test]
    fn cropping_extracts_exactly_the_requested_bgra_pixels() {
        // 4x2 BGRA canvas; each pixel is (b,g,r,a) = (x, y, 0, 255).
        let mut pixels = vec![0u8; 4 * 2 * 4];
        for y in 0..2u8 {
            for x in 0..4u8 {
                let i = (y as usize * 4 + x as usize) * 4;
                pixels[i] = x;
                pixels[i + 1] = y;
                pixels[i + 3] = 255;
            }
        }
        let crop = crop_bgra_region(
            &pixels,
            4,
            2,
            Rect {
                x: 2,
                y: 0,
                width: 2,
                height: 2,
            },
        )
        .expect("crop");
        assert_eq!(crop.len(), 2 * 2 * 4);
        assert_eq!(crop[0], 2); // x=2, y=0
        assert_eq!(crop[4], 3); // x=3, y=0
        assert_eq!(crop[8], 2); // x=2, y=1
        assert_eq!(crop[9], 1); // g = y = 1
    }

    #[test]
    fn out_of_bounds_crops_are_rejected() {
        assert!(crop_bgra_region(
            &[0u8; 16],
            2,
            2,
            Rect {
                x: 1,
                y: 1,
                width: 2,
                height: 2
            }
        )
        .is_none());
        assert!(crop_bgra_region(
            &[0u8; 16],
            2,
            2,
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 1
            }
        )
        .is_none());
    }

    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    /// Throwaway parent directory used by the filesystem tests below.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fastpin-imagesplit-{tag}-{}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp parent");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolve_export_directory_respects_collisions() {
        let tmp = TempDir::new("collision");
        let first = resolve_export_directory(&tmp.0, "Shot").expect("first path");
        assert!(first.ends_with("Split Images_Shot"));
        std::fs::create_dir(&first).expect("create first");
        let second = resolve_export_directory(&tmp.0, "Shot").expect("second path");
        assert!(second.ends_with("Split Images_Shot (2)"));
        std::fs::create_dir(&second).expect("create second");
        let third = resolve_export_directory(&tmp.0, "Shot").expect("third path");
        assert!(third.ends_with("Split Images_Shot (3)"));
        // Resolving alone must not create any directory.
        assert!(!third.exists());
    }

    #[test]
    fn staging_directories_are_unique_and_hidden() {
        let tmp = TempDir::new("staging");
        let a = create_staging_directory(&tmp.0).expect("first staging dir");
        let b = create_staging_directory(&tmp.0).expect("second staging dir");
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        assert!(a
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.')));
    }

    #[test]
    fn invalid_input_reports_failure_without_a_directory() {
        let tmp = TempDir::new("invalid");
        let layout = calculate_layout(10, 10, &settings(SplitDirection::Vertical, 2));
        // Buffer far too small for the declared canvas.
        let result = export_split_images(
            &[0u8; 4],
            &layout,
            crate::config::ImageFormat::Png,
            100,
            &tmp.0,
            "Shot",
        );
        assert!(result.error.contains("无效"));
        assert!(result.directory.as_os_str().is_empty());
        assert!(!result.succeeded());
    }
}
