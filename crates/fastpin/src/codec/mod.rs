//! WIC image encoding (PNG / JPG / WebP) + CF_DIB clipboard. Used by overlay
//! confirm, the long-capture preview and --once-capture.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::ptr;
use std::slice;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::{BITMAPINFOHEADER, BI_RGB};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_ContainerFormatPng,
    GUID_ContainerFormatWebp, GUID_WICPixelFormat24bppBGR, GUID_WICPixelFormat32bppBGRA,
    IWICBitmapEncoder, IWICBitmapFrameEncode, IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::StructuredStorage::{
    CreateStreamOnHGlobal, GetHGlobalFromStream, IPropertyBag2, PROPBAG2,
};
use windows::Win32::System::Com::{CoCreateInstance, IStream, CLSCTX_INPROC_SERVER};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
};
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT, PROPBAG2_TYPE_DATA};
use windows::Win32::System::Variant::{VARIANT, VT_R4};

use crate::config::ImageFormat;
use crate::error::{Error, Result};
use crate::image_budget::{BudgetError, BUDGET};

/// Whether WIC can encode `format` on this machine.
///
/// JPEG ships with every Windows install; WebP comes from the optional
/// "Webp Image Extensions" package and is genuinely absent on a clean install
/// (encoding then fails with `WINCODEC_ERR_COMPONENTNOTFOUND`, 0x88982F50).
/// The settings page uses this to offer only formats that work here, and the
/// export path uses it to fall back instead of losing the screenshot.
pub fn format_available(format: ImageFormat) -> bool {
    use std::sync::OnceLock;
    // Probed once: creating an encoder is cheap but not free, and the answer
    // cannot change while the process runs.
    static PNG: OnceLock<bool> = OnceLock::new();
    static JPEG: OnceLock<bool> = OnceLock::new();
    static WEBP: OnceLock<bool> = OnceLock::new();
    let slot = match format {
        ImageFormat::Png => &PNG,
        ImageFormat::Jpeg => &JPEG,
        ImageFormat::Webp => &WEBP,
    };
    if let Some(available) = slot.get() {
        return *available;
    }
    match unsafe { probe_encoder(format) } {
        // A definitive answer (either way) is worth caching.
        Some(available) => {
            let _ = slot.set(available);
            available
        }
        // COM was not initialized on this thread. Caching `false` here would
        // mark PNG/JPEG unavailable for the rest of the process, so ask again
        // next time instead.
        None => false,
    }
}

/// `Some(available)` once COM can answer; `None` when this thread has not
/// initialized COM, where the failure says nothing about the codec.
unsafe fn probe_encoder(format: ImageFormat) -> Option<bool> {
    let container = match format {
        ImageFormat::Png => &GUID_ContainerFormatPng,
        ImageFormat::Jpeg => &GUID_ContainerFormatJpeg,
        ImageFormat::Webp => &GUID_ContainerFormatWebp,
    };
    match CoCreateInstance::<_, IWICImagingFactory>(
        &CLSID_WICImagingFactory,
        None,
        CLSCTX_INPROC_SERVER,
    ) {
        Ok(factory) => Some(factory.CreateEncoder(container, ptr::null()).is_ok()),
        // CO_E_NOTINITIALIZED (0x800401F0): the probe ran on a thread where
        // COM has not been initialized, so the answer is not meaningful.
        Err(e) if e.code().0 as u32 == 0x8004_01F0 => None,
        Err(_) => Some(false),
    }
}

/// The format actually used for an export. A `.webp` selection on a machine
/// without the WebP codec exports PNG rather than failing, so a settings file
/// copied from another PC still saves the screenshot.
pub fn effective_format(requested: ImageFormat) -> ImageFormat {
    if format_available(requested) {
        requested
    } else {
        ImageFormat::Png
    }
}

pub fn encode_png_bgra(pixels: &[u8], width: u32, height: u32, stride: u32) -> Result<Vec<u8>> {
    encode_bgra(ImageFormat::Png, 90, pixels, width, height, stride)
}

/// 按统一预算把图片解码成 RGBA：文件大小 → 头部声明尺寸 → 解码后的真实尺寸
/// 三重检查都在这一处完成，输出缓冲也走 `BUDGET` 的可失败分配。历史缩略图、
/// 水印和剪贴板解码原先各自抄了一份同样的检查，现在共用这一份；任一关失败
/// 都返回 `None`，由调用方按原路径降级。
pub fn decode_image_budgeted(path: &Path) -> Option<image::RgbaImage> {
    let meta = path.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    BUDGET.checked_len(meta.len()).ok()?;
    let (header_w, header_h) = image::image_dimensions(path).ok()?;
    BUDGET.checked_dims(header_w as u64, header_h as u64).ok()?;

    let decoded = image::open(path).ok()?.to_rgba8();
    let (w, h) = decoded.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    BUDGET.checked_dims(w as u64, h as u64).ok()?;
    let mut rgba = BUDGET.alloc_rgba(w as u64, h as u64).ok()?;
    rgba.copy_from_slice(decoded.as_raw());
    image::RgbaImage::from_raw(w, h, rgba)
}

/// Decode an image file into a tight BGRA buffer suitable for the clipboard DIB.
///
/// Image files are decoded as RGBA, so the R and B channels are swapped to reach
/// FastPin's internal BGRA convention. Returns `(bgra, width, height)`, or
/// `None` when the file cannot be decoded or has zero dimensions. Shared by the
/// history gallery, the settings grid, the collage and the image-split tools so
/// the channel order stays identical everywhere.
pub fn decode_bgra(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    // 三重检查在 `decode_image_budgeted` 里统一做；这里只负责 RGBA → BGRA。
    let decoded = decode_image_budgeted(path)?;
    let (w, h) = decoded.dimensions();
    let src = decoded.into_raw();
    // 第二次拷贝（RGBA → BGRA）同样走预算，不能用 infallible `vec!`。
    let mut bgra = BUDGET.alloc_rgba(w as u64, h as u64).ok()?;
    // `as_chunks` yields `&[u8; 4]` / `&mut [u8; 4]` so the swap stays in-bounds
    // and clippy's `chunks_exact` lint is silenced without losing the zip.
    for (s, d) in src
        .as_chunks::<4>()
        .0
        .iter()
        .zip(bgra.as_chunks_mut::<4>().0.iter_mut())
    {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = s[3];
    }
    Some((bgra, w, h))
}

/// `GlobalLock` / `GlobalUnlock` 的 RAII 配对。HGLOBAL 最终交给
/// `CreateStreamOnHGlobal(..., true)` 释放（内部 `GlobalFree`），若在
/// 编码后的拷贝/预算检查/分配失败路径上提前返回而忘了 `GlobalUnlock`，
/// 这块仍被锁定的内存就释放不掉。guard 保证任何退出路径都解锁。
struct LockedGlobal(HGLOBAL);

impl Drop for LockedGlobal {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalUnlock(self.0);
        }
    }
}

/// Encodes a BGRA buffer in `format`. `quality` is 1..=100 and only affects the
/// lossy formats; PNG ignores it.
///
/// JPG has no alpha channel, so it is encoded from a 24bpp BGR copy with the
/// alpha dropped rather than from the BGRA buffer: handing BGRA to the JPEG
/// encoder produces a file whose colours are read as BGRX, which is what a
/// silent alpha-flattened export looked like before this existed.
pub fn encode_bgra(
    format: ImageFormat,
    quality: u8,
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> Result<Vec<u8>> {
    // Refuse an oversized or short input before touching WIC: `WritePixels`
    // reads `stride * height` bytes from this slice.
    let budget = BUDGET;
    budget.check_image(width as u64, height as u64, stride as u64, pixels.len())?;

    // The BGR copy lives as long as the encoder call it feeds.
    let bgr: Option<Vec<u8>> = if format == ImageFormat::Jpeg {
        Some(bgra_to_bgr(pixels, width, height, stride)?)
    } else {
        None
    };

    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream: IStream =
            CreateStreamOnHGlobal(windows::Win32::Foundation::HGLOBAL::default(), true)?;
        let container = match format {
            ImageFormat::Png => &GUID_ContainerFormatPng,
            ImageFormat::Jpeg => &GUID_ContainerFormatJpeg,
            ImageFormat::Webp => &GUID_ContainerFormatWebp,
        };
        let encoder: IWICBitmapEncoder = factory.CreateEncoder(container, ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame: Option<IWICBitmapFrameEncode> = None;
        let mut bag: Option<IPropertyBag2> = None;
        encoder.CreateNewFrame(&mut frame, &mut bag)?;
        let frame = frame.ok_or_else(|| Error::new("WIC CreateNewFrame"))?;
        frame.Initialize(None)?;
        frame.SetSize(width, height)?;

        if let Some(bag) = bag.as_ref() {
            set_encoder_quality(bag, format, quality)?;
        }

        let (mut pf, out_stride) = match format {
            ImageFormat::Jpeg => (GUID_WICPixelFormat24bppBGR, width * 3),
            _ => (GUID_WICPixelFormat32bppBGRA, stride),
        };
        frame.SetPixelFormat(&mut pf)?;
        match bgr.as_deref() {
            Some(buf) => frame.WritePixels(height, out_stride, buf)?,
            None => frame.WritePixels(height, stride, pixels)?,
        }
        frame.Commit()?;
        encoder.Commit()?;

        let hg = GetHGlobalFromStream(&stream)?;
        let ptr = GlobalLock(hg);
        if ptr.is_null() {
            return Err(Error::new("GlobalLock failed for image stream"));
        }
        // 之后任何 `?` / 提前返回都由 guard 负责解锁。
        let _unlock = LockedGlobal(hg);
        let size = GlobalSize(hg);
        // The encoded bytes of an in-budget image fit the budget; the copy out
        // of the HGLOBAL is fallible instead of an aborting `to_vec`.
        budget.checked_len(size as u64)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| BudgetError::AllocFailed { bytes: size as u64 })?;
        bytes.extend_from_slice(slice::from_raw_parts(ptr as *const u8, size));
        Ok(match format {
            // WIC pads PNG to a block boundary with zero bytes past IEND.
            ImageFormat::Png => trim_png(bytes),
            _ => bytes,
        })
    }
}

/// WIC expects `ImageQuality` as a float in 0.0..=1.0, but the UI exposes
/// 1..=100. Writing the raw byte made every setting above 1 clamp to 1.0 in
/// WIC, so changing 90 -> 10 still exported at maximum quality. Clamp the UI
/// value first, then normalize it into WIC's range. Pure so it can be tested
/// without COM.
fn wic_quality(quality: u32) -> f32 {
    quality.clamp(1, 100) as f32 / 100.0
}

/// Writes `ImageQuality` into the frame's encoder options. Only the lossy
/// formats declare it: asking a PNG encoder for a quality property is an error
/// on some WIC builds, and passing a property the encoder does not know is how
/// `CreateNewFrame` starts failing with `WINCODEC_ERR_UNSUPPORTEDPROPERTY`.
unsafe fn set_encoder_quality(bag: &IPropertyBag2, format: ImageFormat, quality: u8) -> Result<()> {
    if !format.honours_quality() {
        return Ok(());
    }
    let name: Vec<u16> = "ImageQuality\0".encode_utf16().collect();
    let prop = PROPBAG2 {
        dwType: PROPBAG2_TYPE_DATA.0 as u32,
        vt: VT_R4,
        cfType: 0,
        dwHint: 0,
        pstrName: windows::core::PWSTR(name.as_ptr() as *mut u16),
        clsid: windows::core::GUID::zeroed(),
    };
    let mut value = VARIANT::default();
    // WIC's ImageQuality is a 4-byte float, not the byte `quality` reads as
    // elsewhere; VT_UI1 makes `IPropertyBag2::Write` fail with
    // WINCODEC_ERR_BADIMAGE (0x88982F8E, "位图属性类型异常").
    {
        let inner = &mut *value.Anonymous.Anonymous;
        inner.vt = VT_R4;
        inner.Anonymous.fltVal = wic_quality(quality as u32);
    }
    bag.Write(1, &prop, &value)
        .map_err(|e| Error::new(format!("WIC ImageQuality {quality}: {e}")))
}

/// Flattens BGRA to the packed 24bpp BGR the JPEG encoder wants. Alpha is
/// dropped by compositing over white: a transparent screenshot region would
/// otherwise become black, which is not what the user saw on screen.
fn bgra_to_bgr(pixels: &[u8], width: u32, height: u32, stride: u32) -> Result<Vec<u8>> {
    let row_src = stride as usize;
    let row_min = width as usize * 4;
    if row_src < row_min {
        return Err(Error::new(format!(
            "JPEG source stride {row_src} is smaller than {row_min} bytes"
        )));
    }
    let row_dst = width as usize * 3;
    let needed = row_src
        .checked_mul(height as usize)
        .ok_or_else(|| Error::new("JPEG source size overflowed"))?;
    if pixels.len() < needed {
        return Err(Error::new(format!(
            "JPEG source needs {needed} bytes, got {}",
            pixels.len()
        )));
    }
    let total = row_dst
        .checked_mul(height as usize)
        .ok_or_else(|| Error::new("JPEG output size overflowed"))?;
    let mut out = Vec::new();
    out.try_reserve_exact(total)
        .map_err(|_| BudgetError::AllocFailed {
            bytes: total as u64,
        })?;
    for y in 0..height as usize {
        let src = &pixels[y * row_src..y * row_src + row_min];
        for px in src.as_chunks::<4>().0 {
            let (b, g, r, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
            if a == 255 {
                out.push(b as u8);
                out.push(g as u8);
                out.push(r as u8);
            } else {
                // Composite over white so a transparent pixel is not black.
                let inv = 255 - a;
                out.push(((b * a + 255 * inv) / 255) as u8);
                out.push(((g * a + 255 * inv) / 255) as u8);
                out.push(((r * a + 255 * inv) / 255) as u8);
            }
        }
    }
    Ok(out)
}

fn trim_png(mut bytes: Vec<u8>) -> Vec<u8> {
    const SIG: &[u8] = &[137, 80, 78, 71, 13, 10, 26, 10];
    if bytes.len() < 8 || &bytes[..8] != SIG {
        return bytes;
    }
    let mut i = 8usize;
    while i + 12 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        let ctype = &bytes[i + 4..i + 8];
        let next = i + 12 + len;
        if next > bytes.len() {
            break;
        }
        if ctype == b"IEND" {
            bytes.truncate(next);
            break;
        }
        i = next;
    }
    bytes
}

pub fn write_png_file(
    path: &Path,
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> Result<()> {
    write_image_file(path, ImageFormat::Png, 90, pixels, width, height, stride)
}

/// Writes `pixels` to `path` in `format`. The caller owns the extension: this
/// writes bytes and never renames a file to match the format.
pub fn write_image_file(
    path: &Path,
    format: ImageFormat,
    quality: u8,
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = encode_bgra(format, quality, pixels, width, height, stride)?;
    let mut f = File::create(path)?;
    f.write_all(&bytes)?;
    Ok(())
}

/// Size of the CF_DIB block `set_clipboard_dib` would allocate for a
/// `width x height` 32bpp image, header included. Pure, so callers can decide
/// to fall back to the PNG clipboard before copying ~1 GiB into a global block.
/// `None` when the dimensions or the total exceed the shared image budget.
pub fn dib_bytes(width: u32, height: u32) -> Option<usize> {
    let budget = BUDGET;
    let image = budget.checked_bytes(width as u64, height as u64, 4).ok()?;
    let header = std::mem::size_of::<BITMAPINFOHEADER>();
    budget.checked_len(image as u64 + header as u64).ok()
}

/// `OpenClipboard` 重试：资源管理器、输入法等程序会瞬时占用剪贴板，只试一次
/// 就把失败弹成错误框对用户是假故障。5 次、每次 30 ms；全部失败才返回错误。
fn open_clipboard_with_retry() -> windows::core::Result<()> {
    const ATTEMPTS: usize = 5;
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match unsafe { OpenClipboard(Some(HWND::default())) } {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
    Err(last.expect("clipboard open has at least one attempt"))
}

pub fn set_clipboard_dib(pixels: &[u8], width: u32, height: u32, stride: u32) -> Result<()> {
    let budget = BUDGET;
    // Checked before the `u32` math below: `width * 4 * height` used to wrap,
    // and the DIB copy reads `stride * height` bytes from `pixels`.
    budget.check_image(width as u64, height as u64, stride as u64, pixels.len())?;
    let header_size = std::mem::size_of::<BITMAPINFOHEADER>();
    let packed_stride = width * 4;
    let image_size = budget.checked_bytes(width as u64, height as u64, 4)?;
    let total = budget.checked_len(image_size as u64 + header_size as u64)?;
    let image_size = image_size as u32;
    unsafe {
        let hg = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total)?;
        let ptr = GlobalLock(hg) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(hg));
            return Err(Error::new("GlobalLock failed for DIB"));
        }
        let header = ptr as *mut BITMAPINFOHEADER;
        (*header).biSize = header_size as u32;
        (*header).biWidth = width as i32;
        // Bottom-up, not top-down: Windows' on-demand CF_DIB -> CF_BITMAP
        // synthesis fails for negative-height DIBs, and most paste targets
        // (WeChat, GDI+ apps) ask for exactly that format.
        (*header).biHeight = height as i32;
        (*header).biPlanes = 1;
        (*header).biBitCount = 32;
        (*header).biCompression = BI_RGB.0;
        (*header).biSizeImage = image_size;
        let dest = ptr.add(header_size);
        for y in 0..height as usize {
            let src = (height as usize - 1 - y) * stride as usize;
            let dst = y * packed_stride as usize;
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr().add(src),
                dest.add(dst),
                packed_stride as usize,
            );
        }
        let _ = GlobalUnlock(hg);

        if let Err(e) = open_clipboard_with_retry() {
            let _ = GlobalFree(Some(hg));
            return Err(e.into());
        }
        let _ = EmptyClipboard();
        let handed_over = SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(hg.0)));
        // Close before reporting: an early return here would leave the whole
        // system's clipboard locked by this process.
        let _ = CloseClipboard();
        match handed_over {
            // On success the clipboard owns the block; freeing it would be a
            // double free.
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = GlobalFree(Some(hg));
                Err(e.into())
            }
        }
    }
}

/// Places PNG bytes on the clipboard under the registered `PNG` format. Used
/// when a CF_DIB copy would exceed the shared image budget (a 16k-tall capture
/// needs ~1 GiB for the DIB alone): browsers, Office and most chat apps paste
/// PNG directly, and targets that only understand CF_DIB get a normal paste
/// error instead of the app dying on a failed allocation.
pub fn set_clipboard_png(png: &[u8]) -> Result<()> {
    let wide: Vec<u16> = "PNG".encode_utf16().chain([0]).collect();
    let format = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
    if format == 0 {
        return Err(Error::new("RegisterClipboardFormatW(PNG) failed"));
    }
    unsafe {
        let hg = GlobalAlloc(GMEM_MOVEABLE, png.len())?;
        let ptr = GlobalLock(hg) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(hg));
            return Err(Error::new("GlobalLock failed for PNG"));
        }
        std::ptr::copy_nonoverlapping(png.as_ptr(), ptr, png.len());
        let _ = GlobalUnlock(hg);
        if let Err(e) = open_clipboard_with_retry() {
            let _ = GlobalFree(Some(hg));
            return Err(e.into());
        }
        let _ = EmptyClipboard();
        let handed_over = SetClipboardData(format, Some(HANDLE(hg.0)));
        let _ = CloseClipboard();
        match handed_over {
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = GlobalFree(Some(hg));
                Err(e.into())
            }
        }
    }
}

/// Copies OCR / translation output. Mirrors `set_clipboard_dib`, including its
/// close-before-reporting rule.
pub fn set_clipboard_text(text: &str) -> Result<()> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    unsafe {
        let hg = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, utf16.len() * 2)?;
        let ptr = GlobalLock(hg) as *mut u16;
        if ptr.is_null() {
            let _ = GlobalFree(Some(hg));
            return Err(Error::new("GlobalLock failed for text"));
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
        let _ = GlobalUnlock(hg);

        if let Err(e) = open_clipboard_with_retry() {
            let _ = GlobalFree(Some(hg));
            return Err(e.into());
        }
        let _ = EmptyClipboard();
        let handed_over = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hg.0)));
        let _ = CloseClipboard();
        match handed_over {
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = GlobalFree(Some(hg));
                Err(e.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    const SIG: &[u8] = &[137, 80, 78, 71, 13, 10, 26, 10];

    #[test]
    fn wic_quality_normalizes_the_ui_range() {
        assert_eq!(
            wic_quality(1),
            0.01_f32,
            "1 maps to the lowest non-zero WIC quality"
        );
        assert_eq!(
            wic_quality(50),
            0.5_f32,
            "50 maps to the middle of WIC's range"
        );
        assert_eq!(
            wic_quality(100),
            1.0_f32,
            "100 maps to the top of WIC's range"
        );
        assert_eq!(wic_quality(0), 0.01_f32, "values below 1 clamp up");
        assert_eq!(wic_quality(101), 1.0_f32, "values above 100 clamp down");
        assert_eq!(
            wic_quality(u32::MAX),
            1.0_f32,
            "a far out-of-range value still clamps down"
        );
    }

    #[test]
    fn encode_png_has_signature_and_iend() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let pixels: Vec<u8> = vec![
            0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
        ];
        let bytes = encode_png_bgra(&pixels, 2, 2, 8).expect("png");
        assert_eq!(&bytes[..8], SIG, "missing PNG signature");
        assert!(bytes.windows(4).any(|w| w == b"IEND"));
        assert!(bytes.len() > 32);
    }

    #[test]
    fn encode_png_rejects_short_and_oversized_buffers() {
        // 2x2 at stride 8 needs 16 bytes; fewer must be an error rather than
        // a read past the slice inside WIC.
        let err = encode_png_bgra(&[0u8; 8], 2, 2, 8).unwrap_err();
        assert!(err.to_string().contains("16"), "{err}");
        // A side past the shared budget is refused before WIC is created.
        let err = encode_png_bgra(&[], 16_385, 1, 16_385 * 4).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("16385"), "{msg}");
        assert!(msg.contains("16384"), "{msg}");
    }

    #[test]
    fn jpeg_and_webp_encoders_produce_their_own_containers() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        // A 16x16 opaque BGRA block: big enough for both encoders to work.
        let mut pixels = vec![0u8; 16 * 16 * 4];
        for px in pixels.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[40, 80, 120, 255]);
        }

        assert!(format_available(ImageFormat::Png), "PNG ships with WIC");
        let jpg = encode_bgra(ImageFormat::Jpeg, 85, &pixels, 16, 16, 64).expect("jpeg");
        assert_eq!(&jpg[..2], &[0xFF, 0xD8], "JPEG SOI");
        assert_eq!(&jpg[jpg.len() - 2..], &[0xFF, 0xD9], "JPEG EOI");

        let png = encode_bgra(ImageFormat::Png, 85, &pixels, 16, 16, 64).expect("png");
        assert_eq!(&png[..8], SIG);

        // WebP is an optional Windows codec. When present it must encode; when
        // absent the export path must fall back rather than lose the image.
        if format_available(ImageFormat::Webp) {
            let webp = encode_bgra(ImageFormat::Webp, 85, &pixels, 16, 16, 64).expect("webp");
            assert_eq!(&webp[..4], b"RIFF");
            assert_eq!(&webp[8..12], b"WEBP");
        } else {
            assert_eq!(
                effective_format(ImageFormat::Webp),
                ImageFormat::Png,
                "a missing WebP codec falls back to PNG"
            );
        }
    }

    /// JPEG has no alpha. A transparent region must flatten to white, not to
    /// the black a naive alpha drop produces.
    #[test]
    fn jpeg_flattens_transparency_over_white() {
        // Two pixels: opaque red, fully transparent.
        let pixels: Vec<u8> = vec![0, 0, 255, 255, 0, 0, 0, 0];
        let bgr = bgra_to_bgr(&pixels, 2, 1, 8).expect("bgr");
        assert_eq!(&bgr[..3], &[0, 0, 255], "opaque red stays red");
        assert_eq!(&bgr[3..], &[255, 255, 255], "transparent becomes white");
    }

    #[test]
    fn jpeg_refuses_a_short_source_buffer() {
        // 2x1 at stride 8 needs 8 bytes; 4 must be refused, not read past.
        let err = bgra_to_bgr(&[0u8; 4], 2, 1, 8).unwrap_err();
        assert!(err.to_string().contains('8'), "{err}");
    }

    /// Diagnosis for 'copy-only pastes nothing': the DIB must come back out
    /// of the clipboard with the same pixels.
    #[test]
    fn clipboard_dib_round_trip() {
        use windows::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, OpenClipboard,
        };
        use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

        let pixels: Vec<u8> = vec![1, 2, 3, 255, 5, 6, 7, 255, 9, 10, 11, 255, 13, 14, 15, 255];
        set_clipboard_dib(&pixels, 2, 2, 8).expect("set_clipboard_dib");

        unsafe {
            OpenClipboard(Some(HWND::default())).expect("OpenClipboard");
            let h = GetClipboardData(CF_DIB.0 as u32).expect("GetClipboardData(CF_DIB)");
            let h = windows::Win32::Foundation::HGLOBAL(h.0 as *mut _);
            let size = GlobalSize(h);
            let ptr = GlobalLock(h) as *const u8;
            assert!(!ptr.is_null());
            let data = std::slice::from_raw_parts(ptr, size);
            let header = &*(data.as_ptr() as *const BITMAPINFOHEADER);
            assert_eq!(header.biWidth, 2);
            assert_eq!(header.biHeight, 2, "bottom-up DIB");
            let image = &data[std::mem::size_of::<BITMAPINFOHEADER>()..];
            // Bottom-up: the first stored row is the last source row.
            assert_eq!(
                image,
                &[9, 10, 11, 255, 13, 14, 15, 255, 1, 2, 3, 255, 5, 6, 7, 255]
            );
            let _ = GlobalUnlock(h);
            let _ = CloseClipboard();
        }
    }

    #[test]
    fn dib_bytes_is_bounded_and_matches_the_header_math() {
        let header = std::mem::size_of::<BITMAPINFOHEADER>();
        assert_eq!(dib_bytes(0, 0), Some(header));
        assert_eq!(dib_bytes(2, 3), Some(2 * 3 * 4 + header));
        // The largest square the side caps allow is exactly 1 GiB + header,
        // still inside the 1 GiB + 128 MiB budget.
        assert_eq!(dib_bytes(16_384, 16_384), Some(1024 * 1024 * 1024 + header));
        // One pixel over a side, and a wrapped u32 product, are refused.
        assert_eq!(dib_bytes(16_385, 1), None);
        assert_eq!(dib_bytes(u32::MAX, u32::MAX), None);
    }
}
