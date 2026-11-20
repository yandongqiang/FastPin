//! Screen colour picker, ported from snow-shot's `ScreenshotColorPicker`.
//!
//! The picker samples one pixel from a frozen BGRA capture and presents it in
//! one of four formats: `#RRGGBB`, `RRGGBB`, `rgb(r, g, b)` and
//! `hsl(h°, s%, l%)`. All of that is pure data work with no GPUI dependency so
//! the sampling and conversion rules are unit-testable without a display; the
//! overlay drives it from the magnifier buffer that already tracks the cursor.

/// An opaque RGB sample. Alpha is intentionally dropped: a screen capture has
/// no meaningful alpha and snow-shot's picker builds `QColor(r, g, b, 255)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Read one pixel out of a (possibly strided) BGRA8 buffer. `None` when the
    /// coordinate falls outside the image or the buffer is too short. BGRA byte
    /// order is `[B, G, R, A]` — matching every other BGRA surface in FastPin.
    pub fn from_bgra(
        pixels: &[u8],
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Option<Self> {
        if x >= width || y >= height {
            return None;
        }
        let row = y.checked_mul(stride)?;
        let i = row.checked_add(x.checked_mul(4)?)? as usize;
        let slice = pixels.get(i..i + 3)?;
        Some(Self {
            b: slice[0],
            g: slice[1],
            r: slice[2],
        })
    }
}

/// The four display formats, in the exact order `cycleColorFormat` rotates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorFormat {
    Hex,
    HexWithoutHash,
    Rgb,
    Hsl,
}

impl ColorFormat {
    /// snow-shot's `cycleColorFormat`: Hex → HexWithoutHash → Rgb → Hsl → Hex.
    pub fn next(self) -> Self {
        match self {
            Self::Hex => Self::HexWithoutHash,
            Self::HexWithoutHash => Self::Rgb,
            Self::Rgb => Self::Hsl,
            Self::Hsl => Self::Hex,
        }
    }
}

/// Hue / saturation / lightness of a colour. `h` is degrees in 0..360, `s` and
/// `l` are ratios in 0..1 (the formatter multiplies by 100 for display).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hsl {
    pub h: f64,
    pub s: f64,
    pub l: f64,
}

/// Standard RGB→HSL conversion. Matches `QColor::toHsl()` closely enough for
/// display: pure red/green/blue land on 0/120/240°, greys have zero saturation.
pub fn to_hsl(color: Color) -> Hsl {
    let r = f64::from(color.r) / 255.0;
    let g = f64::from(color.g) / 255.0;
    let b = f64::from(color.b) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    let s = if d == 0.0 {
        0.0
    } else {
        d / (1.0 - (2.0 * l - 1.0).abs())
    };
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let h = if h < 0.0 { h + 360.0 } else { h };
    Hsl { h, s, l }
}

/// Render a colour as text in the requested format, mirroring snow-shot's
/// `ScreenshotColorPickerWindow::formatColor`.
pub fn format_color(color: Color, format: ColorFormat) -> String {
    match format {
        ColorFormat::Hex => format!("#{:02X}{:02X}{:02X}", color.r, color.g, color.b),
        ColorFormat::HexWithoutHash => format!("{:02X}{:02X}{:02X}", color.r, color.g, color.b),
        ColorFormat::Rgb => format!("rgb({}, {}, {})", color.r, color.g, color.b),
        ColorFormat::Hsl => {
            let hsl = to_hsl(color);
            format!(
                "hsl({:.0}°, {:.1}%, {:.1}%)",
                hsl.h,
                hsl.s * 100.0,
                hsl.l * 100.0
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn bgra_bytes_are_read_in_rgb_order() {
        // Tight BGRA for pure red: B=0 G=0 R=255 A=255.
        let buf = [0, 0, 255, 255];
        assert_eq!(
            Color::from_bgra(&buf, 0, 0, 1, 1, 4),
            Some(Color::new(255, 0, 0))
        );
        let blue = [255, 0, 0, 255];
        assert_eq!(
            Color::from_bgra(&blue, 0, 0, 1, 1, 4),
            Some(Color::new(0, 0, 255))
        );
    }

    #[test]
    fn stride_is_honoured_for_multi_row_buffers() {
        // 2×2 BGRA: row0 = red, green; row1 = blue, white.
        let buf = [
            0, 0, 255, 255, 0, 255, 0, 255, // row 0
            255, 0, 0, 255, 255, 255, 255, 255, // row 1
        ];
        assert_eq!(
            Color::from_bgra(&buf, 1, 0, 2, 2, 8),
            Some(Color::new(0, 255, 0))
        );
        assert_eq!(
            Color::from_bgra(&buf, 0, 1, 2, 2, 8),
            Some(Color::new(0, 0, 255))
        );
    }

    #[test]
    fn out_of_range_and_short_buffers_return_none() {
        let buf = [0, 0, 255, 255];
        assert_eq!(Color::from_bgra(&buf, 1, 0, 1, 1, 4), None);
        assert_eq!(Color::from_bgra(&buf, 0, 1, 1, 1, 4), None);
        // The second row of a strided buffer falls past a one-row allocation.
        assert_eq!(Color::from_bgra(&buf, 0, 1, 1, 2, 8), None);
        // A truncated buffer missing the red byte cannot produce a colour.
        assert_eq!(Color::from_bgra(&[0, 0], 0, 0, 1, 1, 4), None);
    }

    #[test]
    fn all_four_formats_are_rendered() {
        let red = Color::new(255, 0, 0);
        assert_eq!(format_color(red, ColorFormat::Hex), "#FF0000");
        assert_eq!(format_color(red, ColorFormat::HexWithoutHash), "FF0000");
        assert_eq!(format_color(red, ColorFormat::Rgb), "rgb(255, 0, 0)");
        assert_eq!(
            format_color(red, ColorFormat::Hsl),
            "hsl(0°, 100.0%, 50.0%)"
        );
    }

    #[test]
    fn format_rotation_wraps_back_to_hex() {
        let mut format = ColorFormat::Hex;
        format = format.next();
        assert_eq!(format, ColorFormat::HexWithoutHash);
        format = format.next();
        assert_eq!(format, ColorFormat::Rgb);
        format = format.next();
        assert_eq!(format, ColorFormat::Hsl);
        format = format.next();
        assert_eq!(format, ColorFormat::Hex);
    }

    #[test]
    fn hsl_lands_on_the_expected_hues() {
        let hsl = to_hsl(Color::new(0, 255, 0));
        assert!(close(hsl.h, 120.0), "hue {}", hsl.h);
        assert!(close(hsl.s, 1.0));
        assert!(close(hsl.l, 0.5));

        let hsl = to_hsl(Color::new(0, 0, 255));
        assert!(close(hsl.h, 240.0), "hue {}", hsl.h);
    }

    #[test]
    fn greys_have_no_saturation() {
        let white = to_hsl(Color::new(255, 255, 255));
        assert!(close(white.s, 0.0));
        assert!(close(white.l, 1.0));
        let black = to_hsl(Color::new(0, 0, 0));
        assert!(close(black.s, 0.0));
        assert!(close(black.l, 0.0));
        let grey = to_hsl(Color::new(128, 128, 128));
        assert!(close(grey.s, 0.0));
    }
}
