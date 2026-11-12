//! Overlay toolbar catalog and placement. Ported from C# OverlayToolbarCatalog / ToolbarAnchor.

use crate::geom::{Point, Rect};

pub const COLOR: &str = "color";
pub const SELECT: &str = "select";
pub const RECT: &str = "rect";
pub const ELLIPSE: &str = "ellipse";
/// The merged shape button and its two picker sub-options.
pub const SHAPE: &str = "shape";
pub const SHAPE_RECT: &str = "shapeRect";
pub const SHAPE_ELLIPSE: &str = "shapeEllipse";
pub const LINE: &str = "line";
/// The bar's arrow button (ARROW_GROUP icon) and the three arrow tool ids it
/// selects. The three variants are chosen on the properties row, not a picker.
pub const ARROW_GROUP: &str = "arrowGroup";
pub const ARROW: &str = "arrow";
pub const POLYLINE_ARROW: &str = "polylineArrow";
pub const CURVE_ARROW: &str = "curveArrow";
pub const LINE_SOLID: &str = "lineSolid";
pub const LINE_DASHED: &str = "lineDashed";
pub const LINE_CLOUD: &str = "lineCloud";
pub const PEN: &str = "pen";
pub const TEXT: &str = "text";
pub const MOSAIC: &str = "mosaic";
pub const NUMBER: &str = "number";
pub const UNDO: &str = "undo";
pub const REDO: &str = "redo";
pub const CLEAR: &str = "clear";
pub const OCR: &str = "ocr";
pub const TRANSLATE: &str = "translate";
pub const LONG_CAPTURE: &str = "longCapture";
pub const PIN: &str = "pin";
pub const SAVE: &str = "save";
pub const COMPLETE: &str = "complete";
pub const CANCEL: &str = "cancel";
pub const COPY: &str = "copy";

// ---- second-row property buttons -------------------------------------------
pub const PROP_STROKE: &str = "propStroke";
pub const PROP_FILL: &str = "propFill";
pub const PROP_ARROW_START: &str = "propArrowStart";
pub const PROP_ARROW_END: &str = "propArrowEnd";
pub const PROP_ARROW_BOTH: &str = "propArrowBoth";
pub const PROP_ARROW_NONE: &str = "propArrowNone";
pub const PROP_HEAD_FILLED: &str = "propHeadFilled";
pub const PROP_HEAD_OPEN: &str = "propHeadOpen";
pub const PROP_HEAD_BAR: &str = "propHeadBar";
/// Bubble styles offered inline on the number row.
pub const PROP_BADGE_FILLED: &str = "propBadgeFilled";
pub const PROP_BADGE_HOLLOW: &str = "propBadgeHollow";
pub const PROP_BADGE_SQUARE: &str = "propBadgeSquare";

/// Stroke width range of the draggable slider, in image px.
pub const STROKE_MIN: f64 = 1.0;
pub const STROKE_MAX: f64 = 20.0;

/// Number bubble radius range, in image px (`BADGE_RADIUS` sits inside it).
pub const BADGE_MIN_RADIUS: f64 = 8.0;
pub const BADGE_MAX_RADIUS: f64 = 40.0;

pub const CUSTOMIZABLE: &[&str] = &[
    COLOR,
    SELECT,
    SHAPE,
    RECT,
    ELLIPSE,
    LINE,
    ARROW_GROUP,
    ARROW,
    POLYLINE_ARROW,
    CURVE_ARROW,
    PEN,
    TEXT,
    MOSAIC,
    NUMBER,
    UNDO,
    REDO,
    CLEAR,
    OCR,
    TRANSLATE,
    LONG_CAPTURE,
    PIN,
    SAVE,
    LINE_SOLID,
    LINE_DASHED,
    LINE_CLOUD,
];

#[allow(dead_code)]
pub const DEFAULT_HIDDEN: &[&str] = &[ELLIPSE, TEXT, REDO];

pub const PHASE1_ITEMS: &[&str] = &[
    SELECT,
    SHAPE,
    ARROW_GROUP,
    PEN,
    MOSAIC,
    TEXT,
    NUMBER,
    COLOR,
    LINE_SOLID,
    LINE_DASHED,
    LINE_CLOUD,
    UNDO,
    REDO,
    CLEAR,
    OCR,
    TRANSLATE,
    LONG_CAPTURE,
    PIN,
    SAVE,
    COMPLETE,
    CANCEL,
];

pub const BTN: f64 = 36.0;
pub const ICON: f32 = 20.0;
/// The colour button's swatch is smaller than the svg icons so it doesn't
/// look oversized next to them.
pub const SWATCH_ICON: f32 = 16.0;
pub const PAD: f64 = 4.0;
pub const ITEM_GAP: f64 = 2.0;
pub const H: f64 = 44.0;
pub const ANCHOR_GAP: f64 = 8.0;
pub const SWATCH: f64 = 22.0;
pub const SWATCH_GAP: f64 = 6.0;

// ---- slider geometry --------------------------------------------------------
/// The properties popup has a 1 px border; children start inside it.
pub const BORDER: f64 = 1.0;
pub const ROW_GAP: f64 = 6.0;
/// Height of one label + slider + readout row. Matches the track's hit area so
/// the popup's reserved height never clips it.
pub const SLIDER_ROW_H: f64 = TRACK_HIT_H;

/// Width of the second-row stroke-width slider cell.
pub const SLIDER_W: f64 = 132.0;
/// Visible track length; the knob (KNOB px) travels inside it.
pub const SLIDER_TRACK_W: f64 = 84.0;
pub const SLIDER_READOUT_W: f64 = 40.0;
pub const TRACK_H: f64 = 6.0;
pub const TRACK_HIT_H: f64 = 28.0;
pub const KNOB: f64 = 12.0;

/// Alpha row of the colour popup: label, track, percentage readout.
pub const ALPHA_LABEL_W: f64 = 30.0;
pub const ALPHA_TRACK_W: f64 = 116.0;
pub const ALPHA_READOUT_W: f64 = 36.0;


pub fn alpha_row_width() -> f64 {
    ALPHA_LABEL_W + ROW_GAP + ALPHA_TRACK_W + ROW_GAP + ALPHA_READOUT_W
}

/// The colour popup grew a second row for the alpha slider; the swatch row
/// stays centred on the bar button.
pub fn swatches_size(n: usize) -> (f64, f64) {
    let n = n as f64;
    let swatches = PAD * 2.0 + n * SWATCH + (n - 1.0).max(0.0) * SWATCH_GAP;
    (
        swatches.max(PAD * 2.0 + alpha_row_width()),
        PAD * 2.0 + SWATCH + ROW_GAP + SLIDER_ROW_H,
    )
}

/// Window-local x of the alpha track's left edge inside its popup.
pub fn alpha_track_x(popup_x: f64) -> f64 {
    popup_x + BORDER + PAD + ALPHA_LABEL_W + ROW_GAP
}

/// Pointer x mapped onto a track. The knob's own width is excluded so the
/// centre of the knob reaches both ends exactly.
pub fn slider_fraction(x: f64, track_x: f64, track_w: f64) -> f64 {
    let usable = (track_w - KNOB).max(1.0);
    ((x - track_x - KNOB * 0.5) / usable).clamp(0.0, 1.0)
}

pub fn width_for_fraction(f: f64) -> f64 {
    (STROKE_MIN + (STROKE_MAX - STROKE_MIN) * f.clamp(0.0, 1.0))
        .round()
        .clamp(STROKE_MIN, STROKE_MAX)
}

pub fn fraction_for_width(width: f64) -> f64 {
    (width.clamp(STROKE_MIN, STROKE_MAX) - STROKE_MIN) / (STROKE_MAX - STROKE_MIN)
}

pub fn alpha_for_fraction(f: f64) -> u8 {
    (f.clamp(0.0, 1.0) * 100.0).round() as u8
}

pub fn badge_radius_for_fraction(f: f64) -> f64 {
    (BADGE_MIN_RADIUS + (BADGE_MAX_RADIUS - BADGE_MIN_RADIUS) * f.clamp(0.0, 1.0))
        .round()
        .clamp(BADGE_MIN_RADIUS, BADGE_MAX_RADIUS)
}

pub fn fraction_for_badge_radius(radius: f64) -> f64 {
    (radius.clamp(BADGE_MIN_RADIUS, BADGE_MAX_RADIUS) - BADGE_MIN_RADIUS)
        / (BADGE_MAX_RADIUS - BADGE_MIN_RADIUS)
}

/// Centre of button `index` on a bar drawn at `bar`.
pub fn item_center_x(bar: Rect, index: usize) -> f64 {
    bar.x + PAD + index as f64 * (BTN + ITEM_GAP) + BTN / 2.0
}

/// The colour popup hangs above the bar, centred on its button. It flips below
/// only when there is no room above -- which happens when the selection fills
/// the monitor and `place_toolbar` has already clamped the bar to the top edge.
pub fn place_swatches(
    button_center_x: f64,
    bar: Rect,
    popup_width: f64,
    popup_height: f64,
    monitor: Rect,
    gap: f64,
) -> Point {
    let max_x = (monitor.x + monitor.width - popup_width).max(monitor.x);
    let x = (button_center_x - popup_width / 2.0).clamp(monitor.x, max_x);
    let above = bar.y - gap - popup_height;
    let y = if above >= monitor.y {
        above
    } else {
        bar.y + bar.height + gap
    };
    Point { x, y }
}

pub fn bar_size(n: usize) -> (f64, f64) {
    let n = n as f64;
    let w = PAD * 2.0 + n * BTN + (n - 1.0).max(0.0) * ITEM_GAP;
    (w, H)
}

/// Every svg the toolbar can request, including sub-tool icons that are not
/// themselves entries in `PHASE1_ITEMS` (the two shape sub-options). Keeping
/// this explicit means `AssetSource::list` stays complete even when a button is
/// merged into a picker and disappears from the main bar.
pub const TOOLBAR_ICONS: &[&str] = &[
    "toolbar/select.svg",
    "toolbar/rect.svg",
    "toolbar/ellipse.svg",
    "toolbar/shape.svg",
    "toolbar/arrowGroup.svg",
    "toolbar/number.svg",
    "toolbar/lineSolid.svg",
    "toolbar/lineDashed.svg",
    "toolbar/lineCloud.svg",
    "toolbar/arrow.svg",
    "toolbar/polylineArrow.svg",
    "toolbar/curveArrow.svg",
    "toolbar/pen.svg",
    "toolbar/mosaic.svg",
    "toolbar/text.svg",
    "toolbar/undo.svg",
    "toolbar/redo.svg",
    "toolbar/clear.svg",
    "toolbar/ocr.svg",
    "toolbar/translate.svg",
    "toolbar/longCapture.svg",
    "toolbar/pin.svg",
    "toolbar/save.svg",
    "toolbar/complete.svg",
    "toolbar/cancel.svg",
    "toolbar/propStroke.svg",
    "toolbar/propFill.svg",
    "toolbar/propArrowStart.svg",
    "toolbar/propArrowEnd.svg",
    "toolbar/propArrowBoth.svg",
    "toolbar/propArrowNone.svg",
    "toolbar/propHeadFilled.svg",
    "toolbar/propHeadOpen.svg",
    "toolbar/propHeadBar.svg",
    "toolbar/propBadgeFilled.svg",
    "toolbar/propBadgeHollow.svg",
    "toolbar/propBadgeSquare.svg",
];

pub fn icon_path(id: &str) -> Option<&'static str> {
    Some(match id {
        SELECT => "toolbar/select.svg",
        RECT => "toolbar/rect.svg",
        ELLIPSE => "toolbar/ellipse.svg",
        SHAPE => "toolbar/shape.svg",
        ARROW_GROUP => "toolbar/arrowGroup.svg",
        SHAPE_RECT => "toolbar/rect.svg",
        SHAPE_ELLIPSE => "toolbar/ellipse.svg",
        ARROW => "toolbar/arrow.svg",
        POLYLINE_ARROW => "toolbar/polylineArrow.svg",
        CURVE_ARROW => "toolbar/curveArrow.svg",
        LINE_SOLID => "toolbar/lineSolid.svg",
        LINE_DASHED => "toolbar/lineDashed.svg",
        LINE_CLOUD => "toolbar/lineCloud.svg",
        PEN => "toolbar/pen.svg",
        MOSAIC => "toolbar/mosaic.svg",
        TEXT => "toolbar/text.svg",
        NUMBER => "toolbar/number.svg",
        UNDO => "toolbar/undo.svg",
        REDO => "toolbar/redo.svg",
        CLEAR => "toolbar/clear.svg",
        OCR => "toolbar/ocr.svg",
        TRANSLATE => "toolbar/translate.svg",
        LONG_CAPTURE => "toolbar/longCapture.svg",
        PIN => "toolbar/pin.svg",
        SAVE => "toolbar/save.svg",
        COMPLETE => "toolbar/complete.svg",
        CANCEL => "toolbar/cancel.svg",
        PROP_STROKE => "toolbar/propStroke.svg",
        PROP_FILL => "toolbar/propFill.svg",
        PROP_ARROW_START => "toolbar/propArrowStart.svg",
        PROP_ARROW_END => "toolbar/propArrowEnd.svg",
        PROP_ARROW_BOTH => "toolbar/propArrowBoth.svg",
        PROP_ARROW_NONE => "toolbar/propArrowNone.svg",
        PROP_HEAD_FILLED => "toolbar/propHeadFilled.svg",
        PROP_HEAD_OPEN => "toolbar/propHeadOpen.svg",
        PROP_HEAD_BAR => "toolbar/propHeadBar.svg",
        PROP_BADGE_FILLED => "toolbar/propBadgeFilled.svg",
        PROP_BADGE_HOLLOW => "toolbar/propBadgeHollow.svg",
        PROP_BADGE_SQUARE => "toolbar/propBadgeSquare.svg",
        _ => return None,
    })
}

/// The three line-style buttons only make sense for closed shapes (rect/ellipse).
pub fn is_line_style(id: &str) -> bool {
    matches!(id, LINE_SOLID | LINE_DASHED | LINE_CLOUD)
}

/// Hover label. Every `PHASE1_ITEMS` entry needs one; the rest are unreachable
/// until they ship, so they fall back to the id.
pub fn label(id: &str) -> &'static str {
    match id {
        SELECT => "选择",
        RECT => "矩形",
        ELLIPSE => "椭圆",
        SHAPE => "形状",
        ARROW_GROUP => "箭头",
        SHAPE_RECT => "矩形",
        SHAPE_ELLIPSE => "椭圆",
        ARROW => "箭头",
        POLYLINE_ARROW => "折线箭头",
        CURVE_ARROW => "曲线箭头",
        PEN => "画笔",
        MOSAIC => "马赛克",
        TEXT => "文字",
        NUMBER => "序号",
        COLOR => "颜色",
        LINE_SOLID => "实线",
        LINE_DASHED => "虚线",
        LINE_CLOUD => "云线",
        UNDO => "撤销",
        REDO => "重做",
        CLEAR => "清空",
        OCR => "取字",
        TRANSLATE => "翻译",
        LONG_CAPTURE => "长截图",
        PIN => "钉住",
        SAVE => "保存",
        COMPLETE => "完成",
        CANCEL => "取消",
        PROP_STROKE => "线框开关",
        PROP_FILL => "内部填充",
        PROP_ARROW_START => "起点箭头",
        PROP_ARROW_END => "终点箭头",
        PROP_ARROW_BOTH => "两端箭头",
        PROP_ARROW_NONE => "无箭头",
        PROP_HEAD_FILLED => "实心箭头",
        PROP_HEAD_OPEN => "开口箭头",
        PROP_HEAD_BAR => "线条箭头",
        PROP_BADGE_FILLED => "实心圆",
        PROP_BADGE_HOLLOW => "空心圆",
        PROP_BADGE_SQUARE => "方形",
        _ => "",
    }
}

pub fn is_visible(id: &str, hidden: &[String]) -> bool {
    if id == COPY {
        return false;
    }
    if id == COMPLETE || id == CANCEL {
        return true;
    }
    !CUSTOMIZABLE.contains(&id) || !hidden.iter().any(|h| h == id)
}

/// A hairline separates the drawing tools from history, history from export
/// actions, and those from 完成/取消.
pub fn starts_group(id: &str) -> bool {
    matches!(id, UNDO | OCR | COMPLETE)
}

/// Centre of the gap before slot `index`. The separator is painted there
/// instead of taking a slot of its own, so `bar_size` and the click mapping
/// stay exactly as they were.
pub fn separator_x(index: usize) -> f64 {
    PAD + index as f64 * (BTN + ITEM_GAP) - ITEM_GAP / 2.0
}

/// Prefer below the selection; flip above if it would leave `monitor`; clamp inside.
pub fn place_toolbar(
    selection: Rect,
    toolbar_width: f64,
    toolbar_height: f64,
    monitor: Rect,
    gap: f64,
) -> Point {
    let mut x = selection.x + selection.width / 2.0 - toolbar_width / 2.0;
    let below = selection.y + selection.height + gap;
    let above = selection.y - gap - toolbar_height;
    let mut y = if below + toolbar_height <= monitor.y + monitor.height {
        below
    } else {
        above
    };
    let max_x = (monitor.x + monitor.width - toolbar_width).max(monitor.x);
    let max_y = (monitor.y + monitor.height - toolbar_height).max(monitor.y);
    x = x.clamp(monitor.x, max_x);
    y = y.clamp(monitor.y, max_y);
    Point { x, y }
}

/// Where the panel's vertical edge anchors.
pub enum PanelY {
    /// Top edge at `bar.bottom + gap`.
    BelowBar,
    /// Bottom edge at `bar.y - gap`; the caller bottom-anchors because the
    /// content height is unknown before layout.
    AboveBar,
    /// Explicit top edge (beside the selection).
    Top(f64),
}

/// Where the OCR/translate panel goes relative to the toolbar.
pub struct PanelPlacement {
    pub x: f64,
    pub y: PanelY,
    pub body_max_h: f64,
}

/// Places the panel so it stays on the monitor and, whenever possible, off the
/// selection — the user compares the recognized text against it. It keeps to
/// the toolbar's side of the selection first; when that side is too short it
/// moves beside the selection; only when neither works (selection fills the
/// monitor) does it take the roomier side and overlap. The body scrolls, so
/// shrinking max_h to fit loses no text.
/// Eight loose parameters mirror the geometry this places against; bundling
/// them into a struct would only rename the same fields at every call site.
#[allow(clippy::too_many_arguments)]
pub fn place_panel(
    bar: Rect,
    selection: Rect,
    panel_w: f64,
    chrome: f64,
    body_limit: (f64, f64),
    monitor: Rect,
    gap: f64,
    margin: f64,
) -> PanelPlacement {
    let (body_min, body_max) = body_limit;
    let mon_l = monitor.x + margin;
    let mon_t = monitor.y + margin;
    let mon_r = monitor.x + monitor.width - margin;
    let mon_b = monitor.y + monitor.height - margin;
    let clamp_x = |x: f64| x.clamp(mon_l, (mon_r - panel_w).max(mon_l));
    let fit = |room: f64| (room - chrome).clamp(body_min, body_max);

    let room_below = mon_b - (bar.y + bar.height + gap);
    let room_above = bar.y - gap - mon_t;
    let bar_below_sel = bar.y + bar.height / 2.0 >= selection.y + selection.height / 2.0;
    let need = chrome + body_min;

    // The toolbar's side of the selection: the panel cannot cover it there.
    // The toolbar itself is already clamped to the monitor by its own
    // (narrower) width; the wider panel clamps x on its own.
    if bar_below_sel && room_below >= need {
        return PanelPlacement {
            x: clamp_x(bar.x),
            y: PanelY::BelowBar,
            body_max_h: fit(room_below),
        };
    }
    if !bar_below_sel && room_above >= need {
        return PanelPlacement {
            x: clamp_x(bar.x),
            y: PanelY::AboveBar,
            body_max_h: fit(room_above),
        };
    }

    // Beside the selection (right first), kept inside its vertical span so the
    // panel cannot hit the toolbar either, which sits outside that span.
    let right_x = selection.x + selection.width + gap;
    let left_x = selection.x - gap - panel_w;
    let top = selection.y.max(mon_t);
    let vroom = (selection.y + selection.height).min(mon_b) - top;
    let beside = if right_x + panel_w <= mon_r {
        Some(right_x)
    } else if left_x >= mon_l {
        Some(left_x)
    } else {
        None
    };
    if let (Some(x), true) = (beside, vroom >= need) {
        return PanelPlacement {
            x,
            y: PanelY::Top(top),
            body_max_h: fit(vroom),
        };
    }

    // The selection fills the monitor: overlap is unavoidable, take the
    // roomier side of the toolbar.
    let (y, room) = if room_below >= room_above {
        (PanelY::BelowBar, room_below)
    } else {
        (PanelY::AboveBar, room_above)
    };
    PanelPlacement {
        x: clamp_x(bar.x),
        y,
        body_max_h: fit(room),
    }
}

// ---- second row layout ------------------------------------------------------

/// One cell of the properties row: an icon button, the draggable width
/// slider, or the number bubble size slider (both track + px readout).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropCell {
    Btn(&'static str),
    WidthSlider,
    BadgeSizeSlider,
}

impl PropCell {
    pub fn width(self) -> f64 {
        match self {
            Self::Btn(_) => BTN,
            Self::WidthSlider | Self::BadgeSizeSlider => SLIDER_W,
        }
    }
}

/// Offset of cell `index` from the row's left border, mirroring the flex
/// layout (`px(PAD)`, `gap(ITEM_GAP)`). Callers add `BORDER` for the popup's
/// 1 px frame.
pub fn prop_cell_x(cells: &[PropCell], index: usize) -> f64 {
    PAD + cells
        .iter()
        .take(index)
        .map(|c| c.width() + ITEM_GAP)
        .sum::<f64>()
}

/// Same chrome as the main bar (padding, gap, height), but the cells differ.
pub fn props_size(cells: &[PropCell]) -> (f64, f64) {
    if cells.is_empty() {
        return (0.0, 0.0);
    }
    let inner: f64 =
        cells.iter().map(|c| c.width()).sum::<f64>() + (cells.len() - 1) as f64 * ITEM_GAP;
    (PAD * 2.0 + inner, H)
}

/// Places the second row centred under (or above) the first, clamped to the
/// monitor. `below` mirrors which side of the selection the first row took.
pub fn place_second_row(
    bar: Rect,
    row_width: f64,
    row_height: f64,
    monitor: Rect,
    below: bool,
    gap: f64,
) -> Point {
    let max_x = (monitor.x + monitor.width - row_width).max(monitor.x);
    let x = (bar.x + bar.width / 2.0 - row_width / 2.0).clamp(monitor.x, max_x);
    let y = if below {
        bar.y + bar.height + gap
    } else {
        bar.y - gap - row_height
    };
    let max_y = (monitor.y + monitor.height - row_height).max(monitor.y);
    Point {
        x,
        y: y.clamp(monitor.y, max_y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hidden(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn complete_and_cancel_are_always_visible() {
        let hidden = hidden(&[COMPLETE, CANCEL, SAVE]);
        assert!(is_visible(COMPLETE, &hidden));
        assert!(is_visible(CANCEL, &hidden));
        assert!(!is_visible(SAVE, &hidden));
    }

    #[test]
    fn copy_is_never_on_the_bar() {
        assert!(!is_visible(COPY, &hidden(DEFAULT_HIDDEN)));
        assert!(!is_visible(COPY, &[]));
    }

    /// An empty list is the shipped default: nothing hidden. `DEFAULT_HIDDEN`
    /// is the C# set, kept for when the settings page offers a reset.
    #[test]
    fn an_empty_list_hides_nothing() {
        for id in PHASE1_ITEMS {
            assert!(is_visible(id, &[]), "{id}");
        }
    }

    #[test]
    fn default_hidden_matches_csharp() {
        let hidden = hidden(DEFAULT_HIDDEN);
        assert!(!is_visible(ELLIPSE, &hidden));
        assert!(!is_visible(TEXT, &hidden));
        assert!(!is_visible(REDO, &hidden));
        assert!(is_visible(COLOR, &hidden));
        assert!(is_visible(SELECT, &hidden));
        assert!(is_visible(RECT, &hidden));
        assert!(is_visible(UNDO, &hidden));
        assert!(is_visible(SAVE, &hidden));
    }

    #[test]
    fn toolbar_prefers_below_selection() {
        let pos = place_toolbar(
            Rect::new(100.0, 80.0, 200.0, 120.0),
            200.0,
            40.0,
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            8.0,
        );
        assert_eq!(pos.x, 100.0);
        assert_eq!(pos.y, 208.0);
    }

    #[test]
    fn toolbar_flips_above_when_below_does_not_fit() {
        let pos = place_toolbar(
            Rect::new(100.0, 1000.0, 200.0, 60.0),
            400.0,
            40.0,
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            8.0,
        );
        assert_eq!(pos.y, 1000.0 - 8.0 - 40.0);
    }

    #[test]
    fn toolbar_clamps_into_monitor() {
        let pos = place_toolbar(
            Rect::new(10.0, 10.0, 40.0, 20.0),
            400.0,
            40.0,
            Rect::new(0.0, 0.0, 500.0, 80.0),
            8.0,
        );
        assert!(pos.x >= 0.0);
        assert!(pos.x + 400.0 <= 500.0);
        assert!(pos.y >= 0.0);
        assert!(pos.y + 40.0 <= 80.0);
    }

    #[test]
    fn panel_prefers_below_toolbar() {
        let p = place_panel(
            Rect::new(100.0, 208.0, 538.0, 44.0),
            Rect::new(100.0, 80.0, 200.0, 120.0),
            320.0,
            80.0,
            (48.0, 260.0),
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            8.0,
            4.0,
        );
        assert!(matches!(p.y, PanelY::BelowBar));
        assert_eq!(p.x, 100.0);
        assert_eq!(p.body_max_h, 260.0);
    }

    #[test]
    fn panel_keeps_toolbar_side_instead_of_roomier_side() {
        // The toolbar sits above a bottom-heavy selection; below the bar is
        // roomier but the panel would cover the selection from its top edge.
        let p = place_panel(
            Rect::new(100.0, 148.0, 538.0, 44.0),
            Rect::new(100.0, 200.0, 800.0, 800.0),
            320.0,
            80.0,
            (48.0, 260.0),
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            8.0,
            4.0,
        );
        assert!(matches!(p.y, PanelY::AboveBar));
        // room above = 148 - 8 - 4 = 136; body = 136 - 80 chrome = 56.
        assert_eq!(p.body_max_h, 56.0);
    }

    #[test]
    fn panel_moves_beside_selection_when_no_vertical_room() {
        // The selection nearly fills the monitor: the toolbar clamps above it
        // with no room on either side, but 1120 px remain on the right.
        let p = place_panel(
            Rect::new(100.0, 8.0, 538.0, 44.0),
            Rect::new(100.0, 60.0, 800.0, 980.0),
            320.0,
            80.0,
            (48.0, 260.0),
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            8.0,
            4.0,
        );
        assert!(matches!(p.y, PanelY::Top(60.0)));
        assert_eq!(p.x, 100.0 + 800.0 + 8.0);
        assert_eq!(p.body_max_h, 260.0);
    }

    #[test]
    fn panel_beside_falls_back_to_the_left() {
        // The selection hugs the right edge; beside must go left.
        let p = place_panel(
            Rect::new(100.0, 8.0, 538.0, 44.0),
            Rect::new(1500.0, 60.0, 380.0, 980.0),
            320.0,
            80.0,
            (48.0, 260.0),
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            8.0,
            4.0,
        );
        assert!(matches!(p.y, PanelY::Top(_)));
        assert_eq!(p.x, 1500.0 - 8.0 - 320.0);
    }

    #[test]
    fn panel_overlaps_only_when_selection_fills_the_monitor() {
        // Full-screen selection: no vertical room and no side room, so it
        // takes the roomier side of the toolbar (above) and overlaps.
        let p = place_panel(
            Rect::new(100.0, 1032.0, 538.0, 44.0),
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            320.0,
            80.0,
            (48.0, 260.0),
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            8.0,
            4.0,
        );
        assert!(matches!(p.y, PanelY::AboveBar));
        assert_eq!(p.body_max_h, 260.0);
    }

    #[test]
    fn panel_shrinks_body_to_fit() {
        // Short monitor: below the bar is 396 - 234 = 162 px, which fits the
        // minimum but not the full body: 162 - 80 chrome = 82 px.
        let p = place_panel(
            Rect::new(100.0, 182.0, 462.0, 44.0),
            Rect::new(100.0, 20.0, 800.0, 150.0),
            320.0,
            80.0,
            (48.0, 260.0),
            Rect::new(0.0, 0.0, 1920.0, 400.0),
            8.0,
            4.0,
        );
        assert!(matches!(p.y, PanelY::BelowBar));
        assert_eq!(p.body_max_h, 82.0);
    }

    #[test]
    fn panel_clamps_into_monitor() {
        // A heavily hidden toolbar can be far narrower than the 320 px panel
        // (2 visible buttons = 82 px). Clamped to the monitor's right edge, its
        // x would push the panel off-screen; the panel slides left on its own.
        let p = place_panel(
            Rect::new(1218.0, 208.0, 82.0, 44.0),
            Rect::new(900.0, 80.0, 200.0, 120.0),
            320.0,
            80.0,
            (48.0, 260.0),
            Rect::new(800.0, 0.0, 500.0, 1080.0),
            8.0,
            4.0,
        );
        assert!(matches!(p.y, PanelY::BelowBar));
        assert_eq!(p.x, 976.0);
        assert!(p.x >= 804.0);
        assert!(p.x + 320.0 <= 1296.0);
    }

    #[test]
    fn bar_size_matches_phase1_layout() {
        assert_eq!(bar_size(12), (462.0, 44.0));
        assert_eq!(bar_size(1), (44.0, 44.0));
        assert_eq!(bar_size(0), (8.0, 44.0));
    }

    /// COLOR is painted from state; every other item has an svg asset. No
    /// button uses a text glyph any more.
    #[test]
    fn every_phase1_item_has_a_svg_icon_except_the_colour_swatch() {
        for id in PHASE1_ITEMS {
            assert_eq!(icon_path(id).is_some(), *id != COLOR, "{id}");
        }
    }

    #[test]
    fn swatches_hang_above_the_bar_centred_on_their_button() {
        let mon = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        let bar = Rect::new(500.0, 600.0, bar_size(15).0, H);
        let (w, h) = swatches_size(8);
        let cx = item_center_x(bar, 6);
        let p = place_swatches(cx, bar, w, h, mon, ANCHOR_GAP);
        assert_eq!(p.x, cx - w / 2.0);
        assert_eq!(p.y, bar.y - ANCHOR_GAP - h);
    }

    /// Selection filling the monitor clamps the bar to the top edge, and a popup
    /// above it would be off screen.
    #[test]
    fn swatches_flip_below_when_there_is_no_room_above() {
        let mon = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        let bar = Rect::new(500.0, 0.0, bar_size(15).0, H);
        let (w, h) = swatches_size(8);
        let p = place_swatches(item_center_x(bar, 6), bar, w, h, mon, ANCHOR_GAP);
        assert_eq!(p.y, bar.y + bar.height + ANCHOR_GAP);
    }

    #[test]
    fn swatches_stay_inside_the_monitor_horizontally() {
        let mon = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        let bar = Rect::new(1800.0, 600.0, bar_size(15).0, H);
        let (w, h) = swatches_size(8);
        let right = place_swatches(1915.0, bar, w, h, mon, ANCHOR_GAP);
        assert_eq!(right.x, mon.width - w);
        let left = place_swatches(5.0, bar, w, h, mon, ANCHOR_GAP);
        assert_eq!(left.x, mon.x);
    }

    /// Every second-row button must have a served svg and a Chinese tooltip.
    #[test]
    fn every_prop_button_has_a_served_icon_and_label() {
        use crate::overlay::ToolbarAssets;
        use gpui::AssetSource;
        let ids = [
            PROP_STROKE,
            PROP_FILL,
            PROP_ARROW_START,
            PROP_ARROW_END,
            PROP_ARROW_BOTH,
            PROP_ARROW_NONE,
            PROP_HEAD_FILLED,
            PROP_HEAD_OPEN,
            PROP_HEAD_BAR,
            PROP_BADGE_FILLED,
            PROP_BADGE_HOLLOW,
            PROP_BADGE_SQUARE,
        ];
        for id in ids {
            let path = icon_path(id).unwrap_or_else(|| panic!("{id} has no icon"));
            assert!(!label(id).is_empty(), "{id} has no label");
            assert!(
                ToolbarAssets.load(path).ok().flatten().is_some(),
                "{path} is not served"
            );
        }
    }

    #[test]
    fn second_row_sits_under_the_bar_and_stays_on_the_monitor() {
        let mon = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        let bar = Rect::new(500.0, 600.0, bar_size(20).0, H);
        let cells = [PropCell::Btn(PROP_FILL), PropCell::WidthSlider];
        let (w, h) = props_size(&cells);
        assert_eq!(w, PAD * 2.0 + BTN + SLIDER_W + ITEM_GAP);
        let p = place_second_row(bar, w, h, mon, true, ANCHOR_GAP);
        assert_eq!(p.y, bar.y + bar.height + ANCHOR_GAP);
        assert_eq!(p.x, bar.x + bar.width / 2.0 - w / 2.0);
        // Toolbar clamped to the bottom edge: the row flips/clamps inside.
        let bar = Rect::new(1800.0, 1036.0, H, H);
        let p = place_second_row(bar, w, h, mon, true, ANCHOR_GAP);
        assert!(p.x >= mon.x && p.x + w <= mon.x + mon.width);
        assert!(p.y >= mon.y && p.y + h <= mon.y + mon.height);
    }

    #[test]
    fn width_slider_maps_the_track_ends_and_clamps() {
        let (tx, tw) = (100.0, SLIDER_TRACK_W);
        // Left end of the usable track is the minimum, right end the maximum.
        assert_eq!(width_for_fraction(slider_fraction(tx, tx, tw)), STROKE_MIN);
        assert_eq!(
            width_for_fraction(slider_fraction(tx + tw, tx, tw)),
            STROKE_MAX
        );
        // Beyond either end still clamps.
        assert_eq!(
            width_for_fraction(slider_fraction(tx - 80.0, tx, tw)),
            STROKE_MIN
        );
        assert_eq!(
            width_for_fraction(slider_fraction(tx + tw + 80.0, tx, tw)),
            STROKE_MAX
        );
        // The midpoint rounds to a whole pixel inside the range.
        let mid = width_for_fraction(slider_fraction(tx + tw / 2.0, tx, tw));
        assert!(
            (STROKE_MIN..=STROKE_MAX).contains(&mid) && mid.fract() == 0.0,
            "{mid}"
        );
    }

    #[test]
    fn stroke_width_and_badge_radius_roundtrip_through_fractions() {
        for w in [1.0, 2.0, 7.0, 20.0] {
            assert_eq!(width_for_fraction(fraction_for_width(w)), w);
        }
        for r in [8.0, 12.0, 27.0, 40.0] {
            assert_eq!(badge_radius_for_fraction(fraction_for_badge_radius(r)), r);
        }
        assert_eq!(alpha_for_fraction(-1.0), 0);
        assert_eq!(alpha_for_fraction(0.55), 55);
        assert_eq!(alpha_for_fraction(9.0), 100);
    }

    #[test]
    fn the_colour_popup_grew_a_row_for_the_alpha_slider() {
        let (w, h) = swatches_size(8);
        assert!(h > SWATCH + PAD * 2.0, "popup must be taller than one row");
        assert!(w >= PAD * 2.0 + alpha_row_width());
        assert!(alpha_track_x(10.0) > 10.0);
        // Every slider row fits inside the popup height.
        assert!(h >= PAD * 2.0 + SWATCH + ROW_GAP + SLIDER_ROW_H);
    }

    /// Three style buttons plus the inline size slider fit one row without
    /// clipping: 3 buttons, 3 gaps and the slider cell.
    #[test]
    fn the_number_row_fits_three_styles_and_a_size_slider() {
        let cells = [
            PropCell::Btn(PROP_BADGE_FILLED),
            PropCell::Btn(PROP_BADGE_HOLLOW),
            PropCell::Btn(PROP_BADGE_SQUARE),
            PropCell::BadgeSizeSlider,
        ];
        let (w, h) = props_size(&cells);
        assert_eq!(w, PAD * 2.0 + 3.0 * BTN + SLIDER_W + 3.0 * ITEM_GAP);
        assert_eq!(h, H);
        // The slider cell ends exactly on the row's right padding edge.
        let cell_x = prop_cell_x(&cells, 3);
        assert!(cell_x > 3.0 * (BTN + ITEM_GAP));
        assert_eq!(cell_x + SLIDER_W + PAD, w);
    }

    #[test]
    fn every_phase1_item_has_a_label() {
        for id in PHASE1_ITEMS {
            assert!(!label(id).is_empty(), "{id}");
        }
    }

    #[test]
    fn separators_land_in_the_gap_without_moving_any_slot() {
        for i in 1..PHASE1_ITEMS.len() {
            let prev_right = PAD + (i - 1) as f64 * (BTN + ITEM_GAP) + BTN;
            let next_left = PAD + i as f64 * (BTN + ITEM_GAP);
            let x = separator_x(i);
            assert!(x > prev_right && x < next_left, "slot {i}: {x}");
        }
        // The groups the separators separate, left to right.
        assert!(starts_group(UNDO));
        assert!(starts_group(OCR));
        assert!(starts_group(COMPLETE));
        assert!(!starts_group(SELECT));
        assert!(!starts_group(SAVE));
        // Purely painted: the bar width is still the slot formula.
        assert_eq!(bar_size(12), (462.0, 44.0));
    }
}
