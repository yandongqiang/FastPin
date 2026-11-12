//! Settings-window widgets. The metrics follow the Citepix settings window:
//! a 200px pane with filled pills, a search bar across the content area, a
//! page heading with a per-page reset button, and one rounded surface holding
//! rows separated by hairlines.
//!
//! Every widget returns a plain element and leaves the click handler to the
//! caller, so the view keeps `cx.listener` and these stay view-agnostic.

use gpui::{
    div, prelude::*, px, svg, CursorStyle, Div, ElementId, FontWeight, Hsla, SharedString,
    Stateful,
};

/// Height of a one-line row. Two-line rows grow with their description.
pub const ROW_MIN_H: f32 = 60.;
/// Rows stop widening here; across a 1000px window a label and its control
/// would otherwise sit at opposite edges and stop reading as one row.
pub const CONTENT_MAX_W: f32 = 760.;
/// The toolbar grid stays four to a row, like Citepix's wrap grid.
pub const TOOLBAR_COLUMNS: usize = 4;

// ---- palette ---------------------------------------------------------------
//
// Everything here goes through `overlay::theme`, whose snapshot follows the
// gpui-kit appearance, so the settings window cannot stay light while the
// toolbar turns dark (or the other way round).

pub fn page_bg() -> Hsla {
    crate::overlay::theme::muted()
}

/// The pane sits a step darker than the page; averaging the two neutral tokens
/// keeps that relationship in both appearances.
pub fn nav_bg() -> Hsla {
    let page = crate::overlay::theme::muted();
    let secondary = crate::overlay::theme::elevated_surface();
    Hsla {
        l: (page.l + secondary.l) / 2.0,
        ..page
    }
}

pub fn card_bg() -> Hsla {
    crate::overlay::theme::surface_solid()
}

pub fn input_bg() -> Hsla {
    crate::overlay::theme::input()
}

fn hover_bg() -> Hsla {
    crate::overlay::theme::elevated_surface()
}

/// Nav rows sit on `nav_bg`, which in dark mode equals the secondary token —
/// a translucent ink overlay reads as hover in both appearances.
fn nav_hover() -> Hsla {
    crate::overlay::theme::primary_fg().opacity(0.08)
}

/// The primary/selected fill; shared with the annotation toolbar.
pub fn accent() -> Hsla {
    crate::overlay::theme::accent()
}

pub fn accent_soft() -> Hsla {
    crate::overlay::theme::active_background()
}

pub fn accent_wash() -> Hsla {
    crate::overlay::theme::selection_wash()
}

/// The "重置所有设置" button.
pub fn danger_fill() -> Hsla {
    crate::overlay::theme::danger()
}

fn danger_hover() -> Hsla {
    crate::overlay::theme::danger_hover()
}

fn muted_bg() -> Hsla {
    crate::overlay::theme::muted()
}

fn icon(path: &'static str, size: f32, color: Hsla) -> Div {
    div()
        .flex_none()
        .child(svg().path(path).w(px(size)).h(px(size)).text_color(color))
}

// ---- navigation ------------------------------------------------------------

/// One entry of the left pane: 36 tall, filled light purple when selected.
pub fn nav_item(
    id: impl Into<ElementId>,
    icon_path: &'static str,
    text: impl Into<SharedString>,
    selected: bool,
) -> Stateful<Div> {
    let fg = if selected {
        accent()
    } else {
        crate::overlay::theme::secondary_fg()
    };
    div()
        .id(id)
        .h(px(36.))
        .mx(px(10.))
        .my(px(1.))
        .px(px(10.))
        .rounded(px(8.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .cursor(CursorStyle::PointingHand)
        .when(selected, |el| el.bg(accent_wash()))
        .when(!selected, |el| el.hover(|s| s.bg(nav_hover())))
        .child(icon(icon_path, 16., fg))
        .child(
            div()
                .text_size(px(13.))
                .when(selected, |el| el.font_weight(FontWeight::MEDIUM))
                .text_color(crate::overlay::theme::primary_fg())
                .child(text.into()),
        )
}

// ---- page chrome -----------------------------------------------------------

/// Title (bold) plus the grey page description.
pub fn page_heading(title: impl Into<SharedString>, desc: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(19.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(crate::overlay::theme::primary_fg())
                .child(title.into()),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(crate::overlay::theme::secondary_fg())
                .child(desc.into()),
        )
}

/// The rounded surface a group of rows sits on.
pub fn section() -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .rounded(px(8.))
        .bg(card_bg())
        .border_1()
        .border_color(crate::overlay::theme::border())
}

pub fn divider() -> Div {
    div()
        .w_full()
        .h(px(1.))
        .flex_none()
        .bg(crate::overlay::theme::border())
}

/// A section with hairline dividers between its rows.
pub fn rows<E, I>(items: I) -> Div
where
    E: IntoElement,
    I: IntoIterator<Item = E>,
{
    let mut el = section();
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            el = el.child(divider());
        }
        el = el.child(item);
    }
    el
}

/// Title and description on the left; the caller appends the control, which
/// should be wrapped in `control()` so it keeps its width.
pub fn row(title: impl Into<SharedString>, desc: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .min_h(px(ROW_MIN_H))
        .px(px(16.))
        .py(px(12.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(24.))
        .child(label(title, Some(desc.into())))
}

/// Label on top, content stretched below — for inputs, paths and grids.
pub fn stack(title: impl Into<SharedString>, desc: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .px(px(16.))
        .py(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(label(title, Some(desc.into())))
}

/// Keeps a control at its natural width inside a row.
pub fn control(child: impl IntoElement) -> Div {
    div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .child(child)
}

fn label(header: impl Into<SharedString>, desc: Option<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .min_w(px(0.))
        .child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(crate::overlay::theme::primary_fg())
                .child(header.into()),
        )
        .children(desc.map(|d| {
            div()
                .text_size(px(11.))
                .text_color(crate::overlay::theme::secondary_fg())
                .child(d)
        }))
}

/// A row that exists to say the feature is absent — never a dead control.
pub fn unsupported_row(title: impl Into<SharedString>, desc: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .min_h(px(ROW_MIN_H))
        .px(px(16.))
        .py(px(12.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(24.))
        .bg(muted_bg())
        .child(label(title, Some(desc.into())))
        .child(
            div()
                .flex_none()
                .h(px(22.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(px(11.))
                .bg(muted_bg())
                .border_1()
                .border_color(crate::overlay::theme::border())
                .text_size(px(11.))
                .text_color(crate::overlay::theme::disabled_fg())
                .child("暂不支持"),
        )
}

/// Static value shown where a disabled page has no control to offer.
pub fn muted_text(text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .text_size(px(12.))
        .text_color(crate::overlay::theme::disabled_fg())
        .child(text.into())
}

/// A small grey tag, used to name the source page in search results.
pub fn page_tag(text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .h(px(20.))
        .px(px(8.))
        .flex()
        .items_center()
        .rounded(px(10.))
        .bg(accent_wash())
        .text_size(px(11.))
        .text_color(accent())
        .child(text.into())
}

/// Transient save/error feedback, beside the page heading.
pub fn status_pill(text: impl Into<SharedString>, error: bool) -> Div {
    let (fg, glyph) = if error {
        (crate::overlay::theme::danger(), "settings/close.svg")
    } else {
        (accent(), "settings/check.svg")
    };
    div()
        .flex_none()
        .h(px(24.))
        .px(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .rounded(px(12.))
        .bg(fg.opacity(0.12))
        .child(icon(glyph, 13., fg))
        .child(div().text_size(px(12.)).text_color(fg).child(text.into()))
}

/// Secondary note under a group, with the info glyph the pages use.
pub fn hint(text: impl Into<SharedString>) -> Div {
    div()
        .mt(px(8.))
        .flex()
        .flex_row()
        .items_start()
        .gap(px(6.))
        .child(icon("settings/info.svg", 14., crate::overlay::theme::secondary_fg()).mt(px(1.)))
        .child(
            div()
                .text_size(px(11.))
                .text_color(crate::overlay::theme::secondary_fg())
                .child(text.into()),
        )
}

/// Group caption above a section.
pub fn group_header(text: impl Into<SharedString>) -> Div {
    div()
        .mt(px(16.))
        .mb(px(8.))
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(crate::overlay::theme::secondary_fg())
        .child(text.into())
}

// ---- controls --------------------------------------------------------------

/// A path shown back to the user: monospace, single line, clipped rather than
/// wrapped so a deep path cannot stretch the row.
pub fn path_line(text: impl Into<SharedString>) -> Div {
    div()
        .font_family("Consolas")
        .text_size(px(12.))
        .text_color(crate::overlay::theme::secondary_fg())
        .overflow_hidden()
        .text_ellipsis()
        .child(text.into())
}

/// Capsule switch, 38x22. Track = accent when on, secondary when off; the
/// knob takes the fill's opposite so it stays visible in both appearances.
pub fn toggle(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(38.))
        .h(px(22.))
        .rounded(px(11.))
        .bg(if on {
            accent()
        } else {
            crate::overlay::theme::elevated_surface()
        })
        .flex()
        .items_center()
        .cursor(CursorStyle::PointingHand)
        .child(
            div()
                .w(px(16.))
                .h(px(16.))
                .rounded(px(8.))
                .bg(if on {
                    crate::overlay::theme::on_accent()
                } else {
                    crate::overlay::theme::primary_fg()
                })
                .ml(if on { px(19.) } else { px(3.) }),
        )
}

pub fn checkbox(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    checked: bool,
) -> Stateful<Div> {
    let (box_bg, box_edge) = if checked {
        (accent(), accent())
    } else {
        (card_bg(), crate::overlay::theme::border())
    };
    div()
        .id(id)
        .w(px(132.))
        .h(px(34.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .cursor(CursorStyle::PointingHand)
        .child(
            div()
                .flex_none()
                .w(px(16.))
                .h(px(16.))
                .rounded(px(4.))
                .bg(box_bg)
                .border_1()
                .border_color(box_edge)
                .flex()
                .items_center()
                .justify_center()
                .when(checked, |el| {
                    el.child(icon("settings/check.svg", 11., crate::overlay::theme::on_accent()))
                }),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(crate::overlay::theme::primary_fg())
                .child(text.into()),
        )
}

pub fn button(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    button_shell(id).px(px(14.)).child(text.into())
}

/// Filled purple button for the per-page reset.
pub fn primary_button(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(30.))
        .px(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(accent())
        .text_size(px(12.))
        .text_color(crate::overlay::theme::on_accent())
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(crate::overlay::theme::accent_hover()))
        .child(text.into())
}

/// The red "重置所有设置" button at the bottom of the pane.
pub fn danger_button(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(32.))
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(danger_fill())
        .text_size(px(12.))
        .text_color(crate::overlay::theme::on_danger())
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(danger_hover()))
        .child(text.into())
}

/// Icon-only button, as the "open folder" rows use.
pub fn icon_button(id: impl Into<ElementId>, icon_path: &'static str) -> Stateful<Div> {
    button_shell(id)
        .w(px(30.))
        .child(icon(icon_path, 15., crate::overlay::theme::primary_fg()))
}

fn button_shell(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(30.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(card_bg())
        .border_1()
        .border_color(crate::overlay::theme::border())
        .text_size(px(12.))
        .text_color(crate::overlay::theme::primary_fg())
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(hover_bg()))
}

// ---- dropdowns -------------------------------------------------------------

/// The closed state of a dropdown: current value plus a chevron. `width` lets a
/// short value sit beside its label instead of stretching the card.
pub fn select(
    id: impl Into<ElementId>,
    value: impl Into<SharedString>,
    open: bool,
    width: Option<f32>,
) -> Stateful<Div> {
    let base = div().id(id);
    let base = match width {
        Some(w) => base.flex_none().w(px(w)),
        None => base.w_full(),
    };
    base.h(px(32.))
        .px(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .rounded(px(6.))
        .bg(input_bg())
        .border_1()
        .border_color(if open {
            accent()
        } else {
            crate::overlay::theme::border()
        })
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(hover_bg()))
        .child(
            div()
                .text_size(px(12.))
                .text_color(crate::overlay::theme::primary_fg())
                .child(value.into()),
        )
        .child(icon(
            "settings/chevron.svg",
            12.,
            crate::overlay::theme::secondary_fg(),
        ))
}

/// One row of an open dropdown.
pub fn select_option(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .h(px(30.))
        .px(px(8.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .rounded(px(6.))
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(hover_bg()))
        .child(
            div()
                .text_size(px(12.))
                .text_color(crate::overlay::theme::primary_fg())
                .child(text.into()),
        )
        .when(selected, |el| el.child(icon("settings/check.svg", 11., accent())))
}

/// Wraps the open option rows so they read as one surface under the field.
pub fn select_list() -> Div {
    div()
        .w_full()
        .mt(px(4.))
        .p(px(4.))
        .flex()
        .flex_col()
        .rounded(px(8.))
        .bg(input_bg())
        .border_1()
        .border_color(crate::overlay::theme::border())
}

/// The collapsed right-hand side of an expander row: a summary plus a chevron.
pub fn expander_summary(summary: impl Into<SharedString>, open: bool) -> Div {
    div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(crate::overlay::theme::secondary_fg())
                .child(summary.into()),
        )
        .child(icon(
            "settings/chevron.svg",
            14.,
            if open {
                accent()
            } else {
                crate::overlay::theme::secondary_fg()
            },
        ))
}

/// The hotkey input box: monospace combo, purple while recording.
pub fn hotkey_field(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    recording: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(190.))
        .h(px(32.))
        .px(px(12.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(if recording {
            accent_soft()
        } else {
            input_bg()
        })
        .border_1()
        .border_color(if recording {
            accent()
        } else {
            crate::overlay::theme::border()
        })
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(hover_bg()))
        .child(
            div()
                .font_family("Consolas")
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if recording {
                    accent()
                } else {
                    crate::overlay::theme::primary_fg()
                })
                .child(text.into()),
        )
}

/// Cancels an in-progress recording. Without it the only ways out are clicking
/// the field again or waiting for the timeout, neither of which is visible.
pub fn cancel_button(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(28.))
        .h(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(hover_bg()))
        .child(icon(
            "settings/close.svg",
            14.,
            crate::overlay::theme::secondary_fg(),
        ))
}
