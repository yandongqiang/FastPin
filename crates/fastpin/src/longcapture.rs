//! Long-screenshot stitching, ported from the C# `FastPin.Core.LongCapture`
//! (GrayscaleConvert / VerticalFrameAligner / LongImageBuffer).
//!
//! The C# runner sampled a fixed screen region every 450 ms while the user
//! scrolled by hand; here we keep only the pure math: grayscale conversion,
//! SAD-based vertical alignment, and the growing BGRA buffer. No GPU or UI
//! lives in this module so every path is unit-testable off-screen.
//!
//! Two refinements over the C# original, both needed on real 2000x1000
//! selections: the SAD search is coarse-to-fine (a 4x-downsampled sweep, then
//! a full-resolution refinement around the winner), and horizontal sessions
//! rotate every frame 90 degrees so one vertical matcher serves both axes.

use std::borrow::Cow;

/// BGRA → 8-bit luma with the same integer weights as the C# build
/// (`29/150/77` on B/G/R, one rounding-free shift). Used for alignment only —
/// the stitched output stays in BGRA.
pub fn bgra_to_gray(bgra: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut gray = vec![0u8; width * height];
    for (g, px) in gray.iter_mut().zip(bgra.as_chunks::<4>().0) {
        *g = ((px[0] as u32 * 29 + px[1] as u32 * 150 + px[2] as u32 * 77) >> 8) as u8;
    }
    gray
}

/// Outcome of aligning one frame against the previous. `offset_y` counts the
/// overlapping rows to skip at the top of `current`; the new content is
/// `[offset_y..height)`.
pub struct FrameAlignResult {
    pub offset_y: usize,
    pub score: f64,
    pub accepted: bool,
    /// `runner_up / best_sad`: how much worse the best rival offset is, i.e.
    /// the uniqueness of the match. `INFINITY` when there is no rival at all,
    /// `0.0` when both are zero (blank content).
    pub runner_up_ratio: f64,
}

impl FrameAlignResult {
    fn rejected() -> Self {
        FrameAlignResult {
            offset_y: 0,
            score: 0.0,
            accepted: false,
            runner_up_ratio: 0.0,
        }
    }
}

/// A full-quality match clears this on its own; between `MEDIUM_SCORE` and
/// this the rival margin has to be wider (see [`accepts`]).
const STRONG_SCORE: f64 = 0.90;
/// Real screens are antialiased, low-contrast and occasionally move a
/// sub-pixel, so a genuine scroll can land below the old hard 0.9. Two
/// *unrelated* frames score ~0.67 (E|a-b| = 85/255), so this floor still
/// leaves a wide gap between "noisy scroll" and "different content".
const MEDIUM_SCORE: f64 = 0.78;
/// Rival margin required at `STRONG_SCORE`: the runner-up SAD must be at
/// least twice the winner's (the original guard).
const STRONG_RATIO: f64 = 2.0;
/// Rival margin required in the medium band: a looser score bar has to buy
/// its acceptance with a *less* ambiguous match.
const MEDIUM_RATIO: f64 = 2.5;
/// Downsampling factor of the coarse alignment pass.
const COARSE_STEP: usize = 4;
/// Below this size a coarse pass costs more than it saves; the full sweep is
/// both cheap and exact.
const COARSE_MIN_DIM: usize = 64;

/// Adaptive acceptance: a high score needs only the original 2x rival margin;
/// a medium score must be even less ambiguous. `accept_score` is a floor the
/// caller may raise, never lower -- tests and hosts cannot accidentally
/// re-enable the 0.35 behaviour that accepted unrelated frames.
fn accepts(score: f64, runner_up_ratio: f64, accept_score: f64) -> bool {
    let medium = MEDIUM_SCORE.max(accept_score);
    if score >= medium.max(STRONG_SCORE) {
        runner_up_ratio > STRONG_RATIO
    } else {
        score >= medium && runner_up_ratio > MEDIUM_RATIO
    }
}

fn ratio(best_sad: u64, runner_up: u64) -> f64 {
    if best_sad == 0 {
        if runner_up == 0 {
            0.0
        } else {
            f64::INFINITY
        }
    } else {
        runner_up as f64 / best_sad as f64
    }
}

/// Finds the previous frame's bottom strip inside `current` by SAD over the
/// top `height - strip` candidate rows. Matches the C# `VerticalFrameAligner`
/// exactly, including the clamp order: a frame shorter than the strip falls
/// through `search_max <= 0` rather than shrinking the strip further.
/// Downward scroll only: content moves up, fresh rows enter at the bottom.
pub fn align_vertical(
    previous: &[u8],
    current: &[u8],
    width: usize,
    height: usize,
    strip_height: usize,
    accept_score: f64,
) -> FrameAlignResult {
    align_impl(
        previous,
        current,
        width,
        height,
        strip_height,
        accept_score,
        false,
    )
}

/// Upward scroll (content moves down, fresh rows enter at the TOP). Finds the
/// previous frame's TOP strip inside `current` by the same SAD/acceptance rules
/// as `align_vertical`, but `offset_y` means "number of fresh rows at the top
/// of `current`" (rows `[0..offset_y)`) rather than "start of new rows at the
/// bottom". The caller prepends those rows. Keeping one shared core means the
/// early-exit SAD, the ambiguity guard and the score are identical in both
/// directions.
pub fn align_vertical_up(
    previous: &[u8],
    current: &[u8],
    width: usize,
    height: usize,
    strip_height: usize,
    accept_score: f64,
) -> FrameAlignResult {
    align_impl(
        previous,
        current,
        width,
        height,
        strip_height,
        accept_score,
        true,
    )
}

/// `up = false`: match previous BOTTOM strip, `offset_y` = start of new rows
/// at the bottom. `up = true`: match previous TOP strip, `offset_y` = count of
/// fresh rows at the top. Everything else is shared so the two directions
/// cannot drift.
fn align_impl(
    previous: &[u8],
    current: &[u8],
    width: usize,
    height: usize,
    strip_height: usize,
    accept_score: f64,
    up: bool,
) -> FrameAlignResult {
    if width == 0
        || height == 0
        || previous.len() < width * height
        || current.len() < width * height
    {
        return FrameAlignResult::rejected();
    }

    let strip = strip_height.clamp(4, (height / 3).max(4));
    if height <= strip {
        return FrameAlignResult::rejected();
    }
    let search_max = height - strip;

    // Down: previous bottom strip. Up: previous top strip.
    let prev_start = if up { 0 } else { (height - strip) * width };

    // A 2000x1000 region costs ~1.2e8 SAD terms per direction at full
    // resolution, on the UI thread. The coarse pass runs the same SAD on a
    // 4x-downsampled pair (1/16 the work per candidate, 1/4 the candidates),
    // then the full-resolution search only looks +/-2 coarse steps around the
    // winner, which is where the exact row must be. Small frames keep the
    // exact full sweep so short selections stay byte-identical to before.
    let (best_y, best_sad, runner_up) =
        if width >= COARSE_MIN_DIM && height >= COARSE_MIN_DIM && search_max >= COARSE_STEP * 4 {
            coarse_to_fine(
                previous, current, width, height, strip, prev_start, search_max,
            )
        } else {
            full_sweep(previous, current, width, strip, prev_start, search_max)
        };

    let score = 1.0 - best_sad as f64 / (strip * width * 255) as f64;
    let runner_up_ratio = ratio(best_sad, runner_up);
    let accepted = accepts(score, runner_up_ratio, accept_score);
    // Down: best_y is where the previous bottom strip starts in `current`; skip
    // the whole overlap, so new rows begin at best_y + strip (append those).
    // Up: best_y is where the previous top strip starts in `current`; the rows
    // ABOVE it (current[0..best_y)) are fresh (prepend those).
    let offset_y = if up {
        best_y
    } else {
        height.min(best_y + strip)
    };
    FrameAlignResult {
        offset_y,
        score,
        accepted,
        runner_up_ratio,
    }
}

/// SAD of the previous strip against `current` starting at row `y`.
fn sad_at(
    previous: &[u8],
    current: &[u8],
    width: usize,
    strip: usize,
    prev_start: usize,
    y: usize,
) -> u64 {
    let mut sad = 0u64;
    for row in 0..strip {
        let p = prev_start + row * width;
        let c = (y + row) * width;
        for i in 0..width {
            sad += previous[p + i].abs_diff(current[c + i]) as u64;
        }
    }
    sad
}

/// Exact full-resolution sweep with the row-wise early exit: a candidate whose
/// partial SAD already exceeds twice the best completed SAD cannot become the
/// new best (its full SAD can only be larger), so it is abandoned rather than
/// summed to the end. The true best is never abandoned this way, since its
/// partial sum can never exceed its own final SAD, which is <= best_so_far.
fn full_sweep(
    previous: &[u8],
    current: &[u8],
    width: usize,
    strip: usize,
    prev_start: usize,
    search_max: usize,
) -> (usize, u64, u64) {
    let mut sads = Vec::with_capacity(search_max + 1);
    let mut best_so_far = u64::MAX;
    for y in 0..=search_max {
        let curr_start = y * width;
        let limit = best_so_far.saturating_mul(2);
        let mut sad = 0u64;
        let mut abandoned = false;
        for row in 0..strip {
            let p = prev_start + row * width;
            let c = curr_start + row * width;
            for i in 0..width {
                sad += previous[p + i].abs_diff(current[c + i]) as u64;
            }
            if sad > limit {
                abandoned = true;
                break;
            }
        }
        if abandoned {
            sads.push(u64::MAX);
            continue;
        }
        best_so_far = best_so_far.min(sad);
        sads.push(sad);
    }

    let best_y = (0..=search_max).min_by_key(|&y| sads[y]).unwrap_or(0);
    let best_sad = sads[best_y];
    // The strip has to be found in exactly one place. Blank or repeating
    // content matches just as well several rows away, and so does a frame with
    // no overlap left after a fast scroll; stitching that guess duplicates or
    // drops rows silently. Offsets within `strip` of the winner are the
    // shoulder of the same match, not a rival one.
    let runner_up = sads
        .iter()
        .enumerate()
        .filter(|(y, _)| y.abs_diff(best_y) > strip)
        .map(|(_, &s)| s)
        .min()
        .unwrap_or(u64::MAX);
    (best_y, best_sad, runner_up)
}

/// Box-averages `rows` rows starting at `row0` down by `step` in both axes.
/// Averaging (not nearest sampling) keeps thin glyph strokes and 1px borders
/// visible in the coarse pass, so its minimum stays on the true row.
fn downsample_rows(
    gray: &[u8],
    width: usize,
    row0: usize,
    rows: usize,
    step: usize,
) -> (Vec<u8>, usize, usize) {
    let cw = width.div_ceil(step);
    let ch = rows.div_ceil(step);
    let mut out = vec![0u8; cw * ch];
    for cy in 0..ch {
        let y0 = cy * step;
        let y1 = (y0 + step).min(rows);
        for cx in 0..cw {
            let x0 = cx * step;
            let x1 = (x0 + step).min(width);
            let mut sum = 0u32;
            let mut n = 0u32;
            for y in y0..y1 {
                let base = (row0 + y) * width;
                for x in x0..x1 {
                    sum += gray[base + x] as u32;
                    n += 1;
                }
            }
            out[cy * cw + cx] = (sum / n.max(1)) as u8;
        }
    }
    (out, cw, ch)
}

/// Coarse-to-fine search: the SAD minimum is first located on the downsampled
/// pair, then refined at full resolution in a `+/-2 coarse steps` window. The
/// reported score and runner-up are both full-resolution, so acceptance keeps
/// exactly the same meaning as [`full_sweep`].
fn coarse_to_fine(
    previous: &[u8],
    current: &[u8],
    width: usize,
    height: usize,
    strip: usize,
    prev_start: usize,
    search_max: usize,
) -> (usize, u64, u64) {
    let step = COARSE_STEP;
    let (prev_c, cw, cstrip) = downsample_rows(previous, width, prev_start / width, strip, step);
    let (curr_c, ccw, ch) = downsample_rows(current, width, 0, height, step);
    debug_assert_eq!(cw, ccw);
    // Not enough coarse rows to search: the full sweep is both exact and
    // cheap at this size.
    if ch <= cstrip || cstrip == 0 {
        return full_sweep(previous, current, width, strip, prev_start, search_max);
    }
    let csearch_max = ch - cstrip;

    let mut csads = Vec::with_capacity(csearch_max + 1);
    for y in 0..=csearch_max {
        let mut sad = 0u64;
        for row in 0..cstrip {
            let p = row * cw;
            let c = (y + row) * cw;
            for i in 0..cw {
                sad += prev_c[p + i].abs_diff(curr_c[c + i]) as u64;
            }
        }
        csads.push(sad);
    }
    let cbest = (0..=csearch_max).min_by_key(|&y| csads[y]).unwrap_or(0);
    let center = cbest * step;

    // Full-resolution refinement. Ascending order keeps the first-minimum
    // tie-break the naive sweep uses.
    let lo = center.saturating_sub(2 * step);
    let hi = (center + 2 * step).min(search_max);
    let mut best_y = lo;
    let mut best_sad = u64::MAX;
    for y in lo..=hi {
        let sad = sad_at(previous, current, width, strip, prev_start, y);
        if sad < best_sad {
            best_sad = sad;
            best_y = y;
        }
    }

    // Ambiguity guard, in full-resolution units: the strongest coarse rival,
    // re-checked at full resolution over its own +/-2-step window. Only one
    // rival window is needed -- a second-best competing match is what makes an
    // offset a guess, and the coarse pass already ranks them.
    let rival = csads
        .iter()
        .enumerate()
        .filter(|(y, _)| (y * step).abs_diff(best_y) > strip)
        .min_by_key(|(_, &s)| s)
        .map(|(y, _)| y);
    let mut runner_up = u64::MAX;
    if let Some(ry) = rival {
        let rcenter = ry * step;
        let rlo = rcenter.saturating_sub(2 * step);
        let rhi = (rcenter + 2 * step).min(search_max);
        for y in rlo..=rhi {
            if y.abs_diff(best_y) <= strip {
                continue;
            }
            runner_up = runner_up.min(sad_at(previous, current, width, strip, prev_start, y));
        }
    }
    (best_y, best_sad, runner_up)
}

/// Minimum fresh rows before we stitch a frame; smaller deltas are the strip
/// matching itself on a stationary page, and appending/prepending them would
/// duplicate the seam.
pub const MIN_NEW_ROWS: usize = 2;

/// Default score floor the host passes to [`classify_scroll`]: the medium band
/// of [`accepts`], which tolerates antialiasing/noise on a real scroll while
/// still rejecting unrelated content (~0.67).
pub const ACCEPT_SCORE: f64 = MEDIUM_SCORE;

/// Everything a tick wants for its diagnostic log, plus the decision.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollReport {
    /// `Some((is_up, offset_y, new_rows))` when a genuine scroll was accepted.
    pub direction: Option<(bool, usize, usize)>,
    pub down_score: f64,
    pub down_ratio: f64,
    pub up_score: f64,
    pub up_ratio: f64,
    /// `false` when the down pass accepted and the (expensive) up pass was
    /// skipped -- the common case.
    pub up_tried: bool,
    /// Sampled fraction of pixels that changed by more than 24 levels. Only
    /// computed when both directions were rejected; a high value there means
    /// "scrolled too fast to align", not "nothing happened".
    pub changed: f64,
}

/// Pure scroll-direction decision, shared by `overlay::long_tick` and the unit
/// tests so the host and its tests can never disagree. Returns
/// `(is_up, offset_y, new_rows)`:
///
/// - down (`is_up=false`): append `current[offset_y..height)`; `offset_y` is
///   the end of the overlap, `new_rows = height - offset_y`.
/// - up (`is_up=true`): prepend `current[0..new_rows)`; `offset_y == new_rows`.
///
/// A genuine up-scroll cannot produce a valid bottom-strip match (those rows
/// scrolled off the bottom) and vice versa, so trying down first is safe and
/// halves the common cost: the up pass only runs when down was not accepted.
/// `None` = stationary / rejected.
pub fn classify_scroll(
    prev_gray: &[u8],
    gray: &[u8],
    width: usize,
    height: usize,
    strip: usize,
    accept_score: f64,
) -> Option<(bool, usize, usize)> {
    classify_scroll_diag(prev_gray, gray, width, height, strip, accept_score).direction
}

/// [`classify_scroll`] plus the scores/ratios a diagnostic tick log wants.
pub fn classify_scroll_diag(
    prev_gray: &[u8],
    gray: &[u8],
    width: usize,
    height: usize,
    strip: usize,
    accept_score: f64,
) -> ScrollReport {
    let down = align_vertical(prev_gray, gray, width, height, strip, accept_score);
    let down_rows = height.saturating_sub(down.offset_y);
    let mut report = ScrollReport {
        down_score: down.score,
        down_ratio: down.runner_up_ratio,
        ..ScrollReport::default()
    };
    if down.accepted && down_rows > MIN_NEW_ROWS {
        report.direction = Some((false, down.offset_y, down_rows));
        return report;
    }

    let up = align_vertical_up(prev_gray, gray, width, height, strip, accept_score);
    report.up_tried = true;
    report.up_score = up.score;
    report.up_ratio = up.runner_up_ratio;
    if up.accepted && up.offset_y > MIN_NEW_ROWS {
        report.direction = Some((true, up.offset_y, up.offset_y));
        return report;
    }

    report.changed = frame_change_ratio(prev_gray, gray);
    report
}

/// Sampled fraction of pixels that changed by more than `CHANGE_DELTA` levels.
/// Used only to tell "the page moved but too fast to align" from "nothing
/// happened"; sampling every 4th pixel keeps it cheap on the reject path.
pub fn frame_change_ratio(previous: &[u8], current: &[u8]) -> f64 {
    const CHANGE_DELTA: u8 = 24;
    const STEP: usize = 4;
    if previous.len() != current.len() || previous.is_empty() {
        return 0.0;
    }
    let mut changed = 0u64;
    let mut total = 0u64;
    for (i, (a, b)) in previous.iter().zip(current.iter()).enumerate() {
        if i % STEP != 0 {
            continue;
        }
        total += 1;
        if a.abs_diff(*b) > CHANGE_DELTA {
            changed += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        changed as f64 / total as f64
    }
}

// ---- scroll axis: horizontal sessions rotate into the vertical model ------

/// Which way the user scrolls. The SAD matcher and `Stitcher` only understand
/// a vertical scroll axis, so a horizontal session rotates every frame 90
/// degrees clockwise while capturing and rotates the stitched image back on
/// export (the reference scrollshot does the same with `rotate90`/`rotate270`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Axis {
    #[default]
    Vertical,
    Horizontal,
}

impl Axis {
    pub fn flipped(self) -> Self {
        match self {
            Axis::Vertical => Axis::Horizontal,
            Axis::Horizontal => Axis::Vertical,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Axis::Vertical => "垂直",
            Axis::Horizontal => "水平",
        }
    }
}

/// A selection at least twice as wide as tall is a horizontal long page (a
/// wide viewport strip); anything else -- including a 16:9 window or a
/// full-screen selection (1.78) -- behaves like the original vertical
/// capture, because vertical scrolling is overwhelmingly the common case and
/// the bar's direction toggle corrects a miss.
pub const HORIZONTAL_ASPECT: f64 = 2.0;

pub fn infer_axis(width: usize, height: usize) -> Axis {
    if width as f64 >= height as f64 * HORIZONTAL_ASPECT {
        Axis::Horizontal
    } else {
        Axis::Vertical
    }
}

/// Rotates a tight BGRA frame 90 degrees clockwise. `dst(w=height, h=width)`
/// at (x', y') = (height-1-y, x) for every source pixel (x, y): the right edge
/// -- where new content enters on a rightward scroll -- becomes the bottom
/// edge, exactly what the bottom-strip matcher expects.
pub fn rotate_bgra90(bgra: &[u8], width: usize, height: usize) -> (Vec<u8>, usize, usize) {
    let (dw, dh) = (height, width);
    let mut out = vec![0u8; width * height * 4];
    if bgra.len() < width * height * 4 {
        return (out, dw, dh);
    }
    for y in 0..height {
        for x in 0..width {
            let s = (y * width + x) * 4;
            let d = (x * dw + (dw - 1 - y)) * 4;
            out[d..d + 4].copy_from_slice(&bgra[s..s + 4]);
        }
    }
    (out, dw, dh)
}

/// Inverse of [`rotate_bgra90`] (90 degrees counter-clockwise).
pub fn rotate_bgra270(bgra: &[u8], width: usize, height: usize) -> (Vec<u8>, usize, usize) {
    let (dw, dh) = (height, width);
    let mut out = vec![0u8; width * height * 4];
    if bgra.len() < width * height * 4 {
        return (out, dw, dh);
    }
    for y in 0..height {
        for x in 0..width {
            let s = (y * width + x) * 4;
            let d = ((width - 1 - x) * dw + y) * 4;
            out[d..d + 4].copy_from_slice(&bgra[s..s + 4]);
        }
    }
    (out, dw, dh)
}

/// Frame in the matcher's coordinate space (scroll axis = height). Vertical
/// frames are borrowed, so the vertical path pays no extra copy.
pub fn normalize_bgra<'a>(
    frame: &'a [u8],
    width: usize,
    height: usize,
    axis: Axis,
) -> (Cow<'a, [u8]>, usize, usize) {
    match axis {
        Axis::Vertical => (Cow::Borrowed(frame), width, height),
        Axis::Horizontal => {
            let (px, w, h) = rotate_bgra90(frame, width, height);
            (Cow::Owned(px), w, h)
        }
    }
}

/// Inverse of [`normalize_bgra`]: the stitched image back in screen space.
pub fn denormalize_bgra<'a>(
    frame: &'a [u8],
    width: usize,
    height: usize,
    axis: Axis,
) -> (Cow<'a, [u8]>, usize, usize) {
    match axis {
        Axis::Vertical => (Cow::Borrowed(frame), width, height),
        Axis::Horizontal => {
            let (px, w, h) = rotate_bgra270(frame, width, height);
            (Cow::Owned(px), w, h)
        }
    }
}

/// Height bookkeeping only — the C# runner checks `TryAppend` before keeping
/// a frame, so the budget decision stays separate from pixel storage.
pub struct LongImageBuffer {
    max_height: usize,
    total_height: usize,
    segment_heights: Vec<usize>,
}

impl Default for LongImageBuffer {
    fn default() -> Self {
        Self::new(16384)
    }
}

impl LongImageBuffer {
    pub fn new(max_height: usize) -> Self {
        LongImageBuffer {
            max_height: max_height.max(1),
            total_height: 0,
            segment_heights: Vec::new(),
        }
    }

    pub fn max_height(&self) -> usize {
        self.max_height
    }

    pub fn total_height(&self) -> usize {
        self.total_height
    }

    pub fn segment_count(&self) -> usize {
        self.segment_heights.len()
    }

    pub fn can_append(&self, height: usize) -> bool {
        height > 0 && self.total_height + height <= self.max_height
    }

    pub fn try_append(&mut self, height: usize) -> bool {
        if !self.can_append(height) {
            return false;
        }
        self.segment_heights.push(height);
        self.total_height += height;
        true
    }

    /// Head insertion for an upward scroll: same budget as `try_append`, but
    /// the new segment goes in front so `segment_heights` stays in image order.
    pub fn can_prepend(&self, height: usize) -> bool {
        height > 0 && self.total_height + height <= self.max_height
    }

    pub fn try_prepend(&mut self, height: usize) -> bool {
        if !self.can_prepend(height) {
            return false;
        }
        self.segment_heights.insert(0, height);
        self.total_height += height;
        true
    }
}

/// Growing BGRA pixel buffer for the stitched image. `LongImageBuffer`
/// mirrors the C# height accounting; this type owns the pixels that the
/// accounting describes, so a rejected append leaves both untouched.
pub struct Stitcher {
    width: usize,
    max_height: usize,
    heights: Vec<usize>,
    pixels: Vec<u8>,
}

impl Stitcher {
    /// The first frame is appended whole; exceeding the budget up front means
    /// the region is taller than a D3D texture can hold, so there is nothing
    /// to stitch into.
    pub fn new(width: usize, first_frame_height: usize, max_height: usize) -> Option<Self> {
        let max_height = max_height.max(1);
        if width == 0 || first_frame_height == 0 || first_frame_height > max_height {
            return None;
        }
        let mut s = Stitcher {
            width,
            max_height,
            heights: Vec::new(),
            pixels: Vec::with_capacity(width * 4 * first_frame_height),
        };
        s.heights.push(first_frame_height);
        s.pixels.resize(width * 4 * first_frame_height, 0);
        Some(s)
    }

    /// Writes the first frame's pixels. `new` only reserves and accounts for
    /// the first segment because the caller sizes the buffer before the first
    /// capture lands; the segment count is unchanged.
    pub fn fill_first(&mut self, frame_bgra: &[u8]) -> bool {
        let Some(&rows) = self.heights.first() else {
            return false;
        };
        let bytes = rows * self.width * 4;
        if frame_bgra.len() < bytes {
            return false;
        }
        self.pixels[..bytes].copy_from_slice(&frame_bgra[..bytes]);
        true
    }
    /// Appends the rows `[offset_y..frame_height)` of `frame_bgra`. Rows are
    /// packed (`frame_width * 4` bytes); a width mismatch or an over-budget
    /// append leaves the buffer unchanged.
    pub fn append(
        &mut self,
        frame_bgra: &[u8],
        frame_width: usize,
        frame_height: usize,
        offset_y: usize,
    ) -> bool {
        if frame_width != self.width || frame_bgra.len() < frame_width * 4 * frame_height {
            return false;
        }
        let new_rows = frame_height.saturating_sub(offset_y);
        if new_rows == 0 {
            return false;
        }
        let total = self.total_height();
        if total + new_rows > self.max_height {
            return false;
        }
        let row_bytes = self.width * 4;
        self.pixels
            .extend_from_slice(&frame_bgra[offset_y * row_bytes..frame_height * row_bytes]);
        self.heights.push(new_rows);
        true
    }

    /// Prepends the rows `[0..new_rows)` of `frame_bgra` at the TOP of the
    /// stitched image. Used when the user scrolls up: fresh content enters from
    /// the top, so it must be inserted in front while every already-stitched
    /// row keeps its bytes and relative order. Reallocates the pixel buffer
    /// (one 450 ms tick, rare) so `into_pixels` stays a single contiguous vec;
    /// `heights` gains a front segment so `total_height` stays consistent.
    /// A width mismatch, zero/oversized `new_rows` or an over-budget prepend
    /// leaves the whole buffer untouched.
    pub fn prepend(
        &mut self,
        frame_bgra: &[u8],
        frame_width: usize,
        frame_height: usize,
        new_rows: usize,
    ) -> bool {
        if frame_width != self.width || frame_bgra.len() < frame_width * 4 * frame_height {
            return false;
        }
        if new_rows == 0 || new_rows > frame_height {
            return false;
        }
        if self.total_height() + new_rows > self.max_height {
            return false;
        }
        let row_bytes = self.width * 4;
        let bytes = new_rows * row_bytes;
        let mut pixels = Vec::with_capacity(self.pixels.len() + bytes);
        pixels.extend_from_slice(&frame_bgra[..bytes]);
        pixels.extend_from_slice(&self.pixels);
        self.pixels = pixels;
        self.heights.insert(0, new_rows);
        true
    }

    pub fn total_height(&self) -> usize {
        self.heights.iter().sum()
    }

    pub fn segment_count(&self) -> usize {
        self.heights.len()
    }

    /// Hands back (bgra, width, height); stride is always `width * 4`.
    pub fn into_pixels(self) -> (Vec<u8>, u32, u32) {
        let height = self.total_height() as u32;
        (self.pixels, self.width as u32, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray_frame(rows: &[&[u8]], width: usize) -> Vec<u8> {
        // Each row is a repeating grayscale value sequence; build w*h bytes.
        let mut out = Vec::with_capacity(rows.len() * width);
        for row in rows {
            for x in 0..width {
                out.push(row[x % row.len()]);
            }
        }
        out
    }

    fn bgra_frame(rows: &[&[u8]], width: usize) -> Vec<u8> {
        // Repeating BGRA quads per row.
        let mut out = Vec::with_capacity(rows.len() * width * 4);
        for row in rows {
            for x in 0..width {
                let o = (x * 4) % row.len();
                out.extend_from_slice(&row[o..o + 4]);
            }
        }
        out
    }

    #[test]
    fn gray_weights_match_csharp() {
        // BGRA order: red is (0,0,255), green (0,255,0), blue (255,0,0).
        assert_eq!(bgra_to_gray(&[0, 0, 255, 255], 1, 1), vec![76]);
        assert_eq!(bgra_to_gray(&[0, 255, 0, 255], 1, 1), vec![149]);
        assert_eq!(bgra_to_gray(&[255, 0, 0, 255], 1, 1), vec![28]);
        assert_eq!(bgra_to_gray(&[255, 255, 255, 255], 1, 1), vec![255]);
        // Alpha is ignored.
        assert_eq!(bgra_to_gray(&[255, 255, 255, 0], 1, 1), vec![255]);
    }

    /// Deterministic byte stream, one distinct row at a time: alignment needs a
    /// unique SAD minimum, and repeating rows would match at every period.
    fn lcg_rows(height: usize, width: usize, seed: u32) -> Vec<Vec<u8>> {
        let mut state = seed;
        let mut rows = Vec::with_capacity(height);
        for _ in 0..height {
            rows.push(
                (0..width)
                    .map(|_| {
                        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                        (state >> 24) as u8
                    })
                    .collect(),
            );
        }
        rows
    }

    #[test]
    fn identical_frames_have_no_new_content() {
        let rows = lcg_rows(32, 8, 0x12345678);
        let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
        let frame = gray_frame(&refs, 8);
        let r = align_vertical(&frame, &frame, 8, 32, 8, 0.35);
        assert!(r.accepted, "score {}", r.score);
        assert_eq!(r.offset_y, 32);
        assert!((r.score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn scrolled_by_k_appends_k_rows() {
        let width = 8;
        let height = 32;
        let strip = 8;
        let k = 5;
        // `previous` is a deterministic pseudo-random pattern per row so SAD
        // has a unique minimum; `current` is a full frame: `previous` scrolled
        // down by k with k fresh rows entering at the bottom.
        let prev_rows = lcg_rows(height, width, 0x12345678);
        let new_rows = lcg_rows(k, width, 0xABCDEF01);
        let prev_refs: Vec<&[u8]> = prev_rows.iter().map(Vec::as_slice).collect();
        let curr_rows: Vec<&[u8]> = prev_refs[k..]
            .iter()
            .copied()
            .chain(new_rows.iter().map(Vec::as_slice))
            .collect();
        let prev = gray_frame(&prev_refs, width);
        let curr = gray_frame(&curr_rows, width);
        let r = align_vertical(&prev, &curr, width, height, strip, 0.35);
        assert!(r.accepted, "score {}", r.score);
        assert_eq!(r.offset_y, height - k);
    }

    #[test]
    fn unrelated_frames_are_rejected() {
        // Independent noise scores ~2/3 (E|a-b| = 85/255), which is why the
        // caller passes 0.9: a wrong window swapped in mid-capture must not
        // clear the bar on score alone.
        let width = 8;
        let height = 32;
        let a_rows = lcg_rows(height, width, 7);
        let b_rows = lcg_rows(height, width, 0xDEADBEEF);
        let a = gray_frame(&a_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(), width);
        let b = gray_frame(&b_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(), width);
        let r = align_vertical(&a, &b, width, height, 8, 0.9);
        assert!(!r.accepted, "score {}", r.score);
    }

    #[test]
    fn ambiguous_matches_are_rejected() {
        // A blank page: the previous bottom strip matches everywhere, so the
        // SAD score is a perfect 1.0 while the offset is a coin flip. Scoring
        // alone would accept this and append a screen of blank rows.
        let frame = vec![255u8; 8 * 32];
        let r = align_vertical(&frame, &frame, 8, 32, 8, 0.35);
        assert!(!r.accepted, "score {}", r.score);
    }

    #[test]
    fn tiny_frames_cannot_align() {
        // height 4 clamps strip to 4 → search_max == 0.
        let frame = vec![128u8; 4 * 4];
        let r = align_vertical(&frame, &frame, 4, 4, 8, 0.35);
        assert!(!r.accepted);
        assert_eq!(r.offset_y, 0);
        // Empty inputs are rejected, not a panic.
        let r = align_vertical(&[], &[], 0, 0, 8, 0.35);
        assert!(!r.accepted);
    }

    /// Old full-SAD implementation, kept only to check the row-wise early-exit
    /// version in `align_vertical` against it: same candidates, same `best_y`
    /// tie-break (first minimum), same score, no abandonment.
    fn align_vertical_naive(
        previous: &[u8],
        current: &[u8],
        width: usize,
        height: usize,
        strip_height: usize,
        accept_score: f64,
    ) -> FrameAlignResult {
        if width == 0
            || height == 0
            || previous.len() < width * height
            || current.len() < width * height
        {
            return FrameAlignResult::rejected();
        }
        let strip = strip_height.clamp(4, (height / 3).max(4));
        if height <= strip {
            return FrameAlignResult::rejected();
        }
        let search_max = height - strip;
        let prev_start = (height - strip) * width;
        let len = strip * width;
        let mut sads = Vec::with_capacity(search_max + 1);
        for y in 0..=search_max {
            let curr_start = y * width;
            let mut sad = 0u64;
            for i in 0..len {
                sad += previous[prev_start + i].abs_diff(current[curr_start + i]) as u64;
            }
            sads.push(sad);
        }
        let best_y = (0..=search_max).min_by_key(|&y| sads[y]).unwrap_or(0);
        let best_sad = sads[best_y];
        let runner_up = sads
            .iter()
            .enumerate()
            .filter(|(y, _)| y.abs_diff(best_y) > strip)
            .map(|(_, &s)| s)
            .min()
            .unwrap_or(u64::MAX);
        let score = 1.0 - best_sad as f64 / (len * 255) as f64;
        let runner_up_ratio = ratio(best_sad, runner_up);
        let accepted = accepts(score, runner_up_ratio, accept_score);
        let offset_y = height.min(best_y + strip);
        FrameAlignResult {
            offset_y,
            score,
            accepted,
            runner_up_ratio,
        }
    }

    /// The row-wise early-exit `align_vertical` must decide exactly like the
    /// full-SAD `align_vertical_naive` on every candidate frame pair: same
    /// `offset_y`, `accepted` and `score`.
    #[test]
    fn early_exit_sad_matches_naive_full_sad() {
        let width = 8;
        let height = 32;
        let strip = 8;

        let cases: Vec<(Vec<u8>, Vec<u8>)> = {
            let identical = {
                let rows = lcg_rows(height, width, 0x1);
                let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
                let f = gray_frame(&refs, width);
                (f.clone(), f)
            };
            let scrolled = {
                let k = 5;
                let prev_rows = lcg_rows(height, width, 0x2);
                let new_rows = lcg_rows(k, width, 0x3);
                let prev_refs: Vec<&[u8]> = prev_rows.iter().map(Vec::as_slice).collect();
                let curr_rows: Vec<&[u8]> = prev_refs[k..]
                    .iter()
                    .copied()
                    .chain(new_rows.iter().map(Vec::as_slice))
                    .collect();
                (gray_frame(&prev_refs, width), gray_frame(&curr_rows, width))
            };
            let unrelated = {
                let a_rows = lcg_rows(height, width, 0x4);
                let b_rows = lcg_rows(height, width, 0x5);
                let a_refs: Vec<&[u8]> = a_rows.iter().map(Vec::as_slice).collect();
                let b_refs: Vec<&[u8]> = b_rows.iter().map(Vec::as_slice).collect();
                (gray_frame(&a_refs, width), gray_frame(&b_refs, width))
            };
            let blank = {
                let f = vec![255u8; width * height];
                (f.clone(), f)
            };
            vec![identical, scrolled, unrelated, blank]
        };

        for (previous, current) in cases {
            for accept_score in [0.35, 0.9] {
                let fast = align_vertical(&previous, &current, width, height, strip, accept_score);
                let naive =
                    align_vertical_naive(&previous, &current, width, height, strip, accept_score);
                assert_eq!(fast.offset_y, naive.offset_y);
                assert_eq!(fast.accepted, naive.accepted);
                assert!(
                    (fast.score - naive.score).abs() < 1e-9,
                    "fast={} naive={}",
                    fast.score,
                    naive.score
                );
            }
        }
    }

    #[test]
    fn buffer_stops_at_max_height() {
        let mut buf = LongImageBuffer::new(20);
        assert!(buf.try_append(10));
        assert!(buf.try_append(10));
        assert!(!buf.can_append(1));
        assert!(!buf.try_append(1));
        assert_eq!(buf.total_height(), 20);
        assert_eq!(buf.segment_count(), 2);
        assert!(!buf.try_append(0));
    }

    #[test]
    fn stitcher_appends_only_new_rows() {
        let width = 2;
        let row0: &[u8] = &[1, 2, 3, 4];
        let row1: &[u8] = &[5, 6, 7, 8];
        let mut s = Stitcher::new(width, 4, 16).unwrap();
        // The caller writes the first frame's crop into the buffer; here the
        // interesting part is what `append` adds after it.
        // Second frame scrolled by 3: only its last row is new.
        let row2: &[u8] = &[9, 10, 11, 12];
        let second = bgra_frame(&[row1, row0, row1, row2], width);
        assert!(s.append(&second, width, 4, 3));
        assert_eq!(s.total_height(), 5);
        assert_eq!(s.segment_count(), 2);
        let (out, ow, oh) = s.into_pixels();
        assert_eq!((ow, oh), (2, 5));
        // First frame is zero-initialized until the caller blits the crop, so
        // only the appended row carries the second frame's bytes.
        assert_eq!(&out[4 * 8..], &bgra_frame(&[row2], width)[..]);
    }

    #[test]
    fn fill_first_writes_the_reserved_rows() {
        let mut s = Stitcher::new(2, 2, 8).unwrap();
        let frame = [9u8; 2 * 2 * 4];
        assert!(s.fill_first(&frame));
        assert_eq!(s.segment_count(), 1);
        let (out, _, _) = s.into_pixels();
        assert_eq!(out, frame.to_vec());
        // Short input is rejected without touching the buffer.
        let mut s = Stitcher::new(2, 2, 8).unwrap();
        assert!(!s.fill_first(&[1u8; 3]));
        let (out, _, _) = s.into_pixels();
        assert_eq!(out, vec![0u8; 2 * 2 * 4]);
    }

    #[test]
    fn stitcher_rejects_over_budget_and_bad_width() {
        let width = 2;
        let frame = bgra_frame(&[&[0u8, 0, 0, 0], &[0u8, 0, 0, 0]], width);
        let mut s = Stitcher::new(width, 2, 4).unwrap();
        // 2 + 3 > 4.
        let three_rows = bgra_frame(&[&[0u8, 0, 0, 0][..]; 3], width);
        assert!(!s.append(&three_rows, width, 3, 0));
        assert_eq!(s.total_height(), 2);
        assert_eq!(s.segment_count(), 1);
        // Wrong width must not touch state either.
        assert!(!s.append(&frame, 3, 2, 0));
        assert_eq!(s.total_height(), 2);
        // First frame taller than the budget yields no stitcher at all.
        assert!(Stitcher::new(width, 5, 4).is_none());
    }

    // ---- bidirectional (up-scroll) support ---------------------------------

    /// BGRA whose `bgra_to_gray` reproduces `gray` exactly: B=G=R=g, A=255,
    /// and 29+150+77 = 256 so (256*g)>>8 == g.
    fn bgra_from_gray(gray: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(gray.len() * 4);
        for &g in gray {
            out.extend_from_slice(&[g, g, g, 255]);
        }
        out
    }

    /// A synthetic source page of unique gray rows, with helpers to cut a
    /// viewport frame (BGRA) and its grayscale alignment frame.
    struct Page {
        bgra: Vec<u8>,
        width: usize,
        height: usize,
    }

    impl Page {
        fn new(n_rows: usize, width: usize, height: usize, seed: u32) -> Self {
            let rows = lcg_rows(n_rows, width, seed);
            let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
            let gray = gray_frame(&refs, width);
            Page {
                bgra: bgra_from_gray(&gray),
                width,
                height,
            }
        }
        fn row_bytes(&self) -> usize {
            self.width * 4
        }
        fn frame(&self, top: usize) -> Vec<u8> {
            let rb = self.row_bytes();
            self.bgra[top * rb..(top + self.height) * rb].to_vec()
        }
        fn gray(&self, top: usize) -> Vec<u8> {
            bgra_to_gray(&self.frame(top), self.width, self.height)
        }
        fn rows(&self, top: usize, count: usize) -> Vec<u8> {
            let rb = self.row_bytes();
            self.bgra[top * rb..(top + count) * rb].to_vec()
        }
    }

    fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && needle.len() <= hay.len()
            && hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn up_scroll_by_k_reports_k_fresh_top_rows() {
        let width = 8;
        let height = 32;
        let strip = 8;
        let k = 5;
        let prev_rows = lcg_rows(height, width, 0x12345678);
        let new_rows = lcg_rows(k, width, 0xABCDEF01);
        let prev_refs: Vec<&[u8]> = prev_rows.iter().map(Vec::as_slice).collect();
        // current = k fresh rows at TOP + prev[0..height-k).
        let curr_rows: Vec<&[u8]> = new_rows
            .iter()
            .map(Vec::as_slice)
            .chain(prev_refs[..height - k].iter().copied())
            .collect();
        let prev = gray_frame(&prev_refs, width);
        let curr = gray_frame(&curr_rows, width);
        let r = align_vertical_up(&prev, &curr, width, height, strip, 0.9);
        assert!(r.accepted, "score {}", r.score);
        assert_eq!(r.offset_y, k, "fresh rows at the top");
        // An up-scroll cannot produce a valid bottom-strip match: those rows
        // scrolled off the bottom, so the down aligner must not win.
        let down = align_vertical(&prev, &curr, width, height, strip, 0.9);
        assert!(!down.accepted || height - down.offset_y <= MIN_NEW_ROWS);
    }

    #[test]
    fn identical_frames_have_no_new_top_content() {
        let rows = lcg_rows(32, 8, 0x12345678);
        let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
        let frame = gray_frame(&refs, 8);
        let r = align_vertical_up(&frame, &frame, 8, 32, 8, 0.9);
        assert!(r.accepted);
        assert_eq!(r.offset_y, 0);
        // A stationary page is skipped by the shared direction classifier.
        assert_eq!(classify_scroll(&frame, &frame, 8, 32, 8, 0.9), None);
    }

    #[test]
    fn classify_scroll_picks_down_vs_up() {
        let width = 8;
        let height = 32;
        let strip = 8;
        let k = 5;
        let prev_rows = lcg_rows(height, width, 1);
        let prev_refs: Vec<&[u8]> = prev_rows.iter().map(Vec::as_slice).collect();
        let prev = gray_frame(&prev_refs, width);
        // Down: prev[k..] + fresh rows at the bottom.
        let dn = lcg_rows(k, width, 2);
        let down_rows: Vec<&[u8]> = prev_refs[k..]
            .iter()
            .copied()
            .chain(dn.iter().map(Vec::as_slice))
            .collect();
        let down = gray_frame(&down_rows, width);
        assert_eq!(
            classify_scroll(&prev, &down, width, height, strip, 0.9),
            Some((false, height - k, k))
        );
        // Up: fresh rows at the top + prev[..height-k).
        let upn = lcg_rows(k, width, 3);
        let up_rows: Vec<&[u8]> = upn
            .iter()
            .map(Vec::as_slice)
            .chain(prev_refs[..height - k].iter().copied())
            .collect();
        let up = gray_frame(&up_rows, width);
        assert_eq!(
            classify_scroll(&prev, &up, width, height, strip, 0.9),
            Some((true, k, k))
        );
    }

    #[test]
    fn stitcher_prepend_preserves_content_and_accounts_height() {
        let width = 2;
        let a: &[u8] = &[1, 2, 3, 4];
        let b: &[u8] = &[5, 6, 7, 8];
        let fresh: &[u8] = &[9, 10, 11, 12];
        let mut s = Stitcher::new(width, 2, 16).unwrap();
        let first = bgra_frame(&[a, b], width);
        assert!(s.fill_first(&first));
        // Up-scroll frame: fresh row on top, then the two existing rows.
        let frame = bgra_frame(&[fresh, a, b], width);
        assert!(s.prepend(&frame, width, 3, 1));
        assert_eq!(s.total_height(), 3);
        assert_eq!(s.segment_count(), 2);
        let (out, ow, oh) = s.into_pixels();
        assert_eq!((ow, oh), (2, 3));
        assert_eq!(out, bgra_frame(&[fresh, a, b], width));

        // Rejections leave the buffer completely untouched.
        let mut s = Stitcher::new(width, 2, 16).unwrap();
        assert!(s.fill_first(&first));
        assert!(!s.prepend(&frame, width, 3, 0));
        assert!(!s.prepend(&frame, width, 3, 4));
        assert!(!s.prepend(&frame, 3, 3, 1));
        let (out, _, oh) = s.into_pixels();
        assert_eq!(oh, 2);
        assert_eq!(out, first.to_vec());
    }

    #[test]
    fn buffer_prepend_mirrors_append_budget() {
        let mut buf = LongImageBuffer::new(20);
        assert!(buf.try_append(10));
        assert!(buf.try_prepend(5));
        assert_eq!(buf.total_height(), 15);
        assert_eq!(buf.segment_count(), 2);
        assert!(buf.try_prepend(5));
        assert_eq!(buf.total_height(), 20);
        assert!(!buf.can_prepend(1));
        assert!(!buf.try_prepend(1));
        assert!(!buf.try_prepend(0));
    }

    /// Scrolling UP as the primary direction must reconstruct the source rows
    /// above the starting viewport exactly, pixel for pixel.
    #[test]
    fn pure_up_scroll_reconstructs_source_above_the_start() {
        let width = 8;
        let height = 32;
        let strip = 8;
        let k = 5;
        let page = Page::new(120, width, height, 0xA1B2C3D4);
        let start = 40;
        let m = 4;
        let mut s = Stitcher::new(width, height, 16384).unwrap();
        assert!(s.fill_first(&page.frame(start)));
        let mut prev_gray = page.gray(start);
        let mut top = start;
        for _ in 0..m {
            top -= k;
            let px = page.frame(top);
            let gray = page.gray(top);
            match classify_scroll(&prev_gray, &gray, width, height, strip, 0.9) {
                Some((true, offset, new_rows)) => {
                    assert_eq!(new_rows, k);
                    assert!(s.prepend(&px, width, height, offset));
                }
                other => panic!("up-scroll not detected: {other:?}"),
            }
            prev_gray = gray;
        }
        let expected_top = start - m * k;
        let expected = page.rows(expected_top, height + m * k);
        let (out, ow, oh) = s.into_pixels();
        assert_eq!((ow as usize, oh as usize), (width, height + m * k));
        assert_eq!(out, expected, "up-scroll must reconstruct the source page");
    }

    /// The user's sequence: scroll DOWN N frames, then UP M frames. Up-scroll
    /// must grow the long image (prepend fresh top rows), keep every already
    /// stitched byte intact, and never stall the session.
    #[test]
    fn down_then_up_scroll_keeps_content_and_grows_height() {
        let width = 8;
        let height = 32;
        let strip = 8;
        let k = 5;
        let page = Page::new(160, width, height, 0x5EED1234);
        let start = 40;
        let mut s = Stitcher::new(width, height, 16384).unwrap();
        let first = page.frame(start);
        assert!(s.fill_first(&first));
        let mut prev_gray = page.gray(start);
        let mut top = start;

        // Phase 1: N=3 down-scrolls -> append at the bottom.
        for _ in 0..3 {
            top += k;
            let px = page.frame(top);
            let gray = page.gray(top);
            match classify_scroll(&prev_gray, &gray, width, height, strip, 0.9) {
                Some((false, offset, new_rows)) => {
                    assert_eq!(new_rows, k);
                    assert!(s.append(&px, width, height, offset));
                }
                other => panic!("down-scroll not detected: {other:?}"),
            }
            prev_gray = gray;
        }
        let h_after_down = s.total_height();
        assert_eq!(h_after_down, height + 3 * k);

        // Phase 2: M=2 up-scrolls -> prepend at the top. Reversing over rows
        // already captured must not corrupt them and must not hang the session.
        for _ in 0..2 {
            top -= k;
            let px = page.frame(top);
            let gray = page.gray(top);
            match classify_scroll(&prev_gray, &gray, width, height, strip, 0.9) {
                Some((true, offset, new_rows)) => {
                    assert_eq!(new_rows, k);
                    assert!(s.prepend(&px, width, height, offset));
                }
                other => panic!("up-scroll not detected: {other:?}"),
            }
            prev_gray = gray;
        }
        let h_after_up = s.total_height();
        assert_eq!(h_after_up, h_after_down + 2 * k);
        assert!(h_after_up > h_after_down, "up-scroll must grow the image");

        let (out, ow, oh) = s.into_pixels();
        assert_eq!((ow as usize, oh as usize), (width, h_after_up));
        assert!(contains_subslice(&out, &first), "initial frame lost");
        // The front k rows really are the last up-frame's fresh top content.
        let rb = width * 4;
        assert_eq!(&out[..k * rb], &page.rows(top, k)[..], "top new content");
    }

    // ---- coarse-to-fine alignment -----------------------------------------

    /// A frame pair big enough to take the coarse path, scrolled by `k` with
    /// `k` fresh rows at the bottom.
    fn scrolled_pair(width: usize, height: usize, k: usize, seed: u32) -> (Vec<u8>, Vec<u8>) {
        let prev_rows = lcg_rows(height, width, seed);
        let new_rows = lcg_rows(k, width, seed ^ 0xA5A5_A5A5);
        let prev_refs: Vec<&[u8]> = prev_rows.iter().map(Vec::as_slice).collect();
        let curr_rows: Vec<&[u8]> = prev_refs[k..]
            .iter()
            .copied()
            .chain(new_rows.iter().map(Vec::as_slice))
            .collect();
        (gray_frame(&prev_refs, width), gray_frame(&curr_rows, width))
    }

    #[test]
    fn coarse_to_fine_finds_the_same_row_as_the_full_sweep() {
        let width = 200;
        let height = 400;
        let strip = 64;
        let k = 37;
        let (prev, curr) = scrolled_pair(width, height, k, 0x0C0F_FEE1);
        for accept_score in [ACCEPT_SCORE, 0.9] {
            let fast = align_vertical(&prev, &curr, width, height, strip, accept_score);
            let naive = align_vertical_naive(&prev, &curr, width, height, strip, accept_score);
            assert!(fast.accepted, "score {}", fast.score);
            assert_eq!(fast.offset_y, height - k, "exact row on clean content");
            assert_eq!(fast.offset_y, naive.offset_y);
            assert!((fast.score - naive.score).abs() < 1e-9);
        }
    }

    #[test]
    fn coarse_to_fine_is_within_one_row_on_noisy_content() {
        let width = 180;
        let height = 320;
        let strip = 48;
        let k = 23;
        let (prev, curr) = scrolled_pair(width, height, k, 0x1357_9BDF);
        // Antialiasing / sub-pixel drift noise on the incoming frame.
        let noisy: Vec<u8> = curr
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let d = ((i * 37) % 13) as i16 - 6;
                (v as i16 + d).clamp(0, 255) as u8
            })
            .collect();
        let fast = align_vertical(&prev, &noisy, width, height, strip, ACCEPT_SCORE);
        let naive = align_vertical_naive(&prev, &noisy, width, height, strip, ACCEPT_SCORE);
        assert!(
            fast.accepted,
            "score {} ratio {}",
            fast.score, fast.runner_up_ratio
        );
        assert!(
            fast.offset_y.abs_diff(naive.offset_y) <= 1,
            "fast={} naive={}",
            fast.offset_y,
            naive.offset_y
        );
    }

    #[test]
    fn noisy_scroll_is_accepted_but_unrelated_is_not() {
        let width = 160;
        let height = 300;
        let strip = 48;
        let k = 19;
        let (prev, curr) = scrolled_pair(width, height, k, 0x2468_ACE0);
        let noisy: Vec<u8> = curr.iter().map(|&v| v.saturating_add(7)).collect();
        let report = classify_scroll_diag(&prev, &noisy, width, height, strip, ACCEPT_SCORE);
        match report.direction {
            Some((false, offset, new_rows)) => {
                assert_eq!(new_rows, k);
                assert!(offset.abs_diff(height - k) <= 1);
            }
            other => panic!("noisy down-scroll rejected: {other:?}"),
        }

        // Low contrast: every level within 100..140 still matches exactly.
        let low = |f: &[u8]| -> Vec<u8> { f.iter().map(|&v| 100 + (v % 41)).collect() };
        let report =
            classify_scroll_diag(&low(&prev), &low(&curr), width, height, strip, ACCEPT_SCORE);
        assert_eq!(report.direction.map(|d| d.2), Some(k));
    }

    #[test]
    fn static_and_unrelated_frames_are_rejected_at_the_default_floor() {
        let width = 160;
        let height = 300;
        let strip = 48;
        let (prev, _) = scrolled_pair(width, height, 19, 0x0BAD_F00D);
        // Stationary page: the down pass finds the same frame, zero new rows;
        // the up pass likewise. Nothing to stitch.
        assert_eq!(
            classify_scroll(&prev, &prev, width, height, strip, ACCEPT_SCORE),
            None
        );
        // A different window swapped in mid-capture.
        let other_rows = lcg_rows(height, width, 0xFEED_FACE);
        let other = gray_frame(
            &other_rows.iter().map(Vec::as_slice).collect::<Vec<_>>(),
            width,
        );
        let report = classify_scroll_diag(&prev, &other, width, height, strip, ACCEPT_SCORE);
        assert_eq!(report.direction, None);
        // ... and it is visibly different, which is what the host reports as
        // "too fast" only when the frames really did change.
        assert!(report.changed > 0.3, "changed {}", report.changed);
    }

    #[test]
    fn down_scroll_skips_the_up_pass() {
        let width = 160;
        let height = 300;
        let strip = 48;
        let k = 19;
        let (prev, curr) = scrolled_pair(width, height, k, 0x1122_3344);
        let report = classify_scroll_diag(&prev, &curr, width, height, strip, ACCEPT_SCORE);
        assert_eq!(report.direction.map(|d| (d.0, d.2)), Some((false, k)));
        assert!(!report.up_tried, "down won, so the up pass must be skipped");
        assert!(report.down_ratio > STRONG_RATIO);
    }

    /// The whole point of the coarse pass: the per-tick SAD on a 2000x1000
    /// region drops by an order of magnitude. Timings are printed for the
    /// report and the assertion only guards the direction of the change.
    #[test]
    fn coarse_pass_is_much_cheaper_than_the_full_sweep() {
        let width = 2000;
        let height = 1000;
        let strip = 32;
        let (prev, curr) = scrolled_pair(width, height, 40, 0x0F1E_2D3C);

        let t0 = std::time::Instant::now();
        let naive = align_vertical_naive(&prev, &curr, width, height, strip, ACCEPT_SCORE);
        let naive_ms = t0.elapsed().as_secs_f64() * 1e3;

        let t0 = std::time::Instant::now();
        let fast = align_vertical(&prev, &curr, width, height, strip, ACCEPT_SCORE);
        let fast_ms = t0.elapsed().as_secs_f64() * 1e3;

        eprintln!("align 2000x1000: full sweep {naive_ms:.2} ms, coarse-to-fine {fast_ms:.2} ms");
        assert!(fast.accepted, "score {}", fast.score);
        assert_eq!(fast.offset_y, naive.offset_y);
        assert!(
            fast_ms * 2.0 < naive_ms,
            "coarse {fast_ms:.2} ms vs full {naive_ms:.2} ms"
        );
    }

    // ---- horizontal (left/right) scrolling --------------------------------

    #[test]
    fn rotation_round_trips_every_pixel() {
        let width = 5;
        let height = 3;
        let mut frame = Vec::with_capacity(width * height * 4);
        for i in 0..width * height {
            frame.extend_from_slice(&[i as u8, (i * 2) as u8, (i * 3) as u8, 255]);
        }
        let (rot, rw, rh) = rotate_bgra90(&frame, width, height);
        assert_eq!((rw, rh), (height, width));
        let (back, bw, bh) = rotate_bgra270(&rot, rw, rh);
        assert_eq!((bw, bh), (width, height));
        assert_eq!(back, frame);
    }

    #[test]
    fn axis_inference_matches_the_selection_shape() {
        // A wide strip is horizontal; a 16:9 window/screen stays vertical.
        assert_eq!(infer_axis(800, 400), Axis::Horizontal);
        assert_eq!(infer_axis(1200, 300), Axis::Horizontal);
        assert_eq!(infer_axis(1920, 1080), Axis::Vertical);
        assert_eq!(infer_axis(700, 400), Axis::Vertical);
        assert_eq!(infer_axis(400, 800), Axis::Vertical);
        assert_eq!(infer_axis(400, 400), Axis::Vertical);
        assert_eq!(infer_axis(790, 400), Axis::Vertical);
    }

    /// A wide page (wider than the viewport) scrolled right, stitched through
    /// the rotated model, must reproduce the source columns pixel for pixel.
    /// Pixels are pseudo-random so a column slice has one unique match and
    /// unrelated slices really are unrelated (no periodic structure that a
    /// shifted strip could accidentally satisfy).
    fn wide_page(width: usize, height: usize) -> Vec<u8> {
        let mut state = 0x2468_ACE0u32;
        let mut out = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let g = (state >> 24) as u8;
            out.extend_from_slice(&[g, g, g, 255]);
        }
        out
    }

    fn columns(page: &[u8], page_w: usize, page_h: usize, left: usize, w: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * page_h * 4);
        for y in 0..page_h {
            let start = (y * page_w + left) * 4;
            out.extend_from_slice(&page[start..start + w * 4]);
        }
        out
    }

    /// `dir_right = true` scrolls the viewport right (new content at the right
    /// edge, which becomes the bottom edge after rotation -> append).
    fn horizontal_walk(dir_right: bool) -> (Vec<u8>, usize, usize, usize, usize, usize) {
        let page_w = 400;
        let vh = 32;
        let vw = 64;
        let k = 11;
        let m = 4;
        let page = wide_page(page_w, vh);
        let start = if dir_right { 40 } else { 200 };
        let mut stitcher: Option<Stitcher> = None;
        let mut prev_gray = Vec::new();
        let mut left = start;
        for i in 0..=m {
            let frame = columns(&page, page_w, vh, left, vw);
            let (norm, nw, nh) = normalize_bgra(&frame, vw, vh, Axis::Horizontal);
            if i == 0 {
                let mut s = Stitcher::new(nw, nh, 16384).unwrap();
                assert!(s.fill_first(&norm));
                stitcher = Some(s);
                prev_gray = bgra_to_gray(&norm, nw, nh);
            } else {
                let gray = bgra_to_gray(&norm, nw, nh);
                let strip = (nh / 4).min(32);
                let report = classify_scroll_diag(&prev_gray, &gray, nw, nh, strip, ACCEPT_SCORE);
                let s = stitcher.as_mut().unwrap();
                match report.direction {
                    Some((is_up, offset, new_rows)) if is_up == !dir_right => {
                        assert_eq!(new_rows, k, "full column step must be new");
                        if is_up {
                            assert!(s.prepend(&norm, nw, nh, offset));
                        } else {
                            assert!(s.append(&norm, nw, nh, offset));
                        }
                    }
                    other => panic!("horizontal scroll not detected: {other:?}"),
                }
                prev_gray = gray;
            }
            if dir_right {
                left += k;
            } else {
                left -= k;
            }
        }
        let (px, w, h) = stitcher.unwrap().into_pixels();
        let (back, bw, bh) = denormalize_bgra(&px, w as usize, h as usize, Axis::Horizontal);
        (back.into_owned(), bw, bh, start, vw, m * k)
    }

    #[test]
    fn horizontal_right_scroll_reconstructs_source_pixel_exact() {
        let (out, bw, bh, _start, vw, grown) = horizontal_walk(true);
        assert_eq!((bw, bh), (vw + grown, 32), "exported size is screen space");
        let page = wide_page(400, 32);
        let expected = columns(&page, 400, 32, 40, vw + grown);
        assert_eq!(
            out, expected,
            "right-scroll column order must match the page"
        );
    }

    #[test]
    fn horizontal_left_scroll_reconstructs_source_pixel_exact() {
        let (out, bw, bh, _start, vw, grown) = horizontal_walk(false);
        assert_eq!((bw, bh), (vw + grown, 32));
        let page = wide_page(400, 32);
        let expected = columns(&page, 400, 32, 200 - grown, vw + grown);
        assert_eq!(out, expected, "left-scroll prepends fresh columns");
    }
}
