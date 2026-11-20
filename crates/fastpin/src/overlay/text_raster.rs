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

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
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

        let text_nul = wide_null(text);
        let text_w = &text_nul[..text_nul.len() - 1];
        let max_extent = (1u32 << 15) as f32;
        let layout = dwrite.CreateTextLayout(text_w, &format, max_extent, max_extent)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;
        let width = (metrics.widthIncludingTrailingWhitespace.ceil() as u32).max(1);
        let height = (metrics.height.ceil() as u32).max(1);

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

        // Width comes from DWrite (up to 32767 px) but the product is still
        // unchecked in release builds: a wrapped stride/length would make
        // CopyPixels write past the buffer. Route it through the shared cap.
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
