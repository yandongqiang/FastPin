//! CPU composite of annotations onto a BGRA8 crop. Export blends each element's
//! coverage mask into the crop; the overlay preview turns the SAME mask into
//! alpha-weighted quads, so live preview and saved image share one rasterization.

use crate::config::{Watermark, WatermarkPosition};
use crate::geom::{Point, Rect};
use crate::overlay::editor::{
    badge_font_size, cloud_polyline, dash_segments, ellipse_ring, ArrowEnds, ArrowHead, BadgeStyle,
    DashPattern, Element, LineStyle, Shape,
};
use crate::overlay::text_raster;

struct Bgra<'a> {
    pixels: &'a mut [u8],
    width: u32,
    height: u32,
    stride: u32,
}

pub fn apply_annotations(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    stride: u32,
    origin_x: f64,
    origin_y: f64,
    elements: &[Element],
) {
    let mut buf = Bgra {
        pixels,
        width,
        height,
        stride,
    };
    for el in elements {
        let mut local = el.clone();
        local.bounds.x -= origin_x;
        local.bounds.y -= origin_y;
        match &mut local.shape {
            Shape::Arrow { start, end, .. } => {
                start.x -= origin_x;
                start.y -= origin_y;
                end.x -= origin_x;
                end.y -= origin_y;
            }
            Shape::Pen { points }
            | Shape::PolylineArrow { points, .. }
            | Shape::CurveArrow { points, .. } => {
                for p in points {
                    p.x -= origin_x;
                    p.y -= origin_y;
                }
            }
            _ => {}
        }
        draw_element(&mut buf, &local);
    }
    // Burn the watermark last so it sits above every annotation, matching
    // FastPin's post-process stage. Skipped entirely when the config is empty,
    // so a screenshot with no watermark is byte-identical to before.
    let watermark = crate::config::get().screenshot.watermark;
    if watermark.is_visible() {
        apply_watermark(&mut buf, width, height, &watermark);
    }
}

fn draw_element(buf: &mut Bgra<'_>, el: &Element) {
    let color = argb_to_bgra(el.stroke);
    match &el.shape {
        Shape::Mosaic { points, block_size } => draw_mosaic(buf, points, *block_size),
        Shape::Text { text, font_size } => {
            draw_text(buf, el.bounds, text, *font_size, el.stroke);
        }
        Shape::Badge {
            number,
            style,
            radius,
        } => {
            draw_badge(buf, el, *number, *style, *radius);
        }
        // Rect, Arrow and Pen all rasterize through the shared coverage mask, so
        // what the overlay preview shows is what this export writes. Fill goes
        // down first, then the outline covers its inner edge.
        _ => {
            let coverage = element_coverage(el);
            if let (Some(mask), Some(fill)) = (&coverage.fill, el.fill) {
                blend_mask(buf, mask, argb_to_bgra(fill));
            }
            if let Some(mask) = &coverage.stroke {
                blend_mask(buf, mask, color);
            }
        }
    }
}

fn argb_to_bgra(argb: u32) -> [u8; 4] {
    let a = ((argb >> 24) & 0xFF) as u8;
    let r = ((argb >> 16) & 0xFF) as u8;
    let g = ((argb >> 8) & 0xFF) as u8;
    let b = (argb & 0xFF) as u8;
    [b, g, r, a]
}

impl Bgra<'_> {
    fn put(&mut self, x: i32, y: i32, c: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let i = y as usize * self.stride as usize + x as usize * 4;
        if i + 3 >= self.pixels.len() {
            return;
        }
        self.pixels[i] = c[0];
        self.pixels[i + 1] = c[1];
        self.pixels[i + 2] = c[2];
        self.pixels[i + 3] = c[3];
    }

    /// Straight-alpha source-over of `c` at `coverage` (0..=1) opacity.
    ///
    /// The alpha channel is composited (`a_out = a_s + a_d·(1-a_s)`), not
    /// linearly mixed: a 50 % annotation over the opaque screenshot must keep
    /// the saved PNG opaque while its colour lands halfway between stroke and
    /// background. Colours are un-premultiplied by `a_out` so repeated
    /// semi-transparent strokes stay correct.
    fn blend(&mut self, x: i32, y: i32, c: [u8; 4], coverage: f64) {
        let sa = (coverage * c[3] as f64 / 255.0).clamp(0.0, 1.0);
        if sa <= 0.0 {
            return;
        }
        if sa >= 1.0 {
            self.put(x, y, c);
            return;
        }
        let dst = self.get(x, y);
        let da = dst[3] as f64 / 255.0;
        let ao = sa + da * (1.0 - sa);
        if ao <= 0.0 {
            return;
        }
        let mix = |s: u8, d: u8| {
            ((s as f64 * sa + d as f64 * da * (1.0 - sa)) / ao)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        self.put(
            x,
            y,
            [
                mix(c[0], dst[0]),
                mix(c[1], dst[1]),
                mix(c[2], dst[2]),
                (ao * 255.0).round() as u8,
            ],
        );
    }

    fn get(&self, x: i32, y: i32) -> [u8; 4] {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return [0, 0, 0, 0];
        }
        let i = y as usize * self.stride as usize + x as usize * 4;
        if i + 3 >= self.pixels.len() {
            return [0, 0, 0, 0];
        }
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }
}

/// Per-pixel coverage of one element, in the physical-pixel space the export
/// composite samples. The overlay preview converts this same mask into
/// alpha-weighted quads, one per constant-coverage run, so the live preview and
/// the exported crop are the same rasterization on different output targets.
pub struct CoverageMask {
    pub x0: i32,
    pub y0: i32,
    pub width: usize,
    pub height: usize,
    /// Row-major coverage: 0 = untouched, 255 = fully covered.
    pub data: Vec<u8>,
}

/// One horizontal span of equal coverage on a physical pixel row. Merging
/// equal-coverage neighbours keeps the preview's quad count proportional to the
/// edge length, not the filled area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverageRun {
    pub y: i32,
    pub x0: i32,
    pub x1: i32,
    pub alpha: u8,
}

/// Preview-side form of `ElementCoverage`: the same masks already scanned into
/// constant-coverage runs, one list per layer so the fill keeps its own colour.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ElementRuns {
    pub fill: Vec<CoverageRun>,
    pub stroke: Vec<CoverageRun>,
}

impl ElementRuns {
    pub fn is_empty(&self) -> bool {
        self.fill.is_empty() && self.stroke.is_empty()
    }
}

impl CoverageMask {
    /// Runs of equal coverage, row by row. Runs never overlap and every pixel
    /// they cover lies on the mask's integer pixel grid.
    pub fn runs(&self) -> Vec<CoverageRun> {
        let mut out = Vec::new();
        for row in 0..self.height {
            let base = row * self.width;
            let y = self.y0 + row as i32;
            let mut x = 0;
            while x < self.width {
                let a = self.data[base + x];
                if a == 0 {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < self.width && self.data[base + x] == a {
                    x += 1;
                }
                out.push(CoverageRun {
                    y,
                    x0: self.x0 + start as i32,
                    x1: self.x0 + x as i32,
                    alpha: a,
                });
            }
        }
        out
    }
}

/// Fill and stroke coverage of one element, split so each can be blended in
/// its own colour. The overlay preview and the export raster both consume
/// this pair, so the two can never drift.
#[derive(Default)]
pub struct ElementCoverage {
    pub fill: Option<CoverageMask>,
    pub stroke: Option<CoverageMask>,
}

pub fn element_coverage(el: &Element) -> ElementCoverage {
    // Line style belongs to closed shapes (rect / ellipse) only. Every other
    // stroke is forced solid, so a stale dash/cloud choice from a previous
    // shape can never leak into a pen stroke or an arrow.
    match &el.shape {
        Shape::Rect => ElementCoverage {
            fill: el.fill.and_then(|_| rect_fill_mask(el.bounds)),
            stroke: el
                .stroke_enabled
                .then(|| {
                    styled_mask(
                        &rect_outline_points(el.bounds),
                        el.stroke_width,
                        el.line_style,
                        true,
                    )
                })
                .flatten(),
        },
        Shape::Ellipse => ElementCoverage {
            fill: el.fill.and_then(|_| ellipse_fill_mask(el.bounds)),
            stroke: el
                .stroke_enabled
                .then(|| {
                    styled_mask(
                        &ellipse_ring(el.bounds, 96),
                        el.stroke_width,
                        el.line_style,
                        true,
                    )
                })
                .flatten(),
        },
        Shape::Arrow { start, end, head } => {
            arrow_coverage(el, &[*start, *end], *head, false, true)
        }
        Shape::PolylineArrow { points, head } => arrow_coverage(el, points, *head, false, false),
        Shape::CurveArrow { points, head } => arrow_coverage(el, points, *head, true, false),
        Shape::Pen { points } => ElementCoverage {
            fill: None,
            stroke: styled_mask(points, el.stroke_width, LineStyle::Solid, false),
        },
        // Mosaic smudges pixels, text is a GPU-side div in the overlay, and a
        // badge is a disc + numeral drawn directly.
        Shape::Mosaic { .. } | Shape::Text { .. } | Shape::Badge { .. } => {
            ElementCoverage::default()
        }
    }
}

/// Union of fill and outline: the single mask the mask-based tests inspect.
/// The preview and the export both use `element_coverage` so the fill keeps
/// its own colour.
#[cfg(test)]
pub fn element_mask(el: &Element) -> Option<CoverageMask> {
    let cov = element_coverage(el);
    match (cov.fill, cov.stroke) {
        (Some(f), Some(s)) => Some(merge_masks(&f, &s)),
        (Some(m), None) | (None, Some(m)) => Some(m),
        (None, None) => None,
    }
}

/// Shaft (straight, polyline or Catmull-Rom curve) plus heads at the requested
/// ends. `straight` keeps the QQ tapered body for the default single filled
/// head, so existing arrows look unchanged.
fn arrow_coverage(
    el: &Element,
    points: &[Point],
    head: f64,
    curved: bool,
    straight: bool,
) -> ElementCoverage {
    if straight
        && el.stroke_enabled
        && el.arrow_head == ArrowHead::Filled
        && el.arrow_ends == ArrowEnds::End
    {
        if let (Some(start), Some(end)) = (points.first(), points.get(1)) {
            if let Some(g) = crate::overlay::editor::filled_arrow(*start, *end, el.stroke_width) {
                return ElementCoverage {
                    fill: None,
                    stroke: triangles_mask(&g.triangles()),
                };
            }
        }
    }
    let shaft = if curved {
        crate::overlay::editor::smooth_polyline(points)
    } else {
        points.to_vec()
    };
    let end_frame = el
        .arrow_ends
        .has_end()
        .then(|| head_frame(&shaft, true, head, el.stroke_width))
        .flatten();
    let start_frame = el
        .arrow_ends
        .has_start()
        .then(|| head_frame(&shaft, false, head, el.stroke_width))
        .flatten();
    // A solid head already covers the last `len` px of the shaft, so the body
    // stops just inside the head base. That keeps the stroke's round cap from
    // bulging past the tip (which made the tip look recessed behind the line
    // end) while the 1 px overlap leaves no seam for antialiasing to open.
    let body = if el.arrow_head == ArrowHead::Filled {
        let mut body = shaft;
        if let Some(f) = &end_frame {
            body = trim_tail(&body, (f.len - 1.0).max(0.0));
        }
        if let Some(f) = &start_frame {
            body = trim_head(&body, (f.len - 1.0).max(0.0));
        }
        body
    } else {
        shaft.clone()
    };
    let mut stroke = if el.stroke_enabled {
        styled_mask(&body, el.stroke_width, LineStyle::Solid, false)
    } else {
        None
    };
    if let Some(f) = &end_frame {
        stroke = union_mask(stroke, head_mask(el.arrow_head, f, el.stroke_width));
    }
    if let Some(f) = &start_frame {
        stroke = union_mask(stroke, head_mask(el.arrow_head, f, el.stroke_width));
    }
    ElementCoverage { fill: None, stroke }
}

/// Cut `amount` px off the far end of a path, interpolating the new last point
/// and never collapsing the stroke below its first vertex.
fn trim_tail(points: &[Point], amount: f64) -> Vec<Point> {
    if points.len() < 2 || amount <= 0.0 {
        return points.to_vec();
    }
    let mut right = amount;
    for i in (0..points.len() - 1).rev() {
        let (a, b) = (points[i], points[i + 1]);
        let len = (b.x - a.x).hypot(b.y - a.y);
        if len >= right {
            let t = 1.0 - right / len;
            let cut = Point {
                x: a.x + (b.x - a.x) * t,
                y: a.y + (b.y - a.y) * t,
            };
            if i == 0 && (cut.x - points[0].x).hypot(cut.y - points[0].y) < 1.0 {
                return points.to_vec();
            }
            let mut out = points[..=i].to_vec();
            out.push(cut);
            return out;
        }
        right -= len;
    }
    points.to_vec()
}

/// Mirror of [`trim_tail`] for a head sitting on the first point.
fn trim_head(points: &[Point], amount: f64) -> Vec<Point> {
    let mut reversed: Vec<Point> = points.iter().rev().copied().collect();
    reversed = trim_tail(&reversed, amount);
    reversed.reverse();
    reversed
}

fn union_mask(a: Option<CoverageMask>, b: Option<CoverageMask>) -> Option<CoverageMask> {
    match (a, b) {
        (Some(a), Some(b)) => Some(merge_masks(&a, &b)),
        (Some(m), None) | (None, Some(m)) => Some(m),
        (None, None) => None,
    }
}

/// Where an arrow head sits: tip, base and the two base corners, aimed along
/// the local shaft tangent (outward at the chosen end).
struct HeadFrame {
    tip: Point,
    base: Point,
    left: Point,
    right: Point,
    len: f64,
}

/// How far the head may drift off the local tangent before the walked span
/// stops: a corner near the tip must not aim the head across the bend.
const HEAD_MAX_DRIFT: f64 = 0.15;

/// Span of stroke the head direction is averaged over: long enough to smooth
/// the sub-pixel steps a curve subdivides into, short enough to stay local on a
/// tight arc.
const HEAD_SPAN: f64 = 3.0;

/// Unit tangent at `tip`, pointing out of the stroke. Segments are averaged by
/// length over a short span, and the walk stops at a corner, so a bent polyline
/// aims its head along the last leg while a curve uses its true end tangent.
/// A chord to a distant point would lag behind the tangent and tilt the head
/// off the line end.
fn head_direction(shaft: &[Point], at_end: bool) -> Option<(f64, f64)> {
    let mut sum = (0.0_f64, 0.0_f64);
    let mut first: Option<(f64, f64)> = None;
    let mut span = 0.0_f64;
    for k in 1..shaft.len() {
        let (near, far) = if at_end {
            (shaft[shaft.len() - k], shaft[shaft.len() - 1 - k])
        } else {
            (shaft[k - 1], shaft[k])
        };
        let dx = near.x - far.x;
        let dy = near.y - far.y;
        let len = dx.hypot(dy);
        if len <= 1e-6 {
            continue;
        }
        let (ux, uy) = (dx / len, dy / len);
        match first {
            None => {
                // Skip sub-pixel steps: their direction is mostly rounding noise.
                if len < 0.75 {
                    continue;
                }
                first = Some((ux, uy));
            }
            Some((fx, fy)) => {
                if (ux * fx + uy * fy).clamp(-1.0, 1.0).acos() > HEAD_MAX_DRIFT {
                    break;
                }
            }
        }
        sum.0 += ux * len;
        sum.1 += uy * len;
        span += len;
        if span >= HEAD_SPAN {
            break;
        }
    }
    let len = sum.0.hypot(sum.1);
    (len > 1e-6).then(|| (sum.0 / len, sum.1 / len))
}

fn head_frame(shaft: &[Point], at_end: bool, head: f64, thick: f64) -> Option<HeadFrame> {
    let tip = if at_end {
        *shaft.last()?
    } else {
        *shaft.first()?
    };
    let (ux, uy) = head_direction(shaft, at_end)?;
    let (px, py) = (-uy, ux);
    let head_len = head.max(thick * 3.2).max(10.0);
    let half = head_len * 0.5;
    let base = Point {
        x: tip.x - ux * head_len,
        y: tip.y - uy * head_len,
    };
    Some(HeadFrame {
        tip,
        base,
        left: Point {
            x: base.x + px * half,
            y: base.y + py * half,
        },
        right: Point {
            x: base.x - px * half,
            y: base.y - py * half,
        },
        len: head_len,
    })
}

fn head_mask(kind: ArrowHead, f: &HeadFrame, thick: f64) -> Option<CoverageMask> {
    match kind {
        ArrowHead::Filled => triangles_mask(&[(f.tip, f.left, f.right)]),
        ArrowHead::Open => {
            stroke_segments_mask(&[[f.tip, f.left], [f.tip, f.right]], thick.max(1.5))
        }
        ArrowHead::Bar => {
            let dx = f.tip.x - f.base.x;
            let dy = f.tip.y - f.base.y;
            let l = dx.hypot(dy).max(1e-6);
            let (px, py) = (-dy / l, dx / l);
            let half = f.len * 0.5;
            stroke_segments_mask(
                &[[
                    Point {
                        x: f.tip.x + px * half,
                        y: f.tip.y + py * half,
                    },
                    Point {
                        x: f.tip.x - px * half,
                        y: f.tip.y - py * half,
                    },
                ]],
                thick.max(1.5),
            )
        }
    }
}

/// Solid interior of a rectangle, antialiased by the shared triangle sampler.
fn rect_fill_mask(r: Rect) -> Option<CoverageMask> {
    let p0 = Point { x: r.x, y: r.y };
    let p1 = Point {
        x: r.x + r.width,
        y: r.y,
    };
    let p2 = Point {
        x: r.x + r.width,
        y: r.y + r.height,
    };
    let p3 = Point {
        x: r.x,
        y: r.y + r.height,
    };
    triangles_mask(&[(p0, p1, p2), (p0, p2, p3)])
}

/// Solid interior of an ellipse. Coverage falls off over the last half pixel
/// of the normalized radius so the rim stays antialiased like a stroke.
fn ellipse_fill_mask(r: Rect) -> Option<CoverageMask> {
    let cx = r.x + r.width * 0.5;
    let cy = r.y + r.height * 0.5;
    let rx = (r.width * 0.5).max(0.5);
    let ry = (r.height * 0.5).max(0.5);
    let x0 = r.x.floor() as i32;
    let y0 = r.y.floor() as i32;
    let x1 = (r.x + r.width).ceil() as i32;
    let y1 = (r.y + r.height).ceil() as i32;
    let width = (x1 - x0).max(0) as usize;
    let height = (y1 - y0).max(0) as usize;
    if width == 0 || height == 0 {
        return None;
    }
    let scale = rx.min(ry);
    let mut data = vec![0u8; width * height];
    for y in y0..y1 {
        for x in x0..x1 {
            let nx = (x as f64 + 0.5 - cx) / rx;
            let ny = (y as f64 + 0.5 - cy) / ry;
            let cov = ((1.0 - nx.hypot(ny)) * scale + 0.5).clamp(0.0, 1.0);
            if cov > 0.0 {
                data[(y - y0) as usize * width + (x - x0) as usize] = (cov * 255.0).round() as u8;
            }
        }
    }
    Some(CoverageMask {
        x0,
        y0,
        width,
        height,
        data,
    })
}

/// Union of two coverage masks (max per pixel).
fn merge_masks(a: &CoverageMask, b: &CoverageMask) -> CoverageMask {
    let x0 = a.x0.min(b.x0);
    let y0 = a.y0.min(b.y0);
    let x1 = (a.x0 + a.width as i32).max(b.x0 + b.width as i32);
    let y1 = (a.y0 + a.height as i32).max(b.y0 + b.height as i32);
    let width = (x1 - x0).max(0) as usize;
    let height = (y1 - y0).max(0) as usize;
    let mut data = vec![0u8; width * height];
    for m in [a, b] {
        for row in 0..m.height {
            for col in 0..m.width {
                let v = m.data[row * m.width + col];
                if v == 0 {
                    continue;
                }
                let x = (m.x0 + col as i32 - x0) as usize;
                let y = (m.y0 + row as i32 - y0) as usize;
                if x < width && y < height {
                    let i = y * width + x;
                    data[i] = data[i].max(v);
                }
            }
        }
    }
    CoverageMask {
        x0,
        y0,
        width,
        height,
        data,
    }
}

/// Stroke a polyline honouring its line style. `closed` shapes may be scalloped
/// into a revision cloud; an open path falls back to solid for Cloud.
fn styled_mask(pts: &[Point], thick: f64, style: LineStyle, closed: bool) -> Option<CoverageMask> {
    match style {
        LineStyle::Solid => stroke_mask(pts, thick),
        LineStyle::Dashed => {
            let segs = dash_segments(pts, DashPattern::for_width(thick));
            if segs.is_empty() {
                return None;
            }
            stroke_segments_mask(&segs, thick)
        }
        LineStyle::Cloud => {
            if closed {
                stroke_mask(&cloud_polyline(pts, thick * 2.5), thick)
            } else {
                stroke_mask(pts, thick)
            }
        }
    }
}

fn rect_outline_points(r: Rect) -> [Point; 5] {
    let p0 = Point { x: r.x, y: r.y };
    let p1 = Point {
        x: r.x + r.width,
        y: r.y,
    };
    let p2 = Point {
        x: r.x + r.width,
        y: r.y + r.height,
    };
    let p3 = Point {
        x: r.x,
        y: r.y + r.height,
    };
    [p0, p1, p2, p3, p0]
}

/// Antialiased stroke coverage: round-capped capsules, coverage from the distance
/// to each segment, so caps and joints come out round. Coverage is accumulated
/// into the mask with max, so overlapping segments (a pen stroke samples far
/// denser than its own width) do not darken each other's soft edges.
///
/// A lone point is a segment of zero length -- `dist_to_segment` handles that,
/// so a single dot needs no separate case.
pub fn stroke_mask(pts: &[Point], thick: f64) -> Option<CoverageMask> {
    let first = pts.first()?;
    let segments: Vec<[Point; 2]> = if pts.len() == 1 {
        vec![[*first, *first]]
    } else {
        pts.windows(2).map(|s| [s[0], s[1]]).collect()
    };
    stroke_segments_mask(&segments, thick)
}

/// Same distance field as `stroke_mask`, but over an explicit set of independent
/// capsules (one per dash). Max-accumulation keeps their round caps soft.
pub fn stroke_segments_mask(segments: &[[Point; 2]], thick: f64) -> Option<CoverageMask> {
    let r = (thick.max(1.0) / 2.0).max(0.5);
    let pad = r + 1.0;
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    for [a, b] in segments {
        for p in [a, b] {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
    }
    let x0 = (min_x - pad).floor() as i32;
    let y0 = (min_y - pad).floor() as i32;
    let x1 = (max_x + pad).ceil() as i32;
    let y1 = (max_y + pad).ceil() as i32;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let width = (x1 - x0) as usize;
    let height = (y1 - y0) as usize;
    let mut data = vec![0u8; width * height];
    for [a, b] in segments {
        let sx0 = ((a.x.min(b.x) - pad).floor() as i32).clamp(x0, x1);
        let sy0 = ((a.y.min(b.y) - pad).floor() as i32).clamp(y0, y1);
        let sx1 = ((a.x.max(b.x) + pad).ceil() as i32).clamp(x0, x1);
        let sy1 = ((a.y.max(b.y) + pad).ceil() as i32).clamp(y0, y1);
        for y in sy0..sy1 {
            for x in sx0..sx1 {
                let p = Point {
                    x: x as f64 + 0.5,
                    y: y as f64 + 0.5,
                };
                let d = crate::overlay::editor::dist_to_segment(p, *a, *b);
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let i = (y - y0) as usize * width + (x - x0) as usize;
                data[i] = data[i].max((cov * 255.0).round() as u8);
            }
        }
    }
    Some(CoverageMask {
        x0,
        y0,
        width,
        height,
        data,
    })
}

/// Coverage of the union of `tris` from a 4x4 box supersample. Enough for
/// straight edges; go analytic only if 16 samples ever show banding on a
/// near-horizontal edge. Sampling the union in one pass keeps the arrow's
/// shared neck edge from blending twice.
pub fn triangles_mask(tris: &[(Point, Point, Point)]) -> Option<CoverageMask> {
    const S: i32 = 4;
    if tris.is_empty() {
        return None;
    }
    let mut min = Point {
        x: f64::MAX,
        y: f64::MAX,
    };
    let mut max = Point {
        x: f64::MIN,
        y: f64::MIN,
    };
    for (a, b, c) in tris {
        for p in [a, b, c] {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
    }
    if min.x > max.x {
        return None;
    }
    let x0 = min.x.floor() as i32;
    let y0 = min.y.floor() as i32;
    let x1 = max.x.ceil() as i32;
    let y1 = max.y.ceil() as i32;
    let width = (x1 - x0 + 1) as usize;
    let height = (y1 - y0 + 1) as usize;
    let mut data = vec![0u8; width * height];
    for y in y0..=y1 {
        for x in x0..=x1 {
            let mut hits = 0;
            for sy in 0..S {
                for sx in 0..S {
                    let px = x as f64 + (sx as f64 + 0.5) / S as f64;
                    let py = y as f64 + (sy as f64 + 0.5) / S as f64;
                    if tris
                        .iter()
                        .any(|(a, b, c)| point_in_triangle(px, py, *a, *b, *c))
                    {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                let cov = hits as f64 / (S * S) as f64;
                data[(y - y0) as usize * width + (x - x0) as usize] = (cov * 255.0).round() as u8;
            }
        }
    }
    Some(CoverageMask {
        x0,
        y0,
        width,
        height,
        data,
    })
}

/// Source-over blend of a coverage mask into the crop. One blend per pixel,
/// matching the max-accumulated mask the preview draws as disjoint quads.
fn blend_mask(buf: &mut Bgra<'_>, mask: &CoverageMask, c: [u8; 4]) {
    for row in 0..mask.height {
        let y = mask.y0 + row as i32;
        if y < 0 || y >= buf.height as i32 {
            continue;
        }
        for col in 0..mask.width {
            let m = mask.data[row * mask.width + col];
            if m > 0 {
                buf.blend(mask.x0 + col as i32, y, c, m as f64 / 255.0);
            }
        }
    }
}

fn point_in_triangle(px: f64, py: f64, a: Point, b: Point, c: Point) -> bool {
    let d1 = (px - b.x) * (a.y - b.y) - (a.x - b.x) * (py - b.y);
    let d2 = (px - c.x) * (b.y - c.y) - (b.x - c.x) * (py - c.y);
    let d3 = (px - a.x) * (c.y - a.y) - (c.x - a.x) * (py - a.y);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

/// Flatten each grid cell the smudge path covers to that cell's mean colour.
/// Cells are image-aligned and never overlap, so writing them back in place cannot
/// contaminate a later cell's average.
fn draw_mosaic(buf: &mut Bgra<'_>, points: &[Point], block_size: i32) {
    let block = block_size.max(2);
    for (gx, gy) in crate::overlay::editor::mosaic_cells(points, block) {
        let x0 = (gx * block).clamp(0, buf.width as i32);
        let y0 = (gy * block).clamp(0, buf.height as i32);
        let x1 = (gx * block + block).clamp(0, buf.width as i32);
        let y1 = (gy * block + block).clamp(0, buf.height as i32);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let mut sum = [0u32; 4];
        let mut n = 0u32;
        for y in y0..y1 {
            for x in x0..x1 {
                let p = buf.get(x, y);
                for i in 0..4 {
                    sum[i] += p[i] as u32;
                }
                n += 1;
            }
        }
        if n == 0 {
            continue;
        }
        let avg = [
            (sum[0] / n) as u8,
            (sum[1] / n) as u8,
            (sum[2] / n) as u8,
            (sum[3] / n) as u8,
        ];
        for y in y0..y1 {
            for x in x0..x1 {
                buf.put(x, y, avg);
            }
        }
    }
}

/// Per-pixel bubble coverage at one pixel centre, in image px. Shared by the
/// export raster and its geometry tests so both see the same shape.
fn badge_coverage(style: BadgeStyle, dx: f64, dy: f64, r: f64, ring: f64) -> f64 {
    match style {
        BadgeStyle::Filled => (r + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0),
        BadgeStyle::Hollow => {
            // Ring = outer disc minus the inner disc.
            let outer = (r + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0);
            let inner = (r - ring + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0);
            outer - inner
        }
        BadgeStyle::Square => (r + 0.5 - dx.abs().max(dy.abs())).clamp(0.0, 1.0),
    }
}

/// Number bubble: filled disc, ring, or square plus the numeral, centred on the
/// element. The bubble uses the element's stroke colour and alpha; the numeral
/// is white on a filled bubble and the stroke colour on a hollow one. Mirrors
/// the overlay preview (`annotate::badge_layer`), which draws the same shape as
/// gpui `div`s.
fn draw_badge(buf: &mut Bgra<'_>, el: &Element, number: u32, style: BadgeStyle, radius: f64) {
    let r = radius.max(1.0);
    let cx = el.bounds.x + el.bounds.width * 0.5;
    let cy = el.bounds.y + el.bounds.height * 0.5;
    let fill = argb_to_bgra(el.stroke);
    let ring = (r * 0.16).max(1.5);
    let x0 = (cx - r - 1.0).floor() as i32;
    let y0 = (cy - r - 1.0).floor() as i32;
    let x1 = (cx + r + 1.0).ceil() as i32;
    let y1 = (cy + r + 1.0).ceil() as i32;
    for y in y0..y1 {
        for x in x0..x1 {
            let cov = badge_coverage(style, x as f64 + 0.5 - cx, y as f64 + 0.5 - cy, r, ring);
            if cov > 0.0 {
                buf.blend(x, y, fill, cov);
            }
        }
    }
    // A hollow bubble's digit is drawn in the stroke colour so it stays legible
    // on whatever is behind the transparent centre; a filled bubble keeps white.
    let numeral = match style {
        BadgeStyle::Hollow => el.stroke,
        _ => 0xFFFF_FFFF,
    };
    let font = badge_font_size(r);
    if let Ok((px, w, h)) = text_raster::rasterize(&number.to_string(), font, numeral) {
        // Premultiplied BGRA: out = src + dst over (1 - srcA).
        let lw = w as i32;
        let lh = h as i32;
        let ox = (cx - w as f64 / 2.0).floor() as i32;
        let oy = (cy - h as f64 / 2.0).floor() as i32;
        let src_stride = (w * 4) as usize;
        for row in 0..lh {
            for col in 0..lw {
                let i = row as usize * src_stride + col as usize * 4;
                let sa = px[i + 3] as f64 / 255.0;
                if sa <= 0.0 {
                    continue;
                }
                let x = ox + col;
                let y = oy + row;
                let dst = buf.get(x, y);
                let mix = |s: u8, d: u8| (s as f64 + d as f64 * (1.0 - sa)).round() as u8;
                buf.put(
                    x,
                    y,
                    [
                        mix(px[i], dst[0]),
                        mix(px[i + 1], dst[1]),
                        mix(px[i + 2], dst[2]),
                        dst[3],
                    ],
                );
            }
        }
    }
}

/// Burn a configured watermark onto the exported image. An image watermark that
/// loads takes precedence; otherwise the (possibly tiled) text layer is drawn.
/// Every failure path is silent: a watermark must never break a capture.
fn apply_watermark(buf: &mut Bgra<'_>, width: u32, height: u32, wm: &Watermark) {
    let margin = 12i64;
    let alpha = (wm.opacity.clamp(10, 100) as f64 / 100.0).max(0.0);
    let position = WatermarkPosition::from_key(&wm.position);
    if let Some((px, w, h)) = load_watermark_image(wm) {
        let (ox, oy) = watermark_origin((width, height), (w, h), position, margin);
        blend_premult_layer(buf, ox, oy, width, height, &px, w, h, alpha);
        return;
    }
    let text = wm.resolved_text();
    if text.is_empty() {
        return;
    }
    let font_size: f64 = 24.0 * wm.scale.clamp(0.5, 3.0) as f64;
    let Ok((px, w, h)) = text_raster::rasterize(&text, font_size, 0xFFFF_FFFF) else {
        return;
    };
    if wm.tiled {
        tile_premult_layer(buf, width, height, &px, w, h, alpha);
    } else {
        let (ox, oy) = watermark_origin((width, height), (w, h), position, margin);
        blend_premult_layer(buf, ox, oy, width, height, &px, w, h, alpha);
    }
}

/// Top-left corner of a `text`-sized layer inside a `canvas`, kept `margin`
/// pixels from the edges per the 9-grid `position`. Mirrors FastPin's
/// `watermark_origin`.
fn watermark_origin(
    canvas: (u32, u32),
    text: (u32, u32),
    position: WatermarkPosition,
    margin: i64,
) -> (i64, i64) {
    let margin = margin.min(canvas.0.min(canvas.1) as i64);
    let free_w = canvas.0 as i64 - text.0 as i64 - margin * 2;
    let free_h = canvas.1 as i64 - text.1 as i64 - margin * 2;
    let fx = match position {
        WatermarkPosition::TopLeft
        | WatermarkPosition::MiddleLeft
        | WatermarkPosition::BottomLeft => 0.0,
        WatermarkPosition::TopCenter
        | WatermarkPosition::Center
        | WatermarkPosition::BottomCenter => 0.5,
        WatermarkPosition::TopRight
        | WatermarkPosition::MiddleRight
        | WatermarkPosition::BottomRight => 1.0,
    };
    let y = match position {
        WatermarkPosition::TopLeft | WatermarkPosition::TopCenter | WatermarkPosition::TopRight => {
            margin
        }
        WatermarkPosition::MiddleLeft
        | WatermarkPosition::Center
        | WatermarkPosition::MiddleRight => margin + free_h / 2,
        WatermarkPosition::BottomLeft
        | WatermarkPosition::BottomCenter
        | WatermarkPosition::BottomRight => margin + free_h,
    };
    let x = margin + (free_w as f64 * fx).round() as i64;
    (x, y)
}

/// Blend one premultiplied-BGRA layer at `(ox, oy)` with a global opacity. A
/// layer pixel already carries its own coverage in alpha; `alpha` scales it.
/// Out-of-bounds pixels are clipped, not wrapped.
fn blend_premult_layer(
    buf: &mut Bgra<'_>,
    ox: i64,
    oy: i64,
    width: u32,
    height: u32,
    px: &[u8],
    w: u32,
    h: u32,
    alpha: f64,
) {
    let src_stride = (w * 4) as usize;
    for row in 0..h as i64 {
        for col in 0..w as i64 {
            let i = (row * src_stride as i64 + col * 4) as usize;
            let sa = px[i + 3] as f64 / 255.0;
            let eff = sa * alpha;
            if eff <= 0.0 {
                continue;
            }
            let x = ox + col;
            let y = oy + row;
            if x < 0 || y < 0 || x >= width as i64 || y >= height as i64 {
                continue;
            }
            let dst = buf.get(x as i32, y as i32);
            let mix = |s: u8, d: u8| (s as f64 * alpha + d as f64 * (1.0 - eff)).round() as u8;
            buf.put(
                x as i32,
                y as i32,
                [
                    mix(px[i], dst[0]),
                    mix(px[i + 1], dst[1]),
                    mix(px[i + 2], dst[2]),
                    dst[3],
                ],
            );
        }
    }
}

/// Repeat a premultiplied-BGRA layer across the whole image on a staggered
/// grid, so the watermark reads as a classic tiled pattern. Rows alternate
/// between a tile at the left edge and one shifted by half a step.
fn tile_premult_layer(
    buf: &mut Bgra<'_>,
    width: u32,
    height: u32,
    px: &[u8],
    w: u32,
    h: u32,
    alpha: f64,
) {
    let (lw, lh) = (w as i64, h as i64);
    if lw == 0 || lh == 0 {
        return;
    }
    let step_x = (lw + lw / 2).max(1);
    let step_y = (lh * 2).max(1);
    let mut row = 0u32;
    let mut y = 0i64;
    while y < height as i64 {
        let offset = if row.is_multiple_of(2) { 0 } else { step_x / 2 };
        let mut x = offset - if offset > 0 { step_x } else { 0 };
        while x < width as i64 {
            blend_premult_layer(buf, x, y, width, height, px, w, h, alpha);
            x += step_x;
        }
        y += step_y;
        row += 1;
    }
}

/// Decode an image watermark into a premultiplied-BGRA buffer. Only an existing
/// file is attempted; any decode or read error is swallowed so a bad asset can
/// never blank a capture. No path is ever constructed from user input — the
/// configured path is used verbatim.
fn load_watermark_image(wm: &Watermark) -> Option<(Vec<u8>, u32, u32)> {
    let path = wm.image_path.as_ref()?;
    if !path.is_file() {
        return None;
    }
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let src = img.into_raw();
    let mut out = vec![0u8; (w * h * 4) as usize];
    for i in 0..(w * h) as usize {
        let (r, g, b, a) = (src[i * 4], src[i * 4 + 1], src[i * 4 + 2], src[i * 4 + 3]);
        let af = a as f64 / 255.0;
        out[i * 4] = (b as f64 * af).round() as u8;
        out[i * 4 + 1] = (g as f64 * af).round() as u8;
        out[i * 4 + 2] = (r as f64 * af).round() as u8;
        out[i * 4 + 3] = a;
    }
    Some((out, w, h))
}

/// Rasterizes `text` through DirectWrite/Direct2D (the same face as the overlay
/// preview) and composites the premultiplied result at `bounds`'s origin. The
/// raster's own size defines the extent -- unlike the old placeholder there is
/// no `bounds.width` clamp, matching the preview, which does not clip either.
fn draw_text(buf: &mut Bgra<'_>, bounds: Rect, text: &str, font_size: f64, argb: u32) {
    let (px_data, w, h) = match text_raster::rasterize(text, font_size, argb) {
        Ok(v) => v,
        Err(e) => {
            crate::perf::log_app(&format!("text raster failed: {e}"));
            return;
        }
    };
    let x0 = bounds.x.floor() as i32;
    let y0 = bounds.y.floor() as i32;
    let stride = (w * 4) as usize;
    for row in 0..h as usize {
        for col in 0..w as usize {
            let i = row * stride + col * 4;
            let src = [px_data[i], px_data[i + 1], px_data[i + 2], px_data[i + 3]];
            if src[3] == 0 {
                continue;
            }
            let x = x0 + col as i32;
            let y = y0 + row as i32;
            let dst = buf.get(x, y);
            let inv = 1.0 - src[3] as f64 / 255.0;
            let mix = |s: u8, d: u8| (s as f64 + d as f64 * inv).round() as u8;
            buf.put(
                x,
                y,
                [
                    mix(src[0], dst[0]),
                    mix(src[1], dst[1]),
                    mix(src[2], dst[2]),
                    mix(src[3], dst[3]),
                ],
            );
        }
    }
}

/// Same helper Complete/Save uses after GPU crop: annotate then PNG-encode.
#[cfg(test)]
pub fn export_annotated_png(
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    origin_x: f64,
    origin_y: f64,
    elements: &[Element],
) -> crate::error::Result<Vec<u8>> {
    let mut buf = pixels.to_vec();
    apply_annotations(
        &mut buf, width, height, stride, origin_x, origin_y, elements,
    );
    crate::codec::encode_png_bgra(&buf, width, height, stride)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::editor::{Element, Shape, DEFAULT_MOSAIC_BLOCK};
    use crate::overlay::theme::STROKE_ARGB;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    fn solid(w: u32, h: u32, c: [u8; 4]) -> (Vec<u8>, u32) {
        let stride = w * 4;
        let mut px = vec![0u8; (stride * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = (y * stride + x * 4) as usize;
                px[i..i + 4].copy_from_slice(&c);
            }
        }
        (px, stride)
    }

    fn unique(w: u32, h: u32) -> (Vec<u8>, u32) {
        let stride = w * 4;
        let mut px = vec![0u8; (stride * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = (y * stride + x * 4) as usize;
                px[i] = x as u8;
                px[i + 1] = y as u8;
                px[i + 2] = (x + y) as u8;
                px[i + 3] = 255;
            }
        }
        (px, stride)
    }

    fn rect_el(bounds: Rect) -> Element {
        Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 1,
            bounds,
            stroke: STROKE_ARGB,
            stroke_width: 3.0,
            line_style: Default::default(),
            shape: Shape::Rect,
        }
    }

    /// A smudge stroke sweeping left-to-right across the middle of `bounds`.
    fn mosaic_el(bounds: Rect) -> Element {
        let y = bounds.y + bounds.height / 2.0;
        Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 2,
            bounds,
            stroke: STROKE_ARGB,
            stroke_width: 1.0,
            line_style: Default::default(),
            shape: Shape::Mosaic {
                points: vec![
                    crate::geom::Point { x: bounds.x, y },
                    crate::geom::Point {
                        x: bounds.x + bounds.width,
                        y,
                    },
                ],
                block_size: DEFAULT_MOSAIC_BLOCK,
            },
        }
    }

    #[test]
    fn mosaic_flattens_cells_under_the_stroke_and_leaves_the_rest() {
        let (mut px, stride) = unique(64, 64);
        let original = px.clone();
        // One dab at (32,32). With block 16 the brush radius is 24, so the only
        // cell centres within reach are (24,24) (40,24) (24,40) (40,40) -- the four
        // cells spanning x 16..48, y 16..48.
        let el = Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 2,
            bounds: Rect::new(8.0, 8.0, 48.0, 48.0),
            stroke: STROKE_ARGB,
            stroke_width: 1.0,
            line_style: Default::default(),
            shape: Shape::Mosaic {
                points: vec![crate::geom::Point { x: 32.0, y: 32.0 }],
                block_size: 16,
            },
        };
        apply_annotations(&mut px, 64, 64, stride, 0.0, 0.0, &[el]);
        let at = |buf: &[u8], x: u32, y: u32| {
            let i = (y * stride + x * 4) as usize;
            buf[i..i + 4].to_vec()
        };
        for y in 0..64u32 {
            for x in 0..64u32 {
                let touched = (16..48).contains(&x) && (16..48).contains(&y);
                if !touched {
                    assert_eq!(at(&px, x, y), at(&original, x, y), "untouched {x},{y}");
                }
            }
        }
        for (cx, cy) in [(16u32, 16u32), (32, 16), (16, 32), (32, 32)] {
            let sample = at(&px, cx, cy);
            for y in cy..cy + 16 {
                for x in cx..cx + 16 {
                    assert_eq!(at(&px, x, y), sample, "cell at {cx},{cy}: {x},{y}");
                }
            }
            assert_ne!(sample, at(&original, cx, cy), "cell {cx},{cy} unchanged");
        }
    }

    /// The point of image-aligned cells: whatever offset the stroke starts at, every
    /// 16x16 cell of the image is either untouched or flattened whole. A grid that
    /// followed the stroke would leave cells half-changed or holding two colours.
    #[test]
    fn mosaic_cells_stay_on_the_image_grid_whatever_the_stroke_offset() {
        for start_x in [16.0, 21.0, 28.0, 33.5] {
            let (mut px, stride) = unique(96, 64);
            let original = px.clone();
            let el = Element {
                stroke_enabled: true,
                fill: None,
                arrow_ends: Default::default(),
                arrow_head: Default::default(),
                id: 2,
                bounds: Rect::new(0.0, 0.0, 96.0, 64.0),
                stroke: STROKE_ARGB,
                stroke_width: 1.0,
                line_style: Default::default(),
                shape: Shape::Mosaic {
                    points: vec![
                        crate::geom::Point {
                            x: start_x,
                            y: 30.0,
                        },
                        crate::geom::Point {
                            x: start_x + 30.0,
                            y: 34.0,
                        },
                    ],
                    block_size: 16,
                },
            };
            apply_annotations(&mut px, 96, 64, stride, 0.0, 0.0, &[el]);
            let at = |buf: &[u8], x: u32, y: u32| {
                let i = (y * stride + x * 4) as usize;
                buf[i..i + 4].to_vec()
            };
            for gy in 0..4u32 {
                for gx in 0..6u32 {
                    let (x0, y0) = (gx * 16, gy * 16);
                    let cells: Vec<Vec<u8>> = (y0..y0 + 16)
                        .flat_map(|y| (x0..x0 + 16).map(move |x| (x, y)))
                        .map(|(x, y)| at(&px, x, y))
                        .collect();
                    let untouched = (y0..y0 + 16)
                        .flat_map(|y| (x0..x0 + 16).map(move |x| (x, y)))
                        .all(|(x, y)| at(&px, x, y) == at(&original, x, y));
                    if untouched {
                        continue;
                    }
                    assert!(
                        cells.windows(2).all(|w| w[0] == w[1]),
                        "start_x {start_x}: cell {gx},{gy} is not uniform"
                    );
                }
            }
        }
    }

    #[test]
    fn annotated_png_differs_from_unmarked_crop() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let (plain, stride) = solid(32, 32, [10, 20, 30, 255]);
        let unmarked = crate::codec::encode_png_bgra(&plain, 32, 32, stride).expect("png");
        let marked = export_annotated_png(
            &plain,
            32,
            32,
            stride,
            0.0,
            0.0,
            &[rect_el(Rect::new(4.0, 4.0, 20.0, 16.0))],
        )
        .expect("annotated png");
        assert_ne!(marked, unmarked);
        assert_eq!(&marked[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    }

    #[test]
    fn mosaic_export_png_differs_and_cells_are_constant() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let (src, stride) = unique(32, 32);
        let unmarked = crate::codec::encode_png_bgra(&src, 32, 32, stride).expect("png");
        let mut marked_px = src.clone();
        apply_annotations(
            &mut marked_px,
            32,
            32,
            stride,
            0.0,
            0.0,
            &[mosaic_el(Rect::new(0.0, 0.0, 32.0, 32.0))],
        );
        let marked = crate::codec::encode_png_bgra(&marked_px, 32, 32, stride).expect("ann");
        assert_ne!(marked, unmarked);
        for by in [0i32, 16] {
            for bx in [0i32, 16] {
                let i0 = (by as u32 * stride + bx as u32 * 4) as usize;
                let sample = marked_px[i0..i0 + 4].to_vec();
                for y in by..by + 16 {
                    for x in bx..bx + 16 {
                        let i = (y as u32 * stride + x as u32 * 4) as usize;
                        assert_eq!(&marked_px[i..i + 4], sample.as_slice());
                    }
                }
            }
        }
    }

    #[test]
    fn mosaic_reaching_past_the_crop_leaves_pixels_opaque() {
        let (mut px, stride) = solid(32, 32, [10, 20, 30, 255]);
        let el = Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 9,
            bounds: Rect::new(-8.0, 8.0, 16.0, 16.0),
            stroke: STROKE_ARGB,
            stroke_width: 1.0,
            line_style: Default::default(),
            shape: Shape::Mosaic {
                points: vec![crate::geom::Point { x: -8.0, y: 16.0 }],
                block_size: 16,
            },
        };
        apply_annotations(&mut px, 32, 32, stride, 0.0, 0.0, &[el]);
        for y in 0..32u32 {
            for x in 0..32u32 {
                let i = (y * stride + x * 4) as usize;
                assert_eq!(px[i + 3], 255, "pixel {x},{y} went transparent");
            }
        }
    }

    #[test]
    fn mask_runs_merge_equal_coverage_and_skip_gaps() {
        let mask = CoverageMask {
            x0: 10,
            y0: 20,
            width: 4,
            height: 2,
            data: vec![0, 7, 7, 0, 3, 0, 9, 9],
        };
        assert_eq!(
            mask.runs(),
            vec![
                CoverageRun {
                    y: 20,
                    x0: 11,
                    x1: 13,
                    alpha: 7,
                },
                CoverageRun {
                    y: 21,
                    x0: 10,
                    x1: 11,
                    alpha: 3,
                },
                CoverageRun {
                    y: 21,
                    x0: 12,
                    x1: 14,
                    alpha: 9,
                },
            ]
        );
    }

    /// The preview draws `element_mask` as quads; the export blends the same mask.
    /// A slanted stroke must therefore have partial coverage at its edges -- the
    /// exact pixels that used to stair-step when the preview scanlined instead.
    #[test]
    fn slanted_arrow_mask_has_soft_edges_in_both_axes() {
        for angle_deg in [0.0_f64, 15.0, 30.0, 45.0, 60.0, 75.0] {
            let rad = angle_deg.to_radians();
            let el = Element {
                stroke_enabled: true,
                fill: None,
                arrow_ends: Default::default(),
                arrow_head: Default::default(),
                id: 5,
                bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
                stroke: 0xFFFF_FFFF,
                stroke_width: 3.0,
                line_style: Default::default(),
                shape: Shape::Arrow {
                    start: crate::geom::Point { x: 8.0, y: 32.0 },
                    end: crate::geom::Point {
                        x: 8.0 + 48.0 * rad.cos(),
                        y: 32.0 + 48.0 * rad.sin(),
                    },
                    head: 12.0,
                },
            };
            let mask = element_mask(&el).expect("arrow mask");
            let partial = mask.data.iter().filter(|&&m| m > 0 && m < 255).count();
            let solid = mask.data.iter().filter(|&&m| m == 255).count();
            assert!(solid > 10, "{angle_deg}°: no solid interior");
            assert!(
                partial > 20,
                "{angle_deg}°: expected soft edge pixels, got {partial}"
            );
            let runs = mask.runs();
            for w in runs.windows(2) {
                if w[0].y == w[1].y {
                    assert!(w[0].x1 <= w[1].x0, "runs overlap: {:?} {:?}", w[0], w[1]);
                }
            }
        }
    }

    /// Preview and export rasterize from one mask: white-on-black export pixels
    /// must equal the mask value the preview turns into quad alpha, per pixel.
    #[test]
    fn preview_mask_matches_export_pixels() {
        let el = Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 6,
            bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
            stroke: 0xFFFF_FFFF,
            stroke_width: 4.0,
            line_style: Default::default(),
            shape: Shape::Pen {
                points: vec![
                    crate::geom::Point { x: 6.0, y: 6.0 },
                    crate::geom::Point { x: 40.0, y: 30.0 },
                    crate::geom::Point { x: 20.0, y: 56.0 },
                ],
            },
        };
        let (mut px, stride) = solid(64, 64, [0, 0, 0, 255]);
        apply_annotations(&mut px, 64, 64, stride, 0.0, 0.0, std::slice::from_ref(&el));
        let mask = element_mask(&el).expect("pen mask");
        for row in 0..mask.height {
            for col in 0..mask.width {
                let m = mask.data[row * mask.width + col];
                let x = (mask.x0 + col as i32) as u32;
                let y = (mask.y0 + row as i32) as u32;
                let i = (y * stride + x * 4) as usize;
                assert_eq!(px[i], m, "pixel {x},{y}: export vs preview mask");
            }
        }
    }

    #[test]
    fn pen_stroke_edges_are_antialiased() {
        let (mut px, stride) = solid(64, 64, [0, 0, 0, 255]);
        let el = Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 7,
            bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
            stroke: 0xFFFF_FFFF,
            stroke_width: 4.0,
            line_style: Default::default(),
            shape: Shape::Pen {
                points: vec![
                    crate::geom::Point { x: 6.0, y: 6.0 },
                    crate::geom::Point { x: 40.0, y: 30.0 },
                    crate::geom::Point { x: 20.0, y: 56.0 },
                ],
            },
        };
        apply_annotations(&mut px, 64, 64, stride, 0.0, 0.0, &[el]);
        let partial = px
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 0 && p[0] < 255)
            .count();
        let solid_px = px.as_chunks::<4>().0.iter().filter(|p| p[0] == 255).count();
        assert!(solid_px > 50, "stroke barely drawn: {solid_px}");
        assert!(partial > 40, "expected soft edge pixels, got {partial}");
    }

    #[test]
    fn filled_arrow_edges_are_antialiased() {
        let (mut px, stride) = solid(64, 32, [0, 0, 0, 255]);
        let el = Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 4,
            bounds: Rect::new(2.0, 2.0, 58.0, 28.0),
            stroke: 0xFFFF_FFFF,
            stroke_width: 3.0,
            line_style: Default::default(),
            shape: Shape::Arrow {
                // Slanted, so every edge crosses pixels partially.
                start: crate::geom::Point { x: 4.0, y: 4.0 },
                end: crate::geom::Point { x: 60.0, y: 28.0 },
                head: 12.0,
            },
        };
        apply_annotations(&mut px, 64, 32, stride, 0.0, 0.0, &[el]);
        let partial = px
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 0 && p[0] < 255)
            .count();
        assert!(partial > 20, "expected soft edge pixels, got {partial}");
    }

    #[test]
    fn filled_arrow_head_is_wider_than_shaft() {
        let (mut px, stride) = solid(64, 32, [0, 0, 0, 255]);
        let el = Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 3,
            bounds: Rect::new(2.0, 2.0, 58.0, 28.0),
            stroke: STROKE_ARGB,
            stroke_width: 3.0,
            line_style: Default::default(),
            shape: Shape::Arrow {
                start: crate::geom::Point { x: 4.0, y: 16.0 },
                end: crate::geom::Point { x: 60.0, y: 16.0 },
                head: 12.0,
            },
        };
        apply_annotations(&mut px, 64, 32, stride, 0.0, 0.0, &[el]);
        let col = |x: u32| {
            (0..32u32)
                .filter(|&y| {
                    let i = (y * stride + x * 4) as usize;
                    px[i] != 0 || px[i + 1] != 0 || px[i + 2] != 0
                })
                .count()
        };
        let tail = col(8);
        let neck = col(40);
        let head = col(46);
        assert!(tail >= 1, "tail too thin: {tail}");
        assert!(
            neck > tail + 2,
            "body should taper: tail {tail} neck {neck}"
        );
        assert!(head > neck, "head {head} should be wider than neck {neck}");
    }

    /// Real glyphs, not the old placeholder block: antialiased edges and a mix
    /// of opaque/transparent pixels, not a solid rectangle.
    #[test]
    fn text_export_draws_real_glyphs() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let (buf, w, h) = text_raster::rasterize("Hi 你好 123", 24.0, 0xFFFF_FFFF).expect("raster");
        assert!(w >= 40, "width too small: {w}");
        assert!(h >= 20, "height too small: {h}");
        let total = (w * h) as usize;
        let mut opaque = 0;
        let mut partial = 0;
        let mut transparent = 0;
        for px in buf.as_chunks::<4>().0 {
            match px[3] {
                255 => opaque += 1,
                0 => transparent += 1,
                _ => partial += 1,
            }
        }
        assert!(opaque > 0, "no opaque pixels");
        assert!(partial > 0, "no antialiased edge pixels");
        assert!(
            opaque < total / 2,
            "looks like a solid block: {opaque}/{total}"
        );
        assert!(
            transparent as f64 > total as f64 * 0.3,
            "too little background: {transparent}/{total}"
        );
    }

    /// The raster lands at the element's bounds and nowhere else in the crop.
    #[test]
    fn text_annotation_lands_in_the_crop() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let (mut px, stride) = solid(200, 64, [0, 0, 0, 255]);
        let original = px.clone();
        let el = Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 10,
            bounds: Rect::new(10.0, 10.0, 140.0, 30.0),
            stroke: 0xFFFF_FFFF,
            stroke_width: 1.0,
            line_style: Default::default(),
            shape: Shape::Text {
                text: "FastPin".to_string(),
                font_size: 20.0,
            },
        };
        apply_annotations(&mut px, 200, 64, stride, 0.0, 0.0, &[el]);
        let at = |buf: &[u8], x: u32, y: u32| {
            let i = (y * stride + x * 4) as usize;
            buf[i..i + 4].to_vec()
        };
        let mut changed_in_box = false;
        for y in 0..64u32 {
            for x in 0..200u32 {
                let inside = (10..150).contains(&x) && (10..40).contains(&y);
                let differs = at(&px, x, y) != at(&original, x, y);
                if inside {
                    changed_in_box |= differs;
                } else if !(0..60).contains(&y) {
                    assert!(!differs, "pixel {x},{y} changed far from the text");
                }
            }
        }
        assert!(changed_in_box, "no pixels changed inside the text bounds");
    }

    fn covered(mask: &CoverageMask) -> usize {
        mask.data.iter().filter(|&&v| v > 0).count()
    }

    #[test]
    fn ellipse_line_styles_produce_different_masks() {
        let base = Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 40,
            bounds: Rect::new(2.0, 2.0, 60.0, 40.0),
            stroke: 0xFFFF_FFFF,
            stroke_width: 3.0,
            line_style: LineStyle::Solid,
            shape: Shape::Ellipse,
        };
        let solid = element_mask(&base).expect("solid ellipse");
        let mut dashed = base.clone();
        dashed.line_style = LineStyle::Dashed;
        let dashed = element_mask(&dashed).expect("dashed ellipse");
        let mut cloud = base.clone();
        cloud.line_style = LineStyle::Cloud;
        let cloud = element_mask(&cloud).expect("cloud ellipse");
        assert!(covered(&solid) > 100, "solid ellipse barely drawn");
        assert!(covered(&dashed) < covered(&solid), "dashed must have gaps");
        assert!(
            covered(&cloud) > covered(&solid),
            "cloud has a longer outline"
        );
    }

    #[test]
    fn path_arrows_render_a_shaft_and_head() {
        for shape in [
            Shape::PolylineArrow {
                points: vec![
                    crate::geom::Point { x: 6.0, y: 30.0 },
                    crate::geom::Point { x: 30.0, y: 8.0 },
                    crate::geom::Point { x: 58.0, y: 30.0 },
                ],
                head: 12.0,
            },
            Shape::CurveArrow {
                points: vec![
                    crate::geom::Point { x: 6.0, y: 30.0 },
                    crate::geom::Point { x: 30.0, y: 8.0 },
                    crate::geom::Point { x: 58.0, y: 30.0 },
                ],
                head: 12.0,
            },
        ] {
            let el = Element {
                stroke_enabled: true,
                fill: None,
                arrow_ends: Default::default(),
                arrow_head: Default::default(),
                id: 41,
                bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
                stroke: 0xFFFF_FFFF,
                stroke_width: 3.0,
                line_style: LineStyle::Solid,
                shape,
            };
            let mask = element_mask(&el).expect("path arrow mask");
            assert!(
                covered(&mask) > 100,
                "arrow barely drawn: {}",
                covered(&mask)
            );
        }
    }

    #[test]
    fn badge_has_no_vector_mask_it_is_drawn_directly() {
        let el = Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 7,
            bounds: Rect::new(8.0, 8.0, 24.0, 24.0),
            stroke: 0xFFFF_0000,
            stroke_width: 1.0,
            line_style: LineStyle::Solid,
            shape: Shape::Badge {
                number: 1,
                style: Default::default(),
                radius: 12.0,
            },
        };
        assert!(element_mask(&el).is_none());
    }

    /// A dash/cloud choice made on a shape must not change freehand or arrow
    /// strokes: only rect/ellipse consult `el.line_style`.
    #[test]
    fn line_style_only_affects_shapes_not_pen_or_arrow() {
        let points = vec![Point { x: 2.0, y: 20.0 }, Point { x: 60.0, y: 20.0 }];
        let pen = |style| Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 1,
            bounds: Rect::new(0.0, 0.0, 64.0, 40.0),
            stroke: 0xFFFF_FFFF,
            stroke_width: 3.0,
            line_style: style,
            shape: Shape::Pen {
                points: points.clone(),
            },
        };
        let solid = element_mask(&pen(LineStyle::Solid)).expect("pen mask");
        assert_eq!(
            solid.data,
            element_mask(&pen(LineStyle::Dashed)).unwrap().data
        );
        assert_eq!(
            solid.data,
            element_mask(&pen(LineStyle::Cloud)).unwrap().data
        );

        let arrow = |style| Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 2,
            bounds: Rect::new(0.0, 0.0, 64.0, 40.0),
            stroke: 0xFFFF_FFFF,
            stroke_width: 3.0,
            line_style: style,
            shape: Shape::Arrow {
                start: Point { x: 2.0, y: 30.0 },
                end: Point { x: 60.0, y: 30.0 },
                head: 12.0,
            },
        };
        let solid = element_mask(&arrow(LineStyle::Solid)).expect("arrow mask");
        assert_eq!(
            solid.data,
            element_mask(&arrow(LineStyle::Dashed)).unwrap().data
        );
        assert_eq!(
            solid.data,
            element_mask(&arrow(LineStyle::Cloud)).unwrap().data
        );

        // ...but an ellipse with the same styles really does differ.
        let ellipse = |style| Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 3,
            bounds: Rect::new(4.0, 4.0, 56.0, 32.0),
            stroke: 0xFFFF_FFFF,
            stroke_width: 3.0,
            line_style: style,
            shape: Shape::Ellipse,
        };
        assert_ne!(
            element_mask(&ellipse(LineStyle::Solid)).unwrap().data,
            element_mask(&ellipse(LineStyle::Dashed)).unwrap().data
        );
    }

    /// The full "click 序号 → drop a bubble → export" chain, driven through the
    /// real toolbar dispatcher instead of poking `Session::tool` directly. This
    /// guards the number tool being wired end to end (button → draft → raster).
    #[test]
    fn number_tool_from_button_click_to_export_draws_a_badge() {
        use crate::overlay::annotate;
        use crate::overlay::editor::{Session, Tool, BADGE_RADIUS};
        use crate::overlay::toolbar;

        // 1. Clicking the 序号 button goes through the shared UI dispatcher.
        let mut session = Session::new();
        let mut text = None;
        let mut color_open = false;
        assert!(annotate::handle_common_toolbar(
            &mut session,
            &mut text,
            &mut color_open,
            toolbar::NUMBER,
        ));
        assert_eq!(session.tool, Tool::Number);
        // Fix a known colour so the disc is unmistakably not the white bg.
        session.stroke = 0xFF00_00FF; // opaque blue (ARGB)

        // 2. A single click (no drag) still drops a bubble.
        let (cx, cy) = (40.0, 40.0);
        session.pointer_pressed(Point { x: cx, y: cy });
        session.pointer_released();
        assert_eq!(session.elements.len(), 1);
        assert!(matches!(
            session.elements[0].shape,
            Shape::Badge { number: 1, .. }
        ));

        // 3. Export to a white BGRA crop; the disc must burn coloured pixels.
        let (w, h) = (80u32, 80u32);
        let mut px = vec![255u8; (w * h * 4) as usize];
        apply_annotations(&mut px, w, h, w * 4, 0.0, 0.0, &session.elements);
        let at = |x: u32, y: u32| -> [u8; 4] {
            let i = (y * w * 4 + x * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        let r = BADGE_RADIUS;
        let mut coloured = 0;
        for y in (cy - r) as i32..=(cy + r) as i32 {
            for x in (cx - r) as i32..=(cx + r) as i32 {
                if (x as f64 - cx).hypot(y as f64 - cy) <= r - 1.0
                    && at(x as u32, y as u32) != [255, 255, 255, 255]
                {
                    coloured += 1;
                }
            }
        }
        assert!(coloured > 100, "badge disc barely drawn: {coloured} px");
        // Well outside the disc the crop stays untouched.
        assert_eq!(at(2, 2), [255, 255, 255, 255]);

        // 4. A second click numbers 2, and undo renumbers the survivor to 1.
        session.pointer_pressed(Point { x: 64.0, y: 64.0 });
        session.pointer_released();
        assert!(matches!(
            session.elements[1].shape,
            Shape::Badge { number: 2, .. }
        ));
        assert!(session.undo());
        assert_eq!(session.elements.len(), 1);
        assert!(matches!(
            session.elements[0].shape,
            Shape::Badge { number: 1, .. }
        ));
    }

    /// The bubble geometry sampled on a pixel grid, exactly the coverage the
    /// export blends: (covered pixels, total coverage).
    fn bubble_pixels(style: BadgeStyle, radius: f64) -> (usize, f64) {
        let r = radius.max(1.0);
        let ring = (r * 0.16).max(1.5);
        let n = (r.ceil() as i32 + 2).max(2);
        let (mut count, mut sum) = (0, 0.0);
        for y in -n..=n {
            for x in -n..=n {
                let cov = badge_coverage(style, x as f64 + 0.5, y as f64 + 0.5, r, ring);
                if cov > 0.0 {
                    count += 1;
                }
                sum += cov;
            }
        }
        (count, sum)
    }

    /// The size slider must grow the bubble's pixel footprint, not just move
    /// its centre.
    #[test]
    fn a_bigger_badge_covers_more_pixels() {
        let (small, small_sum) = bubble_pixels(BadgeStyle::Filled, 8.0);
        let (large, large_sum) = bubble_pixels(BadgeStyle::Filled, 20.0);
        assert!(
            large > small && large_sum > small_sum,
            "small {small} px / {small_sum}, large {large} px / {large_sum}"
        );
    }

    /// Filled disc / hollow ring / square must rasterize differently: the
    /// interior, the ring and the corners discriminate the three.
    #[test]
    fn badge_styles_produce_different_pixels() {
        let (r, ring) = (12.0, (12.0f64 * 0.16).max(1.5));
        let cov = |style, dx: f64, dy: f64| badge_coverage(style, dx, dy, r, ring);
        // Right of the numeral but inside the disc and the square.
        assert!(cov(BadgeStyle::Filled, 9.0, 0.0) > 0.0);
        assert_eq!(
            cov(BadgeStyle::Hollow, 9.0, 0.0),
            0.0,
            "hollow centre is clear"
        );
        assert!(cov(BadgeStyle::Square, 9.0, 0.0) > 0.0);
        // Bounding-box corner: inside the square, outside the disc.
        assert!(cov(BadgeStyle::Square, 9.0, 9.0) > 0.0);
        assert_eq!(cov(BadgeStyle::Filled, 9.0, 9.0), 0.0);
        // The ring itself is painted even though the centre is not.
        assert!(cov(BadgeStyle::Hollow, 11.0, 0.0) > 0.0);
        assert_eq!(cov(BadgeStyle::Filled, 11.0, 0.0), 1.0);
        // Different total footprints, not three names for the same disc.
        let filled = bubble_pixels(BadgeStyle::Filled, r).1;
        let hollow = bubble_pixels(BadgeStyle::Hollow, r).1;
        let square = bubble_pixels(BadgeStyle::Square, r).1;
        assert!(hollow < filled, "ring {hollow} vs disc {filled}");
        assert!(square > filled, "square {square} vs disc {filled}");
    }

    /// Drives the number tool through the real GUI entry points that are
    /// headlessly reachable, instead of poking `Session::tool` and hardcoding
    /// `toolbar::NUMBER` into the dispatcher like the test above:
    ///   production `visible_items` list -> production `toolbar_item_at` hit
    ///   test (this is the button-id dispatch) -> shared dispatcher ->
    ///   `on_active_press` selection guard -> `pointer_pressed` draft ->
    ///   `pointer_released` commit -> export raster.
    /// GPUI `Window`/`Context` mouse-down/up dispatch, the `on_item` closure
    /// and `badge_layer`'s element-tree paint cannot be built in a headless
    /// unit test; `perf::log_diag` (logs/diag.log) covers those on the real
    /// machine (see the report). This test closes the "button id dispatch ->
    /// create_draft" gap the earlier round left open.
    #[test]
    fn number_tool_via_button_hit_test_and_canvas_events_exports_a_badge() {
        use crate::overlay::annotate;
        use crate::overlay::editor::{Session, Tool, BADGE_RADIUS};
        use crate::overlay::toolbar;
        use crate::overlay::{toolbar_items_for, ToolbarAssets};
        use gpui::AssetSource;

        // 1. 序号 is really on the bar for the default config: not filtered as
        //    a line style, not hidden, and its svg is actually served.
        let items = toolbar_items_for(Tool::Select, &[]);
        let number_idx = items
            .iter()
            .position(|id| *id == toolbar::NUMBER)
            .expect("序号 button must be visible for the default config");
        let icon = toolbar::icon_path(toolbar::NUMBER).expect("序号 must have an svg");
        assert!(
            ToolbarAssets.load(icon).ok().flatten().is_some(),
            "{icon} must be loadable"
        );

        // 2. A real click at the button's centre goes through the production
        //    hit-test: this is the id that `on_item` dispatches to the host.
        let (bar_w, bar_h) = toolbar::bar_size(items.len());
        let geom = Rect::new(0.0, 0.0, bar_w, bar_h);
        let click_x = toolbar::item_center_x(geom, number_idx);
        let dispatched = annotate::toolbar_item_at(
            Point {
                x: click_x,
                y: bar_h / 2.0,
            },
            geom,
            &items,
        );
        assert_eq!(dispatched, Some(toolbar::NUMBER));

        // 3. The shared UI dispatcher (what both `handle_toolbar` and the pin
        //    host call) turns that dispatched id into Tool::Number.
        let mut session = Session::new();
        let mut text = None;
        let mut color_open = false;
        assert!(annotate::handle_common_toolbar(
            &mut session,
            &mut text,
            &mut color_open,
            dispatched.unwrap(),
        ));
        assert_eq!(session.tool, Tool::Number);
        session.stroke = 0xFF00_00FF; // opaque blue, so the disc is unmistakable

        // 4. Canvas mouse-down: `on_active_press` only starts a draft when the
        //    press is inside the selection. Use the real Rect::contains guard
        //    and the real Session::pointer_pressed that the handler calls.
        let selection = Rect::new(0.0, 0.0, 200.0, 200.0);
        let press = Point { x: 40.0, y: 40.0 };
        assert!(selection.contains(press));
        session.pointer_pressed(press);
        assert!(matches!(
            session.draft.as_ref().map(|d| &d.shape),
            Some(Shape::Badge { .. })
        ));
        assert!(session.draft.as_ref().unwrap().bounds.width >= 2.0 * BADGE_RADIUS - 1.0);

        // 5. Canvas mouse-up: `on_up` -> pointer_released commits (the badge's
        //    fixed disc bounds never trip the `tiny` discard).
        session.pointer_released();
        assert_eq!(session.elements.len(), 1);
        assert!(matches!(
            session.elements[0].shape,
            Shape::Badge { number: 1, .. }
        ));

        // 6. Export must burn a visible disc, same as the live preview draws.
        let (w, h) = (80u32, 80u32);
        let mut px = vec![255u8; (w * h * 4) as usize];
        apply_annotations(&mut px, w, h, w * 4, 0.0, 0.0, &session.elements);
        let at = |x: u32, y: u32| -> [u8; 4] {
            let i = (y * w * 4 + x * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        let mut coloured = 0;
        for y in 28..=52 {
            for x in 28..=52 {
                if at(x, y) != [255, 255, 255, 255] {
                    coloured += 1;
                }
            }
        }
        assert!(coloured > 100, "badge disc barely drawn: {coloured} px");
    }

    /// The three arrows are picked inline on the properties row now (no
    /// popup), but each button must still select its real tool and draw the
    /// very same arrow shape as before the merge.
    #[test]
    fn arrow_variants_are_inline_and_still_draw() {
        use crate::overlay::annotate;
        use crate::overlay::editor::{Session, Tool};
        use crate::overlay::toolbar;
        use crate::overlay::toolbar_items_for;

        // The merged button is on the bar; the three raw ids are not.
        let items = toolbar_items_for(Tool::Select, &[]);
        assert!(items.contains(&toolbar::ARROW_GROUP));
        assert!(!items.contains(&toolbar::ARROW));
        assert!(!items.contains(&toolbar::POLYLINE_ARROW));
        assert!(!items.contains(&toolbar::CURVE_ARROW));
        assert_eq!(
            toolbar::icon_path(toolbar::ARROW_GROUP),
            Some("toolbar/arrowGroup.svg")
        );

        let mut s = Session::new();
        let mut text = None;
        let mut color_open = false;
        // The bar button selects the default variant directly.
        assert!(annotate::handle_common_toolbar(
            &mut s,
            &mut text,
            &mut color_open,
            toolbar::ARROW_GROUP,
        ));
        assert_eq!(s.tool, Tool::Arrow);

        // Each inline variant selects its tool, keeps the arrow row, and a
        // real canvas drag still produces the matching arrow shape.
        for (id, want) in [
            (toolbar::ARROW, Tool::Arrow),
            (toolbar::POLYLINE_ARROW, Tool::PolylineArrow),
            (toolbar::CURVE_ARROW, Tool::CurveArrow),
        ] {
            assert!(annotate::handle_common_toolbar(
                &mut s,
                &mut text,
                &mut color_open,
                id,
            ));
            assert_eq!(s.tool, want);
            assert!(annotate::prop_cells(s.tool)
                .contains(&toolbar::PropCell::Btn(toolbar::ARROW)));
            s.pointer_pressed(Point { x: 5.0, y: 5.0 });
            s.pointer_moved(Point { x: 20.0, y: 20.0 });
            s.pointer_released();
            let el = s.elements.last().unwrap();
            match want {
                Tool::Arrow => assert!(matches!(el.shape, Shape::Arrow { .. })),
                Tool::PolylineArrow => assert!(matches!(el.shape, Shape::PolylineArrow { .. })),
                Tool::CurveArrow => assert!(matches!(el.shape, Shape::CurveArrow { .. })),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn watermark_origin_places_each_grid_corner_inside_the_margin() {
        let margin = 12i64;
        let canvas = (200u32, 100u32);
        let text = (40u32, 20u32);
        for pos in WatermarkPosition::ALL {
            let (x, y) = watermark_origin(canvas, text, pos, margin);
            // No negative coords, and the layer never runs off the canvas.
            assert!(x >= 0, "{pos:?} x {x}");
            assert!(y >= 0, "{pos:?} y {y}");
            assert!(x + text.0 as i64 <= canvas.0 as i64);
            assert!(y + text.1 as i64 <= canvas.1 as i64);
        }
        // BottomRight with a 40x20 layer: x = margin + free_w, y = margin + free_h.
        let (x, y) = watermark_origin(canvas, text, WatermarkPosition::BottomRight, margin);
        assert_eq!((x, y), (148, 68));
    }

    /// The empty default config must be a no-op: a capture with no watermark
    /// configured stays byte-identical.
    #[test]
    fn watermark_empty_config_leaves_the_image_untouched() {
        let (mut px, stride) = solid(120, 60, [10, 20, 30, 255]);
        let before = px.clone();
        {
            let mut buf = Bgra {
                pixels: &mut px,
                width: 120,
                height: 60,
                stride,
            };
            apply_watermark(&mut buf, 120, 60, &Watermark::default());
        }
        assert_eq!(px, before, "empty watermark must not change any pixel");
    }

    // ---- second row: fill / stroke / width / alpha / arrow heads ----------

    fn px_at(buf: &[u8], stride: u32, x: u32, y: u32) -> [u8; 4] {
        let i = (y * stride + x * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    fn mask_at(mask: &CoverageMask, x: i32, y: i32) -> u8 {
        let col = x - mask.x0;
        let row = y - mask.y0;
        if col < 0 || row < 0 || col >= mask.width as i32 || row >= mask.height as i32 {
            return 0;
        }
        mask.data[row as usize * mask.width + col as usize]
    }

    /// Fill paints the interior; the outline sits on top of it; turning the
    /// stroke off leaves a fill with no ring.
    #[test]
    fn fill_paints_the_interior_and_stroke_off_removes_the_ring() {
        let bounds = Rect::new(8.0, 8.0, 32.0, 24.0);
        let blue = 0xFF00_00FFu32;
        let red = 0xFFFF_0000u32;

        let mut both = rect_el(bounds);
        both.fill = Some(blue);
        both.stroke = red;
        let inside = {
            let (mut px, stride) = solid(64, 64, [255, 255, 255, 255]);
            apply_annotations(&mut px, 64, 64, stride, 0.0, 0.0, &[both.clone()]);
            px_at(&px, stride, 24, 20)
        };
        assert_eq!(inside, [255, 0, 0, 255], "interior must be the blue fill");

        let edge = {
            let (mut px, stride) = solid(64, 64, [255, 255, 255, 255]);
            apply_annotations(&mut px, 64, 64, stride, 0.0, 0.0, &[both]);
            px_at(&px, stride, 8, 20)
        };
        assert_eq!(edge, [0, 0, 255, 255], "left edge must be the red outline");

        let mut fill_only = rect_el(bounds);
        fill_only.fill = Some(blue);
        fill_only.stroke_enabled = false;
        let (mut px, stride) = solid(64, 64, [255, 255, 255, 255]);
        apply_annotations(&mut px, 64, 64, stride, 0.0, 0.0, &[fill_only]);
        assert_eq!(px_at(&px, stride, 24, 20), [255, 0, 0, 255]);
        // x=4 is outside the bounds; no ring leaks there.
        assert_eq!(px_at(&px, stride, 4, 20), [255, 255, 255, 255]);

        // Stroke on but no fill: the interior stays untouched.
        let mut ring_only = rect_el(bounds);
        ring_only.fill = None;
        let (mut px, stride) = solid(64, 64, [255, 255, 255, 255]);
        apply_annotations(&mut px, 64, 64, stride, 0.0, 0.0, &[ring_only]);
        assert_eq!(px_at(&px, stride, 24, 20), [255, 255, 255, 255]);
    }

    /// A wider stroke must cover strictly more pixels than a thin one.
    #[test]
    fn stroke_width_changes_the_covered_pixel_count() {
        let count = |width: f64| {
            let mut el = rect_el(Rect::new(10.0, 10.0, 40.0, 30.0));
            el.stroke_width = width;
            covered(&element_mask(&el).expect("rect mask"))
        };
        let one = count(1.0);
        let four = count(4.0);
        let eight = count(8.0);
        assert!(one > 0);
        assert!(four >= one * 2, "1px {one} vs 4px {four}");
        assert!(eight > four, "4px {four} vs 8px {eight}");
    }

    /// 50 % red over white: red stays 255, green/blue land at 127, and the
    /// saved PNG stays opaque (alpha is composited, not mixed).
    #[test]
    fn semi_transparent_stroke_blends_source_over() {
        let mut el = rect_el(Rect::new(10.0, 10.0, 40.0, 30.0));
        el.stroke = 0x80FF_0000; // ~50 % red
        el.stroke_width = 4.0;
        let (mut px, stride) = solid(64, 64, [255, 255, 255, 255]);
        apply_annotations(&mut px, 64, 64, stride, 0.0, 0.0, &[el.clone()]);
        // x=10 is the fully covered core of the left edge.
        assert_eq!(px_at(&px, stride, 10, 25), [127, 127, 255, 255]);
        // Untouched interior proves the blend is confined to the outline.
        assert_eq!(px_at(&px, stride, 30, 25), [255, 255, 255, 255]);
    }

    /// `arrow_ends` moves the head between the two tips. The shaft alone is
    /// ~3 px wide; the sampled rows are outside that, so only a head reaches.
    #[test]
    fn arrow_ends_place_heads_at_the_requested_ends() {
        let head_at = |ends, x: i32| {
            let mut el = Element {
                id: 21,
                bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
                stroke: 0xFFFF_FFFF,
                stroke_width: 3.0,
                line_style: LineStyle::Solid,
                shape: Shape::Arrow {
                    start: Point { x: 10.0, y: 32.0 },
                    end: Point { x: 54.0, y: 32.0 },
                    head: 12.0,
                },
                stroke_enabled: true,
                fill: None,
                arrow_ends: ArrowEnds::End,
                arrow_head: ArrowHead::Filled,
            };
            el.arrow_ends = ends;
            let mask = element_coverage(&el).stroke.expect("arrow mask");
            mask_at(&mask, x, 36) > 0
        };
        assert!(!head_at(ArrowEnds::End, 20), "start has no head");
        assert!(head_at(ArrowEnds::End, 44), "end head missing");
        assert!(head_at(ArrowEnds::Start, 20));
        assert!(!head_at(ArrowEnds::Start, 44));
        assert!(head_at(ArrowEnds::Both, 20) && head_at(ArrowEnds::Both, 44));
        assert!(!head_at(ArrowEnds::Neither, 20) && !head_at(ArrowEnds::Neither, 44));
        // Neither still leaves the shaft: the axis pixel is covered.
        let el = Element {
            id: 22,
            bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
            stroke: 0xFFFF_FFFF,
            stroke_width: 3.0,
            line_style: LineStyle::Solid,
            shape: Shape::Arrow {
                start: Point { x: 10.0, y: 32.0 },
                end: Point { x: 54.0, y: 32.0 },
                head: 12.0,
            },
            stroke_enabled: true,
            fill: None,
            arrow_ends: ArrowEnds::Neither,
            arrow_head: ArrowHead::Filled,
        };
        let mask = element_coverage(&el).stroke.expect("shaft mask");
        assert!(mask_at(&mask, 30, 32) > 0, "shaft disappeared");
    }

    /// The three head styles must produce visibly different coverage.
    #[test]
    fn arrow_head_styles_produce_different_masks() {
        let build = |head: ArrowHead| {
            let mut el = Element {
                id: 23,
                bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
                stroke: 0xFFFF_FFFF,
                stroke_width: 3.0,
                line_style: LineStyle::Solid,
                shape: Shape::PolylineArrow {
                    points: vec![
                        Point { x: 8.0, y: 40.0 },
                        Point { x: 32.0, y: 16.0 },
                        Point { x: 56.0, y: 40.0 },
                    ],
                    head: 12.0,
                },
                stroke_enabled: true,
                fill: None,
                arrow_ends: ArrowEnds::End,
                arrow_head: ArrowHead::Filled,
            };
            el.arrow_head = head;
            let mask = element_coverage(&el).stroke.expect("head mask");
            let n = covered(&mask);
            (mask.data, n)
        };
        let (filled, filled_n) = build(ArrowHead::Filled);
        let (open, open_n) = build(ArrowHead::Open);
        let (bar, bar_n) = build(ArrowHead::Bar);
        assert_ne!(filled, open);
        assert_ne!(filled, bar);
        assert_ne!(open, bar);
        assert!(
            filled_n > 100 && open_n > 100 && bar_n > 100,
            "every head must draw: {filled_n}/{open_n}/{bar_n}"
        );
    }

    /// Switching polyline <-> curve must still export two distinct paths.
    #[test]
    fn polyline_and_curve_arrows_export_distinct_paths() {
        let points = vec![
            Point { x: 8.0, y: 44.0 },
            Point { x: 24.0, y: 12.0 },
            Point { x: 40.0, y: 52.0 },
            Point { x: 58.0, y: 20.0 },
        ];
        let render = |shape: Shape| {
            let el = Element {
                id: 24,
                bounds: Rect::new(0.0, 0.0, 64.0, 64.0),
                stroke: 0xFFFF_FFFF,
                stroke_width: 3.0,
                line_style: LineStyle::Solid,
                shape,
                stroke_enabled: true,
                fill: None,
                arrow_ends: ArrowEnds::End,
                arrow_head: ArrowHead::Filled,
            };
            let mut px = vec![0u8; 64 * 64 * 4];
            for b in px.chunks_mut(4) {
                b.copy_from_slice(&[0, 0, 0, 255]);
            }
            apply_annotations(&mut px, 64, 64, 64 * 4, 0.0, 0.0, &[el]);
            px
        };
        let poly = render(Shape::PolylineArrow {
            points: points.clone(),
            head: 12.0,
        });
        let curve = render(Shape::CurveArrow { points, head: 12.0 });
        assert!(poly.iter().any(|&b| b != 0), "polyline drew nothing");
        assert!(curve.iter().any(|&b| b != 0), "curve drew nothing");
        assert_ne!(poly, curve, "polyline and curve must differ");
    }

    /// A text watermark rasterizes through DirectWrite and lands only in the
    /// configured 9-grid corner (bottom-right here), never far away.
    #[test]
    fn watermark_text_burns_pixels_in_the_chosen_corner() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let (mut px, stride) = solid(400, 200, [0, 0, 0, 255]);
        let before = px.clone();
        let wm = Watermark {
            text: "W".into(),
            opacity: 100,
            position: "bottomRight".into(),
            ..Default::default()
        };
        {
            let mut buf = Bgra {
                pixels: &mut px,
                width: 400,
                height: 200,
                stride,
            };
            apply_watermark(&mut buf, 400, 200, &wm);
        }
        let at = |buf: &[u8], x: u32, y: u32| {
            let i = (y * stride + x * 4) as usize;
            buf[i..i + 4].to_vec()
        };
        let mut changed_corner = false;
        for y in 140..200u32 {
            for x in 340..400u32 {
                changed_corner |= at(&px, x, y) != at(&before, x, y);
            }
        }
        assert!(
            changed_corner,
            "no pixels burned in the bottom-right corner"
        );
        for y in 0..100u32 {
            for x in 0..200u32 {
                assert!(
                    at(&px, x, y) == at(&before, x, y),
                    "pixel {x},{y} changed far from the watermark"
                );
            }
        }
    }

    fn head_el(shape: Shape, thick: f64) -> Element {
        Element {
            id: 91,
            bounds: Rect::new(0.0, 0.0, 240.0, 180.0),
            stroke: 0xFFFF_FFFF,
            stroke_width: thick,
            line_style: LineStyle::Solid,
            shape,
            stroke_enabled: true,
            fill: None,
            arrow_ends: ArrowEnds::End,
            arrow_head: ArrowHead::Filled,
        }
    }

    fn shaft_of(shape: &Shape) -> Vec<Point> {
        match shape {
            Shape::PolylineArrow { points, .. } => points.clone(),
            Shape::CurveArrow { points, .. } => crate::overlay::editor::smooth_polyline(points),
            _ => unreachable!("path arrow"),
        }
    }

    /// Outward direction of the last real segment of the stroke.
    fn last_leg(shaft: &[Point]) -> (f64, f64) {
        for w in shaft.windows(2).rev() {
            let (dx, dy) = (w[1].x - w[0].x, w[1].y - w[0].y);
            let len = dx.hypot(dy);
            if len >= 1.0 {
                return (dx / len, dy / len);
            }
        }
        (1.0, 0.0)
    }

    /// (gap to the nearest painted pixel, how far paint reaches past the tip
    /// along `dir`). Only pixels within one head length of the tip count: a bent
    /// stroke's own body can sit far along the end direction without being part
    /// of the head.
    fn tip_metrics(
        mask: &CoverageMask,
        tip: Point,
        dir: (f64, f64),
        radius: f64,
    ) -> (f64, f64) {
        let mut gap = f64::MAX;
        let mut past = f64::MIN;
        for row in 0..mask.height {
            for col in 0..mask.width {
                if mask.data[row * mask.width + col] == 0 {
                    continue;
                }
                let px = mask.x0 as f64 + col as f64 + 0.5;
                let py = mask.y0 as f64 + row as f64 + 0.5;
                let dist = (px - tip.x).hypot(py - tip.y);
                if dist > radius {
                    continue;
                }
                gap = gap.min(dist);
                past = past.max((px - tip.x) * dir.0 + (py - tip.y) * dir.1);
            }
        }
        (gap, past)
    }

    /// A filled head must end exactly on the last path point: the endpoint is
    /// painted and no pixel reaches more than 1 px past it (the old round cap
    /// used to bulge out there, leaving the tip buried behind the line end).
    #[test]
    fn polyline_filled_head_tip_meets_the_endpoint() {
        let mut points = vec![Point { x: 30.0, y: 90.0 }];
        for i in 1..=20 {
            points.push(Point {
                x: 30.0 + i as f64 * 3.0,
                y: 90.0,
            });
        }
        for i in 0..4 {
            points.push(Point {
                x: 90.0 + i as f64 * 2.0,
                y: 90.0 - i as f64 * 3.0,
            });
        }
        for thick in [2.0, 4.0, 6.0] {
            let shape = Shape::PolylineArrow {
                points: points.clone(),
                head: 12.0,
            };
            let tip = *points.last().expect("tip");
            let mask = element_coverage(&head_el(shape, thick))
                .stroke
                .expect("arrow mask");
            let (gap, past) = tip_metrics(&mask, tip, last_leg(&points), 12.0);
            assert!(gap <= 1.0, "tip {gap:.2} px from paint at {thick} px");
            assert!(past <= 1.0, "paint {past:.2} px past the tip at {thick} px");
        }
    }

    /// Same contract for the spline: Catmull-Rom ends on the last control, and
    /// the head has to land there too.
    #[test]
    fn curve_filled_head_tip_meets_the_endpoint() {
        let controls = vec![
            Point { x: 40.0, y: 150.0 },
            Point { x: 60.0, y: 40.0 },
            Point { x: 78.0, y: 30.0 },
            Point { x: 86.0, y: 60.0 },
        ];
        for thick in [2.0, 4.0, 6.0] {
            let shape = Shape::CurveArrow {
                points: controls.clone(),
                head: 12.0,
            };
            let shaft = shaft_of(&shape);
            let tip = *shaft.last().expect("tip");
            let mask = element_coverage(&head_el(shape, thick))
                .stroke
                .expect("arrow mask");
            let (gap, past) = tip_metrics(&mask, tip, last_leg(&shaft), 12.0);

            assert!(gap <= 1.0, "tip {gap:.2} px from paint at {thick} px");
            assert!(past <= 1.0, "paint {past:.2} px past the tip at {thick} px");
        }
    }

    /// Any of the up to four pixels touching a point is painted.
    fn painted_near(mask: &CoverageMask, x: f64, y: f64) -> bool {
        let x0 = x.floor() as i32;
        let y0 = y.floor() as i32;
        [(x0, y0), (x0 - 1, y0), (x0, y0 - 1), (x0 - 1, y0 - 1)]
            .iter()
            .any(|&(px, py)| mask_at(mask, px, py) > 0)
    }

    /// Trimming the shaft under the head must not open a seam: the stroke is
    /// painted all the way from the head base to the tip along the last leg.
    #[test]
    fn no_gap_between_shaft_and_head() {
        let poly = Shape::PolylineArrow {
            points: vec![
                Point { x: 20.0, y: 120.0 },
                Point { x: 80.0, y: 40.0 },
                Point { x: 140.0, y: 100.0 },
            ],
            head: 12.0,
        };
        let curve = Shape::CurveArrow {
            points: vec![
                Point { x: 20.0, y: 120.0 },
                Point { x: 70.0, y: 30.0 },
                Point { x: 120.0, y: 130.0 },
                Point { x: 160.0, y: 70.0 },
            ],
            head: 12.0,
        };
        for shape in [poly, curve] {
            for thick in [2.0, 4.0, 6.0] {
                let shaft = shaft_of(&shape);
                let tip = *shaft.last().expect("tip");
                let (ux, uy) = last_leg(&shaft);
                let frame = head_frame(&shaft, true, 12.0, thick).expect("head");
                let mask = element_coverage(&head_el(shape.clone(), thick))
                    .stroke
                    .expect("arrow mask");
                // Walk the final leg from just inside the base up to the tip.
                let mut d = 0.5;
                while d <= frame.len + 0.5 {
                    let x = tip.x - ux * d;
                    let y = tip.y - uy * d;
                    assert!(
                        painted_near(&mask, x, y),
                        "hole on the centreline {d:.1} px from the tip ({thick} px)"
                    );
                    // The base half-width is `len/2`, so the sides are only
                    // covered where the head is wider than the stroke.
                    let side = (thick * 0.4).min(d * 0.4 - 0.5);
                    if side > 0.0 {
                        for s in [side, -side] {
                            let sx = x + -uy * s;
                            let sy = y + ux * s;
                            assert!(
                                painted_near(&mask, sx, sy),
                                "hole beside the shaft {d:.1} px from the tip"
                            );
                        }
                    }
                    d += 0.5;
                }
            }
        }
    }
}
