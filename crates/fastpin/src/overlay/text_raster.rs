//! DirectWrite + Direct2D rasterization of one text annotation, shared by the
//! export composite so the saved PNG matches the gpui text the overlay preview
//! shows (`OverlayView::text_layer`).

use std::ptr;

use windows::core::PCWSTR;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1RenderTarget, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_TEXT_METRICS,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
    WICBitmapCacheOnDemand,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use crate::error::{Error, Result};

/// Font used by both the overlay preview and the export raster, so what the
/// user sees is what the PNG gets.
pub const FONT_FAMILY: &str = "Microsoft YaHei UI";

/// 文本位图的宽度上限，与 `image_budget::MAX_SIDE`（D3D11 纹理上限）一致。
/// DirectWrite 的排版宽度只是换行提示，WIC 位图仍必须走同一套像素/字节预算。
pub const MAX_TEXT_WIDTH: u32 = crate::image_budget::MAX_SIDE;

/// 文本位图的高度上限。超长文本（粘贴/OCR 回填）到达这个高度即截断并记日志，
/// 而不是逐行增高直到吃满进程内存。
pub const MAX_TEXT_HEIGHT: u32 = 4096;

/// 布局前参与的 UTF-16 单元上限（约 2 MiB 文本），防止 10 MB 单行文本先让
/// DirectWrite 做一遍无界布局再被我们拒绝。
const MAX_TEXT_UNITS: usize = 1 << 20;

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 纯函数：按 UTF-16 单元把超长文本截到 `max_units`，返回片段与是否截断。
/// 逐 char 累加，保证不会切开代理对。
fn truncate_text(text: &str, max_units: usize) -> (&str, bool) {
    let mut end = 0usize;
    let mut units = 0usize;
    for (i, ch) in text.char_indices() {
        let need = ch.len_utf16();
        if units.saturating_add(need) > max_units {
            break;
        }
        units += need;
        end = i + ch.len_utf8();
    }
    (&text[..end], end != text.len())
}

/// 纯函数：把 DirectWrite 量出的浮点尺寸收敛为受预算约束的整数尺寸。
/// 先按 `MAX_TEXT_WIDTH`/`MAX_TEXT_HEIGHT` 截断，再走与 capture/stitch/encode
/// 相同的 `checked_bytes(w, h, 4)`；超限返回 Err，调用方跳过该文字而不分配。
fn raster_size(
    width: f32,
    height: f32,
    budget: crate::image_budget::ImageBudget,
) -> std::result::Result<(u32, u32), crate::image_budget::BudgetError> {
    let width = (width.ceil() as u32).clamp(1, MAX_TEXT_WIDTH);
    let height = (height.ceil() as u32).clamp(1, MAX_TEXT_HEIGHT);
    budget.checked_bytes(width as u64, height as u64, 4)?;
    Ok((width, height))
}

/// Joins COM on the current thread for the lifetime of the value.
///
/// WIC and D2D hand back reference-counted objects whose `Release` is a vtable
/// call, so releasing one on a thread that never joined an apartment faults: the
/// suite caught exactly that inside `IWICBitmap::drop`. `rasterize` is called
/// from the overlay's STA UI thread, from MTA export workers, and from plain
/// test threads, so it cannot assume a caller did this.
///
/// A thread that already belongs to a different apartment reports
/// `RPC_E_CHANGED_MODE`; joining would fail, and the existing apartment is the
/// one to use, so nothing is undone.
struct Apartment {
    uninit: bool,
}

impl Apartment {
    fn join() -> Self {
        let hr = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
        };
        // S_OK and S_FALSE both mean the count was incremented and must be
        // balanced; a failure means this thread keeps its current model.
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

/// Premultiplied BGRA bitmap of `text` at `font_size` px (tight `width*4` stride),
/// sized to the laid-out text. `argb` is the fill colour (alpha honoured).
///
/// COM is joined for the duration of the call and left again on return, so this
/// may be called from any thread. A caller that already lives in an apartment
/// (the overlay UI thread is STA, export workers are MTA) keeps its own model.
pub fn rasterize(text: &str, font_size: f64, argb: u32) -> Result<(Vec<u8>, u32, u32)> {
    let _apartment = Apartment::join();
    unsafe {
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let family = wide_null(FONT_FAMILY);
        let locale = wide_null("");
        let format = dwrite.CreateTextFormat(
            PCWSTR(family.as_ptr()),
            None::<&IDWriteFontCollection>,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            font_size as f32,
            PCWSTR(locale.as_ptr()),
        )?;

        // 先在布局前截断超长文本：否则 10 MB 粘贴内容会让 DirectWrite 先把
        // 上百万行排完，度量值再大也来不及阻止无界位图。
        let (text, truncated) = truncate_text(text, MAX_TEXT_UNITS);
        if truncated {
            crate::perf::log_app(&format!(
                "text raster: input truncated to {MAX_TEXT_UNITS} UTF-16 units"
            ));
        }
        let text_nul = wide_null(text);
        let text_w = &text_nul[..text_nul.len() - 1];
        // 换行宽度同样封顶到纹理上限；原先的 32768 px 会让单行宽过纹理，
        // 多出来的部分在栅格化时只能被裁掉。
        let max_extent = MAX_TEXT_WIDTH as f32;
        let layout = dwrite.CreateTextLayout(text_w, &format, max_extent, max_extent)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;
        // 预算检查必须在 WIC CreateBitmap 之前：那个调用才真正按 w*h*4 分配。
        let (width, height) = raster_size(
            metrics.widthIncludingTrailingWhitespace,
            metrics.height,
            crate::image_budget::BUDGET,
        )?;
        if (metrics.widthIncludingTrailingWhitespace.ceil() as u32) > MAX_TEXT_WIDTH
            || (metrics.height.ceil() as u32) > MAX_TEXT_HEIGHT
        {
            crate::perf::log_app(&format!(
                "text raster: layout {:.0}x{:.0} px clamped to {MAX_TEXT_WIDTH}x{MAX_TEXT_HEIGHT} px",
                metrics.widthIncludingTrailingWhitespace, metrics.height
            ));
        }

        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bitmap = wic.CreateBitmap(
            width,
            height,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapCacheOnDemand,
        )?;

        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let target: ID2D1RenderTarget = d2d.CreateWicBitmapRenderTarget(&bitmap, &props)?;

        let a = ((argb >> 24) & 0xFF) as f32 / 255.0;
        let r = ((argb >> 16) & 0xFF) as f32 / 255.0;
        let g = ((argb >> 8) & 0xFF) as f32 / 255.0;
        let b = (argb & 0xFF) as f32 / 255.0;
        let brush = target.CreateSolidColorBrush(&D2D1_COLOR_F { r, g, b, a }, None)?;

        target.BeginDraw();
        target.Clear(Some(&D2D1_COLOR_F {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        }));
        target.DrawTextLayout(
            windows_numerics::Vector2 { X: 0.0, Y: 0.0 },
            &layout,
            &brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
        );
        target.EndDraw(None, None)?;

        // raster_size 已按共享预算校验过 width*height*4；这里再对实际步长/长度
        // 做一次 checked_len，避免 CopyPixels 写出缓冲区（两级防护，任一级被
        // 后续改动绕开时仍能安全失败）。
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| Error::new("text raster too wide"))?;
        let len = crate::image_budget::BUDGET
            .checked_len(stride as u64 * height as u64)
            .map_err(|e| Error::new(e.to_string()))?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(len)
            .map_err(|_| Error::new("text raster allocation failed"))?;
        buf.resize(len, 0u8);
        bitmap.CopyPixels(ptr::null(), stride, &mut buf)?;
        Ok((buf, width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_budget::{BudgetError, ImageBudget};

    /// 小预算让超限分支可以在不分配大内存的情况下被测到。
    fn small() -> ImageBudget {
        ImageBudget::new(100, 100, 10_000, 40_000)
    }

    /// 正常尺寸用例需要略宽的预算，否则 124px 会先被 max_width 拒绝。
    fn normal() -> ImageBudget {
        ImageBudget::new(200, 100, 10_000, 100_000)
    }

    #[test]
    fn raster_size_rounds_up_and_reports_a_normal_size() {
        let (w, h) = raster_size(123.4, 45.6, normal()).unwrap();
        assert_eq!(w, 124);
        assert_eq!(h, 46);
    }

    #[test]
    fn raster_size_refuses_dimensions_over_the_injected_budget() {
        // 100x100x4 = 40000 B 刚好到顶；再多一行必须被拒绝。
        let err = raster_size(100.0, 101.0, small()).unwrap_err();
        assert!(
            matches!(&err, BudgetError::TooHigh { height: 101, .. }),
            "{err}"
        );
        let err = raster_size(101.0, 10.0, small()).unwrap_err();
        assert!(
            matches!(&err, BudgetError::TooWide { width: 101, .. }),
            "{err}"
        );
    }

    #[test]
    fn raster_size_clamps_huge_measurements_to_the_text_caps() {
        let (w, h) = raster_size(1.0e30, 1.0e30, ImageBudget::STANDARD).unwrap();
        assert_eq!(
            w, MAX_TEXT_WIDTH,
            "width must be capped before WIC allocates"
        );
        assert_eq!(
            h, MAX_TEXT_HEIGHT,
            "height must be capped before WIC allocates"
        );
    }

    #[test]
    fn truncate_text_stops_at_the_unit_cap_without_splitting_a_pair() {
        let text = "a你b"; // 1 + 1 + 1 = 3 UTF-16 units
        let (kept, truncated) = truncate_text(text, 2);
        assert_eq!(kept, "a你");
        assert!(truncated, "the tail must be reported as truncated");
        let (kept, truncated) = truncate_text(text, 3);
        assert_eq!(kept, text);
        assert!(!truncated);

        let long = "x".repeat(MAX_TEXT_UNITS + 16);
        let (kept, truncated) = truncate_text(&long, MAX_TEXT_UNITS);
        assert_eq!(kept.chars().count(), MAX_TEXT_UNITS);
        assert!(truncated);
    }
}
