//! ORB-feature stitching adapter (snow-shot).
//!
//! This module is the bridge between FastPin's long-capture pipeline and the
//! vendored `snow-stitch-images` crate (GPL-3.0-or-later, see `NOTICE`).
//! Snow-shot aligns frames by matching ORB features and estimating the scroll
//! offset from their displacement, which is far more robust than FastPin's
//! old grayscale SAD strip matcher for smooth / sub-pixel scrolling, sticky
//! headers and low-texture pages.
//!
//! The adapter keeps FastPin's public long-capture contracts (`Stitcher`,
//! `StitchError`, `StitchStats`, `Axis`) so the UI overlay and the worker
//! protocol do not change. Internally it:
//!
//!   * converts each captured BGRA frame to RGBA (snow's estimator reads
//!     luminance with R at channel 0, G at 1, B at 2),
//!   * feeds it into a `snow_stitch_images::Stitcher` (configured for vertical
//!     motion; horizontal sessions are rotated to vertical before stitching),
//!   * maps each `StitchDecision` back to a FastPin `ScrollReport`-compatible
//!     result so the existing stall / verdict state machine keeps working,
//!   * enforces FastPin's height cap and image budget on top of snow, which
//!     has no such limits of its own.
//!
//! Frame-sized work stays on the long-lived worker thread; the adapters here
//! are `Send` and own their buffers.

use std::borrow::Cow;

use snow_stitch_images::{
    Frame as SnowFrame, MotionOutcome, MotionStage, PixelFormat, StitchAxis, StitchBranch,
    StitchDecision, StitchError as SnowError, StitchOptions, Stitcher as SnowStitcher,
};

use crate::image_budget::{BUDGET, MAX_SIDE};
use crate::longcapture::{Axis, ScrollReport, StitchError, StitchStats};

/// GRAPHICS_ROUTINE: BGRA (windows capture order) → RGBA (snow order).
/// Swaps R and B in place over a tight 4-byte-per-pixel buffer.
fn bgra_to_rgba_in_place(px: &mut [u8]) {
    let (quads, rest) = px.as_chunks_mut::<4>();
    debug_assert!(rest.is_empty(), "BGRA buffers are always 4-byte aligned");
    for quad in quads {
        quad.swap(0, 2);
    }
}

/// Copies a BGRA buffer into a fresh RGBA buffer.
fn bgra_to_rgba_owned(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = bgra.to_vec();
    bgra_to_rgba_in_place(&mut rgba);
    rgba
}

/// Copies an RGBA buffer into a fresh BGRA buffer (export path).
fn rgba_to_bgra_owned(rgba: &[u8]) -> Vec<u8> {
    let mut bgra = rgba.to_vec();
    bgra_to_rgba_in_place(&mut bgra); // swap is symmetric
    bgra
}

/// Maps a snow stitching error into FastPin's error type so the worker's
/// `stop_reason` / `is_transient` logic keeps compiling and behaves sensibly.
fn map_snow_error(err: SnowError) -> StitchError {
    match err {
        SnowError::InvalidFirstGeometry { width, height } => StitchError::InvalidFrame {
            width: width as usize,
            height: height as usize,
            rows: height as usize,
        },
        SnowError::ViewportMismatch {
            expected, actual, ..
        } => StitchError::WidthMismatch {
            expected: expected.width as usize,
            got: actual.width as usize,
        },
        SnowError::Arithmetic { .. }
        | SnowError::CheckedCrop { .. }
        | SnowError::EmptyInput
        | SnowError::InvalidOptions { .. }
        | SnowError::InvalidFrame { .. }
        | SnowError::Decode { .. }
        | SnowError::Encode { .. }
        | SnowError::DecisionSerialization(_)
        | SnowError::Io { .. } => StitchError::InvalidFrame {
            width: 0,
            height: 0,
            rows: 0,
        },
    }
}

/// Hard bound on the live-preview thumbnail's height. Both preview paths
/// shrink the thumbnail to stay under it, and `preview_patch` treats a capped
/// preview as "reset": once the scale no longer matches the accumulated bands,
/// only a complete render is correct.
const MAX_PREVIEW_HEIGHT: u32 = 4096;

/// Result of pushing one frame through the ORB stitcher.
#[derive(Clone, Debug)]
pub struct OrbFrameResult {
    /// The scroll information, in a shape the worker's verdict mapper already
    /// understands (`direction = Some((is_up, offset, new_rows))` on progress).
    pub report: ScrollReport,
    /// Whether snow accepted a motion on this tick (a distinct concept from
    /// "the frame changed": a low-confidence or indeterminate estimate does
    /// not move the canvas).
    pub accepted: bool,
    /// Rows added to the canvas by this frame (0 when nothing was committed).
    pub growth: usize,
    /// Branch snow chose for this frame.
    pub branch: StitchBranch,
}

/// Thin wrapper over the vendored snow stitcher.
///
/// Always stitches in snow's `Vertical` axis: a horizontal FastPin session is
/// rotated 90° on the way in and back on the way out (the rotation path FastPin
/// already used for SAD is retained, because it is well-tested).
pub struct OrbStitcher {
    inner: SnowStitcher,
    axis: Axis,
    /// FastPin's hard height cap (snow has none).
    max_height: usize,
    stats: StitchStats,
    /// Frame geometry snow locked onto on the first push.
    expected_width: usize,
    expected_height: usize,
    /// Geometry of the last live-preview band handed to the UI. `None` until
    /// the first patch, which is always a full reset.
    preview_last: Option<PreviewLast>,
    /// Branch snow chose on the most recent push, so `preview_patch` knows
    /// whether the new canvas rows landed at the bottom (Append) or the top
    /// (Prepend) and can place its band accordingly.
    last_branch: StitchBranch,
    /// Canvas rows the most recent push rewrote at the growing edge, splice
    /// overlap included (snow's `canvas_band_height`). `0` before the first
    /// committed motion.
    ///
    /// Snow does not simply append the new rows: it truncates the overlap and
    /// re-copies a whole band from the newest frame, so the canvas content
    /// inside the overlap genuinely changes. A preview that only appends the
    /// new rows would keep showing the stale pixels there and drift out of
    /// registration as the overlapping bands accumulate (upstream absorbs the
    /// same error by redrawing the band; see `previewLayout` in
    /// `screenshotscrollingpipeline.cpp`).
    last_canvas_band: u32,
    /// Rows of that band that are genuinely new (`transition.growth`); the
    /// difference is the splice overlap the band also rewrote.
    last_growth: u32,
}

/// Geometry of the last emitted live-preview band, in preview pixels.
#[derive(Clone, Copy, Debug)]
struct PreviewLast {
    /// Canvas height (snow space) when the band was emitted.
    snow_h: u32,
    /// Total preview height at that moment.
    ph: u32,
    /// Preview width at that moment (constant for a vertical session).
    pw: u32,
    /// Branch of the growth that produced that band.
    branch: StitchBranch,
}

/// One incremental band of the live preview.
///
/// The UI keeps an accumulated BGRA buffer and blits each band into it using
/// these fields, so the worker never has to re-render (or re-send) the whole
/// growing thumbnail.
#[derive(Clone, Debug)]
pub struct PreviewPatch {
    /// Tight BGRA (stride = `width * 4`), `width × height` pixels.
    pub pixels: Vec<u8>,
    /// Preview width in screen-space pixels; constant across a session's bands.
    pub width: u32,
    /// Height of this band in preview rows.
    pub height: u32,
    /// Preview row at which this band starts (`width`-wide rows).
    pub top_offset: u32,
    /// Total preview height after this band is applied.
    pub full_height: u32,
    /// Rows the UI must insert at the top of its buffer before blitting
    /// (non-zero only for a prepend). The band itself starts at row 0.
    pub prepend_shift: u32,
    /// When true the UI must discard its whole accumulated buffer first: this
    /// band is a complete render at a (possibly new) scale.
    pub reset: bool,
}
impl OrbStitcher {
    /// Creates a stitcher in the given screen-space axis.
    ///
    /// `width`/`height` are the first frame's dimensions in screen space; for
    /// a horizontal session they are the pre-rotation dimensions. The snow
    /// canvas is created lazily on the first [`push`](Self::push) call.
    pub fn new(
        axis: Axis,
        width: usize,
        height: usize,
        max_height: usize,
    ) -> Result<Self, StitchError> {
        // Validate the first frame through the same geometry budget the old
        // FastPin stitcher used, so a refusal is reported before capture.
        if width <= 4 || height <= 4 {
            return Err(StitchError::InvalidFrame {
                width,
                height,
                rows: height,
            });
        }
        let (sw, sh) = match axis {
            Axis::Vertical => (width, height),
            Axis::Horizontal => (height, width), // rotated 90°
        };
        BUDGET
            .checked_bytes(sw as u64, sh as u64, 4)
            .map_err(StitchError::Budget)?;
        let side_cap = MAX_SIDE as usize;
        if sh > max_height.min(side_cap) {
            return Err(StitchError::HeightCap {
                total: sh,
                extra: sh,
                max: max_height.min(side_cap),
            });
        }
        let options = StitchOptions {
            axis: StitchAxis::Vertical,
            // FastPin 需要最新一条诊断（场景切换 / 验证拒绝等）。orb 侧每帧读完
            // 就调用 clear_decisions()，所以这里必须保持开启，否则 snow 不会
            // 产生诊断，首帧/分支判定也会失去数据来源。
            record_decisions: true,
            ..StitchOptions::default()
        };
        let inner = SnowStitcher::new(options).map_err(map_snow_error)?;
        Ok(OrbStitcher {
            inner,
            axis,
            max_height: max_height.min(side_cap),
            stats: StitchStats::default(),
            expected_width: sw,
            expected_height: sh,
            preview_last: None,
            last_branch: StitchBranch::NoMovement,
            last_canvas_band: 0,
            last_growth: 0,
        })
    }

    /// Number of frames pushed so far (including the first).
    pub fn input_count(&self) -> usize {
        self.inner.input_count()
    }

    /// Current canvas dimensions in screen space.
    pub fn dimensions(&self) -> Option<(usize, usize)> {
        let (w, h) = self.inner.image_dimensions()?;
        Some(match self.axis {
            Axis::Vertical => (w as usize, h as usize),
            Axis::Horizontal => (h as usize, w as usize),
        })
    }

    /// Pushes one BGRA frame (screen space). Returns `None` for the first frame,
    /// which only initializes the snow stitcher, and `Some` thereafter.
    pub fn push(
        &mut self,
        frame_bgra: &[u8],
        frame_width: usize,
        frame_height: usize,
    ) -> Result<Option<OrbFrameResult>, StitchError> {
        let t = std::time::Instant::now();
        // Normalize the axis to snow's vertical stitching space. For a
        // horizontal session this rotates 90°; the returned dimensions are the
        // rotated ones.
        let (norm, nw, nh): (Cow<'_, [u8]>, usize, usize) = match self.axis {
            Axis::Vertical => (Cow::Borrowed(frame_bgra), frame_width, frame_height),
            Axis::Horizontal => {
                let (px, w, h) =
                    crate::longcapture::rotate_bgra90(frame_bgra, frame_width, frame_height)
                        .map_err(StitchError::Budget)?;
                (Cow::Owned(px), w, h)
            }
        };
        self.stats.copied_bytes = self.stats.copied_bytes.saturating_add((nw * nh * 4) as u64);

        if nw != self.expected_width || nh != self.expected_height {
            // A transient WGC geometry glitch: transient, session stays alive.
            return Err(StitchError::WidthMismatch {
                expected: self.expected_width,
                got: nw,
            });
        }
        if nh > self.max_height {
            return Err(StitchError::HeightCap {
                total: self.canvas_extent(),
                extra: nh,
                max: self.max_height,
            });
        }
        BUDGET
            .checked_bytes(nw as u64, nh as u64, 4)
            .map_err(StitchError::Budget)?;

        let rgba = bgra_to_rgba_owned(&norm);
        let snow_frame = SnowFrame::new(nw as u32, nh as u32, PixelFormat::Rgba8, rgba)
            .map_err(map_snow_error)?;

        let decision = self.inner.push(snow_frame).map_err(map_snow_error)?;
        self.stats.allocations = self.stats.allocations.saturating_add(1);
        let _ = t; // elapsed time is reported by the worker's Spans, not here.

        let Some(decision) = decision else {
            // First frame: snow initialized the canvas; report a neutral result
            // so the worker can treat it as a progress baseline.
            self.last_branch = StitchBranch::NoMovement;
            self.last_canvas_band = 0;
            self.last_growth = 0;
            // 首帧 snow 没有产生诊断，clear 是空操作；放在这里只是让“读完即清”
            // 的契约覆盖所有分支。
            self.inner.clear_decisions();
            return Ok(Some(OrbFrameResult {
                report: ScrollReport::default(),
                accepted: false,
                growth: 0,
                branch: StitchBranch::NoMovement,
            }));
        };
        self.last_branch = decision.branch;
        // `canvas_band_height` is the whole edge band snow rewrote (splice
        // overlap included); `growth` is only the genuinely new part of it.
        // Both are needed to redraw the preview edge the way upstream does.
        self.last_canvas_band = decision.canvas_band_height.unwrap_or(0);
        self.last_growth = decision.growth;
        // `decision` 是 snow `push` 返回的 owned 值，这里按引用映射，不额外克隆。
        let result = map_decision(&decision);
        // 生产路径只消费最新一条诊断：读完立即清空，避免长截图数千帧时
        // snow 的 decisions 数组单调增长占用内存。snow 的 `push` 用
        // `decisions.len()` 定位本帧新增的那条；清空后下帧从索引 0 取，
        // 仍然只拿到唯一的新诊断，首帧判定走上面的 `None` 分支，不受影响。
        self.inner.clear_decisions();
        Ok(Some(result))
    }

    /// Pixels along the stitch axis: snow's canvas height, independent of the
    /// screen-space axis.
    ///
    /// Snow always stitches vertically, so the growing dimension is always its
    /// canvas height: the viewport height for a vertical session, the stitched
    /// long side `L` for a horizontal one (exported width after the 270°
    /// rotation). This is the value the UI must use for the trim extent, the
    /// status "共 N px" and the auto-scroll growth detector. `dimensions()`
    /// swaps the pair for a horizontal session, so its second component is the
    /// *constant* screen height there and must never be used as the extent.
    pub fn canvas_extent(&self) -> usize {
        self.inner
            .image_dimensions()
            .map(|(_, h)| h as usize)
            .unwrap_or(0)
    }

    /// Current stitched extent along the stitch axis.
    ///
    /// This is the source of the worker's `frame.total_height`: a vertical
    /// session's canvas height, or a horizontal session's canvas width after
    /// rotation. Callers that want screen-space dimensions (constant height for
    /// a horizontal session) should use [`Self::dimensions`] instead.
    pub fn canvas_height(&self) -> usize {
        self.canvas_extent()
    }

    /// Materializes the full stitched image in screen-space BGRA.
    pub fn finish(self) -> Result<(Vec<u8>, u32, u32), StitchError> {
        let result = self.inner.finish().map_err(map_snow_error)?;
        let frame = result.image;
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let mut bgra = rgba_to_bgra_owned(frame.pixels());
        if self.axis == Axis::Horizontal {
            let (px, rw, rh) =
                crate::longcapture::rotate_bgra270(&bgra, w, h).map_err(StitchError::Budget)?;
            bgra = px;
            return Ok((bgra, rw as u32, rh as u32));
        }
        BUDGET
            .checked_bytes(w as u64, h as u64, 4)
            .map_err(StitchError::Budget)?;
        Ok((bgra, w as u32, h as u32))
    }

    /// Copies rows from the canvas into a BGRA buffer (screen space).
    pub fn copy_rows(
        &self,
        top: usize,
        rows: usize,
        destination: &mut [u8],
    ) -> Result<(), StitchError> {
        // Snow's copy_rows is vertical-axis only; for horizontal sessions the
        // caller uses finish() instead.
        if self.axis != Axis::Vertical {
            return Err(StitchError::InvalidFrame {
                width: 0,
                height: 0,
                rows: 0,
            });
        }
        self.inner
            .copy_rows(top as u32, rows as u32, destination)
            .map_err(map_snow_error)
    }

    /// Screen-space preview geometry for a given target width: the downsample
    /// scale, the preview width, and the preview height that corresponds to the
    /// whole current canvas. Never magnifies, and keeps the thumbnail within
    /// [`MAX_PREVIEW_HEIGHT`] by shrinking both sides proportionally.
    ///
    /// For a vertical session the canvas grows along the preview's height, so
    /// `ph` grows with it; for a horizontal session the canvas grows along the
    /// preview's width, so `pw` grows and `ph` stays at the viewport height.
    fn preview_geometry(&self, target_width: u32) -> Option<(f32, u32, u32, bool)> {
        // Snow always stitches along its own vertical axis; `image_dimensions`
        // is in that stitching space (its height is the growing dimension).
        let (snow_w, snow_h) = self.inner.image_dimensions()?;
        if snow_w == 0 || snow_h == 0 {
            return None;
        }
        // Screen-space dimensions drive the aspect ratio the user sees.
        let (screen_w, screen_h) = self.dimensions()?;
        let screen_w = screen_w.max(1) as u32;
        let screen_h = screen_h.max(1) as u32;
        // Never magnify: cap the target width at the full image width.
        let scale = (target_width.max(1).min(screen_w) as f32 / screen_w as f32).min(1.0);
        let mut pw = ((screen_w as f32 * scale).round() as u32).max(1);
        let mut ph = ((screen_h as f32 * scale).round() as u32).max(1);
        // Bound the thumbnail itself even for a max-height capture. Shrinking
        // both sides keeps the aspect ratio; `capped` tells the caller that the
        // scale changed, so an incremental band can no longer be appended.
        let mut capped = false;
        if ph > MAX_PREVIEW_HEIGHT {
            let k = MAX_PREVIEW_HEIGHT as f32 / ph as f32;
            pw = ((pw as f32 * k).round() as u32).max(1);
            ph = MAX_PREVIEW_HEIGHT;
            capped = true;
        }
        Some((scale, pw, ph, capped))
    }

    /// Renders the snow canvas span `[start, start + span)` into a BGRA raster
    /// `rw × rh` (snow space) and rotates it into screen space for a horizontal
    /// session. `start`/`span` are **snow canvas pixel rows**, not preview
    /// pixels: snow's `render_scaled_axis` snapshots that exact span and maps
    /// `rh` output rows onto it.
    fn render_range(
        &self,
        start: u32,
        span: u32,
        rw: u32,
        rh: u32,
    ) -> Result<(Vec<u8>, u32, u32), StitchError> {
        let frame = self
            .inner
            .render_scaled_axis(start, span, rw, rh)
            .map_err(map_snow_error)?;
        let bgra = rgba_to_bgra_owned(frame.pixels());
        if self.axis == Axis::Horizontal {
            let (px, w, h) = crate::longcapture::rotate_bgra270(
                &bgra,
                frame.width() as usize,
                frame.height() as usize,
            )
            .map_err(StitchError::Budget)?;
            return Ok((px, w as u32, h as u32));
        }
        Ok((bgra, rw, rh))
    }

    /// Renders a small BGRA thumbnail of the whole current canvas without
    /// consuming the stitcher, for the live preview while scrolling.
    ///
    /// The thumbnail preserves the canvas aspect ratio: `target_width` is an
    /// upper bound on the width (it is clamped to the canvas width, so a narrow
    /// capture is never magnified), and the height follows proportionally.
    /// Snow's `render_scaled_axis` downsamples straight from the tiled canvas,
    /// so this never materializes the full (possibly 16384px tall) image.
    /// `None` is returned before the first frame established a canvas.
    pub fn preview_snapshot(
        &self,
        target_width: u32,
    ) -> Result<Option<(Vec<u8>, u32, u32)>, StitchError> {
        let Some((_, pw, ph, _capped)) = self.preview_geometry(target_width) else {
            return Ok(None);
        };
        let Some((_, snow_h)) = self.inner.image_dimensions() else {
            return Ok(None);
        };
        BUDGET
            .checked_bytes(pw as u64, ph as u64, 4)
            .map_err(StitchError::Budget)?;
        // The target raster dimensions are in snow space; for a horizontal
        // session they are swapped so a 270° rotation yields a screen-space
        // preview of (pw × ph).
        let (rw, rh) = match self.axis {
            Axis::Vertical => (pw, ph),
            Axis::Horizontal => (ph, pw),
        };
        self.render_range(0, snow_h, rw, rh).map(Some)
    }

    /// Renders one slice of the canvas at (near) native resolution, for the
    /// hover preview: `top`/`bottom` are **screen-space canvas rows along the
    /// stitch axis**, which for a horizontal session are columns of the final
    /// image. `target_width` bounds the cross axis of the result, so a tall
    /// capture does not hand the bar a full-width raster it would only shrink.
    ///
    /// Returns screen-space BGRA plus its size, or `None` when the range is
    /// empty or the canvas does not exist yet. Unlike [`Self::preview_patch`]
    /// this reads no preview state: it is a pure query, so a hover can render
    /// without disturbing the incremental accumulator.
    pub fn render_region(
        &self,
        top: u32,
        bottom: u32,
        target_width: u32,
    ) -> Result<Option<(Vec<u8>, u32, u32)>, StitchError> {
        let Some((snow_w, snow_h)) = self.inner.image_dimensions() else {
            return Ok(None);
        };
        if snow_w == 0 || snow_h == 0 || bottom <= top {
            return Ok(None);
        }
        // Clamp to the canvas instead of failing: the pointer can sit on the
        // last row while the canvas grows underneath it.
        let top = top.min(snow_h - 1);
        let bottom = bottom.clamp(top + 1, snow_h.max(top + 1));
        let span = bottom - top;
        // The cross axis is the snow canvas width for both session axes: a
        // horizontal capture is rotated into snow's vertical stitching space,
        // so its screen height became the snow width.
        let cross = snow_w.max(1);
        let scale = (target_width.max(1).min(cross) as f32 / cross as f32).min(1.0);
        let rw = ((cross as f32 * scale).round() as u32).max(1);
        let rh = ((span as f32 * scale).round() as u32).max(1);
        BUDGET
            .checked_bytes(rw as u64, rh as u64, 4)
            .map_err(StitchError::Budget)?;
        self.render_range(top, span, rw, rh).map(Some)
    }

    /// Returns the next incremental band of the live preview, or `None` when
    /// there is nothing new to show.
    ///
    /// The UI accumulates bands into one BGRA buffer (see [`PreviewPatch`]):
    /// a vertical session appends rows at the bottom (or inserts them at the
    /// top for an upward scroll), and only the new canvas rows are rendered
    /// here -- the whole thumbnail is never re-sampled. A `reset` band is a
    /// complete render and must replace the accumulator; it is used for the
    /// first frame, when the preview scale changes (canvas cap), when the
    /// scroll direction flips, and for every horizontal band (a rotated band
    /// would need column-wise insertion, so horizontal previews are simply
    /// re-rendered whole).
    pub fn preview_patch(
        &mut self,
        target_width: u32,
    ) -> Result<Option<PreviewPatch>, StitchError> {
        let Some((_, snow_h)) = self.inner.image_dimensions() else {
            return Ok(None);
        };
        let Some((_scale, pw, ph, capped)) = self.preview_geometry(target_width) else {
            return Ok(None);
        };
        BUDGET
            .checked_bytes(pw as u64, ph as u64, 4)
            .map_err(StitchError::Budget)?;

        // A horizontal band rotates into a horizontal strip that the row-based
        // accumulator cannot place; a capped preview changes scale. Both are
        // handled with one correct full render.
        let must_reset = self.axis == Axis::Horizontal
            || capped
            || match &self.preview_last {
                None => true,
                Some(last) => last.pw != pw || last.ph >= ph,
            };

        if must_reset {
            let (rw, rh) = match self.axis {
                Axis::Vertical => (pw, ph),
                Axis::Horizontal => (ph, pw),
            };
            let (pixels, width, height) = self.render_range(0, snow_h, rw, rh)?;
            // A complete render has no placement, so it records no branch: the
            // next band must be treated as incremental even though the canvas
            // has grown into a new direction since.
            self.preview_last = Some(PreviewLast {
                snow_h,
                ph: height,
                pw: width,
                branch: StitchBranch::NoMovement,
            });
            return Ok(Some(PreviewPatch {
                pixels,
                width,
                height,
                top_offset: 0,
                full_height: height,
                prepend_shift: 0,
                reset: true,
            }));
        }

        let last = *self.preview_last.as_ref().unwrap();
        // A direction flip between bands would misplace the older content in
        // the accumulator; fall back to a clean full render. A `NoMovement`
        // marker means the previous band was a complete render, which placed
        // nothing, so there is nothing to misplace.
        if last.branch != StitchBranch::NoMovement && last.branch != self.last_branch {
            let (pixels, width, height) = self.render_range(0, snow_h, pw, ph)?;
            self.preview_last = Some(PreviewLast {
                snow_h,
                ph: height,
                pw: width,
                branch: StitchBranch::NoMovement,
            });
            return Ok(Some(PreviewPatch {
                pixels,
                width,
                height,
                top_offset: 0,
                full_height: height,
                prepend_shift: 0,
                reset: true,
            }));
        }

        let growth = snow_h.saturating_sub(last.snow_h);
        if growth == 0 {
            // Sub-preview-row growth is picked up by a later band instead of
            // producing a zero-height patch.
            return Ok(None);
        }

        // Snow rewrites a whole edge band, not just the new rows: `band` covers
        // the splice overlap too, and the canvas content inside that overlap is
        // replaced by pixels from the newest frame. The preview has to redraw
        // that overlap as well, otherwise the older, slightly mis-registered
        // pixels stay on screen and the thumbnail drifts as bands accumulate.
        // This mirrors `previewLayout` in `screenshotscrollingpipeline.cpp`,
        // which absorbs all preview-scale rounding into this one edge patch.
        let band = self.last_canvas_band.min(snow_h).max(growth);
        let overlap_snow = band - growth;
        let Some((scale, _, _, _)) = self.preview_geometry(target_width) else {
            return Ok(None);
        };
        let replaced = if overlap_snow == 0 {
            0
        } else {
            ((overlap_snow as f32 * scale).round() as u32).clamp(1, last.ph)
        };
        // Rows the thumbnail gains at its growing edge. Everything else in
        // `patch_h` is the overlap being redrawn on top of rows that already
        // exist, so the buffer only ever grows by this much.
        let new_rows = ph.saturating_sub(last.ph);
        let Some(patch_h) = new_rows.checked_add(replaced).filter(|h| *h > 0) else {
            return Ok(None);
        };
        let prepend = self.last_branch == StitchBranch::Prepend;
        // An append rewrites the band at the canvas bottom; a prepend rewrites
        // it at the top, because that is where the canvas grew.
        let start = if prepend {
            0
        } else {
            snow_h.saturating_sub(band)
        };
        let (pixels, width, height) = self.render_range(start, band, pw, patch_h)?;
        self.preview_last = Some(PreviewLast {
            snow_h,
            ph,
            pw,
            branch: self.last_branch,
        });
        Ok(Some(PreviewPatch {
            pixels,
            width,
            height,
            // An append keeps the untouched rows above the band and lays the
            // redrawn band (overlap first) directly below them, so it ends
            // exactly at the new bottom.
            top_offset: if prepend { 0 } else { last.ph - replaced },
            full_height: ph,
            // A prepend shifts every surviving old row down by the rows the
            // canvas grew -- not by the band height, because the band's own
            // top rows land on old rows that are about to be discarded.
            prepend_shift: if prepend { new_rows } else { 0 },
            reset: false,
        }))
    }

    /// Snapshot stats, for the worker's `FrameResult.stats` field.
    pub fn stats(&self) -> StitchStats {
        self.stats
    }
}

/// Maps a snow decision into the FastPin `ScrollReport` shape the verdict
/// mapper already consumes.
fn map_decision(decision: &StitchDecision) -> OrbFrameResult {
    let branch = decision.branch;
    let growth = decision.growth as usize;
    let accepted_offset = decision.accepted_offset;
    let mut report = ScrollReport::default();

    match branch {
        StitchBranch::Append | StitchBranch::Prepend => {
            if growth > 0 {
                let is_up = branch == StitchBranch::Prepend;
                let offset = accepted_offset.unwrap_or(0).unsigned_abs() as usize;
                report.direction = Some((is_up, offset, growth));
                report.overlap_score = decision.confidence.unwrap_or(0.0) as f64;
                return OrbFrameResult {
                    report,
                    accepted: true,
                    growth,
                    branch,
                };
            }
            // Accepted branch with no growth (e.g. a sub-row shift): treat as
            // movement seen but not committed, so the worker keeps full speed.
            report.changed = crate::longcapture::MOTION_CHANGED + 0.01;
        }
        StitchBranch::Contained | StitchBranch::NoMovement | StitchBranch::Skip => {
            // No canvas growth. Populate diagnostics so the worker can tell a
            // moving-but-unalignable frame from a still one.
            if let Some(motion) = decision.motion.as_ref() {
                match motion {
                    MotionOutcome::Motion { .. } => {
                        report.changed = crate::longcapture::MOTION_CHANGED + 0.01;
                    }
                    MotionOutcome::NoMotion => {}
                    MotionOutcome::Indeterminate => {
                        report.changed = crate::longcapture::MOTION_CHANGED + 0.01;
                    }
                }
            }
            if let Some(diag) = decision.motion_diagnostics.as_ref() {
                // SceneCut is a genuine content jump (the estimator's 3-frame
                // streak), distinct from an ordinary low-confidence / no-
                // candidate match failure. Keep it separate so the stall
                // classifier can show "content changed" instead of lumping it
                // into "unalignable" or, worse, "no scrolling detected".
                report.scene_cut = diag.stage == MotionStage::SceneCut;
                report.rejected_by_verification = matches!(
                    diag.stage,
                    MotionStage::LowConfidence | MotionStage::SceneCut | MotionStage::NoCandidates
                );
                report.static_edge = diag.mutual_matches == 0;
            }
        }
    }
    OrbFrameResult {
        report,
        accepted: false,
        growth: 0,
        branch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_rgba_round_trip() {
        let original = vec![1u8, 2, 3, 255, 10, 20, 30, 255];
        let rgba = bgra_to_rgba_owned(&original);
        assert_eq!(rgba, vec![3, 2, 1, 255, 30, 20, 10, 255]);
        assert_eq!(rgba_to_bgra_owned(&rgba), original);
    }

    #[test]
    fn rejects_tiny_first_frame() {
        assert!(OrbStitcher::new(Axis::Vertical, 4, 100, 16384).is_err());
    }

    /// Deterministic textured page window, mirroring snow's own `document()`
    /// test helper. Grayscale replicated into RGBA gives ORB clean corners.
    fn page_window(width: usize, h: usize, scroll: usize) -> Vec<u8> {
        let mut px = vec![0u8; width * h * 4];
        for y in 0..h {
            for x in 0..width {
                let along = y + scroll;
                let mut hash =
                    (x as u32).wrapping_mul(0xc2b2_ae35) ^ (along as u32).wrapping_mul(0x27d4_eb2d);
                hash ^= hash >> 16;
                hash = hash.wrapping_mul(0x7feb_352d);
                hash ^= hash >> 15;
                let value = (hash >> 24) as u8;
                let i = (y * width + x) * 4;
                px[i] = value;
                px[i + 1] = value;
                px[i + 2] = value;
                px[i + 3] = 255;
            }
        }
        px
    }

    /// 横向滚动的页面窗口：内容沿屏幕 x 轴移动，旋转 90° 后变成 snow 的
    /// 垂直位移，因此 ORB 会像纵向滚动一样增长画布。纹理沿 y（交叉轴）也有
    /// 变化，ORB 才能提取到稳定的角点。
    fn horizontal_page_window(width: usize, height: usize, scroll: usize) -> Vec<u8> {
        let mut px = vec![0u8; width * height * 4];
        for x in 0..width {
            for y in 0..height {
                let along = x + scroll;
                let mut hash =
                    (y as u32).wrapping_mul(0xc2b2_ae35) ^ (along as u32).wrapping_mul(0x27d4_eb2d);
                hash ^= hash >> 16;
                hash = hash.wrapping_mul(0x7feb_352d);
                hash ^= hash >> 15;
                let value = (hash >> 24) as u8;
                let i = (y * width + x) * 4;
                px[i] = value;
                px[i + 1] = value;
                px[i + 2] = value;
                px[i + 3] = 255;
            }
        }
        px
    }

    #[test]
    fn accepts_a_clean_vertical_scroll() {
        let (w, h, shift) = (320usize, 256usize, 40usize);
        let mut stitcher = OrbStitcher::new(Axis::Vertical, w, h, 16384).unwrap();
        let first = page_window(w, h, 0);
        assert!(stitcher.push(&first, w, h).unwrap().is_some());
        let second = page_window(w, h, shift);
        let result = stitcher.push(&second, w, h).unwrap().unwrap();
        assert!(
            result.accepted,
            "expected ORB to accept a clean scroll: {:?}",
            result
        );
        assert!(
            !result.report.direction.unwrap().0,
            "down-scroll is is_up=false"
        );
        assert_eq!(result.growth, shift);
    }

    #[test]
    fn accepts_an_upward_scroll_as_prepend() {
        let (w, h, shift) = (320usize, 256usize, 40usize);
        let mut stitcher = OrbStitcher::new(Axis::Vertical, w, h, 16384).unwrap();
        // Start deeper in the page, then scroll up by `shift`.
        assert!(stitcher
            .push(&page_window(w, h, shift), w, h)
            .unwrap()
            .is_some());
        let result = stitcher.push(&page_window(w, h, 0), w, h).unwrap().unwrap();
        assert!(
            result.accepted,
            "expected ORB to accept upward scroll: {:?}",
            result
        );
        let (is_up, _, growth) = result.report.direction.unwrap();
        assert!(is_up, "up-scroll is is_up=true");
        assert_eq!(growth, shift);
    }

    /// Blits a band into an accumulator exactly the way the UI does, so the
    /// tests below exercise the same contract `overlay::apply_preview_band`
    /// implements.
    fn accumulate(buf: &mut Vec<u8>, buf_h: &mut u32, band: &PreviewPatch) {
        let w = band.width as usize;
        let full_h = band.full_height as usize;
        let row_bytes = w * 4;
        if band.reset || *buf_h == 0 {
            buf.clear();
            *buf_h = 0;
        }
        if buf.len() < row_bytes * full_h {
            buf.resize(row_bytes * full_h, 0);
        }
        if band.prepend_shift > 0 {
            let shift = (band.prepend_shift as usize).min(full_h);
            let old_h = (*buf_h as usize).min(full_h.saturating_sub(shift));
            if shift > 0 && old_h > 0 {
                buf.copy_within(0..old_h * row_bytes, shift * row_bytes);
            }
        }
        let band_h = band.height as usize;
        let y0 = (band.top_offset as usize).min(full_h.saturating_sub(band_h));
        let n = band.pixels.len().min(band_h * row_bytes);
        let start = y0 * row_bytes;
        buf[start..start + n].copy_from_slice(&band.pixels[..n]);
        *buf_h = band.full_height;
    }

    /// At a 1:1 preview scale (target width >= capture width) every preview row
    /// is exactly one canvas row, so summing the incremental bands must
    /// reproduce the whole canvas render bit for bit. This is the test the
    /// first implementation lacked: it checked sizes only, so the coordinate
    /// bug (scaling the snow-space span by the preview scale) slipped through.
    #[test]
    fn incremental_append_bands_reassemble_to_the_full_preview() {
        let (w, h, step) = (320usize, 256usize, 40usize);
        // Snow rewrites a band of `viewport/2` rows at the growing edge, so the
        // band is wider than the scroll step: that overlap is exactly what the
        // redrawn band has to cover.
        let band_h = (h / 2) as u32;
        let mut stitcher = OrbStitcher::new(Axis::Vertical, w, h, 16384).unwrap();
        stitcher.push(&page_window(w, h, 0), w, h).unwrap().unwrap();

        let mut acc = Vec::new();
        let mut acc_h = 0u32;
        let first = stitcher.preview_patch(320).unwrap().unwrap();
        assert!(first.reset, "the first band is a complete render");
        assert_eq!(
            (first.width, first.height, first.full_height),
            (320, 256, 256)
        );
        accumulate(&mut acc, &mut acc_h, &first);

        for i in 1..=3usize {
            let start = i * step;
            let pushed = stitcher
                .push(&page_window(w, h, start), w, h)
                .unwrap()
                .unwrap();
            assert!(pushed.accepted && pushed.growth == step);
            let band = stitcher.preview_patch(320).unwrap().unwrap();
            assert!(!band.reset, "band {i} must be incremental");
            assert_eq!(band.width, 320);
            // The band redraws snow's whole edge band (the splice overlap plus
            // the new rows), not just the new rows: only the overlap part is
            // new information, the rest re-covers rows already on screen.
            assert_eq!(band.height, band_h);
            assert_eq!(band.top_offset, (h + i * step) as u32 - band_h);
            assert_eq!(band.prepend_shift, 0);
            assert_eq!(band.full_height, (h + i * step) as u32);
            accumulate(&mut acc, &mut acc_h, &band);
        }

        let (full, fw, fh) = stitcher.preview_snapshot(320).unwrap().unwrap();
        assert_eq!((fw as usize, fh as usize), (320, h + 3 * step));
        assert_eq!(acc.len(), full.len(), "accumulated preview size");
        assert!(
            acc == full,
            "incremental bands must reassemble into the full preview pixel for pixel"
        );
    }

    /// Same contract for an upward scroll: the band is inserted at the top and
    /// the older content must shift down, which is what `prepend_shift` tells
    /// the UI. A band placed with the wrong anchor would duplicate content.
    #[test]
    fn incremental_prepend_bands_reassemble_to_the_full_preview() {
        let (w, h, step) = (320usize, 256usize, 40usize);
        let band_h = (h / 2) as u32;
        let mut stitcher = OrbStitcher::new(Axis::Vertical, w, h, 16384).unwrap();
        // Start deep in the document, then scroll up in three steps.
        stitcher
            .push(&page_window(w, h, 3 * step), w, h)
            .unwrap()
            .unwrap();

        let mut acc = Vec::new();
        let mut acc_h = 0u32;
        let first = stitcher.preview_patch(320).unwrap().unwrap();
        assert!(first.reset);
        assert_eq!((first.width, first.height), (320, 256));
        accumulate(&mut acc, &mut acc_h, &first);

        for i in 1..=3usize {
            let start = 3 * step - i * step;
            let pushed = stitcher
                .push(&page_window(w, h, start), w, h)
                .unwrap()
                .unwrap();
            assert!(pushed.accepted, "step {i} must be accepted");
            assert_eq!(pushed.branch, StitchBranch::Prepend);
            assert_eq!(pushed.growth, step);
            let band = stitcher.preview_patch(320).unwrap().unwrap();
            assert!(!band.reset, "band {i} must be incremental");
            assert_eq!(band.top_offset, 0, "a prepend band starts at the top");
            // The band covers the overlap as well as the new rows, so it is
            // taller than the scroll step; only the new rows push old content
            // down, which is what `prepend_shift` reports.
            assert_eq!(band.height, band_h);
            assert_eq!(band.prepend_shift, step as u32);
            assert_eq!(band.full_height, (h + i * step) as u32);
            accumulate(&mut acc, &mut acc_h, &band);
        }

        let (full, fw, fh) = stitcher.preview_snapshot(320).unwrap().unwrap();
        assert_eq!((fw as usize, fh as usize), (320, h + 3 * step));
        assert_eq!(acc.len(), full.len(), "accumulated preview size");
        assert!(
            acc == full,
            "prepended bands must reassemble into the full preview pixel for pixel"
        );
    }

    /// A downsampled preview cannot be reproduced exactly, but the bands must
    /// still cover the whole thumbnail: every band's rows land inside
    /// `full_height` and the last one reaches the bottom.
    #[test]
    fn downsampled_bands_cover_the_whole_thumbnail() {
        let (w, h, step) = (320usize, 256usize, 40usize);
        let mut stitcher = OrbStitcher::new(Axis::Vertical, w, h, 16384).unwrap();
        stitcher.push(&page_window(w, h, 0), w, h).unwrap().unwrap();

        let mut acc = Vec::new();
        let mut acc_h = 0u32;
        let first = stitcher.preview_patch(160).unwrap().unwrap();
        assert!(first.reset);
        assert_eq!(first.width, 160);
        accumulate(&mut acc, &mut acc_h, &first);

        let mut last = first;
        for i in 1..=6usize {
            let start = i * step;
            stitcher
                .push(&page_window(w, h, start), w, h)
                .unwrap()
                .unwrap();
            let band = stitcher.preview_patch(160).unwrap().unwrap();
            // Because 160/320 = 0.5, 40 canvas rows map to exactly 20 preview rows.
            assert_eq!(band.width, 160);
            assert_eq!(band.top_offset + band.height, band.full_height);
            accumulate(&mut acc, &mut acc_h, &band);
            last = band;
        }
        assert_eq!(last.full_height, ((h + 6 * step) / 2) as u32);
        assert_eq!(acc_h, last.full_height);
        assert_eq!(acc.len(), 160 * 4 * last.full_height as usize);
    }

    /// A horizontal session emits a complete, correctly oriented band every
    /// time: a rotated vertical band cannot be placed by the row-based
    /// accumulator, so `preview_patch` must say `reset` instead of pretending.
    #[test]
    fn horizontal_bands_are_always_complete_renders() {
        let (w, h, step) = (320usize, 256usize, 40usize);
        // A horizontal session rotates every frame 90° before stitching, so the
        // stitcher expects the pre-rotation geometry: `new` swaps the pair.
        let mut stitcher = OrbStitcher::new(Axis::Horizontal, w, h, 16384).unwrap();
        stitcher.push(&page_window(w, h, 0), w, h).unwrap().unwrap();
        let first = stitcher.preview_patch(320).unwrap().unwrap();
        assert!(first.reset);
        assert_eq!((first.width, first.height), (320, 256));
        stitcher
            .push(&page_window(w, h, step), w, h)
            .unwrap()
            .unwrap();
        let band = stitcher.preview_patch(320).unwrap().unwrap();
        assert!(band.reset, "horizontal previews are re-rendered whole");
        assert_eq!(band.top_offset, 0);
        assert_eq!(band.prepend_shift, 0);
        assert_eq!(
            band.pixels.len(),
            band.width as usize * band.height as usize * 4
        );
        // The orientation is screen-space: the preview stays as wide as the
        // captured region and its height follows the viewport.
        assert_eq!(band.width, 320);
    }

    /// P0 回归：横向会话的增长维度是 snow 画布高度 L（旋转后的屏幕宽），
    /// 不是 `dimensions()` 第二个分量那个常量屏幕高。trim、状态栏、自动滚动
    /// 和 `finish()` 的宽度都必须跟着它走，否则导出会被截断到首屏宽。
    #[test]
    fn horizontal_extent_grows_and_finish_uses_the_grown_width() {
        let (w, h, step) = (320usize, 256usize, 40usize);
        let mut stitcher = OrbStitcher::new(Axis::Horizontal, w, h, 16384).unwrap();
        stitcher
            .push(&horizontal_page_window(w, h, 0), w, h)
            .unwrap()
            .unwrap();
        assert_eq!(
            stitcher.canvas_extent(),
            w,
            "首帧的增长维度是选区宽（旋转后成为 snow 画布高）"
        );
        assert_eq!(
            stitcher.canvas_height(),
            w,
            "canvas_height 是 worker 的 total_height 来源，必须等于增长维度"
        );

        let mut last = stitcher.canvas_extent();
        for i in 1..=3usize {
            let pushed = stitcher
                .push(&horizontal_page_window(w, h, i * step), w, h)
                .unwrap()
                .unwrap();
            assert!(pushed.accepted, "第 {i} 步必须被接受：{pushed:?}");
            let extent = stitcher.canvas_extent();
            assert!(extent > last, "第 {i} 步必须增长画布：{last} → {extent}");
            assert_eq!(
                stitcher.canvas_height(),
                extent,
                "canvas_height 必须始终跟随增长维度"
            );
            last = extent;
        }
        assert!(last >= w + 3 * step, "三步滚动应累计至少 3×step");
        assert_eq!(
            stitcher.dimensions().unwrap().1,
            h,
            "屏幕高是常量，不能拿它当增长维度"
        );

        let (_, fw, fh) = stitcher.finish().unwrap();
        assert_eq!(
            fw as usize, last,
            "finish 的宽必须是增长后的长边 L，而不是首屏宽"
        );
        assert_eq!(fh as usize, h);
    }

    /// The hover preview asks for a slice of canvas rows and must get exactly
    /// those rows: the band it renders has to match the same rows cut out of a
    /// full-canvas render, not the top of the page.
    #[test]
    fn a_region_render_returns_exactly_the_requested_rows() {
        let (w, h, step) = (320usize, 256usize, 40usize);
        let mut stitcher = OrbStitcher::new(Axis::Vertical, w, h, 16384).unwrap();
        stitcher.push(&page_window(w, h, 0), w, h).unwrap().unwrap();
        for i in 1..=3usize {
            stitcher
                .push(&page_window(w, h, i * step), w, h)
                .unwrap()
                .unwrap();
        }
        let canvas = stitcher.canvas_height() as u32;
        assert_eq!(canvas, (h + 3 * step) as u32);

        // 1:1 render of rows [80, 160) — midway, so a "top of the page" bug
        // cannot pass by accident.
        let (slice, sw, sh) = stitcher.render_region(80, 160, 320).unwrap().unwrap();
        assert_eq!((sw, sh), (320, 80));
        assert_eq!(slice.len(), 320 * 80 * 4);

        let (full, fw, fh) = stitcher.preview_snapshot(320).unwrap().unwrap();
        assert_eq!((fw, fh), (320, canvas));
        let row_bytes = 320 * 4;
        let expected = &full[80 * row_bytes..160 * row_bytes];
        assert!(
            slice == expected,
            "a region render must reproduce the canvas rows it was asked for"
        );
    }

    /// A degenerate or empty range yields nothing instead of an error or a
    /// zero-sized raster the UI would have to special-case.
    #[test]
    fn a_region_render_rejects_an_empty_range() {
        let (w, h) = (320usize, 256usize);
        let mut stitcher = OrbStitcher::new(Axis::Vertical, w, h, 16384).unwrap();
        stitcher.push(&page_window(w, h, 0), w, h).unwrap().unwrap();
        assert!(stitcher.render_region(40, 40, 320).unwrap().is_none());
        assert!(stitcher.render_region(80, 40, 320).unwrap().is_none());
    }

    /// A range past the canvas is clamped, not refused: the pointer can sit on
    /// the last row while the canvas is still growing.
    #[test]
    fn a_region_past_the_canvas_is_clamped() {
        let (w, h) = (320usize, 256usize);
        let mut stitcher = OrbStitcher::new(Axis::Vertical, w, h, 16384).unwrap();
        stitcher.push(&page_window(w, h, 0), w, h).unwrap().unwrap();
        let (_, sw, sh) = stitcher.render_region(200, 9999, 320).unwrap().unwrap();
        assert_eq!(sw, 320);
        assert_eq!(sh, 56, "clamped to the 56 rows that exist");
    }
}
