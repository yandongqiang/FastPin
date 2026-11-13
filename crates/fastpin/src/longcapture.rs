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

use crate::image_budget::{BudgetError, BUDGET};

/// BGRA → 8-bit luma with the same integer weights as the C# build
/// (`29/150/77` on B/G/R, one rounding-free shift). Used for alignment only —
/// the stitched output stays in BGRA. The destination is sized through the
/// image budget, so an oversized or short frame is an error rather than a
/// panic or a silently empty plane.
pub fn bgra_to_gray(bgra: &[u8], width: usize, height: usize) -> Result<Vec<u8>, BudgetError> {
    let needed = BUDGET.checked_bytes(width as u64, height as u64, 4)?;
    if bgra.len() < needed {
        return Err(BudgetError::ShortBuffer {
            needed: needed as u64,
            available: bgra.len() as u64,
        });
    }
    let mut gray = BUDGET.alloc_zeroed(needed / 4)?;
    for (g, px) in gray.iter_mut().zip(bgra.as_chunks::<4>().0) {
        *g = ((px[0] as u32 * 29 + px[1] as u32 * 150 + px[2] as u32 * 77) >> 8) as u8;
    }
    Ok(gray)
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

/// Minimum luma standard deviation of the strip being matched. A flat band -- a
/// solid toolbar, a blank margin, a sticky footer of one colour -- matches at
/// every row with SAD 0, so "perfect similarity" on texture-free rows says
/// nothing about the offset. The rival-margin guard rejects an exact tie, but
/// not a strip whose pixel noise happens to rank one row above the rest; this
/// floor makes low-texture content an explicit refusal instead of a guess.
/// Real UI text measures 8-60; a solid fill measures 0.
const MIN_STRIP_TEXTURE: f64 = 4.0;

/// A frame that changed at least this fraction of its sampled pixels did move,
/// so a zero-row match on the edge strip means the band at that edge is fixed
/// content (sticky header/footer) rather than a still page.
const STATIC_EDGE_CHANGED: f64 = 0.02;

/// Score a static-band match has to clear on its own: the strip found itself,
/// essentially unchanged, exactly where it already was.
const STATIC_EDGE_SCORE: f64 = 0.97;

/// Independent-band agreement required by [`verify_shift`]. A correct offset
/// reproduces a band that took no part in the search almost exactly; a
/// periodic-content guess or a one-period slip does not (unless the page
/// really is periodic over its whole height, which no check can tell apart).
/// 0.85 leaves room for antialiasing, sub-pixel drift and a small dynamic
/// region (a video thumbnail, a blinking caret) inside the band.
const VERIFY_MIN_SCORE: f64 = 0.85;

/// How much worse the seam row pair may be than the frame's own row-to-row
/// variation. A stitch at the wrong offset pairs rows that belong far apart,
/// which is much rougher than the adjacent rows around them; a correct one is
/// indistinguishable from the interior. Relative, because text pages and
/// photo pages have very different absolute row differences.
const SEAM_MAX_RATIO: f64 = 1.9;

/// Absolute floor for the interior row difference used as the seam baseline,
/// in luma levels. Without it a flat page (baseline 0) would demand a seam
/// difference of 0 and reject every legitimate stitch.
const SEAM_BASELINE_FLOOR: f64 = 6.0;

/// Rows averaged on each side of the seam.
const SEAM_ROWS: usize = 3;

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
    let prev_row = if up {
        0
    } else {
        height.saturating_sub(strip_height.clamp(4, (height / 3).max(4)))
    };
    let Some(m) = search_strip(previous, current, width, height, strip_height, prev_row) else {
        return FrameAlignResult::rejected();
    };
    let accepted =
        accepts(m.score, m.runner_up_ratio, accept_score) && m.texture >= MIN_STRIP_TEXTURE;
    // Down: best_y is where the previous bottom strip starts in `current`; skip
    // the whole overlap, so new rows begin at best_y + strip (append those).
    // Up: best_y is where the previous top strip starts in `current`; the rows
    // ABOVE it (current[0..best_y)) are fresh (prepend those).
    let offset_y = if up {
        m.best_y
    } else {
        height.min(m.best_y + m.strip)
    };
    FrameAlignResult {
        offset_y,
        score: m.score,
        accepted,
        runner_up_ratio: m.runner_up_ratio,
    }
}

/// The result of one strip search: where the previous frame's strip was found
/// in `current`, how well two copies matched and how unique that match is.
/// Shared by both edge aligners and the interior retry, so all three use the
/// same coarse-to-fine sweep, the same early exit and the same score meaning.
struct StripMatch {
    best_y: usize,
    strip: usize,
    score: f64,
    runner_up_ratio: f64,
    /// Luma standard deviation of the matched strip in `previous`.
    texture: f64,
}

/// SAD search for `strip_height` rows of `previous` starting at `prev_row`,
/// scanned over every position of `current`. `None` for unusable inputs.
fn search_strip(
    previous: &[u8],
    current: &[u8],
    width: usize,
    height: usize,
    strip_height: usize,
    prev_row: usize,
) -> Option<StripMatch> {
    if width == 0
        || height == 0
        || previous.len() < width * height
        || current.len() < width * height
    {
        return None;
    }
    let strip = strip_height.clamp(4, (height / 3).max(4));
    if height <= strip || prev_row + strip > height {
        return None;
    }
    let search_max = height - strip;
    let prev_start = prev_row * width;

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

    Some(StripMatch {
        best_y,
        strip,
        score: 1.0 - best_sad as f64 / (strip * width * 255) as f64,
        runner_up_ratio: ratio(best_sad, runner_up),
        texture: luma_std(previous, width, prev_row, strip),
    })
}

/// Luma standard deviation of `rows` rows starting at `row0`.
fn luma_std(gray: &[u8], width: usize, row0: usize, rows: usize) -> f64 {
    let (Some(start), Some(end)) = (row0.checked_mul(width), (row0 + rows).checked_mul(width))
    else {
        return 0.0;
    };
    let Some(slice) = gray.get(start..end) else {
        return 0.0;
    };
    if slice.is_empty() {
        return 0.0;
    }
    let n = slice.len() as f64;
    let mean = slice.iter().map(|&v| v as u64).sum::<u64>() as f64 / n;
    let var = slice
        .iter()
        .map(|&v| {
            let d = v as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n;
    var.sqrt()
}

/// Outcome of matching an interior strip: how far the content moved and how
/// well it matched. Used as evidence that the page is scrolling behind a fixed
/// band, never as a stitch source.
struct InnerMatch {
    /// Rows the content moved by, i.e. the offset the edge would have found if
    /// the band had not been there.
    new_rows: usize,
    accepted: bool,
}

/// Interior-strip match for pages whose frame edge is occupied by a fixed
/// band (a sticky footer at the bottom for a downward scroll, a sticky header
/// at the top for an upward one). The same strip rows are taken from both
/// frames at `anchor_row`, so a match at `best_y` means the content itself
/// moved by `|anchor_row - best_y|` rows -- away from the edge, where the
/// static band cannot drown the signal.
///
/// Deliberately stricter than the edge path, because an interior strip sees
/// less of the page and the offset has to be inferred rather than read off the
/// overlap. A match is only reported when it is unambiguous, textured, lands
/// strictly inside the search range (a boundary hit means the shift is at
/// least `anchor_row`, i.e. unknowable), and moves at most half a frame.
fn align_inner(
    previous: &[u8],
    current: &[u8],
    width: usize,
    height: usize,
    strip_height: usize,
    anchor_row: usize,
    is_up: bool,
) -> Option<InnerMatch> {
    let strip = strip_height.clamp(4, (height / 3).max(4));
    let anchor = anchor_row.min(height.checked_sub(strip)?);
    let m = search_strip(previous, current, width, height, strip, anchor)?;
    let search_max = height - m.strip;
    let new_rows = if is_up {
        m.best_y.checked_sub(anchor)?
    } else {
        anchor.checked_sub(m.best_y)?
    };
    let accepted = new_rows > MIN_NEW_ROWS
        && new_rows <= height / 2
        && m.best_y > 0
        && m.best_y < search_max
        && accepts(m.score, m.runner_up_ratio, STRONG_SCORE)
        && m.texture >= MIN_STRIP_TEXTURE;
    Some(InnerMatch { new_rows, accepted })
}

/// Result of [`verify_shift`]: does an accepted offset survive a second,
/// independent look at the same two frames?
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeamCheck {
    /// Normalised agreement of a band that took no part in the search.
    pub overlap_score: f64,
    /// Normalised difference of the row pair straddling the seam.
    pub seam_score: f64,
    /// Seam difference over the frame's own adjacent-row difference.
    pub seam_ratio: f64,
    pub accepted: bool,
}

impl SeamCheck {
    fn rejected() -> Self {
        SeamCheck {
            overlap_score: 0.0,
            seam_score: 0.0,
            seam_ratio: f64::INFINITY,
            accepted: false,
        }
    }
}

/// Validates an accepted offset before it is stitched (P1-05).
///
/// The SAD score describes *the strip*, not the seam the user will see: a
/// repetitive page can satisfy the strip perfectly at a wrong offset. Two
/// independent checks catch that:
///
/// 1. a band at the middle of the overlap -- never the strip used for the
///    search -- must land on the same content at the implied shift;
/// 2. the rows straddling the seam must not differ much more than the frame's
///    own adjacent rows differ, i.e. the joint must look like page content.
///
/// `new_rows` is the number of rows that would be added, `is_up` the
/// direction. Any out-of-range request is a rejection, not a panic.
pub fn verify_shift(
    previous: &[u8],
    current: &[u8],
    width: usize,
    height: usize,
    band_rows: usize,
    new_rows: usize,
    is_up: bool,
) -> SeamCheck {
    if width == 0
        || height < 8
        || new_rows == 0
        || new_rows >= height
        || previous.len() < width * height
        || current.len() < width * height
    {
        return SeamCheck::rejected();
    }
    let band = band_rows
        .saturating_mul(2)
        .clamp(4, (height / 3).max(4))
        .min(height - new_rows);
    if band < 4 {
        return SeamCheck::rejected();
    }
    // The band lives in the overlap and nowhere near the searched strip:
    // down `previous[b + j] == current[b + j - new_rows]`, up
    // `previous[b + j] == current[b + j + new_rows]`.
    let b = if is_up {
        (height - band - new_rows) / 2
    } else {
        new_rows + (height - new_rows - band) / 2
    };
    // Per-row agreement, then a trimmed mean over the best 3/4 of the rows: a
    // small dynamic region (video, caret, animated banner) spoils a few rows
    // and must not fail the check, while a wrong offset spoils them all.
    let mut rows_scored: Vec<f64> = Vec::with_capacity(band);
    for j in 0..band {
        let p = (b + j) * width;
        let c = if is_up {
            (b + j + new_rows) * width
        } else {
            (b + j - new_rows) * width
        };
        let mut row_sad = 0u64;
        for i in 0..width {
            row_sad += previous[p + i].abs_diff(current[c + i]) as u64;
        }
        rows_scored.push(1.0 - row_sad as f64 / (width * 255) as f64);
    }
    rows_scored.sort_by(|a, b| b.total_cmp(a));
    let keep = (rows_scored.len() * 3 / 4).max(1);
    let overlap_score = rows_scored[..keep].iter().sum::<f64>() / keep as f64;

    // Seam rows: the last `rows` adjacent page-row pairs around the joint.
    // Down, page row (h-1-j) is `previous[height-1-j]` and page row
    // (h-j) is `current[height-j-new_rows]`; up both sides live in `current`,
    // at `new_rows-1-j` (page t0-1-j) and `new_rows-j` (page t0-j).
    let rows = SEAM_ROWS.min(height - new_rows).min(new_rows).max(1);
    let mut seam = 0u64;
    for j in 0..rows {
        let (a, c) = if is_up {
            ((new_rows - 1 - j) * width, (new_rows - j) * width)
        } else {
            ((height - 1 - j) * width, (height - new_rows - j) * width)
        };
        for i in 0..width {
            seam += previous[a + i].abs_diff(current[c + i]) as u64;
        }
    }
    let seam_diff = seam as f64 / (rows * width) as f64;
    let seam_score = 1.0 - seam_diff / 255.0;

    // Baseline: how different are two adjacent rows of this very frame?
    let mut base = 0u64;
    let mut pairs = 0u64;
    let probe = (height / 8).max(1);
    for k in 0..4 {
        let y = (probe * (k + 1)).min(height - 2);
        let a = y * width;
        let c = (y + 1) * width;
        for i in 0..width {
            base += current[a + i].abs_diff(current[c + i]) as u64;
        }
        pairs += 1;
    }
    let interior = base as f64 / (pairs.max(1) as f64 * width as f64);
    let seam_ratio = seam_diff / interior.max(SEAM_BASELINE_FLOOR);

    SeamCheck {
        overlap_score,
        seam_score,
        seam_ratio,
        accepted: overlap_score >= VERIFY_MIN_SCORE && seam_ratio <= SEAM_MAX_RATIO,
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
    /// The edge strip matched itself, unchanged, while the rest of the frame
    /// moved: a fixed header/footer band is covering the strip. When the
    /// interior retry cannot recover, the host tells the user to shrink the
    /// selection instead of waiting forever.
    pub static_edge: bool,
    /// The interior probe confirmed the content behind a stuck edge strip did
    /// move: the band at the frame edge is fixed header/footer, and the host
    /// asks the user to shrink the selection instead of waiting.
    pub inner_content_moved: bool,
    /// Rows the content moved behind that band (0 when the probe did not run
    /// or did not confirm motion). Diagnostic only.
    pub inner_new_rows: usize,
    /// Independent-band agreement of the accepted offset (see
    /// [`verify_shift`]); `0.0` when nothing was verified.
    pub overlap_score: f64,
    /// Seam row-pair similarity of the accepted offset.
    pub seam_score: f64,
    /// A candidate offset was refused by [`verify_shift`]: the strip matched,
    /// the rest of the frame disagreed.
    pub rejected_by_verification: bool,
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
///
/// Order of attempts (`P1-05`):
///
/// 1. the bottom strip (a downward scroll is the common case);
/// 2. the top strip (upward);
/// 3. every accepted offset is confirmed by [`verify_shift`] before it is
///    reported, so a repetitive page cannot slip a wrong offset through on the
///    strip score alone;
/// 4. when the edge strip matched itself and the frame changed anyway, an
///    interior strip at 3/4 / 1/4 height probes whether the *page* moved
///    behind the band. That is evidence for the host's message, not a stitch:
///    the rows such a stitch would add are the band itself.
///
/// A sticky-band stall is therefore reported, never "fixed" by guessing.
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

    // A strip that matches itself at the edge while the rest of the frame
    // moved is the sticky-band signature: the score is perfect and nothing new
    // arrives, forever. `MIN_NEW_ROWS` covers the 1-2 px shoulder of the same
    // match, not a different one.
    let down_static = down_rows <= MIN_NEW_ROWS
        && down.score >= STATIC_EDGE_SCORE
        && down.runner_up_ratio > STRONG_RATIO;

    if down.accepted && down_rows > MIN_NEW_ROWS {
        let seam = verify_shift(prev_gray, gray, width, height, strip, down_rows, false);
        report.overlap_score = seam.overlap_score;
        report.seam_score = seam.seam_score;
        if seam.accepted {
            report.direction = Some((false, down.offset_y, down_rows));
            return report;
        }
        report.rejected_by_verification = true;
    }

    let up = align_vertical_up(prev_gray, gray, width, height, strip, accept_score);
    report.up_tried = true;
    report.up_score = up.score;
    report.up_ratio = up.runner_up_ratio;
    let up_static = up.offset_y <= MIN_NEW_ROWS
        && up.score >= STATIC_EDGE_SCORE
        && up.runner_up_ratio > STRONG_RATIO;
    report.static_edge = down_static || up_static;

    if up.accepted && up.offset_y > MIN_NEW_ROWS {
        let seam = verify_shift(prev_gray, gray, width, height, strip, up.offset_y, true);
        report.overlap_score = seam.overlap_score;
        report.seam_score = seam.seam_score;
        if seam.accepted {
            report.direction = Some((true, up.offset_y, up.offset_y));
            return report;
        }
        report.rejected_by_verification = true;
    }

    report.changed = frame_change_ratio(prev_gray, gray);
    if report.changed < STATIC_EDGE_CHANGED {
        // Nothing moved: a probe would search the same pixels for no reason,
        // and no offset would be trustworthy anyway.
        return report;
    }

    // Static-band probe (P1-05). The edge strip is stuck, but did the *page*
    // move? An interior strip at 3/4 (down) or 1/4 (up) height answers that
    // without touching the band, which turns "something is wrong" into an
    // actionable "the fixed header/footer is covering the strip".
    //
    // The offset it finds is deliberately NOT stitched: the rows the stitch
    // would add are the band itself, so appending/prepending them would
    // duplicate the header/footer in the output. Detection and a message are
    // the correct fix; a guessed stitch is a silent mis-stitch.
    if report.static_edge {
        for (is_up, anchor) in [(false, height * 3 / 4), (true, height / 4)] {
            if let Some(m) = align_inner(prev_gray, gray, width, height, strip, anchor, is_up) {
                if m.accepted {
                    report.inner_content_moved = true;
                    report.inner_new_rows = m.new_rows;
                    break;
                }
            }
        }
    }
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

// ---- stall detection ------------------------------------------------------
//
// A fixed header/footer band occupies exactly the edge strip, so the SAD pass
// reports a perfect match with zero new rows on every tick: the bar keeps
// saying "keep scrolling" while nothing can ever be stitched. Polling forever
// also burns a WGC crop and a full-frame memcpy every 150 ms. The tracker
// turns consecutive no-progress ticks into a state the host can surface, and
// keeps a genuinely paused page alive at a reduced poll rate instead of
// killing a session the user has only briefly stopped reading.

/// Consecutive no-progress ticks before the host reports a stall. At the
/// 150 ms poll this is 1.5 s -- long enough for one slow scroll tick to be
/// absorbed, short enough that a stuck capture is visible.
pub const STALL_TICKS: u32 = 10;

/// What the last tick's progress state means for the session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StallPhase {
    /// Rows were stitched on this tick.
    Progress,
    /// Nothing stitched yet, but under the threshold: stay quiet.
    Watching,
    /// The page did not change for `STALL_TICKS` ticks: the user paused. Keep
    /// the session (and the bar) alive, but poll far less often.
    Paused,
    /// The frame kept changing but no offset could be confirmed: a static band
    /// ate the strip, or the content is unalignable. Stop capturing and say so.
    Stalled,
}

/// Per-session no-progress counter.
#[derive(Clone, Copy, Debug, Default)]
pub struct StallTracker {
    idle: u32,
    phase: Option<StallPhase>,
}

impl StallTracker {
    pub fn new() -> Self {
        StallTracker::default()
    }

    pub fn idle_ticks(&self) -> u32 {
        self.idle
    }

    /// `Paused`: the host may decimate its polling.
    pub fn paused(&self) -> bool {
        self.phase == Some(StallPhase::Paused)
    }

    /// Folds one tick's report into the counter. A stitched tick always
    /// resets it. So does a tick whose frame changed more than `fast_changed`
    /// *without* the static-band signature: the page is clearly scrolling,
    /// the matcher just cannot follow it, and the host keeps nudging "slow
    /// down" instead of ending the session. Everything else counts towards a
    /// stationary pause (frame unchanged) or a stall (frame changed but no
    /// offset confirmed) -- including a static band that moves fast enough to
    /// look like a fast scroll, because a band that never stitches never will.
    pub fn observe(&mut self, report: &ScrollReport, fast_changed: f64) -> StallPhase {
        let fast = report.changed > fast_changed && !report.static_edge;
        let phase = if report.direction.is_some() || fast {
            self.idle = 0;
            if report.direction.is_some() {
                StallPhase::Progress
            } else {
                StallPhase::Watching
            }
        } else {
            self.idle = self.idle.saturating_add(1);
            if self.idle < STALL_TICKS {
                StallPhase::Watching
            } else if report.changed >= STATIC_EDGE_CHANGED {
                StallPhase::Stalled
            } else {
                StallPhase::Paused
            }
        };
        self.phase = Some(phase);
        phase
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
/// edge, exactly what the bottom-strip matcher expects. The output is budget
/// checked and fallibly allocated; a short input is an error.
pub fn rotate_bgra90(
    bgra: &[u8],
    width: usize,
    height: usize,
) -> Result<(Vec<u8>, usize, usize), BudgetError> {
    let mut out = checked_frame(bgra, width, height)?;
    let (dw, dh) = (height, width);
    for y in 0..height {
        for x in 0..width {
            let s = (y * width + x) * 4;
            let d = (x * dw + (dw - 1 - y)) * 4;
            out[d..d + 4].copy_from_slice(&bgra[s..s + 4]);
        }
    }
    Ok((out, dw, dh))
}

/// Inverse of [`rotate_bgra90`] (90 degrees counter-clockwise).
pub fn rotate_bgra270(
    bgra: &[u8],
    width: usize,
    height: usize,
) -> Result<(Vec<u8>, usize, usize), BudgetError> {
    let mut out = checked_frame(bgra, width, height)?;
    let (dw, dh) = (height, width);
    for y in 0..height {
        for x in 0..width {
            let s = (y * width + x) * 4;
            let d = ((width - 1 - x) * dw + y) * 4;
            out[d..d + 4].copy_from_slice(&bgra[s..s + 4]);
        }
    }
    Ok((out, dw, dh))
}

/// Allocates a zeroed BGRA frame of `width * height` after checking both the
/// buffer length and the image budget.
fn checked_frame(bgra: &[u8], width: usize, height: usize) -> Result<Vec<u8>, BudgetError> {
    let needed = BUDGET.checked_bytes(width as u64, height as u64, 4)?;
    if bgra.len() < needed {
        return Err(BudgetError::ShortBuffer {
            needed: needed as u64,
            available: bgra.len() as u64,
        });
    }
    BUDGET.alloc_rgba(width as u64, height as u64)
}

/// Frame in the matcher's coordinate space (scroll axis = height). Vertical
/// frames are borrowed, so the vertical path pays no extra copy.
pub fn normalize_bgra<'a>(
    frame: &'a [u8],
    width: usize,
    height: usize,
    axis: Axis,
) -> Result<(Cow<'a, [u8]>, usize, usize), BudgetError> {
    match axis {
        Axis::Vertical => Ok((Cow::Borrowed(frame), width, height)),
        Axis::Horizontal => {
            let (px, w, h) = rotate_bgra90(frame, width, height)?;
            Ok((Cow::Owned(px), w, h))
        }
    }
}

/// Inverse of [`normalize_bgra`]: the stitched image back in screen space.
pub fn denormalize_bgra<'a>(
    frame: &'a [u8],
    width: usize,
    height: usize,
    axis: Axis,
) -> Result<(Cow<'a, [u8]>, usize, usize), BudgetError> {
    match axis {
        Axis::Vertical => Ok((Cow::Borrowed(frame), width, height)),
        Axis::Horizontal => {
            let (px, w, h) = rotate_bgra270(frame, width, height)?;
            Ok((Cow::Owned(px), w, h))
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

/// Why a stitcher refused an operation. Every variant is returned, never
/// swallowed: the long-capture bar maps them to a status, the log gets the
/// full message.
#[derive(Debug)]
pub enum StitchError {
    /// Zero dimensions, zero new rows, or more rows than the frame carries.
    InvalidFrame {
        width: usize,
        height: usize,
        rows: usize,
    },
    /// The frame does not have the width the stitcher was built for.
    WidthMismatch { expected: usize, got: usize },
    /// The frame buffer is smaller than the size it claims.
    ShortFrame { needed: u64, available: u64 },
    /// Adding the segment would pass the long-capture height cap.
    HeightCap {
        total: usize,
        extra: usize,
        max: usize,
    },
    /// The image budget refused the side, pixel or byte count.
    Budget(BudgetError),
}

impl std::fmt::Display for StitchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StitchError::InvalidFrame {
                width,
                height,
                rows,
            } => write!(
                f,
                "long-capture frame {width}x{height} ({rows} new rows) is not usable"
            ),
            StitchError::WidthMismatch { expected, got } => write!(
                f,
                "frame width {got} px does not match the stitcher width {expected} px"
            ),
            StitchError::ShortFrame { needed, available } => write!(
                f,
                "frame buffer holds {available} bytes but {needed} are required"
            ),
            StitchError::HeightCap { total, extra, max } => write!(
                f,
                "adding {extra} rows to {total} would exceed the {max} row long-capture limit"
            ),
            StitchError::Budget(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for StitchError {}

impl From<BudgetError> for StitchError {
    fn from(e: BudgetError) -> Self {
        StitchError::Budget(e)
    }
}

/// How much room a headroom compaction asks for, as a multiple of what it
/// already has. Each compaction copies the image once, so growth by doubling
/// makes N prepends cost O(N + total) copied bytes instead of O(N * total).
const HEAD_GROWTH: usize = 2;

/// Copy/allocation counters for [`Stitcher`]. `copied_bytes` counts only the
/// bytes this type moves itself (headroom compaction and the final de-head);
/// allocator-internal moves are invisible here. Used by the P1-02 regression
/// test and the tick log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StitchStats {
    pub copied_bytes: u64,
    pub compactions: u64,
    pub allocations: u64,
}

/// Growing BGRA pixel buffer for the stitched image. `LongImageBuffer`
/// mirrors the C# height accounting; this type owns the pixels that the
/// accounting describes, so a rejected append leaves both untouched.
///
/// Upward stitches write into a *headroom* in front of the image instead of
/// rebuilding the buffer: `pixels` is `head_rows + total_height` rows long,
/// the live image starts at row `head_rows`, and a prepend only moves the
/// start back. When the headroom runs out it is re-granted geometrically (one
/// copy for many prepends), which is what keeps a long upward session linear
/// instead of quadratic. `into_pixels` removes the headroom in one copy so
/// the export stays a single contiguous buffer.
pub struct Stitcher {
    width: usize,
    max_height: usize,
    heights: Vec<usize>,
    /// `len == (head_rows + total_height) * width * 4`, see the type docs.
    pixels: Vec<u8>,
    /// Unused rows in front of the live image.
    head_rows: usize,
    /// Size in rows of the last headroom grant, so each re-grant doubles the
    /// previous one instead of restarting at a frame's height -- that is what
    /// turns "a copy every N ticks" into "a copy at every doubling".
    head_grant: usize,
    stats: StitchStats,
}

impl Stitcher {
    /// The first frame is appended whole; exceeding the budget up front means
    /// the region is taller than a D3D texture can hold, so there is nothing
    /// to stitch into. The canvas is budget checked and fallibly allocated.
    pub fn new(
        width: usize,
        first_frame_height: usize,
        max_height: usize,
    ) -> Result<Self, StitchError> {
        let max_height = max_height.max(1);
        if width == 0 || first_frame_height == 0 {
            return Err(StitchError::InvalidFrame {
                width,
                height: first_frame_height,
                rows: first_frame_height,
            });
        }
        if first_frame_height > max_height {
            return Err(StitchError::HeightCap {
                total: 0,
                extra: first_frame_height,
                max: max_height,
            });
        }
        let pixels = BUDGET.alloc_rgba(width as u64, first_frame_height as u64)?;
        let mut heights = Vec::new();
        heights.try_reserve_exact(1).map_err(|_| {
            StitchError::Budget(BudgetError::AllocFailed {
                bytes: std::mem::size_of::<usize>() as u64,
            })
        })?;
        heights.push(first_frame_height);
        Ok(Stitcher {
            width,
            max_height,
            heights,
            pixels,
            head_rows: 0,
            head_grant: 0,
            stats: StitchStats {
                copied_bytes: 0,
                compactions: 0,
                allocations: 1,
            },
        })
    }

    /// Writes the first frame's pixels. `new` only reserves and accounts for
    /// the first segment because the caller sizes the buffer before the first
    /// capture lands; the segment count is unchanged.
    pub fn fill_first(&mut self, frame_bgra: &[u8]) -> bool {
        let Some(&rows) = self.heights.first() else {
            return false;
        };
        let Some(bytes) = rows.checked_mul(self.width).and_then(|v| v.checked_mul(4)) else {
            return false;
        };
        if frame_bgra.len() < bytes {
            return false;
        }
        let start = self.head_bytes();
        self.pixels[start..start + bytes].copy_from_slice(&frame_bgra[..bytes]);
        true
    }
    /// Appends the rows `[offset_y..frame_height)` of `frame_bgra`. Rows are
    /// packed (`frame_width * 4` bytes); a width mismatch, an over-budget
    /// append or a failed allocation leaves the buffer unchanged.
    pub fn append(
        &mut self,
        frame_bgra: &[u8],
        frame_width: usize,
        frame_height: usize,
        offset_y: usize,
    ) -> Result<(), StitchError> {
        let (row_bytes, needed) = self.frame_geometry(frame_width, frame_height)?;
        if (frame_bgra.len() as u64) < needed {
            return Err(StitchError::ShortFrame {
                needed,
                available: frame_bgra.len() as u64,
            });
        }
        let new_rows = frame_height.saturating_sub(offset_y);
        if new_rows == 0 {
            return Err(StitchError::InvalidFrame {
                width: frame_width,
                height: frame_height,
                rows: new_rows,
            });
        }
        let total = self.total_height();
        let new_total = total.checked_add(new_rows).ok_or(StitchError::HeightCap {
            total,
            extra: new_rows,
            max: self.max_height,
        })?;
        if new_total > self.max_height {
            return Err(StitchError::HeightCap {
                total,
                extra: new_rows,
                max: self.max_height,
            });
        }
        // Whole-canvas budget check: catches the side, pixel and byte limits
        // before touching the buffer.
        BUDGET.checked_bytes(self.width as u64, new_total as u64, 4)?;
        // The headroom is spare capacity, not image: hold the physical buffer
        // to the same byte budget, and give the headroom back (one copy) when
        // it is the thing that would push the allocation over.
        let extra = new_rows * row_bytes;
        let physical = self.pixels.len().saturating_add(extra);
        if BUDGET.checked_len(physical as u64).is_err() {
            self.drop_headroom();
        }
        BUDGET.try_grow(&mut self.pixels, extra)?;
        self.heights.try_reserve(1).map_err(|_| {
            StitchError::Budget(BudgetError::AllocFailed {
                bytes: std::mem::size_of::<usize>() as u64,
            })
        })?;
        self.pixels
            .extend_from_slice(&frame_bgra[offset_y * row_bytes..frame_height * row_bytes]);
        self.heights.push(new_rows);
        Ok(())
    }

    /// Prepends the rows `[0..new_rows)` of `frame_bgra` at the TOP of the
    /// stitched image. Used when the user scrolls up: fresh content enters from
    /// the top, so it must be inserted in front while every already-stitched
    /// row keeps its bytes and relative order.
    ///
    /// The rows are written into the headroom in front of the image, so the
    /// usual tick copies nothing at all; only a re-grant of the headroom
    /// (geometric, so it becomes rare) copies the image once. `heights` gains a
    /// front segment so `total_height` stays consistent, and `into_pixels`
    /// folds the headroom away once at the end.
    /// A width mismatch, zero/oversized `new_rows`, an over-budget prepend or
    /// a failed allocation leaves the whole buffer untouched.
    pub fn prepend(
        &mut self,
        frame_bgra: &[u8],
        frame_width: usize,
        frame_height: usize,
        new_rows: usize,
    ) -> Result<(), StitchError> {
        let (row_bytes, needed) = self.frame_geometry(frame_width, frame_height)?;
        if (frame_bgra.len() as u64) < needed {
            return Err(StitchError::ShortFrame {
                needed,
                available: frame_bgra.len() as u64,
            });
        }
        if new_rows == 0 || new_rows > frame_height {
            return Err(StitchError::InvalidFrame {
                width: frame_width,
                height: frame_height,
                rows: new_rows,
            });
        }
        let total = self.total_height();
        let new_total = total.checked_add(new_rows).ok_or(StitchError::HeightCap {
            total,
            extra: new_rows,
            max: self.max_height,
        })?;
        if new_total > self.max_height {
            return Err(StitchError::HeightCap {
                total,
                extra: new_rows,
                max: self.max_height,
            });
        }
        // Same whole-canvas check as `append`, so the caps mean the same thing
        // in both directions.
        BUDGET.checked_bytes(self.width as u64, new_total as u64, 4)?;
        let bytes = new_rows * row_bytes;

        if self.head_rows >= new_rows {
            // Fast path: the rows land in space that is already allocated, so
            // nothing is moved and the pixel pointer does not change.
            self.head_rows -= new_rows;
            let start = self.head_rows * row_bytes;
            self.pixels[start..start + bytes].copy_from_slice(&frame_bgra[..bytes]);
        } else {
            // Grant a new headroom and slide the image back into it: one copy
            // per geometric growth instead of one copy per tick.
            let head = self.headroom_target(new_rows, new_total);
            let physical =
                head.saturating_add(total)
                    .checked_mul(row_bytes)
                    .ok_or(StitchError::Budget(BudgetError::AllocFailed {
                        bytes: u64::MAX,
                    }))?;
            let mut pixels = Vec::new();
            pixels.try_reserve_exact(physical).map_err(|_| {
                StitchError::Budget(BudgetError::AllocFailed {
                    bytes: physical as u64,
                })
            })?;
            let pad = (head - new_rows) * row_bytes;
            // The front of the headroom stays zeroed; only the padding is
            // written here, never the image.
            pixels.resize(pad, 0);
            pixels.extend_from_slice(&frame_bgra[..bytes]);
            pixels.extend_from_slice(self.image_bytes());
            self.stats.copied_bytes += (pad + total * row_bytes) as u64;
            self.stats.compactions += 1;
            self.stats.allocations += 1;
            self.pixels = pixels;
            self.head_rows = head - new_rows;
            self.head_grant = head;
        }

        self.heights.try_reserve(1).map_err(|_| {
            StitchError::Budget(BudgetError::AllocFailed {
                bytes: std::mem::size_of::<usize>() as u64,
            })
        })?;
        self.heights.insert(0, new_rows);
        Ok(())
    }

    /// Bytes of unused space in front of the image.
    fn head_bytes(&self) -> usize {
        self.head_rows * self.width * 4
    }

    /// The live image, without the headroom.
    fn image_bytes(&self) -> &[u8] {
        &self.pixels[self.head_bytes()..]
    }

    /// Moves the image to offset 0 and gives the headroom back. Used when the
    /// headroom would push the physical buffer past the byte budget, and by
    /// `into_pixels`.
    fn drop_headroom(&mut self) {
        let head = self.head_bytes();
        if head == 0 {
            return;
        }
        self.pixels.copy_within(head.., 0);
        self.pixels.truncate(self.pixels.len() - head);
        self.stats.copied_bytes += self.pixels.len() as u64;
        self.stats.compactions += 1;
        self.head_rows = 0;
        self.head_grant = 0;
    }

    /// Rows of headroom to grant for `need` new rows at `total_after` height:
    /// at least `need`, at least a frame's worth, doubling what is already
    /// there, and never so much that the physical buffer would pass the byte
    /// budget (which is what keeps the spare room honest).
    fn headroom_target(&self, need: usize, total_after: usize) -> usize {
        let chunk = self.heights.first().copied().unwrap_or(need);
        let wanted = self
            .head_grant
            .max(chunk)
            .saturating_mul(HEAD_GROWTH)
            .max(need);
        let row_bytes = (self.width * 4).max(1) as u64;
        let max_rows = (BUDGET.max_bytes / row_bytes) as usize;
        wanted.min(max_rows.saturating_sub(total_after).max(need))
    }

    /// Copy/allocation counters; see [`StitchStats`].
    pub fn stats(&self) -> StitchStats {
        self.stats
    }

    /// Validates that `frame_width` matches and returns `(row_bytes, needed)`.
    fn frame_geometry(
        &self,
        frame_width: usize,
        frame_height: usize,
    ) -> Result<(usize, u64), StitchError> {
        if frame_width != self.width {
            return Err(StitchError::WidthMismatch {
                expected: self.width,
                got: frame_width,
            });
        }
        let overflow = StitchError::Budget(BudgetError::Overflow {
            width: self.width as u64,
            height: frame_height as u64,
            bytes_per_pixel: 4,
        });
        let needed = (frame_width as u64)
            .checked_mul(4)
            .and_then(|row| row.checked_mul(frame_height as u64))
            .ok_or(overflow)?;
        let row_bytes =
            frame_width
                .checked_mul(4)
                .ok_or(StitchError::Budget(BudgetError::Overflow {
                    width: frame_width as u64,
                    height: frame_height as u64,
                    bytes_per_pixel: 4,
                }))?;
        Ok((row_bytes, needed))
    }

    pub fn total_height(&self) -> usize {
        self.heights.iter().sum()
    }

    pub fn segment_count(&self) -> usize {
        self.heights.len()
    }

    /// Hands back (bgra, width, height); stride is always `width * 4`. The
    /// headroom is folded away here, so the export is one contiguous image and
    /// the caller never sees the spare rows.
    pub fn into_pixels(mut self) -> (Vec<u8>, u32, u32) {
        let height = self.total_height() as u32;
        self.drop_headroom();
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
        assert_eq!(bgra_to_gray(&[0, 0, 255, 255], 1, 1).unwrap(), vec![76]);
        assert_eq!(bgra_to_gray(&[0, 255, 0, 255], 1, 1).unwrap(), vec![149]);
        assert_eq!(bgra_to_gray(&[255, 0, 0, 255], 1, 1).unwrap(), vec![28]);
        assert_eq!(
            bgra_to_gray(&[255, 255, 255, 255], 1, 1).unwrap(),
            vec![255]
        );
        // Alpha is ignored.
        assert_eq!(bgra_to_gray(&[255, 255, 255, 0], 1, 1).unwrap(), vec![255]);
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
        assert!(s.append(&second, width, 4, 3).is_ok());
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
        assert!(s.append(&three_rows, width, 3, 0).is_err());
        assert_eq!(s.total_height(), 2);
        assert_eq!(s.segment_count(), 1);
        // Wrong width must not touch state either.
        assert!(s.append(&frame, 3, 2, 0).is_err());
        assert_eq!(s.total_height(), 2);
        // First frame taller than the budget yields no stitcher at all.
        assert!(Stitcher::new(width, 5, 4).is_err());
    }

    #[test]
    fn stitcher_refuses_an_over_budget_canvas_without_state_change() {
        // A width past the shared 16384 px budget fails at construction: the
        // error carries the requested side and the limit.
        let err = Stitcher::new(crate::image_budget::MAX_SIDE as usize + 1, 1, 16_384)
            .err()
            .expect("over-wide canvas must be refused");
        assert!(
            matches!(err, StitchError::Budget(BudgetError::TooWide { .. })),
            "{err}"
        );
        assert!(err.to_string().contains("16385"), "{err}");

        // An append that would pass the row cap must leave the accounting and
        // the pixels untouched: a later legal append still lands at row 2.
        let mut s = Stitcher::new(2, 2, 4).unwrap();
        let three = bgra_frame(&[&[7u8; 4][..]; 3], 2);
        assert!(s.append(&three, 2, 3, 0).is_err());
        assert_eq!(s.total_height(), 2);
        assert_eq!(s.segment_count(), 1);
        let row = bgra_frame(&[&[9u8; 4][..]; 1], 2);
        assert!(s.append(&row, 2, 1, 0).is_ok());
        assert_eq!(s.total_height(), 3);
        let (out, w, h) = s.into_pixels();
        assert_eq!((w, h), (2, 3));
        assert_eq!(&out[..16], &[0u8; 16], "first segment untouched");
    }

    #[test]
    fn gray_plane_is_budget_checked() {
        assert!(matches!(
            bgra_to_gray(&[0u8; 4], 2, 2),
            Err(BudgetError::ShortBuffer { .. })
        ));
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
            bgra_to_gray(&self.frame(top), self.width, self.height).unwrap()
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
        assert!(s.prepend(&frame, width, 3, 1).is_ok());
        assert_eq!(s.total_height(), 3);
        assert_eq!(s.segment_count(), 2);
        let (out, ow, oh) = s.into_pixels();
        assert_eq!((ow, oh), (2, 3));
        assert_eq!(out, bgra_frame(&[fresh, a, b], width));

        // Rejections leave the buffer completely untouched.
        let mut s = Stitcher::new(width, 2, 16).unwrap();
        assert!(s.fill_first(&first));
        assert!(s.prepend(&frame, width, 3, 0).is_err());
        assert!(s.prepend(&frame, width, 3, 4).is_err());
        assert!(s.prepend(&frame, 3, 3, 1).is_err());
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
                    assert!(s.prepend(&px, width, height, offset).is_ok());
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
                    assert!(s.append(&px, width, height, offset).is_ok());
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
                    assert!(s.prepend(&px, width, height, offset).is_ok());
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
        let (rot, rw, rh) = rotate_bgra90(&frame, width, height).unwrap();
        assert_eq!((rw, rh), (height, width));
        let (back, bw, bh) = rotate_bgra270(&rot, rw, rh).unwrap();
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
        horizontal_walk_n(dir_right, 4)
    }

    /// [`horizontal_walk`] with an explicit step count, so a long upward run
    /// can be checked against the headroom growth of the stitcher.
    fn horizontal_walk_n(
        dir_right: bool,
        m: usize,
    ) -> (Vec<u8>, usize, usize, usize, usize, usize) {
        let page_w = 400;
        let vh = 32;
        let vw = 64;
        let k = 11;
        let page = wide_page(page_w, vh);
        let start = if dir_right { 40 } else { 200 };
        let mut stitcher: Option<Stitcher> = None;
        let mut prev_gray = Vec::new();
        let mut left = start;
        for i in 0..=m {
            let frame = columns(&page, page_w, vh, left, vw);
            let (norm, nw, nh) = normalize_bgra(&frame, vw, vh, Axis::Horizontal).unwrap();
            if i == 0 {
                let mut s = Stitcher::new(nw, nh, 16384).unwrap();
                assert!(s.fill_first(&norm));
                stitcher = Some(s);
                prev_gray = bgra_to_gray(&norm, nw, nh).unwrap();
            } else {
                let gray = bgra_to_gray(&norm, nw, nh).unwrap();
                let strip = (nh / 4).min(32);
                let report = classify_scroll_diag(&prev_gray, &gray, nw, nh, strip, ACCEPT_SCORE);
                let s = stitcher.as_mut().unwrap();
                match report.direction {
                    Some((is_up, offset, new_rows)) if is_up == !dir_right => {
                        assert_eq!(new_rows, k, "full column step must be new");
                        if is_up {
                            assert!(s.prepend(&norm, nw, nh, offset).is_ok());
                        } else {
                            assert!(s.append(&norm, nw, nh, offset).is_ok());
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
        let (back, bw, bh) =
            denormalize_bgra(&px, w as usize, h as usize, Axis::Horizontal).unwrap();
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

    // ---- P1-02: headroom instead of a full copy per upward tick ------------

    /// `rows` rows of BGRA tagged with `first_tag + row`, so any reordered,
    /// dropped or duplicated row shows up in a byte comparison.
    fn tagged_frame(width: usize, first_tag: u8, rows: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * rows * 4);
        for r in 0..rows {
            let tag = first_tag.wrapping_add(r as u8);
            for _ in 0..width {
                out.extend_from_slice(&[tag, tag, tag, 255]);
            }
        }
        out
    }

    /// The point of P1-02: N upward ticks must not copy the whole image N
    /// times. The old `prepend` rebuilt the buffer every tick, so the bytes it
    /// moved grew with N^2; the headroom version only copies when it re-grants
    /// space, and re-grants double, so the total stays O(image).
    #[test]
    fn prepend_into_headroom_is_pixel_exact_and_copies_linearly() {
        let width = 8;
        let height = 32;
        let row_bytes = width * 4;
        let mut s = Stitcher::new(width, height, 16_384).unwrap();
        let first = tagged_frame(width, 40, height);
        assert!(s.fill_first(&first));

        // Model: the same prepends applied to a plain Vec, row block by block.
        let mut model = first.clone();
        let (n, new_rows) = (200usize, 2usize);
        for i in 0..n {
            let frame = tagged_frame(width, 10u8.wrapping_add((i * 2) as u8), new_rows);
            assert!(s.prepend(&frame, width, new_rows, new_rows).is_ok());
            let mut next = frame.clone();
            next.extend_from_slice(&model);
            model = next;
        }

        let stats = s.stats();
        let total = s.total_height();
        let (out, ow, oh) = s.into_pixels();
        assert_eq!((ow as usize, oh as usize), (width, height + n * new_rows));
        assert_eq!(out, model, "headroom must not change a single byte");

        // Before: every tick allocated a new buffer and copied the whole image,
        // so this session moved sum(32 + 2i, i in 0..200) rows.
        let before_rows: usize = (0..n).map(|i| height + new_rows * i).sum();
        let after_rows = stats.copied_bytes as usize / row_bytes;
        eprintln!(
            "P1-02 prepend x{n}: copied {after_rows} rows in {} compactions \
             ({} allocations) vs {before_rows} rows before",
            stats.compactions, stats.allocations
        );
        assert_eq!(
            stats.compactions,
            stats.allocations - 1,
            "one alloc per re-grant"
        );
        assert!(
            after_rows <= 5 * total,
            "copied {after_rows} rows for a {total} row image"
        );
        assert!(
            after_rows * 20 < before_rows,
            "after {after_rows} rows vs before {before_rows} rows"
        );
    }

    #[test]
    fn prepend_keeps_the_pixel_pointer_while_headroom_lasts() {
        let width = 4;
        let height = 8;
        let mut s = Stitcher::new(width, height, 4096).unwrap();
        assert!(s.fill_first(&tagged_frame(width, 0, height)));
        let row = tagged_frame(width, 200, 1);
        // The first prepend grants headroom and moves the image once.
        assert!(s.prepend(&row, width, 1, 1).is_ok());
        let ptr = s.pixels.as_ptr();
        let copies = s.stats().copied_bytes;
        for i in 0..10u8 {
            let row = tagged_frame(width, 100 + i, 1);
            assert!(s.prepend(&row, width, 1, 1).is_ok());
        }
        assert_eq!(s.pixels.as_ptr(), ptr, "no reallocation inside headroom");
        assert_eq!(s.stats().copied_bytes, copies, "no copy inside headroom");
        assert_eq!(s.stats().compactions, 1);
        assert_eq!(s.total_height(), height + 11);
    }

    #[test]
    fn prepend_then_append_keeps_image_order() {
        // Interleaved directions share one canvas: the reference model is the
        // simplest possible statement of where every row belongs.
        let width = 4;
        let height = 6;
        let mut s = Stitcher::new(width, height, 4096).unwrap();
        let first = tagged_frame(width, 50, height);
        assert!(s.fill_first(&first));
        let mut model = first.clone();
        for i in 0..6u8 {
            let up = tagged_frame(width, 100 + i, 2);
            assert!(s.prepend(&up, width, 2, 2).is_ok());
            let mut next = up;
            next.extend_from_slice(&model);
            model = next;

            let down = tagged_frame(width, 150 + i, 3);
            assert!(s.append(&down, width, 3, 0).is_ok());
            model.extend_from_slice(&down);
        }
        let (out, _, oh) = s.into_pixels();
        assert_eq!(oh as usize, height + 6 * 5);
        assert_eq!(out, model);
    }

    // ---- P1-05: static bands, stall detection and seam validation ---------

    /// A fixed band of `rows` rows: a sticky footer/header, identical in every
    /// frame. Random rows so the band's own content is unique at offset 0.
    fn fixed_band(width: usize, rows: usize, seed: u32) -> Vec<u8> {
        let rows = lcg_rows(rows, width, seed);
        let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
        bgra_from_gray(&gray_frame(&refs, width))
    }

    /// One viewport of a page with `band` pinned to the top or the bottom, and
    /// the page rows starting at `top` behind it.
    fn sticky_frame(page: &Page, band: &[u8], top: usize, at_top: bool) -> Vec<u8> {
        let rb = page.width * 4;
        let band_rows = band.len() / rb;
        let body = page.height - band_rows;
        let mut out = Vec::with_capacity(page.height * rb);
        if at_top {
            out.extend_from_slice(band);
        }
        out.extend_from_slice(&page.bgra[top * rb..(top + body) * rb]);
        if !at_top {
            out.extend_from_slice(band);
        }
        out
    }

    fn gray_of(bgra: &[u8], width: usize, height: usize) -> Vec<u8> {
        bgra_to_gray(bgra, width, height).unwrap()
    }

    #[test]
    fn blank_frames_report_a_pause_instead_of_stitching() {
        let (width, height, strip) = (16, 64, 16);
        let frame = vec![255u8; width * height];
        let mut s = Stitcher::new(width, height, 4096).unwrap();
        assert!(s.fill_first(&bgra_from_gray(&frame)));
        let mut stall = StallTracker::new();
        for _ in 0..6 {
            let r = classify_scroll_diag(&frame, &frame, width, height, strip, ACCEPT_SCORE);
            assert_eq!(r.direction, None, "blank page cannot align: {r:?}");
            assert!(r.changed < STATIC_EDGE_CHANGED);
            stall.observe(&r, 0.45);
        }
        assert_eq!(s.total_height(), height, "nothing may be appended");
        assert_eq!(s.segment_count(), 1);
        // A still page is a pause, not a stall: after the threshold the host
        // slows its polling down but keeps the session alive.
        let r = classify_scroll_diag(&frame, &frame, width, height, strip, ACCEPT_SCORE);
        let mut phase = StallPhase::Watching;
        for _ in 0..STALL_TICKS {
            phase = stall.observe(&r, 0.45);
        }
        assert_eq!(phase, StallPhase::Paused);
        assert!(stall.paused());
    }

    /// Rows with a 16-row period plus a 4-pixel unique suffix: a table whose
    /// rows repeat, but not so exactly that no offset is knowable.
    fn near_periodic_rows(count: usize, width: usize) -> Vec<Vec<u8>> {
        let mut state = 0x5EED_1234u32;
        let base: Vec<Vec<u8>> = (0..16)
            .map(|_| {
                (0..width)
                    .map(|_| {
                        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                        (state >> 24) as u8
                    })
                    .collect()
            })
            .collect();
        (0..count)
            .map(|i| {
                let mut row = base[i % 16].clone();
                for cell in row.iter_mut().skip(width - 4) {
                    *cell = (i * 37) as u8;
                }
                row
            })
            .collect()
    }

    #[test]
    fn repeated_table_rows_never_produce_a_wrong_offset() {
        let (width, height, strip, k) = (32, 96, 32, 8);
        let rows = near_periodic_rows(8 + height + 8 * k + 16, width);
        let frame_at = |top: usize| {
            let refs: Vec<&[u8]> = rows[top..top + height].iter().map(Vec::as_slice).collect();
            gray_frame(&refs, width)
        };
        let mut prev_gray = frame_at(8);
        let mut s = Stitcher::new(width, height, 4096).unwrap();
        assert!(s.fill_first(&bgra_from_gray(&prev_gray)));
        let mut top = 8;
        for _ in 0..8 {
            top += k;
            let gray = frame_at(top);
            let r = classify_scroll_diag(&prev_gray, &gray, width, height, strip, ACCEPT_SCORE);
            match r.direction {
                Some((false, offset, new_rows)) => {
                    assert_eq!(new_rows, k, "the row number must pin the offset");
                    assert_eq!(offset, height - k);
                    assert!(s
                        .append(&bgra_from_gray(&gray), width, height, offset)
                        .is_ok());
                }
                other => panic!("near-periodic down step rejected: {other:?}"),
            }
            prev_gray = gray;
        }
        let (out, _, oh) = s.into_pixels();
        assert_eq!(oh as usize, height + 8 * k);
        // No duplicated or dropped rows: the image is exactly the page slice.
        let expected: Vec<u8> = rows[8..8 + height + 8 * k]
            .iter()
            .flat_map(|r| r.iter().copied())
            .collect();
        assert_eq!(out, bgra_from_gray(&expected));
    }

    #[test]
    fn exactly_periodic_rows_are_refused_rather_than_guessed() {
        // Period 16 under a 32-row strip: the strip matches itself two ways at
        // once, so the offset is a coin flip and must not be stitched.
        let (width, height, strip, k) = (16, 96, 32, 8);
        let mut state = 0xB16B_00B5u32;
        let base: Vec<Vec<u8>> = (0..16)
            .map(|_| {
                (0..width)
                    .map(|_| {
                        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                        (state >> 24) as u8
                    })
                    .collect()
            })
            .collect();
        let rows: Vec<Vec<u8>> = (0..height + 32).map(|i| base[i % 16].clone()).collect();
        let frame_at = |top: usize| {
            let refs: Vec<&[u8]> = rows[top..top + height].iter().map(Vec::as_slice).collect();
            gray_frame(&refs, width)
        };
        let prev = frame_at(0);
        let curr = frame_at(k);
        let r = classify_scroll_diag(&prev, &curr, width, height, strip, ACCEPT_SCORE);
        assert_eq!(r.direction, None, "ambiguous periodic rows: {r:?}");
    }

    #[test]
    fn sticky_footer_is_reported_as_a_static_edge_not_stitched() {
        let (width, height, strip) = (32, 480, 32);
        let page = Page::new(600, width, height, 0xFEED_0001);
        let band = fixed_band(width, 48, 0xBEEF);
        // Content scrolls down 8 px per tick, the footer never moves.
        let prev = sticky_frame(&page, &band, 100, false);
        let curr = sticky_frame(&page, &band, 108, false);
        let (prev_g, curr_g) = (gray_of(&prev, width, height), gray_of(&curr, width, height));

        // The edge strip matched itself perfectly -- the stall the audit found.
        let down = align_vertical(&prev_g, &curr_g, width, height, strip, ACCEPT_SCORE);
        assert!(down.accepted && (height - down.offset_y) <= MIN_NEW_ROWS);

        let r = classify_scroll_diag(&prev_g, &curr_g, width, height, strip, ACCEPT_SCORE);
        assert_eq!(r.direction, None, "the band must not be stitched: {r:?}");
        assert!(r.static_edge, "static edge signature: {r:?}");
        assert!(r.inner_content_moved, "the page did move behind the band");
        assert_eq!(r.inner_new_rows, 8);

        // 完成 is never automatic: the session stops only after the threshold,
        // with the reason visible, and no rows were added meanwhile.
        let mut stall = StallTracker::new();
        let mut phase = StallPhase::Watching;
        for _ in 0..STALL_TICKS {
            phase = stall.observe(&r, 0.45);
        }
        assert_eq!(phase, StallPhase::Stalled);
    }

    #[test]
    fn sticky_header_up_scroll_is_reported_as_a_static_edge() {
        let (width, height, strip) = (32, 480, 32);
        let page = Page::new(600, width, height, 0xFEED_0002);
        let band = fixed_band(width, 48, 0xBEEF);
        // Scrolling up: the page content moves down, the header stays put.
        let prev = sticky_frame(&page, &band, 100, true);
        let curr = sticky_frame(&page, &band, 92, true);
        let (prev_g, curr_g) = (gray_of(&prev, width, height), gray_of(&curr, width, height));
        let up = align_vertical_up(&prev_g, &curr_g, width, height, strip, ACCEPT_SCORE);
        assert!(up.accepted && up.offset_y <= MIN_NEW_ROWS);

        let r = classify_scroll_diag(&prev_g, &curr_g, width, height, strip, ACCEPT_SCORE);
        assert_eq!(
            r.direction, None,
            "the header band must not be stitched: {r:?}"
        );
        assert!(r.static_edge, "{r:?}");
        assert!(r.inner_content_moved, "{r:?}");
        assert_eq!(r.inner_new_rows, 8);
    }

    #[test]
    fn small_step_scroll_accumulates_without_dropping_or_duplicating() {
        // 1 px steps: the first two ticks are below MIN_NEW_ROWS on their own,
        // but `prev_gray` only advances on an accepted stitch, so the third
        // tick sees 3 px and lands exactly.
        let (width, height, strip) = (16, 48, 12);
        let page = Page::new(80, width, height, 0x51A7_0003);
        let mut s = Stitcher::new(width, height, 4096).unwrap();
        assert!(s.fill_first(&page.frame(8)));
        let mut prev_gray = page.gray(8);
        let mut accepted = 0;
        for step in 1..=3 {
            let top = 8 + step;
            let gray = page.gray(top);
            let r = classify_scroll_diag(&prev_gray, &gray, width, height, strip, ACCEPT_SCORE);
            match r.direction {
                Some((false, offset, rows)) => {
                    assert_eq!(rows, step, "accumulated 1 px steps");
                    assert!(s.append(&page.frame(top), width, height, offset).is_ok());
                    prev_gray = gray;
                    accepted += 1;
                }
                None => assert!(step < 3, "1-2 px alone are below the floor"),
                other => panic!("unexpected direction {other:?}"),
            }
        }
        assert_eq!(accepted, 1);
        assert_eq!(s.total_height(), height + 3);
        let (out, _, _) = s.into_pixels();
        assert_eq!(out, page.rows(8, height + 3), "no duplicated rows");
    }

    #[test]
    fn fast_scroll_is_flagged_and_not_stitched() {
        let (width, height, strip) = (32, 96, 20);
        let page = Page::new(200, width, height, 0xFA57_0004);
        let prev = page.gray(10);
        let curr = page.gray(10 + height - 4);
        let r = classify_scroll_diag(&prev, &curr, width, height, strip, ACCEPT_SCORE);
        assert_eq!(r.direction, None, "the overlap is gone: {r:?}");
        assert!(r.changed > 0.45, "changed {}", r.changed);
        let mut stall = StallTracker::new();
        for _ in 0..STALL_TICKS * 2 {
            assert_eq!(
                stall.observe(&r, 0.45),
                StallPhase::Watching,
                "a fast scroll keeps the session alive"
            );
        }
    }

    #[test]
    fn verify_shift_rejects_a_high_similarity_wrong_offset() {
        let (width, height, strip) = (32, 240, 32);
        let page = Page::new(400, width, height, 0x0BAD_0005);
        let prev = page.frame(100);
        let mut curr = page.frame(108); // true shift is 8
        let rb = width * 4;
        // Splice the previous bottom strip 16 rows higher than it belongs: the
        // bottom strip then scores 1.0 at a wrong offset while the rest of the
        // frame still says 8.
        curr[(height - 48) * rb..(height - 16) * rb]
            .copy_from_slice(&prev[(height - 32) * rb..height * rb]);

        let (prev_g, curr_g) = (gray_of(&prev, width, height), gray_of(&curr, width, height));
        let down = align_vertical(&prev_g, &curr_g, width, height, strip, ACCEPT_SCORE);
        assert!(down.accepted, "strip score {}", down.score);
        assert_eq!(height - down.offset_y, 16, "the strip claims 16 rows");

        // The independent band disagrees with the strip: refused.
        assert!(!verify_shift(&prev_g, &curr_g, width, height, strip, 16, false).accepted);
        assert!(verify_shift(&prev_g, &curr_g, width, height, strip, 8, false).accepted);

        let r = classify_scroll_diag(&prev_g, &curr_g, width, height, strip, ACCEPT_SCORE);
        assert_eq!(r.direction, None, "a wrong offset must not be reported");
        assert!(r.rejected_by_verification);
    }

    #[test]
    fn dynamic_content_inside_the_verified_band_still_stitches() {
        let (width, height, strip) = (32, 240, 32);
        let page = Page::new(400, width, height, 0x0D1D_0006);
        let prev = page.frame(100);
        let mut curr = page.frame(108);
        let rb = width * 4;
        // A 16x16 "video" block that changed between the frames, mid-frame and
        // well away from the seam.
        for y in 100..116 {
            for x in 8..24 {
                let i = y * rb + x * 4;
                curr[i] = 255 - curr[i];
                curr[i + 1] = 255 - curr[i + 1];
                curr[i + 2] = 255 - curr[i + 2];
            }
        }
        let (prev_g, curr_g) = (gray_of(&prev, width, height), gray_of(&curr, width, height));
        let r = classify_scroll_diag(&prev_g, &curr_g, width, height, strip, ACCEPT_SCORE);
        match r.direction {
            Some((false, offset, rows)) => {
                assert_eq!(rows, 8);
                assert_eq!(offset, height - 8);
            }
            other => panic!("dynamic region rejected the scroll: {other:?}"),
        }
    }

    #[test]
    fn stall_tracker_reaches_paused_and_stalled() {
        let mut stall = StallTracker::new();
        // Changed frame, no offset: a static band or an unalignable page.
        let changed = ScrollReport {
            changed: 0.2,
            static_edge: true,
            ..ScrollReport::default()
        };
        for i in 1..=STALL_TICKS {
            let phase = stall.observe(&changed, 0.45);
            if i < STALL_TICKS {
                assert_eq!(phase, StallPhase::Watching, "tick {i}");
            } else {
                assert_eq!(phase, StallPhase::Stalled, "tick {i}");
            }
        }
        assert!(!stall.paused());

        // A stitch resets the run.
        let stitched = ScrollReport {
            direction: Some((false, 10, 5)),
            changed: 0.3,
            ..ScrollReport::default()
        };
        assert_eq!(stall.observe(&stitched, 0.45), StallPhase::Progress);
        assert_eq!(stall.idle_ticks(), 0);

        // A still page is a pause after the threshold, not a stall.
        let still = ScrollReport::default();
        let mut phase = StallPhase::Watching;
        for _ in 0..STALL_TICKS {
            phase = stall.observe(&still, 0.45);
        }
        assert_eq!(phase, StallPhase::Paused);
        assert!(stall.paused());
    }

    #[test]
    fn up_and_down_walks_reconstruct_the_page_pixel_exact() {
        // Two independent sessions: a downward walk appends, a long upward walk
        // prepends across several headroom re-grants. Both must reproduce the
        // source page byte for byte, and neither may drop or duplicate a row.
        let (width, height, strip, k) = (8, 32, 8, 5);
        let page = Page::new(320, width, height, 0x51A7_0007);

        let mut down = Stitcher::new(width, height, 16_384).unwrap();
        let mut top = 150usize;
        assert!(down.fill_first(&page.frame(top)));
        let mut prev_gray = page.gray(top);
        for _ in 0..8 {
            top += k;
            let gray = page.gray(top);
            let r = classify_scroll_diag(&prev_gray, &gray, width, height, strip, ACCEPT_SCORE);
            match r.direction {
                Some((false, offset, rows)) if rows == k => {
                    assert!(down.append(&page.frame(top), width, height, offset).is_ok());
                }
                other => panic!("down step failed: {other:?}"),
            }
            prev_gray = gray;
        }
        let (out, _, oh) = down.into_pixels();
        assert_eq!(oh as usize, height + 8 * k);
        assert_eq!(out, page.rows(150, oh as usize));

        let mut up = Stitcher::new(width, height, 16_384).unwrap();
        let mut top = 150usize;
        assert!(up.fill_first(&page.frame(top)));
        let mut prev_gray = page.gray(top);
        for _ in 0..24 {
            top -= k;
            let gray = page.gray(top);
            let r = classify_scroll_diag(&prev_gray, &gray, width, height, strip, ACCEPT_SCORE);
            match r.direction {
                Some((true, offset, rows)) if rows == k => {
                    assert!(up.prepend(&page.frame(top), width, height, offset).is_ok());
                }
                other => panic!("up step failed at top {top}: {other:?}"),
            }
            prev_gray = gray;
        }
        let stats = up.stats();
        assert!(
            stats.compactions >= 2,
            "several headroom re-grants: {stats:?}"
        );
        let (out, _, oh) = up.into_pixels();
        assert_eq!(oh as usize, height + 24 * k);
        assert_eq!(out, page.rows(top, oh as usize));
    }

    #[test]
    fn horizontal_many_left_steps_stay_pixel_exact() {
        // A long leftward run prepends every step: the same headroom path as a
        // vertical up-scroll, through the rotate90/rotate270 model.
        let steps = 12;
        let (out, bw, bh, start, vw, grown) = horizontal_walk_n(false, steps);
        assert_eq!((bw, bh), (vw + grown, 32), "exported size is screen space");
        assert_eq!(grown, steps * 11);
        let page = wide_page(400, 32);
        let expected = columns(&page, 400, 32, start - grown, vw + grown);
        assert_eq!(
            out, expected,
            "left-scroll column order must match the page"
        );
    }
}
