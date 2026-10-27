//! Small controls the boards share: ghost actions whose danger colour only
//! shows on hover, status badges, collapsible section heads, and empty states.

use ely_gpui_component::data_display::{Badge, Tone};
use ely_gpui_component::primitives::{Icon, IconName, Tooltip};
use ely_gpui_component::theme::IconSize;
use gpui::{
    AnyElement, App, ClickEvent, FontWeight, IntoElement, MouseButton, Rgba, Role, SharedString,
    Window, div, prelude::*, px, transparent_black,
};
use ns_ui::{icon_name, icon_tinted, theme};

/// The glyph for an action name. A plain square reads as an unchecked
/// checkbox, so Stop is a square inside a circle.
fn glyph_icon(name: &str) -> IconName {
    match name {
        "stop" => IconName::CircleStop,
        other => icon_name(other),
    }
}

/// Width of the status column, so every badge in a board lines up.
pub const STATE_COL: f32 = 104.;
/// Width of the action column: a start/stop slot and a restart slot.
pub const ACTIONS_COL: f32 = 56.;
/// Height of a labelled outline button. A mark in the same row uses this as
/// its diameter, so the two stay aligned. The outline itself is `border_1`
/// (1px) in `theme.border`; buttons keep `rounded_md`.
pub const TEXT_ACTION_HEIGHT: f32 = 24.;

/// How loud an action is at rest. Nothing is filled: a destructive action
/// stays grey until the pointer is on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Weight {
    Quiet,
    Accent,
    Danger,
}

/// Resting icon colour, colour under the pointer, and the wash behind it.
fn palette(weight: Weight) -> (Rgba, Rgba, Rgba) {
    let theme = theme();
    match weight {
        Weight::Quiet => (theme.muted, theme.text, theme.hover),
        Weight::Accent => (theme.accent, theme.accent_hover, theme.selected),
        Weight::Danger => (theme.muted, theme.err, theme.err_bg),
    }
}

/// A 24px icon control with a tooltip, a focus ring, and no fill at rest.
pub fn row_action(
    id: impl Into<SharedString>,
    glyph: &'static str,
    tip: impl Into<SharedString>,
    weight: Weight,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme();
    let (rest, hot, wash) = palette(weight);
    let id = id.into();
    let tip = tip.into();
    let group = SharedString::from(format!("{id}-group"));
    div()
        .id(id)
        .group(group.clone())
        .role(Role::Button)
        .aria_label(tip.clone())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(24.))
        .h(px(24.))
        .rounded_md()
        .border_1()
        .border_color(transparent_black())
        .cursor_pointer()
        .tab_index(0)
        .hover(move |style| style.bg(wash))
        .focus(move |style| style.border_color(theme.accent))
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .tooltip(Tooltip::text(tip))
        .on_click(on_click)
        .child(
            Icon::new(glyph_icon(glyph))
                .size(IconSize::Sm)
                .color(rest)
                .group_hover_color(group, hot),
        )
}

/// The actions of a whole group: row-sized icons inside one bordered capsule,
/// so they read as acting on every service below, not on one row.
pub fn group_cluster(buttons: Vec<AnyElement>) -> gpui::Div {
    let theme = theme();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_0p5()
        .px_0p5()
        .h(px(26.))
        .rounded_md()
        .border_1()
        .border_color(theme.raised)
        .children(buttons)
}

/// A 24px outlined button with a label. The same weight at rest for every
/// kind; only the hover colour tells a destructive one apart.
pub fn text_action(
    id: impl Into<SharedString>,
    glyph: &'static str,
    label: &'static str,
    weight: Weight,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme();
    let (rest, hot, wash) = palette(weight);
    let id = id.into();
    let group = SharedString::from(format!("{id}-group"));
    div()
        .id(id)
        .group(group.clone())
        .role(Role::Button)
        .aria_label(label)
        .flex()
        .flex_none()
        .items_center()
        .gap_1p5()
        .h(px(TEXT_ACTION_HEIGHT))
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .tab_index(0)
        .hover(move |style| {
            let style = style.bg(wash);
            if weight == Weight::Danger {
                style.border_color(hot)
            } else {
                style
            }
        })
        .focus(move |style| style.border_color(theme.accent))
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(on_click)
        .child(
            Icon::new(glyph_icon(glyph))
                .size(IconSize::Xs)
                .color(rest)
                .group_hover_color(group.clone(), hot),
        )
        .child(
            div()
                .text_color(theme.text)
                .group_hover(group, move |style| {
                    if weight == Weight::Danger {
                        style.text_color(hot)
                    } else {
                        style
                    }
                })
                .child(label),
        )
}

pub fn state_tone(state: &str) -> Tone {
    match state {
        "ready" | "running" => Tone::Success,
        "failed" | "crashed" => Tone::Danger,
        "starting" | "stopping" | "restarting" | "preparing" | "queued-start"
        | "running-unready" => Tone::Warning,
        "succeeded" => Tone::Info,
        _ => Tone::Neutral,
    }
}

/// The state word as the badge shows it. The long ones are shortened so the
/// column keeps one width.
pub fn state_label(state: &str) -> &str {
    match state {
        "running-unready" => "unready",
        "externally-owned" => "external",
        "queued-start" => "queued",
        "" => "unknown",
        other => other,
    }
}

/// A status badge with a dot, in a fixed-width column.
pub fn state_badge(state: &str) -> gpui::Div {
    div()
        .w(px(STATE_COL))
        .flex_none()
        .flex()
        .items_center()
        .child(
            Badge::new(SharedString::from(state_label(state).to_string()))
                .tone(state_tone(state))
                .dot(),
        )
}

/// The same column for a plain tag with no state, such as `disabled`.
pub fn tag_badge(text: &'static str) -> gpui::Div {
    div()
        .w(px(STATE_COL))
        .flex_none()
        .flex()
        .items_center()
        .child(Badge::new(text).tone(Tone::Neutral))
}

/// A collapsible section head: chevron, name, count badge, then the group's
/// actions pushed to the right edge.
pub fn group_header(
    id: SharedString,
    title: &str,
    count: usize,
    collapsed: bool,
    pending: Option<&'static str>,
    actions: Option<AnyElement>,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let theme = theme();
    let chevron = if collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    };
    let toggle = div()
        .id(id)
        .role(Role::Button)
        .aria_label(SharedString::from(format!("{title}, {count} services")))
        .aria_expanded(!collapsed)
        .flex_1()
        .min_w_0()
        .h(px(28.))
        .px_1()
        .flex()
        .items_center()
        .gap_1p5()
        .rounded_md()
        .border_1()
        .border_color(transparent_black())
        .cursor_pointer()
        .tab_index(0)
        .hover(move |style| style.bg(theme.hover))
        .focus(move |style| style.border_color(theme.accent))
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(on_toggle)
        .child(Icon::new(chevron).size(IconSize::Xs).color(theme.muted))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_string()),
        )
        .child(Badge::new(SharedString::from(count.to_string())))
        .children(pending.map(|state| Badge::new(state).tone(Tone::Warning)));
    div()
        .mt_2()
        .flex()
        .items_center()
        .gap_2()
        .child(toggle)
        .children(actions)
}

/// A pane with nothing to list: a glyph, what is missing, what to do, and an
/// optional action.
pub fn empty_state(
    glyph: &'static str,
    title: &'static str,
    detail: impl Into<SharedString>,
    action: Option<AnyElement>,
) -> gpui::Div {
    let theme = theme();
    div()
        .w_full()
        .py_6()
        .px_4()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .text_center()
        .child(icon_tinted(glyph, theme.muted))
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(div().text_xs().text_color(theme.muted).child(detail.into()))
        .children(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_has_one_tone_and_a_short_label() {
        assert_eq!(state_tone("ready"), Tone::Success);
        assert_eq!(state_tone("running"), Tone::Success);
        assert_eq!(state_tone("failed"), Tone::Danger);
        assert_eq!(state_tone("crashed"), Tone::Danger);
        assert_eq!(state_tone("starting"), Tone::Warning);
        assert_eq!(state_tone("running-unready"), Tone::Warning);
        assert_eq!(state_tone("succeeded"), Tone::Info);
        assert_eq!(state_tone("stopped"), Tone::Neutral);
        assert_eq!(state_label("externally-owned"), "external");
        assert_eq!(state_label("running-unready"), "unready");
        assert_eq!(state_label(""), "unknown");
        assert_eq!(state_label("ready"), "ready");
        // No label outgrows the badge column at 7.2px a cell plus chrome.
        for state in [
            "running-unready",
            "externally-owned",
            "queued-start",
            "restarting",
            "preparing",
            "succeeded",
        ] {
            assert!(state_label(state).chars().count() as f32 * 7.2 + 28. <= STATE_COL);
        }
    }
}
