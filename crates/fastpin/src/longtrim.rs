//! Trim range for a running long capture, ported from the snow-shot scrolling
//! capture controller (`ScreenshotScrollingTrimRange`, `setTrimRange`,
//! `trimmedSize`, `requestTrimmedSnapshot`).
//!
//! A long capture keeps growing while the user scrolls, but the interesting
//! part is usually a window inside it: the top of the page is often a header
//! the user does not want, and the bottom is whatever happened to be on screen
//! when they pressed 完成. snow-shot therefore lets the thumbnail's two handles
//! pick a `[top, bottom)` range and exports only that.
//!
//! Everything here is pure arithmetic on canvas rows in screen space, so the
//! growth rules can be unit-tested without a capture session: the range has to
//! survive appends (it grows to the new bottom, because more content became
//! available) and prepends (both edges shift down, because rows were inserted
//! above the old content).

/// A `[top, bottom)` row range inside the stitched canvas, in screen-space
/// canvas rows. Ported from `ScreenshotScrollingTrimRange`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrimRange {
    pub top: u32,
    pub bottom: u32,
}

impl TrimRange {
    /// A range covering the whole canvas: the default for every new session, so
    /// 完成 without touching a handle exports everything.
    pub fn full(extent: u32) -> Self {
        TrimRange {
            top: 0,
            bottom: extent,
        }
    }

    /// Whether the range covers at least one row. Ported from `isValid()`,
    /// which is `bottom > top`.
    pub fn is_valid(self) -> bool {
        self.bottom > self.top
    }

    /// Rows selected by this range.
    pub fn extent(self) -> u32 {
        self.bottom.saturating_sub(self.top)
    }

    /// Clamps into `[0, canvas]`, keeping at least one row when the canvas has
    /// any. `None` for an empty canvas.
    ///
    /// This is the guard every mutation goes through: a session can shrink its
    /// canvas (the height cap, a rebaseline) and the handles must never point
    /// outside it.
    pub fn clamped(self, canvas: u32) -> Option<Self> {
        if canvas == 0 {
            return None;
        }
        let top = self.top.min(canvas - 1);
        let bottom = self.bottom.clamp(top + 1, canvas);
        Some(TrimRange { top, bottom })
    }

    /// Moves one edge, keeping the range valid. `edge` selects which one; the
    /// thumbnail drags call this directly.
    pub fn with_edge(self, edge: TrimEdge, row: u32, canvas: u32) -> Option<Self> {
        if canvas == 0 {
            return None;
        }
        let candidate = match edge {
            // Always keep a row between the handles, so a drag past the other
            // edge stops instead of inverting the range.
            TrimEdge::Top => TrimRange {
                top: row.min(self.bottom.saturating_sub(1)),
                bottom: self.bottom,
            },
            TrimEdge::Bottom => TrimRange {
                top: self.top,
                bottom: row.max(self.top + 1),
            },
        };
        candidate.clamped(canvas)
    }

    /// The range after the canvas grew at the bottom (an append).
    ///
    /// Ported from the controller's append branch, which is literally
    /// `trimRange->bottom = extent`: newly captured content is always included,
    /// so scrolling never hides what just arrived behind an older bottom
    /// handle. The top handle is the user's own choice and is preserved.
    pub fn after_append(self, canvas: u32) -> Option<Self> {
        TrimRange {
            top: self.top,
            bottom: canvas.max(self.top + 1),
        }
        .clamped(canvas)
    }

    /// The range after `added` rows were inserted at the top (a prepend): both
    /// edges shift down so the same content stays selected.
    ///
    /// Ported from `trimRange = {0, min(extent, bottom + addedRows)}` -- the
    /// upstream controller pins the top to 0 on a prepend, because the inserted
    /// rows are above everything the user could previously have selected.
    pub fn after_prepend(self, added: u32, canvas: u32) -> Option<Self> {
        let grown = TrimRange {
            top: 0,
            bottom: self.bottom.saturating_add(added),
        };
        grown.clamped(canvas)
    }

    /// The range after the canvas was replaced wholesale (axis flip, reset,
    /// first frame): everything is selected again.
    pub fn after_reset(canvas: u32) -> Option<Self> {
        TrimRange::full(canvas).clamped(canvas)
    }

    /// Slides the whole range by `rows`, keeping its length. Used by a band drag
    /// (grabbing between the handles moves the window without resizing it).
    ///
    /// Clamped at both canvas edges rather than shrinking, so the selection
    /// keeps its size while it slides.
    pub fn translated(self, rows: i64, canvas: u32) -> Option<Self> {
        let range = self.clamped(canvas)?;
        let length = range.extent() as i64;
        let max_top = canvas as i64 - length;
        let top = (range.top as i64 + rows).clamp(0, max_top.max(0));
        Some(TrimRange {
            top: top as u32,
            bottom: (top + length) as u32,
        })
    }

    /// Copies the selected rows out of a tightly packed BGRA canvas. Used by the
    /// export path on 完成, and by the preview window when it opens trimmed.
    ///
    /// Returns `None` when the range or the buffer does not describe a real
    /// image, so a malformed session can never produce a shifted export.
    pub fn crop_bgra(self, pixels: &[u8], width: u32, canvas: u32) -> Option<(Vec<u8>, u32, u32)> {
        let range = self.clamped(canvas)?;
        let row_bytes = width as usize * 4;
        if row_bytes == 0 {
            return None;
        }
        let needed = row_bytes.checked_mul(canvas as usize)?;
        if pixels.len() < needed {
            return None;
        }
        let start = row_bytes.checked_mul(range.top as usize)?;
        let end = row_bytes.checked_mul(range.bottom as usize)?;
        if end > pixels.len() {
            return None;
        }
        Some((pixels[start..end].to_vec(), width, range.extent()))
    }

    /// Copies the selection out of a BGRA canvas along `axis`.
    ///
    /// A vertical session grows downwards, so the range selects *rows*; a
    /// horizontal session grows sideways, so the same range selects *columns*
    /// and the output is re-packed with a new stride. Both are the same
    /// operation the export performs, which is why they share the clamping and
    /// validation above.
    pub fn crop_bgra_axis(
        self,
        pixels: &[u8],
        width: u32,
        height: u32,
        axis: crate::longcapture::Axis,
    ) -> Option<(Vec<u8>, u32, u32)> {
        match axis {
            crate::longcapture::Axis::Vertical => self.crop_bgra(pixels, width, height),
            crate::longcapture::Axis::Horizontal => {
                // `finish()` 已把画布转回屏幕空间：这里的 `width` 是增长后的
                // 长边 L，`height` 才是常量屏幕高。trim 的两端必须按 L 钳制并
                // 抽列；误用 h 会把 bottom 钉在首屏，导出只剩前 h 列。
                let range = self.clamped(width)?;
                let source_row = width as usize * 4;
                let needed = source_row.checked_mul(height as usize)?;
                if pixels.len() < needed || height == 0 {
                    return None;
                }
                let picked = range.extent() as usize;
                let out_row = picked * 4;
                let mut out = Vec::with_capacity(out_row * height as usize);
                for row in 0..height as usize {
                    let base = row * source_row + range.top as usize * 4;
                    out.extend_from_slice(&pixels[base..base + out_row]);
                }
                Some((out, range.extent(), height))
            }
        }
    }
}

/// Which handle of a trim range is being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimEdge {
    Top,
    Bottom,
}

/// Result of a hit test on the thumbnail's trim handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimHit {
    /// The pointer grabbed a handle and is now dragging it.
    Handle(TrimEdge),
    /// The pointer is inside the selected band: a drag moves the whole range.
    Band,
    /// Nothing trim-related under the pointer.
    None,
}

/// Hit radius around a handle, in the thumbnail's device pixels. Ported from
/// the `kHandleHitRadius` the snow-shot thumbnail uses: wide enough to grab with
/// a mouse, narrow enough that the band is still draggable in between.
pub const HANDLE_HIT_RADIUS: f32 = 6.0;

/// Hit tests the thumbnail's trim handles.
///
/// `y` is the pointer position in thumbnail pixels, `thumb_height` the drawn
/// thumbnail height, and `range`/`canvas` describe the trim in canvas rows. The
/// band itself is returned as [`TrimHit::Band`] so a drag inside it can move
/// both handles together; outside the band nothing is captured, which keeps the
/// scrolling capture bar's own buttons clickable.
pub fn hit_test(y: f32, thumb_height: f32, range: TrimRange, canvas: u32) -> TrimHit {
    if canvas == 0 || thumb_height <= 0.0 {
        return TrimHit::None;
    }
    let scale = thumb_height / canvas as f32;
    let top_y = range.top as f32 * scale;
    let bottom_y = range.bottom as f32 * scale;
    // The bottom handle wins ties so a collapsed-looking band at the very top
    // can still be opened downwards.
    if (y - bottom_y).abs() <= HANDLE_HIT_RADIUS {
        return TrimHit::Handle(TrimEdge::Bottom);
    }
    if (y - top_y).abs() <= HANDLE_HIT_RADIUS {
        return TrimHit::Handle(TrimEdge::Top);
    }
    if y > top_y && y < bottom_y {
        return TrimHit::Band;
    }
    TrimHit::None
}

/// 横向长截图的命中测试：`x` 是指针在缩略图上的水平坐标，`thumb_width` 是
/// 缩略图的绘制宽度。stitch 轴横向时 `[0, L]` 必须映射到宽度而不是那条几乎
/// 不变的屏幕高（否则长边被压成顶部一条细缝，手柄既画不出也点不中）。
/// 几何与 [`hit_test`] 完全相同，这里只换一个传入的轴与尺寸，纵向继续用旧函数。
pub fn hit_test_x(x: f32, thumb_width: f32, range: TrimRange, canvas: u32) -> TrimHit {
    hit_test(x, thumb_width, range, canvas)
}

/// Converts a thumbnail-space drag into a canvas row, clamped to the canvas.
/// Used by both handle drags and band drags.
pub fn row_at(y: f32, thumb_height: f32, canvas: u32) -> u32 {
    if canvas == 0 || thumb_height <= 0.0 {
        return 0;
    }
    let scale = canvas as f32 / thumb_height;
    (y.max(0.0) * scale).round().clamp(0.0, canvas as f32) as u32
}

/// Converts a pointer *delta* in thumbnail pixels into a canvas row count.
///
/// Drags need only the relative motion: the bar is laid out by GPUI, so the
/// handlers never learn the thumbnail's absolute top edge. Recording the
/// pointer position when the drag starts and adding this delta to the row that
/// was grabbed is equivalent and independent of layout.
pub fn rows_for_delta(delta_y: f32, thumb_height: f32, canvas: u32) -> i64 {
    if canvas == 0 || thumb_height <= 0.0 {
        return 0;
    }
    let scale = canvas as f32 / thumb_height;
    (delta_y as f64 * scale as f64).round() as i64
}

/// Applies a drag of one handle: `start_row` is the row the handle was on when
/// the drag began, `delta_y` the pointer movement since then.
pub fn dragged_edge(
    range: TrimRange,
    edge: TrimEdge,
    start_row: u32,
    delta_y: f32,
    thumb_height: f32,
    canvas: u32,
) -> Option<TrimRange> {
    let moved = rows_for_delta(delta_y, thumb_height, canvas);
    let row = (start_row as i64 + moved).clamp(0, canvas as i64) as u32;
    range.with_edge(edge, row, canvas)
}

/// Applies a band drag: the whole range slides by the pointer delta, keeping
/// its length.
pub fn dragged_band(
    range: TrimRange,
    delta_y: f32,
    thumb_height: f32,
    canvas: u32,
) -> Option<TrimRange> {
    let moved = rows_for_delta(delta_y, thumb_height, canvas);
    range.translated(moved, canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_session_selects_everything() {
        let range = TrimRange::full(1000);
        assert!(range.is_valid());
        assert_eq!(range.extent(), 1000);
        assert_eq!(range.top, 0);
        assert_eq!(range.bottom, 1000);
    }

    #[test]
    fn clamping_keeps_the_range_inside_the_canvas() {
        let canvas = 500;
        // A bottom past the canvas is pulled in, not rejected.
        let range = TrimRange {
            top: 100,
            bottom: 900,
        }
        .clamped(canvas)
        .unwrap();
        assert_eq!(
            range,
            TrimRange {
                top: 100,
                bottom: 500
            }
        );
        // A top past the canvas leaves one row selected.
        let range = TrimRange {
            top: 900,
            bottom: 1000,
        }
        .clamped(canvas)
        .unwrap();
        assert_eq!(
            range,
            TrimRange {
                top: 499,
                bottom: 500
            }
        );
        assert_eq!(TrimRange::full(0).clamped(0), None);
    }

    #[test]
    fn an_append_extends_the_bottom_to_the_new_canvas() {
        let range = TrimRange {
            top: 100,
            bottom: 400,
        };
        // 200 new rows at the bottom, canvas is now 600.
        let grown = range.after_append(600).unwrap();
        assert_eq!(
            grown,
            TrimRange {
                top: 100,
                bottom: 600
            }
        );
        assert_eq!(grown.extent(), 500);
        // A user-chosen top survives, an already-full bottom is idempotent.
        assert_eq!(range.after_append(400).unwrap(), range);
    }

    #[test]
    fn a_prepend_shifts_both_edges_down_and_pins_the_top() {
        let range = TrimRange {
            top: 100,
            bottom: 400,
        };
        // 200 rows were inserted above: the selection's content moved down.
        let grown = range.after_prepend(200, 600).unwrap();
        assert!(grown.top == 0, "a prepend selects the newly revealed top");
        assert_eq!(grown.bottom, 600, "the old bottom shifts by the insert");
    }

    #[test]
    fn a_reset_selects_the_whole_new_canvas() {
        assert_eq!(TrimRange::after_reset(300), Some(TrimRange::full(300)));
    }

    #[test]
    fn a_band_drag_slides_the_range_without_resizing_it() {
        let canvas = 500;
        let range = TrimRange {
            top: 100,
            bottom: 300,
        };
        let moved = range.translated(50, canvas).unwrap();
        assert_eq!(
            moved,
            TrimRange {
                top: 150,
                bottom: 350
            }
        );
        assert_eq!(moved.extent(), range.extent());
        // Sliding past either edge clamps instead of shrinking.
        let moved = range.translated(-500, canvas).unwrap();
        assert_eq!(
            moved,
            TrimRange {
                top: 0,
                bottom: 200
            }
        );
        let moved = range.translated(500, canvas).unwrap();
        assert_eq!(
            moved,
            TrimRange {
                top: 300,
                bottom: 500
            }
        );
    }

    #[test]
    fn dragging_a_handle_stops_at_the_other_one() {
        let canvas = 500;
        let range = TrimRange {
            top: 100,
            bottom: 300,
        };
        // The top handle cannot cross the bottom handle.
        let moved = range.with_edge(TrimEdge::Top, 450, canvas).unwrap();
        assert_eq!(moved.top, 299);
        assert_eq!(moved.bottom, 300);
        // The bottom handle cannot cross the top one either.
        let moved = range.with_edge(TrimEdge::Bottom, 10, canvas).unwrap();
        assert_eq!(moved.top, 100);
        assert_eq!(moved.bottom, 101);
    }

    #[test]
    fn hit_testing_finds_both_handles_and_the_band() {
        let canvas = 1000;
        let range = TrimRange {
            top: 200,
            bottom: 600,
        };
        // 1000 canvas rows drawn into 100 px: 1 px = 10 rows.
        let thumb_h = 100.0;
        assert_eq!(
            hit_test(20.0, thumb_h, range, canvas),
            TrimHit::Handle(TrimEdge::Top)
        );
        assert_eq!(
            hit_test(60.0, thumb_h, range, canvas),
            TrimHit::Handle(TrimEdge::Bottom)
        );
        assert_eq!(hit_test(40.0, thumb_h, range, canvas), TrimHit::Band);
        assert_eq!(hit_test(5.0, thumb_h, range, canvas), TrimHit::None);
        assert_eq!(hit_test(95.0, thumb_h, range, canvas), TrimHit::None);
    }

    /// 横向的命中测试按 x 轴、用缩略图宽度 dw 缩放；旧逻辑按 dh 会把
    /// 长边 L 压成顶部细缝，宽画布上的手柄根本点不中。
    #[test]
    fn horizontal_hit_test_uses_the_thumbnail_width() {
        let canvas = 1000u32; // 横向增长长边 L
        let range = TrimRange {
            top: 200,
            bottom: 600,
        };
        let dw = 200.0f32; // 缩略图宽度；高度只有几十像素
                           // 1 px = 5 列：两个手柄落在 x=40 / x=120，band 在两者之间。
        assert_eq!(
            hit_test_x(40.0, dw, range, canvas),
            TrimHit::Handle(TrimEdge::Top)
        );
        assert_eq!(
            hit_test_x(120.0, dw, range, canvas),
            TrimHit::Handle(TrimEdge::Bottom)
        );
        assert_eq!(hit_test_x(80.0, dw, range, canvas), TrimHit::Band);
        assert_eq!(hit_test_x(5.0, dw, range, canvas), TrimHit::None);
        // 同一指针若误用 dh 作为尺寸，120 px 已在"画布"之外，什么都点不中。
        assert_eq!(hit_test_x(120.0, 40.0, range, canvas), TrimHit::None);
    }

    #[test]
    fn thumbnail_rows_map_back_to_canvas_rows() {
        // 1000 canvas rows in 100 px: a drag to y=35 selects row 350.
        assert_eq!(row_at(35.0, 100.0, 1000), 350);
        // Out-of-range drags clamp instead of wrapping.
        assert_eq!(row_at(-10.0, 100.0, 1000), 0);
        assert_eq!(row_at(500.0, 100.0, 1000), 1000);
        assert_eq!(row_at(10.0, 0.0, 1000), 0);
    }

    #[test]
    fn cropping_copies_exactly_the_selected_rows() {
        let (w, h) = (2u32, 4u32);
        // Four rows, each two BGRA pixels: row R is filled with byte R.
        let mut pixels = Vec::new();
        for row in 0..h {
            for _ in 0..w {
                pixels.extend_from_slice(&[row as u8, row as u8, row as u8, 255]);
            }
        }
        let range = TrimRange { top: 1, bottom: 3 };
        let (cropped, cw, ch) = range.crop_bgra(&pixels, w, h).unwrap();
        assert_eq!((cw, ch), (w, 2));
        assert_eq!(cropped.len(), (w * 2 * 4) as usize);
        // Every pixel of the first cropped row is row 1.
        assert_eq!(&cropped[..4], &[1, 1, 1, 255]);
    }

    /// 横向导出的回归用例：`finish()` 返回 (宽=L, 高=屏幕高 h)，trim 必须按 L
    /// 选列。全选时导出宽度等于 L；若按 h 钳制，则只剩前 h 列（旧 bug）。
    #[test]
    fn horizontal_crop_selects_columns_of_the_grown_long_side() {
        use crate::longcapture::Axis;

        let (l, h) = (6u32, 3u32);
        // finish() 的 BGRA：每一列填列号，行内字节一致，便于核对抽列结果。
        let mut pixels = Vec::new();
        for _row in 0..h {
            for col in 0..l {
                pixels.extend_from_slice(&[col as u8, col as u8, col as u8, 255]);
            }
        }

        // 选中 [2, 5) 列：输出宽 = 选中列数，高仍为屏幕高。
        let (out, w, out_h) = TrimRange { top: 2, bottom: 5 }
            .crop_bgra_axis(&pixels, l, h, Axis::Horizontal)
            .unwrap();
        assert_eq!((w, out_h), (3, h), "横向按列裁剪，不按行");
        assert_eq!(out.len(), (3 * h * 4) as usize);
        for row in 0..h as usize {
            let base = row * 3 * 4;
            assert_eq!(&out[base..base + 4], &[2, 2, 2, 255]);
            assert_eq!(&out[base + 8..base + 12], &[4, 4, 4, 255]);
        }

        // 全选导出必须覆盖整个长边，一份像素都不能少。
        let (full, fw, fh) = TrimRange::full(l)
            .crop_bgra_axis(&pixels, l, h, Axis::Horizontal)
            .unwrap();
        assert_eq!((fw, fh), (l, h));
        assert_eq!(full, pixels, "全选不得截断到首屏列数");

        // 旧 bug 的复现：把常量屏幕高 h 当成 canvas 传进来，bottom 被钉在 3。
        let (truncated, tw, _) = TrimRange::full(h)
            .crop_bgra_axis(&pixels, l, h, Axis::Horizontal)
            .unwrap();
        assert_eq!(tw, h);
        assert!(truncated.len() < full.len());
    }

    #[test]
    fn cropping_refuses_a_short_buffer() {
        let range = TrimRange { top: 0, bottom: 4 };
        // Only three of the four rows are present.
        let short = vec![0u8; 3 * 8];
        assert_eq!(range.crop_bgra(&short, 2, 4), None);
        // A range that clamps to nothing is refused too.
        assert_eq!(TrimRange::full(0).crop_bgra(&[], 2, 0), None);
    }

    #[test]
    fn a_handle_drag_follows_the_pointer_delta() {
        let canvas = 1000;
        let thumb_h = 100.0;
        let range = TrimRange {
            top: 200,
            bottom: 600,
        };
        // 1 px of thumbnail is 10 canvas rows: dragging the top handle down
        // 5 px moves it 50 rows, not to row 5.
        let moved = dragged_edge(range, TrimEdge::Top, range.top, 5.0, thumb_h, canvas).unwrap();
        assert_eq!(moved.top, 250);
        assert_eq!(moved.bottom, 600);
        // Dragging the bottom handle up shrinks the selection from the bottom.
        let moved = dragged_edge(
            range,
            TrimEdge::Bottom,
            range.bottom,
            -10.0,
            thumb_h,
            canvas,
        )
        .unwrap();
        assert_eq!(moved.bottom, 500);
        assert_eq!(moved.top, 200);
    }

    #[test]
    fn a_band_drag_slides_by_the_pointer_delta() {
        let canvas = 1000;
        let thumb_h = 100.0;
        let range = TrimRange {
            top: 200,
            bottom: 400,
        };
        let moved = dragged_band(range, 3.0, thumb_h, canvas).unwrap();
        assert_eq!(
            moved,
            TrimRange {
                top: 230,
                bottom: 430
            }
        );
        // A drag past the edge clamps without shrinking the band.
        let moved = dragged_band(range, -1000.0, thumb_h, canvas).unwrap();
        assert_eq!(moved.extent(), 200);
        assert_eq!(moved.top, 0);
    }

    #[test]
    fn delta_conversion_handles_a_zero_sized_thumbnail() {
        assert_eq!(rows_for_delta(10.0, 0.0, 1000), 0);
        assert_eq!(rows_for_delta(10.0, 100.0, 0), 0);
    }
}
