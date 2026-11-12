//! Annotation document, history, and pointer FSM.
//! Ported from C# EditorHistory / OverlayAnnotationController.

use crate::geom::{Point, Rect};

pub const DEFAULT_STROKE_WIDTH: f64 = 4.0;
pub const DEFAULT_MOSAIC_BLOCK: i32 = 16;
/// Physical px; the overlay draft and the export raster both use it.
pub const TEXT_FONT_SIZE: f64 = 20.0;
/// Radius of a badge (number bubble) in image px; mirrors FastPin's 11 px disc.
pub const BADGE_RADIUS: f64 = 12.0;
/// Font size of the badge numeral in image px.
pub const BADGE_FONT_SIZE: f64 = 14.0;

/// Numeral size for a bubble of `radius`, so the digit grows with the disc.
pub fn badge_font_size(radius: f64) -> f64 {
    BADGE_FONT_SIZE * (radius.max(1.0) / BADGE_RADIUS)
}
const MIN_KEEP: f64 = 2.0;

/// Percent opacity (0..=100) to the ARGB alpha byte.
fn alpha_channel(percent: u8) -> u32 {
    (percent.min(100) as u32 * 255 / 100) & 0xFF
}

/// Line style of a stroke: solid, dashed, or the revision cloud (云线).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineStyle {
    #[default]
    Solid,
    Dashed,
    Cloud,
}

impl LineStyle {
    pub const ALL: [Self; 3] = [Self::Solid, Self::Dashed, Self::Cloud];

    pub fn id(self) -> &'static str {
        match self {
            Self::Solid => super::toolbar::LINE_SOLID,
            Self::Dashed => super::toolbar::LINE_DASHED,
            Self::Cloud => super::toolbar::LINE_CLOUD,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            super::toolbar::LINE_SOLID => Self::Solid,
            super::toolbar::LINE_DASHED => Self::Dashed,
            super::toolbar::LINE_CLOUD => Self::Cloud,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Solid => "实线",
            Self::Dashed => "虚线",
            Self::Cloud => "云线",
        }
    }
}

/// Which ends of an arrow carry a head.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArrowEnds {
    Start,
    #[default]
    End,
    Both,
    Neither,
}

impl ArrowEnds {
    pub const ALL: [Self; 4] = [Self::Start, Self::End, Self::Both, Self::Neither];

    pub fn id(self) -> &'static str {
        match self {
            Self::Start => super::toolbar::PROP_ARROW_START,
            Self::End => super::toolbar::PROP_ARROW_END,
            Self::Both => super::toolbar::PROP_ARROW_BOTH,
            Self::Neither => super::toolbar::PROP_ARROW_NONE,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            super::toolbar::PROP_ARROW_START => Self::Start,
            super::toolbar::PROP_ARROW_END => Self::End,
            super::toolbar::PROP_ARROW_BOTH => Self::Both,
            super::toolbar::PROP_ARROW_NONE => Self::Neither,
            _ => return None,
        })
    }

    pub fn has_start(self) -> bool {
        matches!(self, Self::Start | Self::Both)
    }

    pub fn has_end(self) -> bool {
        matches!(self, Self::End | Self::Both)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "起点箭头",
            Self::End => "终点箭头",
            Self::Both => "两端箭头",
            Self::Neither => "无箭头",
        }
    }
}

/// Arrow head shape. `Filled` is the existing QQ-style solid wedge; `Open`
/// is an open V; `Bar` is a perpendicular T stroke.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArrowHead {
    #[default]
    Filled,
    Open,
    Bar,
}

impl ArrowHead {
    pub const ALL: [Self; 3] = [Self::Filled, Self::Open, Self::Bar];

    pub fn id(self) -> &'static str {
        match self {
            Self::Filled => super::toolbar::PROP_HEAD_FILLED,
            Self::Open => super::toolbar::PROP_HEAD_OPEN,
            Self::Bar => super::toolbar::PROP_HEAD_BAR,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            super::toolbar::PROP_HEAD_FILLED => Self::Filled,
            super::toolbar::PROP_HEAD_OPEN => Self::Open,
            super::toolbar::PROP_HEAD_BAR => Self::Bar,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Filled => "实心箭头",
            Self::Open => "开口箭头",
            Self::Bar => "线条箭头",
        }
    }
}

/// Bubble appearance of the number tool: a filled disc, a ring, or a square.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BadgeStyle {
    #[default]
    Filled,
    Hollow,
    Square,
}

impl BadgeStyle {
    pub const ALL: [Self; 3] = [Self::Filled, Self::Hollow, Self::Square];

    pub fn id(self) -> &'static str {
        match self {
            Self::Filled => super::toolbar::PROP_BADGE_FILLED,
            Self::Hollow => super::toolbar::PROP_BADGE_HOLLOW,
            Self::Square => super::toolbar::PROP_BADGE_SQUARE,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            super::toolbar::PROP_BADGE_FILLED => Self::Filled,
            super::toolbar::PROP_BADGE_HOLLOW => Self::Hollow,
            super::toolbar::PROP_BADGE_SQUARE => Self::Square,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Filled => "实心圆",
            Self::Hollow => "空心圆",
            Self::Square => "方形",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Rect,
    Ellipse,
    Arrow {
        start: Point,
        end: Point,
        head: f64,
    },
    PolylineArrow {
        points: Vec<Point>,
        head: f64,
    },
    CurveArrow {
        points: Vec<Point>,
        head: f64,
    },
    Pen {
        points: Vec<Point>,
    },
    Mosaic {
        points: Vec<Point>,
        block_size: i32,
    },
    Text {
        text: String,
        font_size: f64,
    },
    /// Number bubble at `bounds`'s centre carrying `number`. `radius` is the
    /// outer size in image px (older files defaulted to `BADGE_RADIUS`), and
    /// `style` picks the disc/ring/square look. Placed by a single click;
    /// never has a path, so `has_path` is false.
    Badge {
        number: u32,
        style: BadgeStyle,
        radius: f64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Element {
    pub id: u64,
    pub bounds: Rect,
    /// Full ARGB: the high byte is the opacity chosen on the properties row.
    pub stroke: u32,
    pub stroke_width: f64,
    pub line_style: LineStyle,
    pub shape: Shape,
    /// Closed shapes and arrows can hide their outline / shaft entirely.
    pub stroke_enabled: bool,
    /// Interior fill for rect / ellipse, in ARGB. `None` leaves it hollow.
    pub fill: Option<u32>,
    /// Which ends of an arrow carry a head.
    pub arrow_ends: ArrowEnds,
    /// Shape of the arrow head.
    pub arrow_head: ArrowHead,
}

impl Element {
    fn move_by(&mut self, dx: f64, dy: f64) {
        self.bounds.x += dx;
        self.bounds.y += dy;
        match &mut self.shape {
            Shape::Arrow { start, end, .. } => {
                start.x += dx;
                start.y += dy;
                end.x += dx;
                end.y += dy;
            }
            Shape::Pen { points }
            | Shape::Mosaic { points, .. }
            | Shape::PolylineArrow { points, .. }
            | Shape::CurveArrow { points, .. } => {
                for p in points {
                    p.x += dx;
                    p.y += dy;
                }
            }
            _ => {}
        }
    }

    /// The element mapped from one pixel space to another by a uniform
    /// scale about the origin: geometry, stroke width, mosaic block and font
    /// size all follow, so a mark drawn over a zoomed pin lands on the
    /// native pixels at the same relative size.
    pub fn scaled(&self, k: f64) -> Element {
        let mut el = self.clone();
        el.bounds = Rect::new(
            self.bounds.x * k,
            self.bounds.y * k,
            self.bounds.width * k,
            self.bounds.height * k,
        );
        el.stroke_width = self.stroke_width * k;
        match &mut el.shape {
            Shape::Arrow { start, end, head } => {
                *start = Point {
                    x: start.x * k,
                    y: start.y * k,
                };
                *end = Point {
                    x: end.x * k,
                    y: end.y * k,
                };
                *head *= k;
            }
            Shape::Pen { points } => {
                for p in points {
                    p.x *= k;
                    p.y *= k;
                }
            }
            Shape::PolylineArrow { points, head } | Shape::CurveArrow { points, head } => {
                for p in points {
                    p.x *= k;
                    p.y *= k;
                }
                *head *= k;
            }
            Shape::Mosaic { points, block_size } => {
                for p in points {
                    p.x *= k;
                    p.y *= k;
                }
                *block_size = ((*block_size as f64 * k).round() as i32).max(2);
            }
            Shape::Text { font_size, .. } => *font_size *= k,
            Shape::Badge { radius, .. } => *radius *= k,
            Shape::Rect | Shape::Ellipse => {}
        }
        el
    }

    /// Freehand shapes are worth keeping even when the drag never moved.
    fn has_path(&self) -> bool {
        matches!(
            self.shape,
            Shape::Pen { .. }
                | Shape::Mosaic { .. }
                | Shape::PolylineArrow { .. }
                | Shape::CurveArrow { .. }
        )
    }

    pub fn hit_test(&self, p: Point) -> bool {
        match &self.shape {
            Shape::Arrow { start, end, .. } => {
                dist_to_segment(p, *start, *end) <= self.stroke_width.max(4.0)
            }
            Shape::Pen { points }
            | Shape::PolylineArrow { points, .. }
            | Shape::CurveArrow { points, .. } => hit_path(p, points, self.stroke_width.max(4.0)),
            Shape::Mosaic { points, block_size } => {
                hit_path(p, points, mosaic_brush_radius(*block_size))
            }
            _ => self.bounds.contains(p),
        }
    }
}

fn hit_path(p: Point, points: &[Point], tolerance: f64) -> bool {
    match points.len() {
        0 => false,
        1 => dist_to_segment(p, points[0], points[0]) <= tolerance,
        _ => points
            .windows(2)
            .any(|w| dist_to_segment(p, w[0], w[1]) <= tolerance),
    }
}

pub(super) fn dist_to_segment(p: Point, a: Point, b: Point) -> f64 {
    let vx = b.x - a.x;
    let vy = b.y - a.y;
    let len2 = vx * vx + vy * vy;
    if len2 <= f64::EPSILON {
        let dx = p.x - a.x;
        let dy = p.y - a.y;
        return (dx * dx + dy * dy).sqrt();
    }
    let t = ((p.x - a.x) * vx + (p.y - a.y) * vy) / len2;
    let t = t.clamp(0.0, 1.0);
    let qx = a.x + t * vx;
    let qy = a.y + t * vy;
    let ex = p.x - qx;
    let ey = p.y - qy;
    (ex * ex + ey * ey).sqrt()
}

#[derive(Clone, Debug)]
enum Command {
    Add(Element),
    Clear { snapshot: Vec<Element> },
    Move { id: u64, dx: f64, dy: f64 },
}

#[derive(Debug)]
pub struct History {
    undo: Vec<Command>,
    redo: Vec<Command>,
    max_depth: usize,
}

impl History {
    pub fn new(max_depth: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            max_depth: max_depth.max(1),
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn execute(&mut self, doc: &mut Vec<Element>, cmd: Command) {
        apply(doc, &cmd, false);
        self.undo.push(cmd);
        if self.undo.len() > self.max_depth {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn undo(&mut self, doc: &mut Vec<Element>) -> bool {
        let Some(cmd) = self.undo.pop() else {
            return false;
        };
        apply(doc, &cmd, true);
        self.redo.push(cmd);
        true
    }

    pub fn redo(&mut self, doc: &mut Vec<Element>) -> bool {
        let Some(cmd) = self.redo.pop() else {
            return false;
        };
        apply(doc, &cmd, false);
        self.undo.push(cmd);
        true
    }
}

fn apply(doc: &mut Vec<Element>, cmd: &Command, undo: bool) {
    match cmd {
        Command::Add(el) => {
            if undo {
                doc.retain(|e| e.id != el.id);
            } else if !doc.iter().any(|e| e.id == el.id) {
                doc.push(el.clone());
            }
        }
        Command::Clear { snapshot } => {
            if undo {
                *doc = snapshot.clone();
            } else {
                doc.clear();
            }
        }
        Command::Move { id, dx, dy } => {
            let (dx, dy) = if undo { (-dx, -dy) } else { (*dx, *dy) };
            if let Some(el) = doc.iter_mut().find(|e| e.id == *id) {
                el.move_by(dx, dy);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Rect,
    Ellipse,
    Arrow,
    PolylineArrow,
    CurveArrow,
    Pen,
    Mosaic,
    Text,
    Number,
}

impl Tool {
    pub fn id(self) -> &'static str {
        match self {
            Tool::Select => super::toolbar::SELECT,
            Tool::Rect => super::toolbar::RECT,
            Tool::Ellipse => super::toolbar::ELLIPSE,
            Tool::Arrow => super::toolbar::ARROW,
            Tool::PolylineArrow => super::toolbar::POLYLINE_ARROW,
            Tool::CurveArrow => super::toolbar::CURVE_ARROW,
            Tool::Pen => super::toolbar::PEN,
            Tool::Mosaic => super::toolbar::MOSAIC,
            Tool::Text => super::toolbar::TEXT,
            Tool::Number => super::toolbar::NUMBER,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            super::toolbar::SELECT => Tool::Select,
            super::toolbar::RECT => Tool::Rect,
            super::toolbar::ELLIPSE => Tool::Ellipse,
            super::toolbar::ARROW => Tool::Arrow,
            super::toolbar::POLYLINE_ARROW => Tool::PolylineArrow,
            super::toolbar::CURVE_ARROW => Tool::CurveArrow,
            super::toolbar::PEN => Tool::Pen,
            super::toolbar::MOSAIC => Tool::Mosaic,
            super::toolbar::TEXT => Tool::Text,
            super::toolbar::NUMBER => Tool::Number,
            _ => return None,
        })
    }

    /// Closed shapes are the only tools whose outline honours the line style
    /// and whose picker exposes the line-style buttons.
    pub fn is_shape(self) -> bool {
        matches!(self, Tool::Rect | Tool::Ellipse)
    }

    /// The three arrow variants; they share one properties row and are picked
    /// inline there.
    pub fn is_arrow(self) -> bool {
        matches!(self, Tool::Arrow | Tool::PolylineArrow | Tool::CurveArrow)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressKind {
    Capture,
    RequestText,
}

#[derive(Debug)]
pub struct Session {
    pub elements: Vec<Element>,
    pub history: History,
    pub tool: Tool,
    pub draft: Option<Element>,
    pub selected_id: Option<u64>,
    next_id: u64,
    press: Point,
    dragging: bool,
    move_dx: f64,
    move_dy: f64,
    /// Defaults for new elements, snapshotted from settings when the session
    /// starts so a mid-capture settings edit cannot change the tool under the
    /// user's hand.
    /// The colour popup writes this mid-session; the settings value only seeds it.
    pub stroke: u32,
    /// Line style applied to newly created elements.
    pub line_style: LineStyle,
    /// Whether the shape picker (rect / ellipse) is expanded.
    pub shape_open: bool,
    /// Outline / shaft on or off for new shapes and arrows.
    pub stroke_enabled: bool,
    /// Fill the interior of new rect / ellipse elements.
    pub fill_enabled: bool,
    /// Stroke opacity in percent (0..=100); applies to outline and fill.
    pub alpha: u8,
    /// Arrow head placement for new arrows.
    pub arrow_ends: ArrowEnds,
    /// Arrow head style for new arrows.
    pub arrow_head: ArrowHead,
    pub stroke_width: f64,
    /// Bubble style for new numbers.
    pub badge_style: BadgeStyle,
    /// Bubble radius for new numbers, in image px.
    pub badge_size: f64,
    mosaic_block: i32,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        let ann = crate::config::get().screenshot.annotation;
        Self {
            elements: Vec::new(),
            history: History::new(80),
            tool: Tool::Select,
            draft: None,
            selected_id: None,
            next_id: 1,
            press: Point { x: 0.0, y: 0.0 },
            dragging: false,
            move_dx: 0.0,
            move_dy: 0.0,
            stroke: ann.stroke_argb(),
            line_style: LineStyle::Solid,
            shape_open: false,
            stroke_enabled: true,
            fill_enabled: false,
            alpha: 100,
            arrow_ends: ArrowEnds::End,
            arrow_head: ArrowHead::Filled,
            stroke_width: ann.clamped_stroke_width(),
            badge_style: BadgeStyle::Filled,
            badge_size: BADGE_RADIUS,
            mosaic_block: ann.clamped_mosaic_block(),
        }
    }

    /// The stroke colour elements are created with: the palette RGB plus the
    /// opacity chosen on the properties row.
    pub fn stroke_argb(&self) -> u32 {
        (alpha_channel(self.alpha) << 24) | (self.stroke & 0x00FF_FFFF)
    }

    /// Interior fill colour for new shapes: the stroke colour, so lowering the
    /// opacity tints the fill as well. `None` when the fill toggle is off.
    pub fn fill_argb(&self) -> Option<u32> {
        self.fill_enabled.then(|| self.stroke_argb())
    }

    /// Set opacity from a percentage, rounding and clamping.
    pub fn set_alpha(&mut self, percent: f64) {
        self.alpha = percent.round().clamp(0.0, 100.0) as u8;
    }

    /// Set the default stroke width, clamped to the slider's range.
    pub fn set_stroke_width(&mut self, width: f64) {
        self.stroke_width = width.clamp(super::toolbar::STROKE_MIN, super::toolbar::STROKE_MAX);
    }

    /// Set the badge radius, clamped to the popup slider's range.
    pub fn set_badge_size(&mut self, radius: f64) {
        self.badge_size = radius.clamp(
            super::toolbar::BADGE_MIN_RADIUS,
            super::toolbar::BADGE_MAX_RADIUS,
        );
    }

    pub fn visible(&self) -> impl Iterator<Item = &Element> {
        self.elements.iter().chain(self.draft.as_ref())
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn pointer_pressed(&mut self, pt: Point) -> PressKind {
        self.press = pt;
        self.dragging = true;
        self.move_dx = 0.0;
        self.move_dy = 0.0;
        if self.tool == Tool::Select {
            self.selected_id = self
                .elements
                .iter()
                .rev()
                .find(|e| e.hit_test(pt))
                .map(|e| e.id);
            crate::perf::log_diag(&format!(
                "pointer_pressed tool=Select selected={:?} (no draft)",
                self.selected_id
            ));
            return PressKind::Capture;
        }
        if self.tool == Tool::Text {
            self.dragging = false;
            crate::perf::log_diag("pointer_pressed tool=Text (requests text field, no draft)");
            return PressKind::RequestText;
        }
        self.draft = Some(self.create_draft(pt));
        crate::perf::log_diag(&format!(
            "draft created tool={:?} shape={:?} bounds={}x{}",
            self.tool,
            self.draft.as_ref().map(|d| &d.shape),
            self.draft.as_ref().map(|d| d.bounds.width).unwrap_or(0.0),
            self.draft.as_ref().map(|d| d.bounds.height).unwrap_or(0.0),
        ));
        PressKind::Capture
    }

    pub fn pointer_moved(&mut self, pt: Point) {
        if !self.dragging {
            return;
        }
        if self.tool == Tool::Select {
            if let Some(id) = self.selected_id {
                let dx = pt.x - self.press.x;
                let dy = pt.y - self.press.y;
                if let Some(el) = self.elements.iter_mut().find(|e| e.id == id) {
                    el.move_by(dx, dy);
                }
                self.move_dx += dx;
                self.move_dy += dy;
                self.press = pt;
            }
            return;
        }
        if let Some(draft) = self.draft.as_mut() {
            update_draft(draft, self.press, pt);
        }
    }

    pub fn pointer_released(&mut self) {
        if !self.dragging {
            crate::perf::log_diag("pointer_released: ignored, not dragging");
            return;
        }
        self.dragging = false;
        if self.tool == Tool::Select {
            if let Some(id) = self.selected_id {
                if self.move_dx != 0.0 || self.move_dy != 0.0 {
                    if let Some(el) = self.elements.iter_mut().find(|e| e.id == id) {
                        el.move_by(-self.move_dx, -self.move_dy);
                    }
                    self.history.execute(
                        &mut self.elements,
                        Command::Move {
                            id,
                            dx: self.move_dx,
                            dy: self.move_dy,
                        },
                    );
                }
            }
        }
        if let Some(mut draft) = self.draft.take() {
            finalize_path_arrow(&mut draft);
            let tiny = draft.bounds.width < MIN_KEEP
                && draft.bounds.height < MIN_KEEP
                && !draft.has_path();
            if !tiny {
                let is_badge = matches!(draft.shape, Shape::Badge { .. });
                crate::perf::log_diag(&format!(
                    "commit shape={:?} count_after={}",
                    draft.shape,
                    self.elements.len() + 1
                ));
                self.history
                    .execute(&mut self.elements, Command::Add(draft));
                // A freshly dropped badge claims the next number; renumber keeps
                // every badge's number contiguous (1..n) in document order.
                if is_badge {
                    self.renumber_badges();
                }
            } else {
                crate::perf::log_diag(&format!(
                    "discard draft shape={:?}: tiny {}x{}",
                    draft.shape, draft.bounds.width, draft.bounds.height
                ));
            }
        } else {
            crate::perf::log_diag("pointer_released: no draft to commit");
        }
    }

    pub fn add_text(&mut self, pt: Point, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let el = Element {
            id: self.alloc_id(),
            bounds: Rect::new(
                pt.x,
                pt.y,
                (trimmed.chars().count() as f64 * 12.0).max(40.0),
                28.0,
            ),
            stroke: self.stroke_argb(),
            stroke_width: self.stroke_width,
            line_style: self.line_style,
            stroke_enabled: true,
            fill: None,
            arrow_ends: self.arrow_ends,
            arrow_head: self.arrow_head,
            shape: Shape::Text {
                text: trimmed.to_string(),
                font_size: TEXT_FONT_SIZE,
            },
        };
        self.history.execute(&mut self.elements, Command::Add(el));
    }

    pub fn undo(&mut self) -> bool {
        self.selected_id = None;
        let did = self.history.undo(&mut self.elements);
        if did {
            self.renumber_badges();
        }
        did
    }

    pub fn redo(&mut self) -> bool {
        self.selected_id = None;
        let did = self.history.redo(&mut self.elements);
        if did {
            self.renumber_badges();
        }
        did
    }

    pub fn clear(&mut self) {
        let snapshot = self.elements.clone();
        self.history
            .execute(&mut self.elements, Command::Clear { snapshot });
        self.selected_id = None;
        self.draft = None;
    }

    pub fn cancel_draft(&mut self) {
        self.draft = None;
        self.dragging = false;
    }

    /// Next free badge number: badges are numbered 1..n in document order.
    /// A draft badge already in the list counts, so two consecutive clicks never
    /// collide.
    pub fn next_badge_number(&self) -> u32 {
        self.elements
            .iter()
            .filter(|el| matches!(el.shape, Shape::Badge { .. }))
            .count() as u32
            + 1
    }

    /// Close gaps in badge numbering after an undo/redo leaves a hole. Numbers
    /// are reassigned 1..n following `elements` order, so the bubbles always read
    /// 1, 2, 3… with no missing values.
    pub fn renumber_badges(&mut self) {
        let mut next = 1;
        for el in &mut self.elements {
            if let Shape::Badge { number, .. } = &mut el.shape {
                *number = next;
                next += 1;
            }
        }
    }

    fn create_draft(&mut self, pt: Point) -> Element {
        let id = self.alloc_id();
        let bounds = Rect::new(pt.x, pt.y, 1.0, 1.0);
        let shape = match self.tool {
            Tool::Arrow => Shape::Arrow {
                start: pt,
                end: pt,
                head: 12.0,
            },
            Tool::Ellipse => Shape::Ellipse,
            Tool::PolylineArrow => Shape::PolylineArrow {
                points: vec![pt],
                head: 12.0,
            },
            Tool::CurveArrow => Shape::CurveArrow {
                points: vec![pt],
                head: 12.0,
            },
            Tool::Pen => Shape::Pen { points: vec![pt] },
            Tool::Mosaic => Shape::Mosaic {
                points: vec![pt],
                block_size: self.mosaic_block,
            },
            Tool::Number => Shape::Badge {
                number: self.next_badge_number(),
                style: self.badge_style,
                radius: self.badge_size,
            },
            _ => Shape::Rect,
        };
        let mut el = Element {
            id,
            bounds,
            stroke: self.stroke_argb(),
            stroke_width: self.stroke_width,
            line_style: self.line_style,
            shape,
            stroke_enabled: self.stroke_enabled,
            fill: self.fill_argb(),
            arrow_ends: self.arrow_ends,
            arrow_head: self.arrow_head,
        };
        if el.has_path() {
            recalc_path_bounds(&mut el);
        }
        // A badge is a fixed-size disc centred on the press point; keep its
        // bounds (and thus hit-test box) sized so a click with no drag still
        // passes the `tiny` discard check in `pointer_released`.
        if let Shape::Badge { radius, .. } = &el.shape {
            let r = *radius;
            el.bounds = Rect::new(pt.x - r, pt.y - r, r * 2.0, r * 2.0);
        }
        el
    }
}

fn recalc_path_bounds(el: &mut Element) {
    let (Shape::Pen { points }
    | Shape::Mosaic { points, .. }
    | Shape::PolylineArrow { points, .. }
    | Shape::CurveArrow { points, .. }) = &el.shape
    else {
        return;
    };
    if points.is_empty() {
        el.bounds = Rect::new(0.0, 0.0, 0.0, 0.0);
        return;
    }
    let min_x = points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
    let min_y = points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
    let max_x = points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max);
    let max_y = points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);
    el.bounds = Rect::new(
        min_x,
        min_y,
        (max_x - min_x).max(1.0),
        (max_y - min_y).max(1.0),
    );
}

fn update_draft(draft: &mut Element, start: Point, current: Point) {
    match &mut draft.shape {
        Shape::Arrow { end, .. } => {
            *end = current;
            draft.bounds = rect_from_points(start, current);
        }
        Shape::Pen { points }
        | Shape::Mosaic { points, .. }
        | Shape::PolylineArrow { points, .. }
        | Shape::CurveArrow { points, .. } => {
            points.push(current);
            recalc_path_bounds(draft);
        }
        Shape::Badge { radius, .. } => {
            // The bubble follows the cursor; its size never changes, so a drag
            // just relocates the bubble rather than stretching it.
            let r = *radius;
            draft.bounds = Rect::new(current.x - r, current.y - r, r * 2.0, r * 2.0);
        }
        _ => {
            draft.bounds = rect_from_points(start, current);
        }
    }
}

fn rect_from_points(a: Point, b: Point) -> Rect {
    Rect::new(
        a.x.min(b.x),
        a.y.min(b.y),
        (b.x - a.x).abs().max(1.0),
        (b.y - a.y).abs().max(1.0),
    )
}

/// QQ-style annotation arrow: tapered body (point tail → thick neck) + filled wedge head.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilledArrow {
    pub tail: Point,
    pub neck_left: Point,
    pub neck_right: Point,
    pub left: Point,
    pub right: Point,
    pub tip: Point,
}

impl FilledArrow {
    pub fn triangles(self) -> [(Point, Point, Point); 2] {
        [
            (self.tail, self.neck_left, self.neck_right),
            (self.tip, self.left, self.right),
        ]
    }

    // Exercised by the geometry tests below; nothing in the UI asks yet.
    #[allow(dead_code)]
    pub fn neck_width(self) -> f64 {
        dist(self.neck_left, self.neck_right)
    }

    #[allow(dead_code)]
    pub fn head_width(self) -> f64 {
        dist(self.left, self.right)
    }
}

#[allow(dead_code)]
fn dist(a: Point, b: Point) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}

/// Brush half-width of the mosaic smudge tool: three cells across.
pub fn mosaic_brush_radius(block: i32) -> f64 {
    block.max(2) as f64 * 1.5
}

/// Grid cells the smudge path covers, as `(cell_x, cell_y)` indices into the
/// image-wide grid. Aligning to the image rather than to the stroke is what lets
/// two strokes -- or the same stroke extended, or moved -- keep their cells lined
/// up instead of re-tiling from wherever the path happens to start.
pub fn mosaic_cells(points: &[Point], block: i32) -> Vec<(i32, i32)> {
    let block = block.max(2);
    let r = mosaic_brush_radius(block);
    let Some(first) = points.first() else {
        return Vec::new();
    };
    let mut cells = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let segments: Vec<(Point, Point)> = if points.len() == 1 {
        vec![(*first, *first)]
    } else {
        points.windows(2).map(|w| (w[0], w[1])).collect()
    };
    for (a, b) in segments {
        let x0 = ((a.x.min(b.x) - r) / block as f64).floor() as i32;
        let x1 = ((a.x.max(b.x) + r) / block as f64).floor() as i32;
        let y0 = ((a.y.min(b.y) - r) / block as f64).floor() as i32;
        let y1 = ((a.y.max(b.y) + r) / block as f64).floor() as i32;
        for gy in y0..=y1 {
            for gx in x0..=x1 {
                let center = Point {
                    x: (gx as f64 + 0.5) * block as f64,
                    y: (gy as f64 + 0.5) * block as f64,
                };
                if dist_to_segment(center, a, b) <= r && seen.insert((gx, gy)) {
                    cells.push((gx, gy));
                }
            }
        }
    }
    cells
}

pub fn filled_arrow(start: Point, end: Point, stroke: f64) -> Option<FilledArrow> {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 4.0 {
        return None;
    }
    let ux = dx / len;
    let uy = dy / len;
    let px = -uy;
    let py = ux;
    let stroke = stroke.max(1.0);
    // Cap the girth against the length, or a short drag yields a head that is far
    // wider than it is long -- a T-bar rather than an arrow.
    let neck_half = ((stroke * 1.6 + 1.5).max(4.0)).min(len * 0.12);
    let head_half = neck_half * 1.85;
    let mut head_len = (neck_half * 2.4).min(len * 0.28);
    head_len = head_len.max(10.0_f64.min(len * 0.45));
    let neck_at = (len - head_len).max(0.0);
    let at = |along: f64, across: f64| Point {
        x: start.x + ux * along + px * across,
        y: start.y + uy * along + py * across,
    };
    Some(FilledArrow {
        tail: start,
        neck_left: at(neck_at, neck_half),
        neck_right: at(neck_at, -neck_half),
        left: at(neck_at, head_half),
        right: at(neck_at, -head_half),
        tip: end,
    })
}

// ---- line-style geometry ---------------------------------------------------

/// Closed polyline approximation of an ellipse, dense enough that a thick
/// dashed stroke shows no corners.
pub fn ellipse_ring(r: Rect, segments: usize) -> Vec<Point> {
    let segments = segments.max(8);
    let cx = r.x + r.width * 0.5;
    let cy = r.y + r.height * 0.5;
    let rx = (r.width * 0.5).max(0.5);
    let ry = (r.height * 0.5).max(0.5);
    let mut ring = Vec::with_capacity(segments + 1);
    for i in 0..=segments {
        let a = i as f64 * std::f64::consts::TAU / segments as f64;
        ring.push(Point {
            x: cx + rx * a.cos(),
            y: cy + ry * a.sin(),
        });
    }
    ring
}

/// Dash/gap lengths; both scale with the stroke so a thick dashed outline does
/// not turn solid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DashPattern {
    pub on: f64,
    pub off: f64,
}

impl DashPattern {
    pub fn for_width(width: f64) -> Self {
        let width = width.max(1.0);
        Self {
            on: width * 4.0,
            off: width * 2.5,
        }
    }
}

/// Split a polyline into the drawn ("on") segments of a dash pattern.
pub fn dash_segments(points: &[Point], pattern: DashPattern) -> Vec<[Point; 2]> {
    if pattern.on <= 0.0 || pattern.off <= 0.0 || points.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut drawing = true;
    let mut remaining = pattern.on;
    for pair in points.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let length = (end.x - start.x).hypot(end.y - start.y);
        if length <= 1e-4 {
            continue;
        }
        let mut walked = 0.0;
        while walked < length - 1e-4 {
            let step = remaining.min(length - walked);
            if drawing {
                out.push([
                    lerp(start, end, walked / length),
                    lerp(start, end, (walked + step) / length),
                ]);
            }
            walked += step;
            remaining -= step;
            if remaining <= 1e-4 {
                drawing = !drawing;
                remaining = if drawing { pattern.on } else { pattern.off };
            }
        }
    }
    out
}

fn lerp(a: Point, b: Point, t: f64) -> Point {
    Point {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
    }
}

/// Total length of a polyline.
pub fn polyline_length(points: &[Point]) -> f64 {
    points
        .windows(2)
        .map(|w| (w[1].x - w[0].x).hypot(w[1].y - w[0].y))
        .sum()
}

fn point_at_distance(points: &[Point], distance: f64) -> Option<Point> {
    if points.len() < 2 {
        return points.first().copied();
    }
    if distance <= 0.0 {
        return points.first().copied();
    }
    let mut walked = 0.0;
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        let length = (b.x - a.x).hypot(b.y - a.y);
        if length <= 1e-6 {
            continue;
        }
        if walked + length >= distance {
            return Some(lerp(a, b, (distance - walked) / length));
        }
        walked += length;
    }
    points.last().copied()
}

/// Twice the signed area; only the sign is used (positive = clockwise with y down).
fn signed_area(points: &[Point]) -> f64 {
    let mut sum = 0.0;
    for i in 0..points.len() {
        let cur = points[i];
        let next = points[(i + 1) % points.len()];
        sum += cur.x * next.y - next.x * cur.y;
    }
    sum
}

const CLOUD_BUMP_SEGMENTS: usize = 12;

/// Revision-cloud outline: a chain of semicircular scallops laid end to end
/// along a closed ring. Returned as a dense polyline so the shared stroke
/// rasterizer draws it like any other path.
pub fn cloud_polyline(points: &[Point], radius: f64) -> Vec<Point> {
    let radius = radius.max(0.5);
    let mut base = points.to_vec();
    if base.len() < 2 {
        return base;
    }
    let (first, last) = (base[0], base[base.len() - 1]);
    if (first.x - last.x).hypot(first.y - last.y) > 1e-3 {
        base.push(first);
    }
    let total = polyline_length(&base);
    if total <= 1e-3 {
        return base;
    }
    let wanted = (total / (radius * 2.0)).floor().max(1.0);
    let stride = total / wanted;
    if stride * 0.5 < 0.5 {
        return base;
    }
    let outward = if signed_area(&base) >= 0.0 { 1.0 } else { -1.0 };
    let mut out = Vec::with_capacity((wanted as usize + 1) * CLOUD_BUMP_SEGMENTS);
    let mut walked = 0.0;
    while walked < total - 1e-3 {
        let start = point_at_distance(&base, walked).unwrap_or(base[0]);
        let end_distance = (walked + stride).min(total);
        let end = point_at_distance(&base, end_distance).unwrap_or(start);
        push_scallop(&mut out, start, end, outward, walked == 0.0);
        walked = end_distance;
    }
    if out.len() >= 2 {
        out.push(out[0]);
    }
    out
}

fn push_scallop(out: &mut Vec<Point>, start: Point, end: Point, outward: f64, skip_first: bool) {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let chord = dx.hypot(dy);
    if chord <= 1e-4 {
        return;
    }
    let radius = chord * 0.5;
    let center = Point {
        x: (start.x + end.x) * 0.5,
        y: (start.y + end.y) * 0.5,
    };
    let ux = dx / chord;
    let uy = dy / chord;
    let (nx, ny) = (uy * outward, -ux * outward);
    let first = if skip_first { 1 } else { 0 };
    for i in first..=CLOUD_BUMP_SEGMENTS {
        let theta = std::f64::consts::PI * i as f64 / CLOUD_BUMP_SEGMENTS as f64;
        let (sin, cos) = theta.sin_cos();
        out.push(Point {
            x: center.x + radius * (-ux * cos + nx * sin),
            y: center.y + radius * (-uy * cos + ny * sin),
        });
    }
}

// ---- polyline / curve arrow geometry ---------------------------------------

const CURVE_SUBDIVISIONS: usize = 8;
const CURVE_MAX_CONTROLS: usize = 6;

/// Catmull-Rom spline through `points`, duplicating the ends so the curve
/// starts and ends exactly on the first and last control point.
pub fn smooth_polyline(points: &[Point]) -> Vec<Point> {
    if points.len() < 2 {
        return points.to_vec();
    }
    const MAX_CONTROLS: usize = 256;
    let stride = points.len().div_ceil(MAX_CONTROLS);
    let controls: Vec<Point> = if stride > 1 {
        let mut sampled: Vec<Point> = points.iter().step_by(stride).copied().collect();
        if sampled.last() != points.last() {
            sampled.push(*points.last().expect("checked above"));
        }
        sampled
    } else {
        points.to_vec()
    };
    let pts = &controls[..];
    let last = pts.len() - 1;
    let mut out = Vec::with_capacity(pts.len() * CURVE_SUBDIVISIONS);
    for i in 0..last {
        let p0 = pts[i.saturating_sub(1)];
        let p1 = pts[i];
        let p2 = pts[i + 1];
        let p3 = pts[(i + 2).min(last)];
        for step in 0..CURVE_SUBDIVISIONS {
            let t = step as f64 / CURVE_SUBDIVISIONS as f64;
            out.push(catmull_rom_point(p0, p1, p2, p3, t));
        }
    }
    out.push(pts[last]);
    out
}

fn catmull_rom_point(p0: Point, p1: Point, p2: Point, p3: Point, t: f64) -> Point {
    let t2 = t * t;
    let t3 = t2 * t;
    let axis = |a: f64, b: f64, c: f64, d: f64| {
        0.5 * ((2.0 * b)
            + (-a + c) * t
            + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
            + (-a + 3.0 * b - 3.0 * c + d) * t3)
    };
    Point {
        x: axis(p0.x, p1.x, p2.x, p3.x),
        y: axis(p0.y, p1.y, p2.y, p3.y),
    }
}

/// Ramer-Douglas-Peucker simplification: a straight-ish wobble collapses to
/// its two ends while a real bend survives as a vertex.
fn rdp_simplify(points: &[Point], tolerance: f64) -> Vec<Point> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let start = points[0];
    let end = *points.last().expect("checked above");
    let mut worst = 0.0_f64;
    let mut worst_index = 0;
    for (i, p) in points.iter().enumerate().take(points.len() - 1).skip(1) {
        let d = dist_to_segment(*p, start, end);
        if d > worst {
            worst = d;
            worst_index = i;
        }
    }
    if worst <= tolerance {
        return vec![start, end];
    }
    let mut left = rdp_simplify(&points[..=worst_index], tolerance);
    let right = rdp_simplify(&points[worst_index..], tolerance);
    left.pop();
    left.extend(right);
    left
}

/// Collapse a recorded drag into a render-ready polyline/curve arrow: keep the
/// corners that turned, and thin a curve's controls so the spline sweeps a
/// smooth arc instead of chasing hand jitter.
fn finalize_path_arrow(el: &mut Element) {
    let simplified = match &el.shape {
        Shape::PolylineArrow { points, .. } => {
            if points.len() < 3 {
                return;
            }
            rdp_simplify(points, 4.0)
        }
        Shape::CurveArrow { points, .. } => {
            if points.len() < 3 {
                return;
            }
            let simple = rdp_simplify(points, 4.0);
            if simple.len() < 3 {
                simple
            } else {
                let mut out = vec![simple[0]];
                let interior = simple.len() - 2;
                let keep = interior.min(CURVE_MAX_CONTROLS);
                let stride = interior.div_ceil(keep).max(1);
                for p in simple[1..simple.len() - 1].iter().step_by(stride) {
                    out.push(*p);
                }
                out.push(*simple.last().expect("checked above"));
                out
            }
        }
        _ => return,
    };
    match &mut el.shape {
        Shape::PolylineArrow { points, .. } | Shape::CurveArrow { points, .. } => {
            *points = simplified;
        }
        _ => {}
    }
    recalc_path_bounds(el);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    #[test]
    fn rect_press_move_release_adds_element() {
        let mut s = Session::new();
        s.tool = Tool::Rect;
        assert_eq!(s.pointer_pressed(pt(10.0, 10.0)), PressKind::Capture);
        s.pointer_moved(pt(40.0, 30.0));
        s.pointer_released();
        assert_eq!(s.elements.len(), 1);
        assert!(matches!(s.elements[0].shape, Shape::Rect));
        assert_eq!(s.elements[0].bounds, Rect::new(10.0, 10.0, 30.0, 20.0));
    }

    #[test]
    fn tiny_non_pen_draft_is_discarded() {
        let mut s = Session::new();
        s.tool = Tool::Rect;
        s.pointer_pressed(pt(5.0, 5.0));
        s.pointer_moved(pt(5.5, 5.5));
        s.pointer_released();
        assert!(s.elements.is_empty());
    }

    #[test]
    fn tiny_pen_is_kept() {
        let mut s = Session::new();
        s.tool = Tool::Pen;
        s.pointer_pressed(pt(8.0, 8.0));
        s.pointer_released();
        assert_eq!(s.elements.len(), 1);
        assert!(matches!(s.elements[0].shape, Shape::Pen { .. }));
    }

    #[test]
    fn arrow_and_mosaic_and_text_create_matching_shapes() {
        let mut s = Session::new();
        s.tool = Tool::Arrow;
        s.pointer_pressed(pt(0.0, 0.0));
        s.pointer_moved(pt(20.0, 0.0));
        s.pointer_released();
        assert!(matches!(s.elements[0].shape, Shape::Arrow { .. }));

        s.tool = Tool::Mosaic;
        s.pointer_pressed(pt(0.0, 10.0));
        s.pointer_moved(pt(32.0, 42.0));
        s.pointer_released();
        match &s.elements[1].shape {
            Shape::Mosaic { points, block_size } => {
                assert_eq!(*block_size, 16);
                assert_eq!(points.first(), Some(&pt(0.0, 10.0)));
                assert_eq!(points.last(), Some(&pt(32.0, 42.0)));
            }
            other => panic!("{other:?}"),
        }

        s.tool = Tool::Text;
        assert_eq!(s.pointer_pressed(pt(50.0, 50.0)), PressKind::RequestText);
        s.add_text(pt(50.0, 50.0), "hi");
        assert!(matches!(
            s.elements.last().unwrap().shape,
            Shape::Text { .. }
        ));
    }

    #[test]
    fn undo_then_redo_of_an_add() {
        let mut s = Session::new();
        s.tool = Tool::Rect;
        s.pointer_pressed(pt(0.0, 0.0));
        s.pointer_moved(pt(10.0, 10.0));
        s.pointer_released();
        assert_eq!(s.elements.len(), 1);
        assert!(s.undo());
        assert!(s.elements.is_empty());
        assert!(s.redo());
        assert_eq!(s.elements.len(), 1);
    }

    #[test]
    fn new_add_clears_redo_stack() {
        let mut s = Session::new();
        s.tool = Tool::Rect;
        s.pointer_pressed(pt(0.0, 0.0));
        s.pointer_moved(pt(10.0, 10.0));
        s.pointer_released();
        s.undo();
        assert!(s.history.can_redo());
        s.pointer_pressed(pt(20.0, 20.0));
        s.pointer_moved(pt(40.0, 40.0));
        s.pointer_released();
        assert!(!s.history.can_redo());
        assert_eq!(s.elements.len(), 1);
    }

    #[test]
    fn undo_clear_restores_elements() {
        let mut s = Session::new();
        s.tool = Tool::Rect;
        s.pointer_pressed(pt(0.0, 0.0));
        s.pointer_moved(pt(8.0, 8.0));
        s.pointer_released();
        s.clear();
        assert!(s.elements.is_empty());
        s.undo();
        assert_eq!(s.elements.len(), 1);
    }

    #[test]
    fn text_bounds_count_characters_not_bytes() {
        let mut s = Session::new();
        s.add_text(pt(0.0, 0.0), "你好");
        let ascii = {
            let mut t = Session::new();
            t.add_text(pt(0.0, 0.0), "ab");
            t.elements[0].bounds.width
        };
        assert_eq!(s.elements[0].bounds.width, ascii);
    }

    #[test]
    fn empty_text_is_ignored() {
        let mut s = Session::new();
        s.add_text(pt(1.0, 1.0), "   ");
        assert!(s.elements.is_empty());
    }

    #[test]
    fn filled_arrow_is_qq_triangle_not_v_stroke() {
        let g = filled_arrow(pt(0.0, 0.0), pt(100.0, 0.0), 3.0).unwrap();
        assert!((g.tip.x - 100.0).abs() < 1e-9);
        assert!((g.tip.y).abs() < 1e-9);
        assert_eq!(g.tail, pt(0.0, 0.0));
        assert!(g.neck_width() >= 8.0, "neck {}", g.neck_width());
        assert!(
            g.head_width() > g.neck_width() + 2.0,
            "head {} neck {}",
            g.head_width(),
            g.neck_width()
        );
        assert!(g.neck_left.x > 40.0 && g.neck_left.x < 100.0);
        assert!(g.left.y > 0.0 && g.right.y < 0.0);
    }

    #[test]
    fn filled_arrow_body_tapers_from_a_point() {
        let g = filled_arrow(pt(0.0, 0.0), pt(80.0, 0.0), 3.0).unwrap();
        let mid_x = g.neck_left.x * 0.4;
        let t = mid_x / g.neck_left.x;
        let half_at_mid = g.neck_width() / 2.0 * t;
        assert!(half_at_mid < g.neck_width() / 2.0 - 1.0);
        assert!(g.tail.x < g.neck_left.x && g.neck_left.x < g.tip.x);
    }

    #[test]
    fn short_arrow_head_is_not_wider_than_it_is_long() {
        let g = filled_arrow(pt(0.0, 0.0), pt(10.0, 0.0), 4.0).unwrap();
        let head_len = g.tip.x - g.left.x;
        assert!(
            g.head_width() <= head_len * 2.0,
            "head {} wide but only {head_len} long",
            g.head_width()
        );
    }

    #[test]
    fn mosaic_cells_align_to_the_image_grid_not_the_stroke() {
        // Same stroke, shifted by a non-multiple of the block: the covered cells
        // must shift by whole cells, never re-tile from the stroke's own origin.
        let at = |x: f64| mosaic_cells(&[pt(x, 100.0), pt(x + 50.0, 100.0)], 16);
        for (gx, _) in at(37.0) {
            assert_eq!(gx * 16 % 16, 0, "cell must sit on the image grid");
        }
        let a = at(0.0);
        let b = at(32.0);
        assert!(a.iter().all(|(gx, gy)| b.contains(&(gx + 2, *gy))));
    }

    #[test]
    fn mosaic_cells_dedupe_across_segments() {
        let doubled = mosaic_cells(&[pt(50.0, 50.0), pt(80.0, 50.0), pt(50.0, 50.0)], 16);
        let mut uniq = doubled.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(doubled.len(), uniq.len());
    }

    #[test]
    fn scaled_maps_geometry_stroke_and_block() {
        let el = Element {
            stroke_enabled: true,
            fill: None,
            arrow_ends: Default::default(),
            arrow_head: Default::default(),
            id: 1,
            bounds: Rect::new(10.0, 20.0, 30.0, 40.0),
            stroke: 0xFFFF_0000,
            stroke_width: 4.0,
            line_style: Default::default(),
            shape: Shape::Mosaic {
                points: vec![Point { x: 10.0, y: 20.0 }],
                block_size: 16,
            },
        };
        let s = el.scaled(0.5);
        assert_eq!(s.bounds, Rect::new(5.0, 10.0, 15.0, 20.0));
        assert_eq!(s.stroke_width, 2.0);
        match s.shape {
            Shape::Mosaic { points, block_size } => {
                assert_eq!(points[0], Point { x: 5.0, y: 10.0 });
                assert_eq!(block_size, 8);
            }
            _ => panic!("shape changed"),
        }
        // Identity leaves the element untouched.
        assert_eq!(el.scaled(1.0), el);
    }

    #[test]
    fn filled_arrow_none_when_too_short() {
        assert!(filled_arrow(pt(1.0, 1.0), pt(1.4, 1.0), 3.0).is_none());
    }

    #[test]
    fn ellipse_ring_is_closed_and_dense() {
        let ring = ellipse_ring(Rect::new(10.0, 20.0, 80.0, 40.0), 96);
        assert_eq!(ring.len(), 97);
        assert!((ring[0].x - ring[96].x).abs() < 1e-6);
        assert!((ring[0].y - ring[96].y).abs() < 1e-6);
        assert!((ring.iter().map(|p| p.x).fold(f64::MAX, f64::min) - 10.0).abs() < 1e-6);
    }

    #[test]
    fn line_style_and_new_tool_ids_roundtrip() {
        for style in LineStyle::ALL {
            assert_eq!(LineStyle::from_id(style.id()), Some(style));
        }
        for tool in [Tool::Ellipse, Tool::PolylineArrow, Tool::CurveArrow] {
            assert_eq!(Tool::from_id(tool.id()), Some(tool));
        }
    }

    #[test]
    fn dash_segments_leave_gaps() {
        let path = vec![pt(0.0, 0.0), pt(100.0, 0.0)];
        let segs = dash_segments(&path, DashPattern::for_width(4.0));
        assert!(segs.len() > 1);
        let drawn: f64 = segs.iter().map(|[a, b]| (b.x - a.x).hypot(b.y - a.y)).sum();
        assert!(drawn > 0.0 && drawn < 100.0, "drawn length {drawn}");
    }

    #[test]
    fn cloud_polyline_scallops_a_closed_ring() {
        let ring = vec![
            pt(0.0, 0.0),
            pt(60.0, 0.0),
            pt(60.0, 40.0),
            pt(0.0, 40.0),
            pt(0.0, 0.0),
        ];
        let cloud = cloud_polyline(&ring, 5.0);
        assert!(
            cloud.len() > ring.len() * 4,
            "cloud too sparse: {}",
            cloud.len()
        );
        assert_eq!(cloud.first(), cloud.last());
    }

    #[test]
    fn smooth_polyline_keeps_endpoints_and_adds_samples() {
        let controls = vec![pt(0.0, 0.0), pt(30.0, 40.0), pt(60.0, 0.0)];
        let curve = smooth_polyline(&controls);
        assert!(curve.len() > controls.len());
        assert_eq!(curve.first(), Some(&controls[0]));
        assert_eq!(curve.last(), Some(&controls[2]));
    }

    #[test]
    fn rdp_collapses_straight_and_keeps_a_corner() {
        let straight = vec![pt(0.0, 0.0), pt(10.0, 1.0), pt(20.0, 0.0), pt(30.0, 0.0)];
        assert_eq!(rdp_simplify(&straight, 4.0).len(), 2);
        let corner = vec![pt(0.0, 0.0), pt(40.0, 0.0), pt(40.0, 40.0)];
        let simplified = rdp_simplify(&corner, 4.0);
        assert_eq!(simplified.len(), 3);
        assert_eq!(simplified[1], pt(40.0, 0.0));
    }

    #[test]
    fn number_tool_creates_a_sized_badge() {
        let mut s = Session::new();
        s.tool = Tool::Number;
        s.pointer_pressed(pt(100.0, 100.0));
        s.pointer_released();
        let badge = s.elements.last().expect("badge added");
        match &badge.shape {
            Shape::Badge { number, .. } => {
                assert_eq!(*number, 1);
                // A single click (no drag) must still clear the `tiny` discard.
                assert_eq!(badge.bounds.width, BADGE_RADIUS * 2.0);
            }
            other => panic!("expected badge, got {other:?}"),
        }
    }

    #[test]
    fn arrow_ends_and_head_ids_roundtrip() {
        for ends in ArrowEnds::ALL {
            assert_eq!(ArrowEnds::from_id(ends.id()), Some(ends));
        }
        for head in ArrowHead::ALL {
            assert_eq!(ArrowHead::from_id(head.id()), Some(head));
        }
        assert!(ArrowEnds::Both.has_start() && ArrowEnds::Both.has_end());
        assert!(!ArrowEnds::Neither.has_start() && !ArrowEnds::Neither.has_end());
    }

    /// Second-row controls mutate the session defaults, those defaults survive
    /// a tool switch, and the next created element carries them.
    #[test]
    fn property_defaults_persist_and_reach_new_elements() {
        use crate::overlay::{annotate, toolbar};

        let mut s = Session::new();
        s.stroke = 0xFF00_00FF; // palette blue
        assert!(annotate::handle_props(&mut s, toolbar::PROP_STROKE));
        assert!(!s.stroke_enabled);
        assert!(annotate::handle_props(&mut s, toolbar::PROP_FILL));
        assert!(s.fill_enabled);
        // The width slider and the colour popup's alpha slider write the same
        // session fields the next element reads.
        s.set_stroke_width(toolbar::width_for_fraction(1.0));
        assert_eq!(s.stroke_width, toolbar::STROKE_MAX);
        s.set_alpha(toolbar::alpha_for_fraction(0.7) as f64);
        assert_eq!(s.alpha, 70);

        // Switching tools must not reset the properties.
        s.tool = Tool::Ellipse;
        s.pointer_pressed(pt(10.0, 10.0));
        s.pointer_moved(pt(50.0, 40.0));
        s.pointer_released();
        let el = &s.elements[0];
        assert!(!el.stroke_enabled);
        assert_eq!(el.stroke_width, toolbar::STROKE_MAX);
        let alpha = (70 * 255 / 100) << 24;
        assert_eq!(el.stroke, alpha | 0x0000_00FF);
        assert_eq!(el.fill, Some(alpha | 0x0000_00FF));
    }

    /// The bubble popup writes style and radius; a dropped number carries both.
    #[test]
    fn badge_style_and_size_reach_new_elements() {
        use crate::overlay::{annotate, toolbar};

        let mut s = Session::new();
        assert!(annotate::handle_props(&mut s, toolbar::PROP_BADGE_HOLLOW));
        assert_eq!(s.badge_style, BadgeStyle::Hollow);
        s.set_badge_size(24.0);
        s.tool = Tool::Number;
        s.pointer_pressed(pt(100.0, 100.0));
        s.pointer_released();
        let badge = s.elements.last().expect("badge added");
        match &badge.shape {
            Shape::Badge {
                style,
                radius,
                number,
            } => {
                assert_eq!(*number, 1);
                assert_eq!(*style, BadgeStyle::Hollow);
                assert_eq!(*radius, 24.0);
                assert_eq!(badge.bounds.width, 48.0);
            }
            other => panic!("expected badge, got {other:?}"),
        }
        // Out-of-range requests clamp instead of moving the bubble off-limits.
        s.set_badge_size(999.0);
        assert_eq!(s.badge_size, toolbar::BADGE_MAX_RADIUS);
        s.set_badge_size(0.0);
        assert_eq!(s.badge_size, toolbar::BADGE_MIN_RADIUS);
        s.set_stroke_width(-3.0);
        assert_eq!(s.stroke_width, toolbar::STROKE_MIN);
    }

    #[test]
    fn badges_are_numbered_contiguously_and_renumbered_after_undo() {
        let mut s = Session::new();
        s.tool = Tool::Number;
        for i in 1..=3 {
            s.pointer_pressed(pt(10.0 * i as f64, 10.0));
            s.pointer_released();
        }
        let nums: Vec<u32> = s
            .elements
            .iter()
            .filter_map(|el| match &el.shape {
                Shape::Badge { number, .. } => Some(*number),
                _ => None,
            })
            .collect();
        assert_eq!(nums, vec![1, 2, 3]);
        // Undo the last badge; the remaining two must renumber 1, 2.
        s.undo();
        let nums: Vec<u32> = s
            .elements
            .iter()
            .filter_map(|el| match &el.shape {
                Shape::Badge { number, .. } => Some(*number),
                _ => None,
            })
            .collect();
        assert_eq!(nums, vec![1, 2]);
    }
}
