//! Annotation UI shared by the capture overlay and pinned images: the text
//! draft the IME types into, the cached coverage masks and mosaic tiles behind
//! the live preview, the mark painter, and the toolbar / colour popup / text
//! layer elements. The hosting view owns the `Session` and decides what each
//! toolbar button does; this module renders and dispatches.
//!
//! Coordinates: elements live in the host's *edit space* (capture pixels for
//! the overlay, group pixels for a pin). Painters take an `offset`, the edit
//! space point that sits at the window's client origin, so one element list
//! can be drawn by several windows each showing a different part of it.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    div, fill, point, prelude::*, px, rgb, size, svg, white, AnyElement, AnyView, Bounds,
    BoxShadow, Context, CursorStyle, MouseButton, MouseDownEvent, Pixels, Render, SharedString,
    UTF16Selection, Window,
};

use crate::geom::{Point, Rect};
use crate::overlay::composite::{element_coverage, CoverageRun, ElementRuns};
use crate::overlay::editor::{
    badge_font_size, ArrowEnds, ArrowHead, BadgeStyle, Element, LineStyle, Session, Shape, Tool,
    TEXT_FONT_SIZE,
};
use crate::overlay::{text_raster, theme, toolbar};

// ---- text draft -----------------------------------------------------------

/// A text annotation being typed. The caret is always at the end, so the IME
/// plumbing below is the minimum the platform needs: append committed text,
/// keep the composition string marked so the next update replaces it.
pub struct TextDraft {
    /// Edit-space origin of the text.
    pub pt: Point,
    pub buf: String,
    /// Byte range of the IME composition (未上屏) text inside `buf`; the next
    /// composition update or result replaces exactly this range.
    pub marked: Option<Range<usize>>,
}

impl TextDraft {
    pub fn new(pt: Point) -> Self {
        Self {
            pt,
            buf: String::new(),
            marked: None,
        }
    }

    pub fn text_for_range(
        &self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
    ) -> String {
        let buf = &self.buf;
        let range = utf16_to_byte(buf, range_utf16.start)..utf16_to_byte(buf, range_utf16.end);
        actual_range.replace(byte_to_utf16(buf, range.start)..byte_to_utf16(buf, range.end));
        buf[range].to_string()
    }

    pub fn selected_range(&self) -> UTF16Selection {
        let end = byte_to_utf16(&self.buf, self.buf.len());
        UTF16Selection {
            range: end..end,
            reversed: false,
        }
    }

    pub fn marked_range(&self) -> Option<Range<usize>> {
        let m = self.marked.as_ref()?;
        Some(byte_to_utf16(&self.buf, m.start)..byte_to_utf16(&self.buf, m.end))
    }

    pub fn unmark(&mut self) {
        self.marked = None;
    }

    fn resolve(&self, range_utf16: Option<Range<usize>>) -> Range<usize> {
        range_utf16
            .map(|r| utf16_to_byte(&self.buf, r.start)..utf16_to_byte(&self.buf, r.end))
            .or_else(|| self.marked.clone())
            .unwrap_or(self.buf.len()..self.buf.len())
    }

    pub fn replace(&mut self, range_utf16: Option<Range<usize>>, new_text: &str) {
        let range = self.resolve(range_utf16);
        self.buf.replace_range(range, new_text);
        self.marked = None;
    }

    pub fn replace_and_mark(&mut self, range_utf16: Option<Range<usize>>, new_text: &str) {
        let range = self.resolve(range_utf16);
        self.buf.replace_range(range.clone(), new_text);
        self.marked = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
    }

    /// Mid-composition the IME owns Backspace; returns whether a char went.
    pub fn backspace(&mut self) -> bool {
        if self.marked.is_some() {
            return false;
        }
        self.buf.pop().is_some()
    }

    /// Where the IME parks its candidate window: the whole draft line, not the
    /// exact caret x. `offset` is the edit-space point at the element origin.
    pub fn ime_bounds(
        &self,
        element_bounds: Bounds<Pixels>,
        offset: Point,
        scale: f32,
    ) -> Bounds<Pixels> {
        Bounds {
            origin: point(
                element_bounds.origin.x + px((self.pt.x - offset.x) as f32 / scale),
                element_bounds.origin.y + px((self.pt.y - offset.y) as f32 / scale),
            ),
            size: size(px(1.), px(TEXT_FONT_SIZE as f32 / scale)),
        }
    }
}

/// UTF-16 offset (what the platform IME speaks) to a byte offset in `s`.
fn utf16_to_byte(s: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, ch) in s.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += ch.len_utf16();
    }
    s.len()
}

fn byte_to_utf16(s: &str, byte: usize) -> usize {
    s[..byte.min(s.len())].encode_utf16().count()
}

// ---- shared toolbar semantics ----------------------------------------------

/// The buttons every host handles the same way: tool switches, the colour
/// popup toggle, undo / redo / clear, and folding or dropping the text draft.
/// Returns false for ids the host must handle itself (完成, 取消, OCR, ...).
pub fn handle_common_toolbar(
    session: &mut Session,
    text: &mut Option<TextDraft>,
    color_open: &mut bool,
    id: &str,
) -> bool {
    // Diagnostic ladder (logs/diag.log): the button id reaching the shared
    // dispatcher is the first link a real-machine click can confirm.
    crate::perf::log_diag(&format!("dispatch id={id} tool_in={:?}", session.tool));
    // Undo/redo/cancel discard the draft instead, matching their keyboard paths.
    if matches!(id, toolbar::UNDO | toolbar::REDO | toolbar::CANCEL) {
        if text.take().is_some() {
            return true;
        }
    } else {
        commit_text_draft(session, text);
    }
    // Any button other than COLOR dismisses the popup; COLOR toggles it.
    *color_open = id == toolbar::COLOR && !*color_open;
    // Second-row property buttons mutate the session defaults; the next
    // element picks them up. They never change the active tool.
    if handle_props(session, id) {
        return true;
    }
    // The shape button folds rect + ellipse into one picker. Opening it also
    // makes sure a shape tool is active, so the very next click draws a shape;
    // the two sub-options pick which one and dismiss the picker.
    if id == toolbar::SHAPE {
        if !session.tool.is_shape() {
            session.tool = Tool::Rect;
            crate::perf::log_diag("tool -> Rect (shape picker default)");
        }
        session.shape_open = !session.shape_open;
        session.cancel_draft();
        return true;
    }
    if id == toolbar::SHAPE_RECT || id == toolbar::SHAPE_ELLIPSE {
        session.tool = if id == toolbar::SHAPE_RECT {
            Tool::Rect
        } else {
            Tool::Ellipse
        };
        crate::perf::log_diag(&format!("tool -> {:?} (shape sub-option)", session.tool));
        session.shape_open = false;
        session.cancel_draft();
        return true;
    }
    // The arrow button selects an arrow tool directly; the three variants
    // live inline on the properties row, so there is no picker to open. An
    // active variant is kept, so clicking the button only switches back from
    // another tool.
    if id == toolbar::ARROW_GROUP {
        if !session.tool.is_arrow() {
            session.tool = Tool::Arrow;
            crate::perf::log_diag("tool -> Arrow (default variant)");
        }
        session.shape_open = false;
        session.cancel_draft();
        return true;
    }
    // Any other button dismisses the shape picker.
    session.shape_open = false;
    if let Some(style) = LineStyle::from_id(id) {
        session.line_style = style;
        return true;
    }
    if let Some(tool) = Tool::from_id(id) {
        session.cancel_draft();
        session.tool = tool;
        crate::perf::log_diag(&format!("tool -> {tool:?}"));
        return true;
    }
    match id {
        toolbar::COLOR => true,
        toolbar::UNDO => {
            session.undo();
            true
        }
        toolbar::REDO => {
            session.redo();
            true
        }
        toolbar::CLEAR => {
            session.clear();
            true
        }
        _ => false,
    }
}

/// Fold a pending text draft into the document. Any click that leaves the text
/// field -- canvas or toolbar -- has to run this or the typed text is lost.
pub fn commit_text_draft(session: &mut Session, text: &mut Option<TextDraft>) {
    if let Some(edit) = text.take() {
        session.add_text(edit.pt, &edit.buf);
    }
}

/// Second-row property buttons. Returns whether `id` was one of them; the
/// session defaults persist across tool switches, which is what makes the
/// "set once, draw many" flow work. Width, opacity and bubble size are
/// sliders, not buttons, so they never route through here.
pub fn handle_props(session: &mut Session, id: &str) -> bool {
    if id == toolbar::PROP_STROKE {
        session.stroke_enabled = !session.stroke_enabled;
        return true;
    }
    if id == toolbar::PROP_FILL {
        session.fill_enabled = !session.fill_enabled;
        return true;
    }
    if let Some(ends) = ArrowEnds::from_id(id) {
        session.arrow_ends = ends;
        return true;
    }
    if let Some(head) = ArrowHead::from_id(id) {
        session.arrow_head = head;
        return true;
    }
    if let Some(style) = BadgeStyle::from_id(id) {
        session.badge_style = style;
        return true;
    }
    false
}

/// The cells of the second row for `tool`. Shapes get outline/fill/width/line
/// style; arrows get their three variants, then ends/head/width; the number
/// tool shows its three bubble styles plus a size slider inline, so choosing a
/// number needs no second click into a popup. Opacity lives in the colour popup
/// for every tool, so no row carries alpha any more.
pub fn prop_cells(tool: Tool) -> Vec<toolbar::PropCell> {
    use toolbar::PropCell::Btn;
    let mut cells: Vec<toolbar::PropCell> = Vec::new();
    if tool.is_shape() {
        cells.push(Btn(toolbar::PROP_STROKE));
        cells.push(Btn(toolbar::PROP_FILL));
        cells.push(toolbar::PropCell::WidthSlider);
        cells.extend(
            [
                toolbar::LINE_SOLID,
                toolbar::LINE_DASHED,
                toolbar::LINE_CLOUD,
            ]
            .map(Btn),
        );
    } else if tool.is_arrow() {
        // Straight / polyline / curve sit inline ahead of the arrow settings,
        // so every arrow property lives in one row and no picker is needed.
        cells.extend([toolbar::ARROW, toolbar::POLYLINE_ARROW, toolbar::CURVE_ARROW].map(Btn));
        cells.extend(
            [
                toolbar::PROP_ARROW_START,
                toolbar::PROP_ARROW_END,
                toolbar::PROP_ARROW_BOTH,
                toolbar::PROP_ARROW_NONE,
            ]
            .map(Btn),
        );
        cells.extend(
            [
                toolbar::PROP_HEAD_FILLED,
                toolbar::PROP_HEAD_OPEN,
                toolbar::PROP_HEAD_BAR,
            ]
            .map(Btn),
        );
        cells.push(toolbar::PropCell::WidthSlider);
    } else if tool == Tool::Number {
        cells.extend(
            [
                toolbar::PROP_BADGE_FILLED,
                toolbar::PROP_BADGE_HOLLOW,
                toolbar::PROP_BADGE_SQUARE,
            ]
            .map(Btn),
        );
        cells.push(toolbar::PropCell::BadgeSizeSlider);
    }
    cells
}

/// The second row under the toolbar. Its content is entirely driven by the
/// active tool; buttons reuse the first row's style and the width cell is a
/// draggable slider.
pub struct PropsModel {
    /// Logical, window-local top-left of the row.
    pub origin: (f32, f32),
    pub tool: Tool,
    pub stroke_enabled: bool,
    pub fill_enabled: bool,
    pub stroke_width: f64,
    pub line_style: LineStyle,
    pub arrow_ends: ArrowEnds,
    pub arrow_head: ArrowHead,
    pub badge_style: BadgeStyle,
    /// Bubble radius in image px.
    pub badge_size: f64,
}

fn prop_active(model: &PropsModel, id: &str) -> bool {
    if id == toolbar::PROP_STROKE {
        return model.stroke_enabled;
    }
    if id == toolbar::PROP_FILL {
        return model.fill_enabled;
    }
    // The three arrow variants are prop buttons now: the active one tracks the
    // session's arrow tool rather than a stored style.
    if let Some(variant) = Tool::from_id(id).filter(|t| t.is_arrow()) {
        return variant == model.tool;
    }
    if let Some(style) = LineStyle::from_id(id) {
        return style == model.line_style;
    }
    if let Some(ends) = ArrowEnds::from_id(id) {
        return ends == model.arrow_ends;
    }
    if let Some(head) = ArrowHead::from_id(id) {
        return head == model.arrow_head;
    }
    if let Some(style) = BadgeStyle::from_id(id) {
        return style == model.badge_style;
    }
    false
}

/// A callback receiving a 0..=1 slider ratio. Each caller maps it onto its own
/// value range (stroke width, opacity, bubble radius).
pub type OnFraction<V> = Rc<dyn Fn(f64, &mut V, &mut Window, &mut Context<V>)>;

// Drag markers: gpui's active-drag keeps delivering moves after the pointer
// leaves the track (pointer capture), which hover-gated `on_mouse_move` cannot.
struct WidthSliderDrag;
struct AlphaSliderDrag;
struct BadgeSizeSliderDrag;

pub fn props_element<V: 'static>(
    model: PropsModel,
    on_item: OnItem<V>,
    on_width: OnFraction<V>,
    on_badge_size: OnFraction<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let PropsModel {
        origin,
        tool,
        stroke_width,
        badge_size,
        ..
    } = model;
    let cells = prop_cells(tool);
    let mut children: Vec<AnyElement> = Vec::with_capacity(cells.len());
    for (i, cell) in cells.iter().enumerate() {
        match *cell {
            toolbar::PropCell::Btn(id) => children.push(
                toolbar_button(
                    id,
                    prop_active(&model, id),
                    false,
                    None,
                    on_item.clone(),
                    cx,
                )
                .into_any_element(),
            ),
            toolbar::PropCell::WidthSlider => {
                let track_x =
                    origin.0 + toolbar::BORDER as f32 + toolbar::prop_cell_x(&cells, i) as f32;
                children.push(
                    width_slider_cell(track_x, stroke_width, on_width.clone(), cx)
                        .into_any_element(),
                );
            }
            toolbar::PropCell::BadgeSizeSlider => {
                let track_x =
                    origin.0 + toolbar::BORDER as f32 + toolbar::prop_cell_x(&cells, i) as f32;
                children.push(
                    badge_size_slider_cell(track_x, badge_size, on_badge_size.clone(), cx)
                        .into_any_element(),
                );
            }
        }
    }
    div()
        .absolute()
        .left(px(origin.0))
        .top(px(origin.1))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(toolbar::ITEM_GAP as f32))
        .px(px(toolbar::PAD as f32))
        .py(px(toolbar::PAD as f32))
        .rounded_xl()
        .bg(theme::surface())
        .border_1()
        .border_color(theme::border())
        .shadow(chrome_shadow())
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .children(children)
}

/// The width cell: track + `12px` readout, sized to `toolbar::SLIDER_W`.
fn width_slider_cell<V: 'static>(
    track_x: f32,
    width: f64,
    on_change: OnFraction<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let readout = format!("{:.0}px", width);
    div()
        .w(px(toolbar::SLIDER_W as f32))
        .h(px(toolbar::BTN as f32))
        .flex()
        .items_center()
        .child(slider_row(
            "prop-width-track",
            WidthSliderDrag,
            track_x,
            toolbar::SLIDER_TRACK_W as f32,
            toolbar::fraction_for_width(width),
            readout,
            toolbar::SLIDER_READOUT_W as f32,
            on_change,
            cx,
        ))
}

/// The bubble-size cell: same track + readout as the width cell, mapped onto
/// the 8-40 px radius range the export burns.
fn badge_size_slider_cell<V: 'static>(
    track_x: f32,
    radius: f64,
    on_change: OnFraction<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    div()
        .w(px(toolbar::SLIDER_W as f32))
        .h(px(toolbar::BTN as f32))
        .flex()
        .items_center()
        .child(slider_row(
            "badge-size-track",
            BadgeSizeSliderDrag,
            track_x,
            toolbar::SLIDER_TRACK_W as f32,
            toolbar::fraction_for_badge_radius(radius),
            format!("{radius:.0}px"),
            toolbar::SLIDER_READOUT_W as f32,
            on_change,
            cx,
        ))
}

/// Rail + filled portion + knob, then a fixed-width readout. The press already
/// maps to a value (so clicking the track jumps); `on_drag`/`on_drag_move`
/// keep the thumb following the pointer after it leaves the track, and the
/// mouse-up that ends the drag does not touch the value again.
#[allow(clippy::too_many_arguments)]
fn slider_row<T: 'static, V: 'static>(
    id: &'static str,
    drag: T,
    track_x: f32,
    track_w: f32,
    fraction: f64,
    readout: String,
    readout_w: f32,
    on_change: OnFraction<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let f = fraction.clamp(0.0, 1.0) as f32;
    let knob = toolbar::KNOB as f32;
    let usable = (track_w - knob).max(1.0);
    let knob_x = f * usable;
    let hit_h = toolbar::TRACK_HIT_H as f32;
    let rail_h = toolbar::TRACK_H as f32;
    let press = on_change.clone();
    let dragged = on_change.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(toolbar::ROW_GAP as f32))
        .child(
            div()
                .id(id)
                .relative()
                .flex_none()
                .w(px(track_w))
                .h(px(hit_h))
                .cursor(CursorStyle::PointingHand)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        let f = toolbar::slider_fraction(
                            f32::from(event.position.x) as f64,
                            track_x as f64,
                            track_w as f64,
                        );
                        press(f, this, window, cx);
                    }),
                )
                .on_drag(drag, |_, _, _, cx| cx.new(|_| gpui::Empty))
                .on_drag_move::<T>(cx.listener(
                    move |this, event: &gpui::DragMoveEvent<T>, window, cx| {
                        let bounds = event.bounds;
                        let f = toolbar::slider_fraction(
                            f32::from(event.event.position.x) as f64,
                            f32::from(bounds.origin.x) as f64,
                            f32::from(bounds.size.width) as f64,
                        );
                        dragged(f, this, window, cx);
                        cx.stop_propagation();
                    },
                ))
                .child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(px((hit_h - rail_h) / 2.0))
                        .w(px(track_w))
                        .h(px(rail_h))
                        .rounded_full()
                        .bg(theme::input()),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(knob / 2.0))
                        .top(px((hit_h - rail_h) / 2.0))
                        .w(px(knob_x))
                        .h(px(rail_h))
                        .rounded_full()
                        .bg(theme::accent()),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(knob_x))
                        .top(px((hit_h - knob) / 2.0))
                        .w(px(knob))
                        .h(px(knob))
                        .rounded_full()
                        .bg(theme::accent())
                        .border_1()
                        .border_color(theme::surface()),
                ),
        )
        .child(
            div()
                .w(px(readout_w))
                .text_size(px(11.))
                .text_color(theme::secondary_fg())
                .child(readout),
        )
}

// ---- preview caches ---------------------------------------------------------

/// Identity of a mosaic readback: which elements, where, at what block size, and
/// how many path points -- extending a stroke inside its own bounds moves nothing else.
pub type MosaicKey = Vec<(u64, Rect, i32, usize)>;

#[derive(Default)]
pub struct MosaicTiles {
    pub tiles: Vec<(Rect, u32)>,
    pub covered: Vec<u64>,
}

/// Last mosaic readback, keyed by the elements that produced it. Hosts repaint
/// every frame; without this each mosaic costs a readback per frame. Shared via
/// `Rc` so a cache hit does not clone a tiles Vec that can hold thousands.
pub type MosaicCache = RefCell<Option<(MosaicKey, Rc<MosaicTiles>)>>;

/// Averaged mosaic cells for the visible elements. `readback(x, y, w, h)`
/// returns BGRA pixels of that edit-space rect (`pixels, w, h, stride`);
/// `extent` bounds the requests.
pub fn mosaic_tiles(
    cache: &MosaicCache,
    marks: &[Element],
    extent: (u32, u32),
    mut readback: impl FnMut(u32, u32, u32, u32) -> Option<(Vec<u8>, u32, u32, u32)>,
) -> Rc<MosaicTiles> {
    let mut out = MosaicTiles::default();
    let key: MosaicKey = marks
        .iter()
        .filter_map(|el| match &el.shape {
            Shape::Mosaic { points, block_size } => {
                Some((el.id, el.bounds, *block_size, points.len()))
            }
            _ => None,
        })
        .collect();
    if let Some((cached_key, tiles)) = cache.borrow().as_ref() {
        if *cached_key == key {
            return tiles.clone();
        }
    }
    for el in marks {
        let Shape::Mosaic { points, block_size } = &el.shape else {
            continue;
        };
        let block = (*block_size).max(2);
        let cells = crate::overlay::editor::mosaic_cells(points, block);
        if cells.is_empty() {
            continue;
        }
        // Read back one grid-aligned rect covering every cell, then average
        // inside it. Per-cell readbacks would be one round trip per tile.
        // ponytail: every new path point invalidates the cache and re-reads the
        // whole stroke bounds. Fine up to a few hundred cells; if smudging across
        // a full screen drops frames, cache per element id and only read back the
        // cells the newest segment added.
        let gx0 = cells.iter().map(|c| c.0).min().unwrap_or(0);
        let gy0 = cells.iter().map(|c| c.1).min().unwrap_or(0);
        let gx1 = cells.iter().map(|c| c.0).max().unwrap_or(0) + 1;
        let gy1 = cells.iter().map(|c| c.1).max().unwrap_or(0) + 1;
        let x = (gx0 * block).clamp(0, extent.0 as i32) as u32;
        let y = (gy0 * block).clamp(0, extent.1 as i32) as u32;
        let w = (gx1 * block).clamp(0, extent.0 as i32) as u32 - x;
        let h = (gy1 * block).clamp(0, extent.1 as i32) as u32 - y;
        if w < 1 || h < 1 {
            continue;
        }
        let Some((px, cw, ch, stride)) = readback(x, y, w, h) else {
            continue;
        };
        for (gx, gy) in cells {
            let cx0 = (gx * block - x as i32).clamp(0, cw as i32);
            let cy0 = (gy * block - y as i32).clamp(0, ch as i32);
            let cx1 = (gx * block + block - x as i32).clamp(0, cw as i32);
            let cy1 = (gy * block + block - y as i32).clamp(0, ch as i32);
            if cx1 <= cx0 || cy1 <= cy0 {
                continue;
            }
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for sy in cy0..cy1 {
                for sx in cx0..cx1 {
                    let i = (sy as u32 * stride + sx as u32 * 4) as usize;
                    if i + 2 >= px.len() {
                        continue;
                    }
                    b += px[i] as u32;
                    g += px[i + 1] as u32;
                    r += px[i + 2] as u32;
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            let rgb = ((r / n) << 16) | ((g / n) << 8) | (b / n);
            out.tiles.push((
                Rect::new(
                    (gx * block) as f64,
                    (gy * block) as f64,
                    block as f64,
                    block as f64,
                ),
                rgb,
            ));
        }
        out.covered.push(el.id);
    }
    let out = Rc::new(out);
    *cache.borrow_mut() = Some((key, out.clone()));
    out
}

/// Cached coverage runs aligned 1:1 with the element list that produced them.
pub type MarkMasks = (Vec<Element>, Vec<Option<Rc<ElementRuns>>>);
pub type MaskCache = RefCell<Option<MarkMasks>>;

/// Per-element coverage runs, cached against the exact element list. Preview
/// and export rasterize from the same masks (`composite::element_coverage`),
/// so this cache is the only thing standing between a per-frame repaint and
/// re-rasterizing every stroke (and re-scanning it into runs) at 60 fps.
pub fn mark_masks(cache: &MaskCache, marks: &[Element]) -> Vec<Option<Rc<ElementRuns>>> {
    if let Some((key, masks)) = cache.borrow().as_ref() {
        if key.as_slice() == marks {
            return masks.clone();
        }
    }
    let masks: Vec<Option<Rc<ElementRuns>>> = marks
        .iter()
        .map(|el| {
            let cov = element_coverage(el);
            let runs = ElementRuns {
                fill: cov.fill.map(|m| m.runs()).unwrap_or_default(),
                stroke: cov.stroke.map(|m| m.runs()).unwrap_or_default(),
            };
            (!runs.is_empty()).then(|| Rc::new(runs))
        })
        .collect();
    *cache.borrow_mut() = Some((marks.to_vec(), masks.clone()));
    masks
}

// ---- painting ---------------------------------------------------------------

/// Paints the mosaic tiles and every non-text element. Text is a GPUI div in
/// the host's element tree (`text_layer`); the CPU composite handles export.
pub fn paint_marks(
    window: &mut Window,
    marks: &[Element],
    masks: &[Option<Rc<ElementRuns>>],
    mosaic: &MosaicTiles,
    offset: Point,
    scale: f32,
) {
    for (r, color) in &mosaic.tiles {
        window.paint_quad(fill(rect_to_bounds_at(*r, offset, scale), rgb(*color)));
    }
    for (el, mask) in marks.iter().zip(masks.iter()) {
        match &el.shape {
            Shape::Mosaic { .. } => {
                if !mosaic.covered.contains(&el.id) {
                    window.paint_quad(fill(
                        rect_to_bounds_at(el.bounds, offset, scale),
                        gpui::hsla(0., 0., 0.5, 0.35),
                    ));
                }
            }
            Shape::Text { .. } => {}
            _ => {
                let Some(runs) = mask else { continue };
                if let Some(fill_color) = el.fill {
                    paint_mask(window, &runs.fill, offset, scale, argb_color(fill_color));
                }
                paint_mask(window, &runs.stroke, offset, scale, argb_color(el.stroke));
            }
        }
    }
}

/// Full-ARGB to the gpui colour the preview paints, alpha included: the
/// properties row's opacity must look the same on screen as in the export.
fn argb_color(argb: u32) -> gpui::Rgba {
    let mut c = rgb(argb & 0x00FF_FFFF);
    c.a = ((argb >> 24) & 0xFF) as f32 / 255.0;
    c
}

/// Paint one element's coverage mask as alpha-weighted quads, one per
/// constant-coverage run of physical pixels.
///
/// The mask IS the per-pixel coverage the export composite blends
/// (`composite::element_mask`), so the preview cannot drift from the saved
/// image. Each run becomes a quad whose logical coordinates are physical/scale,
/// which `paint_quad` multiplies back by `scale_factor`: at 100%/125%/.../200%
/// DPI every quad lands exactly on whole device pixels, and the coverage alpha
/// supplies the antialiasing the quad pipeline itself does not have (plain
/// filled quads take a hard-edged fast path in the DirectX shader). Runs are
/// disjoint, so overlapping segments never double-blend -- the mask already
/// resolved them with max.
fn paint_mask(
    window: &mut Window,
    runs: &[CoverageRun],
    offset: Point,
    scale: f32,
    color: gpui::Rgba,
) {
    for run in runs {
        let mut c = color;
        c.a *= run.alpha as f32 / 255.0;
        window.paint_quad(fill(
            Bounds {
                origin: point(
                    px((run.x0 as f64 - offset.x) as f32 / scale),
                    px((run.y as f64 - offset.y) as f32 / scale),
                ),
                size: size(px((run.x1 - run.x0) as f32 / scale), px(1.0 / scale)),
            },
            c,
        ));
    }
}

pub fn rect_to_bounds(r: Rect, scale: f32) -> Bounds<Pixels> {
    rect_to_bounds_at(r, Point { x: 0.0, y: 0.0 }, scale)
}

fn rect_to_bounds_at(r: Rect, offset: Point, scale: f32) -> Bounds<Pixels> {
    Bounds {
        origin: point(
            px((r.x - offset.x) as f32 / scale),
            px((r.y - offset.y) as f32 / scale),
        ),
        size: size(px(r.width as f32 / scale), px(r.height as f32 / scale)),
    }
}

// ---- text layer -------------------------------------------------------------

pub struct TextLayer<'a> {
    /// Logical, window-local box the text is clipped to.
    pub frame: Rect,
    /// Edit-space point at the frame's origin, physical px.
    pub offset: Point,
    pub scale: f32,
    pub elements: &'a [Element],
    pub draft: Option<&'a TextDraft>,
    /// Stroke colour the draft is drawn in.
    pub stroke: u32,
}

/// Committed text elements plus the draft, drawn exactly like the committed
/// element will be -- same origin, face, size and colour, no box -- so typing
/// is a live preview.
pub fn text_layer(layer: TextLayer<'_>) -> impl IntoElement {
    let TextLayer {
        frame,
        offset,
        scale,
        elements,
        draft,
        stroke,
    } = layer;
    let committed: Vec<_> = elements
        .iter()
        .filter_map(|el| {
            let Shape::Text { text, font_size } = &el.shape else {
                return None;
            };
            Some(
                div()
                    .absolute()
                    .left(px((el.bounds.x - offset.x) as f32 / scale))
                    .top(px((el.bounds.y - offset.y) as f32 / scale))
                    .font_family(text_raster::FONT_FAMILY)
                    .text_color(argb_color(el.stroke))
                    .text_size(px(*font_size as f32 / scale))
                    .child(text.clone()),
            )
        })
        .collect();
    let draft = draft.map(|edit| {
        div()
            .absolute()
            .left(px((edit.pt.x - offset.x) as f32 / scale))
            .top(px((edit.pt.y - offset.y) as f32 / scale))
            .min_w(px(40.))
            .font_family(text_raster::FONT_FAMILY)
            .text_color(argb_color(stroke))
            .text_size(px(TEXT_FONT_SIZE as f32 / scale))
            .occlude()
            .child(format!("{}|", edit.buf))
    });
    div()
        .absolute()
        .left(px(frame.x as f32))
        .top(px(frame.y as f32))
        .w(px(frame.width as f32))
        .h(px(frame.height as f32))
        .overflow_hidden()
        .children(committed)
        .children(draft)
}

/// Number bubbles live in the element tree like text (they are not a vector
/// coverage mask), so the live preview draws the same disc/ring/square and
/// numeral the export rasterizer burns in `composite::draw_badge`. `elements`
/// includes the draft, so a bubble follows the cursor while the button is held
/// down.
pub fn badge_layer(
    frame: Rect,
    offset: Point,
    scale: f32,
    elements: &[Element],
) -> impl IntoElement {
    let committed = elements
        .iter()
        .filter_map(|el| {
            let Shape::Badge {
                number,
                style,
                radius,
            } = &el.shape
            else {
                return None;
            };
            let left = (el.bounds.x - offset.x) as f32 / scale;
            let top = (el.bounds.y - offset.y) as f32 / scale;
            let size = el.bounds.width as f32 / scale;
            let ink: gpui::Hsla = argb_color(el.stroke).into();
            let ring = (*radius * 0.16).max(1.5) as f32 / scale;
            let font = badge_font_size(*radius) as f32 / scale;
            let mut bubble = div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(size))
                .h(px(size))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(font))
                .font_family(text_raster::FONT_FAMILY);
            bubble = match style {
                BadgeStyle::Filled => bubble.rounded_full().bg(ink).text_color(white()),
                BadgeStyle::Hollow => bubble
                    .rounded_full()
                    .border(px(ring))
                    .border_color(ink)
                    .text_color(ink),
                BadgeStyle::Square => bubble.bg(ink).text_color(white()),
            };
            Some(bubble.child(number.to_string()))
        })
        .collect::<Vec<_>>();
    // Diagnostic ladder: prove the element tree actually received the bubbles.
    // Throttled to count changes so a 60 fps repaint does not spam the file.
    static LAST_BADGE_COUNT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
    let n = committed.len() as i32;
    if n != LAST_BADGE_COUNT.swap(n, std::sync::atomic::Ordering::Relaxed) {
        crate::perf::log_diag(&format!("badge_layer renders {n} bubble(s)"));
    }
    div()
        .absolute()
        .left(px(frame.x as f32))
        .top(px(frame.y as f32))
        .w(px(frame.width as f32))
        .h(px(frame.height as f32))
        .overflow_hidden()
        .children(committed)
}

// ---- toolbar ----------------------------------------------------------------

/// Callback for a toolbar button press, keyed by the button id.
pub type OnItem<V> = Rc<dyn Fn(&'static str, &mut V, &mut Window, &mut Context<V>)>;
/// Callback for a swatch pick: the ARGB stroke colour chosen.
pub type OnPick<V> = Rc<dyn Fn(u32, &mut V, &mut Window, &mut Context<V>)>;

pub struct ToolbarModel {
    /// Logical, window-local top-left of the bar.
    pub origin: (f32, f32),
    pub items: Vec<&'static str>,
    pub tool_id: &'static str,
    pub can_undo: bool,
    pub can_redo: bool,
    pub color_open: bool,
    pub stroke: u32,
    pub line_style: LineStyle,
    pub shape_open: bool,
}

fn chrome_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: theme::base().opacity(0.12),
        offset: point(px(0.), px(4.)),
        blur_radius: px(16.),
        spread_radius: px(0.),
        inset: false,
    }]
}

pub fn toolbar_element<V: 'static>(
    model: ToolbarModel,
    on_item: OnItem<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let ToolbarModel {
        origin,
        items,
        tool_id,
        can_undo,
        can_redo,
        color_open,
        stroke,
        line_style,
        shape_open,
    } = model;
    // Separators are painted in the gaps instead of occupying layout space:
    // hit-testing (`toolbar_item_at`) assumes one uniform BTN + ITEM_GAP pitch,
    // so an extra flex child would shift every slot after it.
    let mut buttons: Vec<AnyElement> = Vec::with_capacity(items.len());
    let mut separators: Vec<AnyElement> = Vec::new();
    for (i, id) in items.iter().enumerate() {
        if i > 0 && toolbar::starts_group(id) {
            let x = toolbar::separator_x(i) as f32;
            separators.push(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(9.))
                    .w(px(1.))
                    .h(px(26.))
                    .bg(theme::secondary_fg().opacity(0.18))
                    .into_any_element(),
            );
        }
        let is_color = *id == toolbar::COLOR;
        // The colour button tracks its popup, line-style buttons track the
        // active style, everything else tracks the active tool.
        let active = if is_color {
            color_open
        } else if *id == toolbar::SHAPE {
            shape_open || tool_id == toolbar::RECT || tool_id == toolbar::ELLIPSE
        } else if *id == toolbar::ARROW_GROUP {
            tool_id == toolbar::ARROW
                || tool_id == toolbar::POLYLINE_ARROW
                || tool_id == toolbar::CURVE_ARROW
        } else if let Some(style) = LineStyle::from_id(id) {
            style == line_style
        } else {
            tool_id == *id
        };
        let dim = (*id == toolbar::UNDO && !can_undo) || (*id == toolbar::REDO && !can_redo);
        buttons.push(
            toolbar_button(
                id,
                active,
                dim,
                is_color.then_some(stroke),
                on_item.clone(),
                cx,
            )
            .into_any_element(),
        );
    }
    div()
        .absolute()
        .left(px(origin.0))
        .top(px(origin.1))
        // The inner row is the positioned ancestor for the painted separators,
        // so they land in the gaps rather than at the window origin.
        .child(
            div()
                .relative()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(toolbar::ITEM_GAP as f32))
                .px(px(toolbar::PAD as f32))
                .py(px(toolbar::PAD as f32))
                .rounded_xl()
                .bg(theme::surface())
                .border_1()
                .border_color(theme::border())
                .shadow(chrome_shadow())
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .children(buttons)
                .children(separators),
        )
}

pub struct SwatchesModel {
    /// Logical, window-local top-left of the popup.
    pub origin: (f32, f32),
    pub current: u32,
    /// Current opacity, percent; the swatch row stays the palette colour and
    /// the slider controls how it is composited.
    pub alpha: u8,
}

/// The colour popup: palette row plus the alpha slider. Opacity lives here so
/// every tool (shape, arrow, pen, text, number, mosaic) shares one control and
/// one source of truth (`Session.alpha`).
pub fn swatches_element<V: 'static>(
    model: SwatchesModel,
    on_pick: OnPick<V>,
    on_alpha: OnFraction<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let SwatchesModel {
        origin,
        current,
        alpha,
    } = model;
    let alpha_label = div()
        .w(px(toolbar::ALPHA_LABEL_W as f32))
        .text_size(px(11.))
        .text_color(theme::secondary_fg())
        .child("透明度");
    div()
        .absolute()
        .left(px(origin.0))
        .top(px(origin.1))
        .flex()
        .flex_col()
        .items_start()
        .gap(px(toolbar::ROW_GAP as f32))
        .p(px(toolbar::PAD as f32))
        .rounded_xl()
        .bg(theme::surface())
        .border_1()
        .border_color(theme::border())
        .shadow(chrome_shadow())
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(toolbar::SWATCH_GAP as f32))
                .children(theme::SWATCHES.map(|(hex, rgb24)| {
                    let argb = 0xFF00_0000 | rgb24;
                    let on_pick = on_pick.clone();
                    div()
                        .id(SharedString::from(hex))
                        .w(px(toolbar::SWATCH as f32))
                        .h(px(toolbar::SWATCH as f32))
                        .rounded_full()
                        .bg(rgb(rgb24))
                        .border_2()
                        .border_color(if current == argb {
                            theme::accent()
                        } else {
                            theme::border()
                        })
                        .cursor(CursorStyle::PointingHand)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                on_pick(argb, this, window, cx);
                            }),
                        )
                })),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(toolbar::ROW_GAP as f32))
                .child(alpha_label)
                .child(slider_row(
                    "alpha-track",
                    AlphaSliderDrag,
                    toolbar::alpha_track_x(origin.0 as f64) as f32,
                    toolbar::ALPHA_TRACK_W as f32,
                    alpha as f64 / 100.0,
                    format!("{alpha}%"),
                    toolbar::ALPHA_READOUT_W as f32,
                    on_alpha,
                    cx,
                )),
        )
}

/// The rect / ellipse picker that the merged shape button opens. Both entries
/// are icon-only buttons; `on_item` receives SHAPE_RECT / SHAPE_ELLIPSE.
pub struct ShapePopupModel {
    /// Logical, window-local top-left of the popup.
    pub origin: (f32, f32),
    /// The active shape, highlighted in the picker.
    pub current: Tool,
}

pub fn shape_popup_element<V: 'static>(
    model: ShapePopupModel,
    on_item: OnItem<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let ShapePopupModel { origin, current } = model;
    div()
        .absolute()
        .left(px(origin.0))
        .top(px(origin.1))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(toolbar::ITEM_GAP as f32))
        .p(px(toolbar::PAD as f32))
        .rounded_xl()
        .bg(theme::surface())
        .border_1()
        .border_color(theme::border())
        .shadow(chrome_shadow())
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .children(
            [toolbar::SHAPE_RECT, toolbar::SHAPE_ELLIPSE]
                .into_iter()
                .map(|id| {
                    let active = (id == toolbar::SHAPE_RECT && current == Tool::Rect)
                        || (id == toolbar::SHAPE_ELLIPSE && current == Tool::Ellipse);
                    toolbar_button(id, active, false, None, on_item.clone(), cx)
                }),
        )
}

/// Hover label for a toolbar button. gpui owns the show delay and placement; this
/// only has to render the bubble.
struct ToolLabel(SharedString);

impl Render for ToolLabel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(6.))
            .py(px(3.))
            .rounded_md()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .text_color(theme::primary_fg())
            .text_size(px(12.))
            .child(self.0.clone())
    }
}

/// `swatch` is `Some` only for COLOR, which shows the current stroke colour
/// instead of an icon.
fn toolbar_button<V: 'static>(
    id: &'static str,
    active: bool,
    dim: bool,
    swatch: Option<u32>,
    on_item: OnItem<V>,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let fg = if dim {
        theme::disabled_fg()
    } else if active {
        theme::accent()
    } else {
        theme::primary_fg()
    };
    let idle_bg = if active {
        theme::active_background()
    } else {
        theme::transparent()
    };
    let hover_bg = if active {
        theme::active_background()
    } else {
        theme::elevated_surface()
    };
    div()
        .id(id)
        .w(px(toolbar::BTN as f32))
        .h(px(toolbar::BTN as f32))
        .rounded(px(7.))
        .flex()
        .items_center()
        .justify_center()
        .bg(idle_bg)
        .text_color(fg)
        .cursor(if dim {
            CursorStyle::Arrow
        } else {
            CursorStyle::PointingHand
        })
        .when(!dim, |el| el.hover(move |s| s.bg(hover_bg)))
        .child(match swatch {
            // Checkerboard behind the colour so the alpha byte is visible, not
            // just stored. A 2x2 grey/white grid reads as "transparent" at
            // 16 px without needing a pattern shader.
            Some(argb) => {
                let mut tint = rgb(argb & 0xFF_FFFF);
                tint.a = ((argb >> 24) & 0xFF) as f32 / 255.0;
                let half = toolbar::SWATCH_ICON / 2.0;
                div()
                    .relative()
                    .w(px(toolbar::SWATCH_ICON))
                    .h(px(toolbar::SWATCH_ICON))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(theme::surface_solid())
                    .border_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .absolute()
                            .left(px(half))
                            .top(px(0.))
                            .w(px(half))
                            .h(px(half))
                            .bg(theme::elevated_surface()),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(half))
                            .w(px(half))
                            .h(px(half))
                            .bg(theme::elevated_surface()),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(0.))
                            .size_full()
                            .rounded_full()
                            .bg(tint),
                    )
                    .into_any_element()
            }
            None => svg()
                .path(toolbar::icon_path(id).unwrap_or(""))
                .w(px(toolbar::ICON))
                .h(px(toolbar::ICON))
                .text_color(fg)
                .into_any_element(),
        })
        .tooltip(move |_, cx| {
            let view: AnyView = cx.new(|_| ToolLabel(toolbar::label(id).into())).into();
            view
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, _, window, cx| {
                on_item(id, view, window, cx);
                cx.stop_propagation();
            }),
        )
}

/// Mirror the rendered layout -- `PAD`, then one `BTN` every `BTN + ITEM_GAP`.
/// Splitting `geom.width` into equal slots instead drifts by the padding and can
/// land a click on Cancel in Complete's slot.
pub fn toolbar_item_at(p: Point, geom: Rect, items: &[&'static str]) -> Option<&'static str> {
    if items.is_empty() || !geom.contains(p) {
        return None;
    }
    let s = geom.width / toolbar::bar_size(items.len()).0;
    let pitch = (toolbar::BTN + toolbar::ITEM_GAP) * s;
    if pitch <= 0.0 {
        return None;
    }
    let rel = p.x - geom.x - toolbar::PAD * s;
    if rel < 0.0 {
        return None;
    }
    let i = (rel / pitch).floor();
    if rel - i * pitch > toolbar::BTN * s {
        return None;
    }
    items.get(i as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::toolbar::PropCell;

    /// Selecting the number tool shows its three bubble styles and the size
    /// slider directly on the second row -- no second click into a popup.
    #[test]
    fn number_row_has_three_styles_and_a_size_slider() {
        assert_eq!(
            prop_cells(Tool::Number),
            vec![
                PropCell::Btn(toolbar::PROP_BADGE_FILLED),
                PropCell::Btn(toolbar::PROP_BADGE_HOLLOW),
                PropCell::Btn(toolbar::PROP_BADGE_SQUARE),
                PropCell::BadgeSizeSlider,
            ]
        );
    }

    /// The row carries no opacity control (it lives in the colour popup) and
    /// no entry that opens a second number panel.
    #[test]
    fn number_row_has_no_alpha_and_no_popup_button() {
        let cells = prop_cells(Tool::Number);
        assert!(!cells.contains(&PropCell::WidthSlider));
        assert!(!cells.iter().any(|c| matches!(
            c,
            PropCell::Btn(id) if *id != toolbar::PROP_BADGE_FILLED
                && *id != toolbar::PROP_BADGE_HOLLOW
                && *id != toolbar::PROP_BADGE_SQUARE
        )));
        assert_eq!(
            cells
                .iter()
                .filter(|c| **c == PropCell::BadgeSizeSlider)
                .count(),
            1
        );
    }

    /// Shape and arrow rows swapped the four width presets for one slider;
    /// the remaining drawing tools keep an empty row (opacity moved to the
    /// colour popup).
    #[test]
    fn shape_and_arrow_rows_use_one_width_slider() {
        for tool in [
            Tool::Rect,
            Tool::Ellipse,
            Tool::Arrow,
            Tool::PolylineArrow,
            Tool::CurveArrow,
        ] {
            let cells = prop_cells(tool);
            assert_eq!(
                cells
                    .iter()
                    .filter(|c| **c == PropCell::WidthSlider)
                    .count(),
                1,
                "{tool:?}"
            );
            assert!(
                !cells.contains(&PropCell::BadgeSizeSlider),
                "{tool:?}"
            );
        }
        for tool in [Tool::Select, Tool::Pen, Tool::Mosaic, Tool::Text] {
            assert!(prop_cells(tool).is_empty(), "{tool:?}");
        }
    }

    /// Every arrow property lives on one row: the three variants first, then
    /// the four end options, the three head styles and the width slider.
    #[test]
    fn arrow_row_has_three_variants_then_ends_then_heads() {
        assert_eq!(
            prop_cells(Tool::Arrow),
            vec![
                PropCell::Btn(toolbar::ARROW),
                PropCell::Btn(toolbar::POLYLINE_ARROW),
                PropCell::Btn(toolbar::CURVE_ARROW),
                PropCell::Btn(toolbar::PROP_ARROW_START),
                PropCell::Btn(toolbar::PROP_ARROW_END),
                PropCell::Btn(toolbar::PROP_ARROW_BOTH),
                PropCell::Btn(toolbar::PROP_ARROW_NONE),
                PropCell::Btn(toolbar::PROP_HEAD_FILLED),
                PropCell::Btn(toolbar::PROP_HEAD_OPEN),
                PropCell::Btn(toolbar::PROP_HEAD_BAR),
                PropCell::WidthSlider,
            ]
        );
        // The other two arrow tools share the identical row.
        for tool in [Tool::PolylineArrow, Tool::CurveArrow] {
            assert_eq!(prop_cells(tool), prop_cells(Tool::Arrow), "{tool:?}");
        }
    }

    /// A variant button on the row switches the tool and leaves the row (and
    /// the property defaults) in place.
    #[test]
    fn arrow_variant_button_switches_the_tool() {
        let mut s = Session::new();
        let mut text = None;
        let mut color_open = false;
        s.arrow_ends = ArrowEnds::Both;
        s.arrow_head = ArrowHead::Bar;
        for (id, want) in [
            (toolbar::POLYLINE_ARROW, Tool::PolylineArrow),
            (toolbar::CURVE_ARROW, Tool::CurveArrow),
            (toolbar::ARROW, Tool::Arrow),
        ] {
            assert!(handle_common_toolbar(&mut s, &mut text, &mut color_open, id));
            assert_eq!(s.tool, want);
            assert_eq!(s.arrow_ends, ArrowEnds::Both, "{id} keeps the ends");
            assert_eq!(s.arrow_head, ArrowHead::Bar, "{id} keeps the head");
            assert_eq!(prop_cells(s.tool).len(), 11, "{id} keeps the row open");
        }
        // The active variant is what the row highlights.
        let model = PropsModel {
            origin: (0.0, 0.0),
            tool: s.tool,
            stroke_enabled: true,
            fill_enabled: false,
            stroke_width: 4.0,
            line_style: LineStyle::Solid,
            arrow_ends: s.arrow_ends,
            arrow_head: s.arrow_head,
            badge_style: BadgeStyle::Filled,
            badge_size: 12.0,
        };
        assert!(prop_active(&model, toolbar::ARROW));
        assert!(!prop_active(&model, toolbar::POLYLINE_ARROW));
    }

    /// The bar button selects an arrow directly; it never opens a popup, and
    /// clicking it again keeps the variant the user already chose.
    #[test]
    fn arrow_button_no_longer_opens_a_popup() {
        let mut s = Session::new();
        let mut text = None;
        let mut color_open = false;
        assert!(!s.tool.is_arrow());
        assert!(handle_common_toolbar(
            &mut s,
            &mut text,
            &mut color_open,
            toolbar::ARROW_GROUP,
        ));
        assert_eq!(s.tool, Tool::Arrow, "defaults to the straight arrow");
        // A second press must not toggle anything off.
        assert!(handle_common_toolbar(
            &mut s,
            &mut text,
            &mut color_open,
            toolbar::ARROW_GROUP,
        ));
        assert_eq!(s.tool, Tool::Arrow);
        // Choosing a variant, then pressing the bar button again, keeps it.
        assert!(handle_common_toolbar(
            &mut s,
            &mut text,
            &mut color_open,
            toolbar::CURVE_ARROW,
        ));
        assert!(handle_common_toolbar(
            &mut s,
            &mut text,
            &mut color_open,
            toolbar::ARROW_GROUP,
        ));
        assert_eq!(s.tool, Tool::CurveArrow);
    }

    /// Opacity has one source: the same percent drives outline and fill.
    #[test]
    fn alpha_drives_both_stroke_and_fill() {
        let mut s = Session::new();
        s.stroke = 0xFF00_00FF;
        s.fill_enabled = true;
        s.set_alpha(40.0);
        let a = (40 * 255 / 100) << 24;
        assert_eq!(s.stroke_argb(), a | 0x0000_00FF);
        assert_eq!(s.fill_argb(), Some(a | 0x0000_00FF));
        s.fill_enabled = false;
        assert_eq!(s.fill_argb(), None);
    }

    /// The inline style buttons write the session default straight away, and
    /// the size slider's fraction maps onto the 8-40 px radius range.
    #[test]
    fn number_style_buttons_write_the_session_default() {
        let mut s = Session::new();
        let mut text = None;
        let mut color_open = false;
        for (id, style) in [
            (toolbar::PROP_BADGE_HOLLOW, BadgeStyle::Hollow),
            (toolbar::PROP_BADGE_SQUARE, BadgeStyle::Square),
            (toolbar::PROP_BADGE_FILLED, BadgeStyle::Filled),
        ] {
            assert!(handle_common_toolbar(&mut s, &mut text, &mut color_open, id));
            assert_eq!(s.badge_style, style);
        }
        for (f, want) in [(0.0, 8.0), (0.5, 24.0), (1.0, 40.0)] {
            s.set_badge_size(toolbar::badge_radius_for_fraction(f));
            assert_eq!(s.badge_size, want);
        }
    }
}
