//! Regenerates `assets/logo/fastpin.ico` from `assets/logo/fastpin.png`.
//!
//! Multi-size ICO is written by hand because `image`'s ICO encoder stores a
//! single frame. Sizes 16-128 use 32bpp BGRA BMP entries plus the AND mask
//! (the widest compatibility); 256 uses a PNG entry, which every supported
//! Windows accepts and keeps the file small.
//!
//! ```text
//! cargo run -p fastpin --example gen_icon
//! ```

use std::path::PathBuf;

use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src = root.join("assets/logo/fastpin.png");
    let dst = root.join("assets/logo/fastpin.ico");

    let source = image::open(&src).expect("open fastpin.png").to_rgba8();
    let entries: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&size| {
            let rgba = image::imageops::resize(&source, size, size, FilterType::Lanczos3);
            let payload = if size >= 256 {
                png_entry(&rgba)
            } else {
                bmp_entry(&rgba)
            };
            (size, payload)
        })
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * entries.len() as u32;
    for (size, payload) in &entries {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(dim); // width, 0 means 256
        out.push(dim); // height
        out.push(0); // palette colours
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bit count
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += payload.len() as u32;
    }
    for (_, payload) in &entries {
        out.extend_from_slice(payload);
    }

    std::fs::write(&dst, &out).expect("write fastpin.ico");
    println!("wrote {} ({} bytes, sizes {SIZES:?})", dst.display(), out.len());
}

fn png_entry(img: &RgbaImage) -> Vec<u8> {
    let mut buf = Vec::new();
    PngEncoder::new(&mut buf)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgba8,
        )
        .expect("encode png frame");
    buf
}

fn bmp_entry(img: &RgbaImage) -> Vec<u8> {
    let (w, h) = img.dimensions();
    let mask_stride = w.div_ceil(32) * 4;
    let xor_len = (w * h * 4) as usize;
    let and_len = (mask_stride * h) as usize;

    let mut out = Vec::with_capacity(40 + xor_len + and_len);
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&((h * 2) as i32).to_le_bytes()); // XOR + AND
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bpp
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(xor_len as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // ppm + palette counts

    // BMP rows run bottom-up; pixels are BGRA.
    for y in (0..h).rev() {
        for x in 0..w {
            let p = img.get_pixel(x, y).0;
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    // AND mask: zero means "use the XOR pixel", i.e. keep alpha.
    out.resize(out.len() + and_len, 0);
    out
}
