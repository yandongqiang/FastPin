//! Brand-icon background cleanup.
//!
//! The source logo (`assets/logo/fastpin.png`) is RGB with a white card
//! behind the glyph, which shows as a white square in the tray. The tray icon
//! wants the card gone but the glyph's own white highlights kept, so this
//! clears white only where it is connected to the image edge: a flood fill
//! seeded from the four borders. Interior white enclosed by the glyph is never
//! reached and stays opaque.

use std::collections::VecDeque;

use image::RgbaImage;

/// A pixel counts as background white when every channel is at least this.
/// The source card is 253–254, so 240 leaves a small margin for JPEG-like
/// noise without eating the glyph's white highlights.
pub const WHITE_THRESHOLD: u8 = 240;

fn is_white(p: &image::Rgba<u8>, threshold: u8) -> bool {
    p[3] > 0 && p[0] >= threshold && p[1] >= threshold && p[2] >= threshold
}

fn push(
    img: &RgbaImage,
    x: u32,
    y: u32,
    threshold: u8,
    queue: &mut VecDeque<(u32, u32)>,
    seen: &mut [bool],
) {
    let w = img.width();
    let i = (y * w + x) as usize;
    if !seen[i] && is_white(img.get_pixel(x, y), threshold) {
        seen[i] = true;
        queue.push_back((x, y));
    }
}

/// Clears the edge-connected white background, returning the pixel count made
/// transparent. Pure: only the passed image is mutated.
pub fn clear_edge_white(img: &mut RgbaImage, threshold: u8) -> u32 {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return 0;
    }

    let mut seen = vec![false; (w * h) as usize];
    let mut queue = VecDeque::new();

    for x in 0..w {
        push(img, x, 0, threshold, &mut queue, &mut seen);
        push(img, x, h - 1, threshold, &mut queue, &mut seen);
    }
    for y in 0..h {
        push(img, 0, y, threshold, &mut queue, &mut seen);
        push(img, w - 1, y, threshold, &mut queue, &mut seen);
    }

    let mut cleared = 0u32;
    while let Some((x, y)) = queue.pop_front() {
        let pixel = img.get_pixel_mut(x, y);
        pixel[3] = 0;
        cleared += 1;
        if x > 0 {
            push(img, x - 1, y, threshold, &mut queue, &mut seen);
        }
        if x + 1 < w {
            push(img, x + 1, y, threshold, &mut queue, &mut seen);
        }
        if y > 0 {
            push(img, x, y - 1, threshold, &mut queue, &mut seen);
        }
        if y + 1 < h {
            push(img, x, y + 1, threshold, &mut queue, &mut seen);
        }
    }
    cleared
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);
    const INK: Rgba<u8> = Rgba([20, 20, 20, 255]);

    fn ringed_card(size: u32) -> RgbaImage {
        let mut img = RgbaImage::from_pixel(size, size, WHITE);
        // A one-pixel ink ring enclosing a white centre.
        for i in 2..size - 2 {
            img.put_pixel(i, 2, INK);
            img.put_pixel(i, size - 3, INK);
            img.put_pixel(2, i, INK);
            img.put_pixel(size - 3, i, INK);
        }
        img
    }

    #[test]
    fn only_edge_connected_white_becomes_transparent() {
        let mut img = ringed_card(9);
        let cleared = clear_edge_white(&mut img, WHITE_THRESHOLD);
        assert!(cleared > 0);
        // The outer card is gone, including all four corners.
        for (x, y) in [(0, 0), (8, 0), (0, 8), (8, 8)] {
            assert_eq!(img.get_pixel(x, y)[3], 0, "corner {x},{y}");
        }
        // The ring is untouched.
        assert_eq!(*img.get_pixel(2, 2), INK);
        // White enclosed by the ring survives.
        assert_eq!(*img.get_pixel(4, 4), WHITE);
    }

    #[test]
    fn non_white_edges_are_left_alone() {
        let mut img = RgbaImage::from_pixel(4, 4, Rgba([10, 20, 30, 255]));
        assert_eq!(clear_edge_white(&mut img, WHITE_THRESHOLD), 0);
        assert!(img.pixels().all(|p| p[3] == 255));
    }

    #[test]
    fn generated_brand_icon_keeps_its_corners_and_body() {
        let img = image::load_from_memory(include_bytes!(
            "../../../assets/logo/fastpin-transparent.png"
        ))
        .expect("decode fastpin-transparent.png")
        .to_rgba8();
        let (w, h) = img.dimensions();
        assert!(w > 16 && h > 16);
        for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
            assert_eq!(img.get_pixel(x, y)[3], 0, "corner {x},{y}");
        }
        let opaque = img.pixels().filter(|p| p[3] > 0).count();
        assert!(opaque > (w * h) as usize / 10, "body missing: {opaque}");
        // An opaque near-white pixel proves the glyph's interior white was
        // kept rather than flood-filled away.
        assert!(img.pixels().any(|p| p[3] == 255
            && p[0] >= WHITE_THRESHOLD
            && p[1] >= WHITE_THRESHOLD
            && p[2] >= WHITE_THRESHOLD));
    }
}
