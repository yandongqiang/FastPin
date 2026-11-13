//! One memory budget for every image buffer the app allocates.
//!
//! Before this module each path carried its own one-dimensional cap
//! (`pin_snap::MAX_CANVAS_SIDE`, `overlay::LONG_MAX_H`) and allocated with
//! `vec![0; n]`, which aborts the process under `panic = "abort"` when the
//! request cannot be satisfied. The limits here are the *existing* usable
//! ceilings, not tighter ones: a 16384 px pin group composed today still
//! composes. What changes is that every request is checked with overflow-safe
//! arithmetic and every allocation is fallible.
//!
//! The budget is a plain value with no globals, so the tests can build small
//! budgets and exercise each limit without allocating gigabytes.

use std::fmt;

/// Longest side of any image the app will allocate. Matches
/// `pin_snap::MAX_CANVAS_SIDE`, the long-capture `LONG_MAX_H` (applied to the
/// scroll axis) and the 16384 px D3D11 texture limit the result is shown
/// through, so no path that worked before is narrowed.
pub const MAX_SIDE: u32 = 16384;

/// 16384 x 16384: the largest canvas the side caps already allowed (a square
/// pin group). Anything larger was already rejected by `compose`, so this is
/// the existing ceiling expressed as a pixel count.
pub const MAX_PIXELS: u64 = MAX_SIDE as u64 * MAX_SIDE as u64;

/// Bytes held by one image. The largest buffer any existing path builds is a
/// 16384^2 BGRA pin group: 4 B/px * 268_435_456 px = exactly 1 GiB. The extra
/// 128 MiB leaves room for the clipboard DIB header, the PNG encoder's output
/// and its copy, so the ceiling above is preserved rather than lowered.
pub const MAX_BYTES: u64 = 1024 * 1024 * 1024 + 128 * 1024 * 1024;

/// Bytes per pixel of the tight BGRA / RGBA buffers used throughout.
pub const RGBA_BPP: u64 = 4;

/// A checked size or allocation request that the budget refused.
///
/// Every variant carries the requested value and the limit so a log line or a
/// status label can say exactly which dimension was too large. Nothing is
/// clamped: callers must propagate the error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BudgetError {
    TooWide {
        width: u64,
        max: u64,
    },
    TooHigh {
        height: u64,
        max: u64,
    },
    TooManyPixels {
        width: u64,
        height: u64,
        pixels: u64,
        max: u64,
    },
    TooManyBytes {
        width: u64,
        height: u64,
        bytes_per_pixel: u64,
        needed: u64,
        max: u64,
    },
    /// A product in the size arithmetic does not fit in `u64`.
    Overflow {
        width: u64,
        height: u64,
        bytes_per_pixel: u64,
    },
    /// The caller's buffer is smaller than the dimensions it describes.
    ShortBuffer {
        needed: u64,
        available: u64,
    },
    /// A row stride smaller than `width * 4`.
    ShortStride {
        stride: u64,
        minimum: u64,
    },
    /// The budget was satisfied but the allocator refused, or `try_reserve`
    /// failed.
    AllocFailed {
        bytes: u64,
    },
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            BudgetError::TooWide { width, max } => {
                write!(f, "image width {width} px exceeds the {max} px width limit")
            }
            BudgetError::TooHigh { height, max } => {
                write!(
                    f,
                    "image height {height} px exceeds the {max} px height limit"
                )
            }
            BudgetError::TooManyPixels {
                width,
                height,
                pixels,
                max,
            } => write!(
                f,
                "image {width}x{height} = {pixels} px exceeds the {max} px budget"
            ),
            BudgetError::TooManyBytes {
                width: 0,
                height: 0,
                needed,
                max,
                ..
            } => write!(f, "buffer of {needed} bytes exceeds the {max} byte budget"),
            BudgetError::TooManyBytes {
                width,
                height,
                bytes_per_pixel,
                needed,
                max,
            } => write!(
                f,
                "image {width}x{height} at {bytes_per_pixel} B/px needs {needed} bytes, \
                 over the {max} byte budget"
            ),
            BudgetError::Overflow {
                width,
                height,
                bytes_per_pixel: 0,
            } => write!(f, "image {width}x{height} overflows pixel-count arithmetic"),
            BudgetError::Overflow {
                width,
                height,
                bytes_per_pixel,
            } => write!(
                f,
                "image {width}x{height} at {bytes_per_pixel} B/px overflows size arithmetic"
            ),
            BudgetError::ShortBuffer { needed, available } => write!(
                f,
                "image buffer holds {available} bytes but {needed} are required"
            ),
            BudgetError::ShortStride { stride, minimum } => {
                write!(f, "row stride {stride} B is below the {minimum} B minimum")
            }
            BudgetError::AllocFailed { bytes } => {
                write!(f, "could not allocate {bytes} bytes for an image")
            }
        }
    }
}

impl std::error::Error for BudgetError {}

impl From<BudgetError> for crate::error::Error {
    fn from(e: BudgetError) -> Self {
        crate::error::Error::new(e.to_string())
    }
}

/// The app-wide budget. `Default` is the standard one; tests build smaller
/// instances to reach each limit without huge allocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageBudget {
    pub max_width: u64,
    pub max_height: u64,
    pub max_pixels: u64,
    pub max_bytes: u64,
}

impl Default for ImageBudget {
    fn default() -> Self {
        Self::STANDARD
    }
}

impl ImageBudget {
    pub const STANDARD: ImageBudget = ImageBudget {
        max_width: MAX_SIDE as u64,
        max_height: MAX_SIDE as u64,
        max_pixels: MAX_PIXELS,
        max_bytes: MAX_BYTES,
    };

    pub const fn new(max_width: u64, max_height: u64, max_pixels: u64, max_bytes: u64) -> Self {
        ImageBudget {
            max_width,
            max_height,
            max_pixels,
            max_bytes,
        }
    }

    /// Validates width, height and the pixel count, returning them as `u64`.
    /// Every product is `checked_mul`, so no input can wrap.
    pub fn checked_dims(&self, width: u64, height: u64) -> Result<(u64, u64), BudgetError> {
        if width > self.max_width {
            return Err(BudgetError::TooWide {
                width,
                max: self.max_width,
            });
        }
        if height > self.max_height {
            return Err(BudgetError::TooHigh {
                height,
                max: self.max_height,
            });
        }
        let pixels = width.checked_mul(height).ok_or(BudgetError::Overflow {
            width,
            height,
            bytes_per_pixel: 0,
        })?;
        if pixels > self.max_pixels {
            return Err(BudgetError::TooManyPixels {
                width,
                height,
                pixels,
                max: self.max_pixels,
            });
        }
        Ok((width, height))
    }

    /// Bytes for a tight `width x height` buffer at `bytes_per_pixel`, after
    /// all four limits. The result fits `usize` on every platform we build
    /// for; the conversion is still checked.
    pub fn checked_bytes(
        &self,
        width: u64,
        height: u64,
        bytes_per_pixel: u64,
    ) -> Result<usize, BudgetError> {
        let (width, height) = self.checked_dims(width, height)?;
        let needed = width
            .checked_mul(height)
            .and_then(|px| px.checked_mul(bytes_per_pixel))
            .ok_or(BudgetError::Overflow {
                width,
                height,
                bytes_per_pixel,
            })?;
        if needed > self.max_bytes {
            return Err(BudgetError::TooManyBytes {
                width,
                height,
                bytes_per_pixel,
                needed,
                max: self.max_bytes,
            });
        }
        usize::try_from(needed).map_err(|_| BudgetError::Overflow {
            width,
            height,
            bytes_per_pixel,
        })
    }

    /// Byte-only check for buffers whose geometry is already known (an
    /// encoded PNG, a clipboard block).
    pub fn checked_len(&self, bytes: u64) -> Result<usize, BudgetError> {
        if bytes > self.max_bytes {
            return Err(BudgetError::TooManyBytes {
                width: 0,
                height: 0,
                bytes_per_pixel: 0,
                needed: bytes,
                max: self.max_bytes,
            });
        }
        usize::try_from(bytes).map_err(|_| BudgetError::Overflow {
            width: 0,
            height: 0,
            bytes_per_pixel: 0,
        })
    }

    /// Full check for a caller-supplied BGRA buffer: dimensions, stride and
    /// the actual slice length. Returns the tight `stride * height` byte
    /// count.
    pub fn check_image(
        &self,
        width: u64,
        height: u64,
        stride: u64,
        buffer_len: usize,
    ) -> Result<usize, BudgetError> {
        let (width, height) = self.checked_dims(width, height)?;
        let minimum = width.checked_mul(RGBA_BPP).ok_or(BudgetError::Overflow {
            width,
            height,
            bytes_per_pixel: RGBA_BPP,
        })?;
        if stride < minimum {
            return Err(BudgetError::ShortStride { stride, minimum });
        }
        let needed = stride.checked_mul(height).ok_or(BudgetError::Overflow {
            width,
            height,
            bytes_per_pixel: RGBA_BPP,
        })?;
        if (buffer_len as u64) < needed {
            return Err(BudgetError::ShortBuffer {
                needed,
                available: buffer_len as u64,
            });
        }
        let needed = self.checked_len(needed)?;
        Ok(needed)
    }

    /// Zeroed, fallibly allocated BGRA buffer of exactly `width * height * 4`
    /// bytes.
    pub fn alloc_rgba(&self, width: u64, height: u64) -> Result<Vec<u8>, BudgetError> {
        let len = self.checked_bytes(width, height, RGBA_BPP)?;
        self.alloc_zeroed(len)
    }

    /// Zeroed, fallibly allocated buffer of `bytes` bytes.
    pub fn alloc_zeroed(&self, bytes: usize) -> Result<Vec<u8>, BudgetError> {
        self.checked_len(bytes as u64)?;
        let mut out = Vec::new();
        out.try_reserve_exact(bytes)
            .map_err(|_| BudgetError::AllocFailed {
                bytes: bytes as u64,
            })?;
        // The capacity was just reserved, so this cannot reallocate.
        out.resize(bytes, 0);
        Ok(out)
    }

    /// Fallibly grows `buf` by `extra` bytes, keeping its contents. Used by
    /// the stitcher before `extend_from_slice`, which would otherwise abort on
    /// an infallible reallocation.
    pub fn try_grow(&self, buf: &mut Vec<u8>, extra: usize) -> Result<(), BudgetError> {
        buf.try_reserve(extra)
            .map_err(|_| BudgetError::AllocFailed {
                bytes: buf.len().saturating_add(extra) as u64,
            })
    }
}

/// The budget every production path uses.
pub const BUDGET: ImageBudget = ImageBudget::STANDARD;

#[cfg(test)]
mod tests {
    use super::*;

    /// A budget with realistic sides but tiny pixel/byte caps, so the pixel
    /// and byte branches are reachable without allocating anything large.
    fn small() -> ImageBudget {
        ImageBudget::new(100, 100, 1_000, 4_000)
    }

    #[test]
    fn normal_dimensions_pass_and_report_their_size() {
        let b = ImageBudget::default();
        assert_eq!(b.checked_dims(1920, 1080).unwrap(), (1920, 1080));
        assert_eq!(b.checked_bytes(1920, 1080, 4).unwrap(), 1920 * 1080 * 4);
        let px = b.alloc_rgba(16, 8).unwrap();
        assert_eq!(px.len(), 16 * 8 * 4);
        assert!(px.iter().all(|&v| v == 0), "allocation must be zeroed");
    }

    #[test]
    fn width_limit_is_reported_with_request_and_limit() {
        let err = ImageBudget::default().checked_dims(16_385, 10).unwrap_err();
        assert!(matches!(
            err,
            BudgetError::TooWide {
                width: 16_385,
                max: 16_384
            }
        ));
        let msg = err.to_string();
        assert!(msg.contains("16385"), "{msg}");
        assert!(msg.contains("16384"), "{msg}");
    }

    #[test]
    fn height_limit_is_reported_with_request_and_limit() {
        let err = ImageBudget::default().checked_dims(10, 16_385).unwrap_err();
        assert!(matches!(
            err,
            BudgetError::TooHigh {
                height: 16_385,
                max: 16_384
            }
        ));
        assert!(err.to_string().contains("16385"));
    }

    #[test]
    fn pixel_limit_is_reported() {
        // 40 x 30 = 1200 > 1000, both sides well under the 100 px limits.
        let err = small().checked_dims(40, 30).unwrap_err();
        assert!(matches!(
            err,
            BudgetError::TooManyPixels {
                pixels: 1_200,
                max: 1_000,
                ..
            }
        ));
        let msg = err.to_string();
        assert!(msg.contains("1200"), "{msg}");
        assert!(msg.contains("1000"), "{msg}");
    }

    #[test]
    fn byte_limit_is_reported() {
        // 40 x 30 = 1200 px <= 5000, but 1200 * 4 = 4800 > 4000 bytes.
        let b = ImageBudget::new(100, 100, 5_000, 4_000);
        let err = b.checked_bytes(40, 30, 4).unwrap_err();
        assert!(matches!(
            err,
            BudgetError::TooManyBytes {
                needed: 4_800,
                max: 4_000,
                ..
            }
        ));
        let msg = err.to_string();
        assert!(msg.contains("4800"), "{msg}");
        assert!(msg.contains("4000"), "{msg}");
    }

    #[test]
    fn u32_max_dimensions_do_not_overflow_or_panic() {
        // Under the standard budget the side check answers first.
        let err = ImageBudget::default()
            .checked_dims(u32::MAX as u64, u32::MAX as u64)
            .unwrap_err();
        assert!(matches!(err, BudgetError::TooWide { .. }));

        // With no caps at all, a pixel product past 2^64 is the thing that
        // has to be caught: (2^32)^2 does not fit in u64.
        let unbounded = ImageBudget::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let err = unbounded.checked_dims(1 << 32, 1 << 32).unwrap_err();
        assert!(matches!(err, BudgetError::Overflow { .. }));
        assert!(err.to_string().contains("overflow"), "{err}");
    }

    #[test]
    fn checked_bytes_overflow_is_an_error_not_a_wrap() {
        let unbounded = ImageBudget::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        assert!(matches!(
            unbounded.checked_bytes(usize::MAX as u64, 2, 4),
            Err(BudgetError::Overflow { .. })
        ));
    }

    #[test]
    fn alloc_rgba_outside_the_pixel_budget_fails_without_allocating() {
        let err = small().alloc_rgba(40, 30).unwrap_err();
        assert!(matches!(err, BudgetError::TooManyPixels { .. }));
    }

    #[test]
    fn alloc_rgba_outside_the_byte_budget_fails() {
        let b = ImageBudget::new(100, 100, 5_000, 4_000);
        assert!(matches!(
            b.alloc_rgba(40, 30),
            Err(BudgetError::TooManyBytes { .. })
        ));
    }

    #[test]
    fn check_image_rejects_a_short_buffer_and_stride() {
        let b = ImageBudget::default();
        assert!(matches!(
            b.check_image(2, 2, 8, 15),
            Err(BudgetError::ShortBuffer {
                needed: 16,
                available: 15
            })
        ));
        assert!(matches!(
            b.check_image(2, 2, 4, 16),
            Err(BudgetError::ShortStride {
                stride: 4,
                minimum: 8
            })
        ));
        assert_eq!(b.check_image(2, 2, 8, 16).unwrap(), 16);
    }

    #[test]
    fn standard_budget_keeps_the_existing_1_gib_ceiling() {
        // The worst group `compose` accepted before this module existed must
        // still be accepted: 16384^2 BGRA = 1 GiB.
        let b = ImageBudget::default();
        let (w, h) = b.checked_dims(16_384, 16_384).unwrap();
        assert_eq!(w * h * 4, 1024 * 1024 * 1024);
        assert!(b.checked_bytes(16_384, 16_384, 4).is_ok());
        // One pixel over either side is still refused.
        assert!(b.checked_bytes(16_385, 16_384, 4).is_err());
        assert!(b.checked_bytes(16_384, 16_385, 4).is_err());
    }

    #[test]
    fn checked_len_caps_encoded_buffers() {
        let b = ImageBudget::default();
        assert_eq!(b.checked_len(1024).unwrap(), 1024);
        let err = b.checked_len(MAX_BYTES + 1).unwrap_err();
        assert!(matches!(err, BudgetError::TooManyBytes { .. }));
        assert!(err.to_string().contains("buffer of"), "{err}");
    }
}
