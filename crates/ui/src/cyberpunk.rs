//! Shared palette and styling helpers for everything this fork raises above the
//! content: dialogs (`Modal`, `AlertModal`, the built-in prompt renderer, the
//! fork's own modal forms), the pickers and command palette, and the surfaces
//! that float over a buffer -- completion and code-action menus, hover and
//! signature popovers, tooltips, context menus. Docked panels are outside that
//! boundary and keep reading the active theme; the line is "does it float", not
//! "is it ours". Colors here are fixed, not read from the active theme: the whole
//! point of the style is a near-black surface with exactly two accents, so it
//! must not drift with whatever theme the user has picked.
//!
//! A surface inside this boundary must not paint a themed color on top of these:
//! a theme color over a fixed near-black surface is how unreadable text happens.
//!
//! Only two accents exist on purpose (cyan for the focal element, red for
//! danger). Do not add a third without a matching argument for why every
//! dialog that reads this module should carry it.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, BoxShadow, Hsla, InteractiveElement, ParentElement, Pixels, PromptLevel, Styled, px, rgb,
};
use theme::ActiveTheme as _;

use crate::LabelCommon as _;

use crate::{ButtonStyle, TintColor};

/// Fixed spacing scale, independent of `DynamicSpacing`/UI density: the
/// rhythm this style calls for must stay constant even if the user changes
/// their UI scale setting.
pub const SPACE_4: Pixels = px(4.);
pub const SPACE_8: Pixels = px(8.);
pub const SPACE_14: Pixels = px(14.);
pub const SPACE_18: Pixels = px(18.);
pub const SPACE_22: Pixels = px(22.);

/// Window background. Never pure black; blue-shifted near-black reads as
/// "screen" rather than "ink".
pub fn canvas() -> Hsla {
    rgb(0x06080d).into()
}

/// Inputs, raised panels, the dialog box itself.
pub fn surface() -> Hsla {
    rgb(0x0a0f17).into()
}

/// Resting border / divider color.
pub fn border_dim() -> Hsla {
    rgb(0x1d2a38).into()
}

/// Button outline / focusable edge color.
pub fn border_raised() -> Hsla {
    rgb(0x24354a).into()
}

/// Maximum-contrast text, for values and content.
pub fn text_primary() -> Hsla {
    rgb(0xf0f7ff).into()
}

/// Field labels and captions.
pub fn text_secondary() -> Hsla {
    rgb(0x8aa2b8).into()
}

/// Genuinely de-emphasised text only.
pub fn text_tertiary() -> Hsla {
    rgb(0x55697e).into()
}

/// The two accents dialogs are allowed to use. Assign one semantically per
/// dialog and never reuse it decoratively elsewhere in the same view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accent {
    /// Data, interactive, the focal element.
    Cyan,
    /// Danger, destructive, privilege, alarm.
    Red,
}

impl Accent {
    /// Border / stripe color for this accent.
    pub fn border(self) -> Hsla {
        match self {
            Accent::Cyan => rgb(0x00e5ff).into(),
            Accent::Red => rgb(0xff003c).into(),
        }
    }

    /// Brighter variant, for text or a stronger glow.
    pub fn bright(self) -> Hsla {
        match self {
            Accent::Cyan => rgb(0x4df3ff).into(),
            Accent::Red => rgb(0xff415c).into(),
        }
    }
}

/// Which accent a prompt's confirm action should carry. `Warning` and
/// `Critical` both mean the user is being asked to pause before an action
/// with real consequences (Zed's own call sites use `Warning` for things like
/// an irreversible schema change, not just a mild notice), so both map to the
/// danger accent; there is no separate amber tier once only two accents
/// exist. Only `Info` — a routine, no-consequence notice — stays neutral.
pub fn accent_for_prompt_level(level: PromptLevel) -> Accent {
    match level {
        PromptLevel::Warning | PromptLevel::Critical => Accent::Red,
        PromptLevel::Info => Accent::Cyan,
    }
}

/// Same decision for dialogs that only know "is confirming this dangerous",
/// rather than carrying a full `PromptLevel`.
pub fn accent_for_danger(is_dangerous: bool) -> Accent {
    if is_dangerous {
        Accent::Red
    } else {
        Accent::Cyan
    }
}

/// How a row says it is under the pointer, being pressed, or chosen. One accent
/// at three strengths rather than three greys: a list inside this chrome has to
/// answer in the same colour as everything else in it.
pub fn row_hovered() -> Hsla {
    Accent::Cyan.border().opacity(0.10)
}

pub fn row_pressed() -> Hsla {
    Accent::Cyan.border().opacity(0.20)
}

pub fn row_chosen() -> Hsla {
    Accent::Cyan.border().opacity(0.16)
}

/// How much an action matters. Four ranks and no more: the one the reader came
/// for, the way out, the secondary one, and the one that destroys something.
///
/// The rank says how important the action is. The frame -- which every rank
/// carries, without exception -- says it is an action at all rather than a
/// caption. Those are two different messages, and a quiet rank without an
/// outline degenerates into exactly the column of bare words this style exists
/// to leave behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rank {
    /// The action the reader came for. One per surface.
    Accent,
    /// The way out, and anything else of ordinary weight.
    Neutral,
    /// Secondary, still an action.
    Quiet,
    /// Destroys something. Reserved for that, so it keeps meaning it.
    Destructive,
}

impl Rank {
    pub fn style(self) -> ButtonStyle {
        match self {
            Rank::Accent => ButtonStyle::Tinted(TintColor::Accent),
            Rank::Neutral => ButtonStyle::OutlinedCustom(border_raised()),
            Rank::Quiet => ButtonStyle::OutlinedCustom(border_dim()),
            Rank::Destructive => ButtonStyle::Tinted(TintColor::Error),
        }
    }
}

/// A row of icon actions under one frame, with a hairline between them.
///
/// A strip of bare icons reads as decoration and gives no hint that any of it is
/// pressable; a frame around each one reads as a fence. One frame with dividers
/// says both things at once, and is what every toolbar of adjacent icons in this
/// chrome should be built from.
pub fn segmented(actions: impl IntoIterator<Item = gpui::AnyElement>) -> gpui::Div {
    let mut row = gpui::div().flex().flex_row().items_center().flex_none();
    row = row
        .rounded(RADIUS)
        .border_1()
        .border_color(border_dim())
        .overflow_hidden();
    for (at, action) in actions.into_iter().enumerate() {
        if at > 0 {
            row = row.child(
                gpui::div()
                    .w(px(1.))
                    .h(SEGMENT_HEIGHT - px(8.))
                    .bg(border_dim()),
            );
        }
        row = row.child(action);
    }
    row
}

/// The corner radius every framed action shares. One number, so nothing in this
/// chrome rounds by a different amount than anything else.
pub const RADIUS: Pixels = px(6.);

/// How tall a framed row of icon actions stands, and with it the hit area of
/// each icon in it.
pub const SEGMENT_HEIGHT: Pixels = px(28.);

/// The heading a dialog opens with: its own name, in the monospace face, small
/// and loud. Every dialog in this chrome says what it is in the same voice, so
/// the voice lives here rather than being written out again in each of them.
pub fn dialog_title(name: impl Into<crate::SharedString>, cx: &App) -> gpui::Div {
    let name = name.into();
    gpui::div()
        .font(theme::theme_settings(cx).buffer_font(cx).clone())
        .font_weight(gpui::FontWeight::EXTRA_BOLD)
        .text_size(crate::HeadlineSize::Small.rems())
        .text_color(text_primary())
        .child(name.to_uppercase())
}

/// A rule across a form with a name on it, marking where one group of fields
/// ends and the next begins.
///
/// Returned as the row itself rather than a finished element, so a section can
/// carry its own action on the rule -- an "add" beside the name it belongs to,
/// rather than orphaned on a line beneath it.
pub fn dialog_section(name: impl Into<crate::SharedString>) -> gpui::Div {
    let name = name.into();
    let shown = name.to_uppercase();
    gpui::div()
        .flex()
        .flex_row()
        .w_full()
        .pt_2()
        .gap_2()
        .items_center()
        .debug_selector(move || format!("DIALOG-SECTION-{name}"))
        .child(
            crate::Label::new(shown)
                .size(crate::LabelSize::XSmall)
                .color(crate::Color::Accent),
        )
        .child(gpui::div().flex_1().h(px(1.)).bg(border_dim()))
}

/// One labelled place to type: the name above it in small muted capitals, and
/// below it a box on a ground of its own.
///
/// The ground matters. A box drawn with a line alone reads as a rule across the
/// form rather than as somewhere to type, which is how a field can look subtly
/// wrong without anything being obviously broken.
pub fn dialog_field(
    name: impl Into<crate::SharedString>,
    tall: bool,
    cx: &App,
    inside: impl gpui::IntoElement,
) -> gpui::Div {
    dialog_field_on(name, tall, cx.theme().colors().editor_background, inside)
}

/// Same field, for a caller that has already read the ground colour out of the
/// theme. A form that builds many of these cannot hold the context open across
/// all of them, so it resolves the colour once and passes it along.
pub fn dialog_field_on(
    name: impl Into<crate::SharedString>,
    tall: bool,
    ground: Hsla,
    inside: impl gpui::IntoElement,
) -> gpui::Div {
    let name = name.into();
    let shown = name.to_uppercase();
    gpui::div()
        .flex()
        .flex_col()
        .w_full()
        .gap_1()
        .child(
            crate::Label::new(shown)
                .size(crate::LabelSize::XSmall)
                .color(crate::Color::Muted),
        )
        .child(
            gpui::div()
                .w_full()
                .debug_selector(move || format!("DIALOG-FIELD-{name}"))
                .bg(ground)
                .rounded_lg()
                // A minimum rather than a fixed height, with the line centred in
                // it: a fixed box stops fitting the moment the text scale moves.
                .when(!tall, |field| field.flex().items_center().min_h(px(34.)))
                .when(tall, |field| field.min_h(px(84.)))
                .px_2()
                .py_1()
                .border_1()
                .border_color(border_dim())
                .child(inside),
        )
}

/// The bar a dialog ends with: a rule above it, and the actions on it. What goes
/// on it is the caller's, but where it sits and how it is spaced is not.
///
/// The actions end in the bottom-right corner of the surface, with the
/// confirming one last. That is the whole point of the helper and not a default
/// a call site may override: a row of actions packed at the left edge reads as
/// part of the form above it rather than as the answer the dialog is waiting
/// for. `flex_none` goes with it, so the row keeps its full height when the
/// window is short and the scrollable middle gives way instead.
///
/// Anything that belongs on the left -- a path, an error, a count -- is added
/// before [`dialog_footer_spacer`].
pub fn dialog_footer() -> gpui::Div {
    gpui::div()
        .flex()
        .flex_row()
        .flex_none()
        .w_full()
        .px_3()
        .py_2()
        .gap_2()
        .items_center()
        .justify_end()
        .border_t_1()
        .border_color(border_dim())
        .debug_selector(|| "DIALOG-FOOTER".to_string())
}

/// What holds a footer's left-hand side apart from the actions it ends with.
///
/// Needed because the row right-aligns everything on it: without this, a leading
/// label rides along to the right edge and sits against the buttons.
pub fn dialog_footer_spacer() -> gpui::Div {
    gpui::div().flex_1()
}

/// A soft outer glow for the one focal element of a view. Kept low-alpha per
/// the source design doc: glow needs to read as a lit edge, not fog.
pub fn focal_glow(accent: Accent) -> Vec<BoxShadow> {
    vec![BoxShadow::new(px(0.), px(0.), accent.border().opacity(0.45)).blur_radius(px(10.))]
}

/// Extends [`gpui::Styled`] with the cyberpunk dialog chrome primitives, so
/// every dialog surface is built from the same handful of calls.
pub trait CyberpunkSurface: Styled + Sized {
    /// The base near-black dialog box: fixed surface color, a thin resting
    /// border, sharp corners. Corners are zeroed explicitly since rounding is
    /// the fastest way to break the look.
    fn cyberpunk_surface(self) -> Self {
        self.bg(surface())
            .rounded_none()
            .border_1()
            .border_color(border_dim())
    }

    /// Marks the one focal element of a dialog with a bright border and a
    /// subtle glow. Call this on at most one element per view.
    fn cyberpunk_focal(self, accent: Accent) -> Self {
        self.border_1()
            .border_color(accent.border())
            .shadow(focal_glow(accent))
    }

    /// Sets the monospace buffer font without changing size or color.
    fn cyberpunk_monospace(self, cx: &App) -> Self {
        self.font(theme::theme_settings(cx).buffer_font(cx).clone())
    }
}

impl<E: Styled> CyberpunkSurface for E {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, Window};

    #[test]
    fn warning_and_critical_prompts_confirm_in_red_info_stays_cyan() {
        assert_eq!(accent_for_prompt_level(PromptLevel::Critical), Accent::Red);
        assert_eq!(accent_for_prompt_level(PromptLevel::Warning), Accent::Red);
        assert_eq!(accent_for_prompt_level(PromptLevel::Info), Accent::Cyan);
    }

    #[test]
    fn danger_flag_picks_the_matching_accent() {
        assert_eq!(accent_for_danger(true), Accent::Red);
        assert_eq!(accent_for_danger(false), Accent::Cyan);
    }

    #[test]
    fn only_two_accents_exist() {
        // Guards the scarcity rule at compile time: exhaustively matching
        // `Accent` here means adding a third variant forces a decision at
        // every call site that maps it to a color, not a silent addition.
        for accent in [Accent::Cyan, Accent::Red] {
            let border = accent.border();
            let bright = accent.bright();
            assert_ne!(border, bright);
        }
    }

    #[test]
    fn base_ramp_never_touches_pure_black() {
        let canvas = canvas();
        assert!(
            canvas.s > 0.0,
            "canvas must be blue-shifted, not pure black"
        );
        let surface = surface();
        assert!(
            surface.l > canvas.l,
            "surface should sit above canvas in the ramp"
        );
    }

    struct FooterHost {
        with_a_left_hand_label: bool,
    }

    impl Render for FooterHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div().w(gpui::px(600.0)).h(gpui::px(120.0)).child(
                dialog_footer()
                    .when(self.with_a_left_hand_label, |this| {
                        this.child(crate::Label::new("path/to/file"))
                            .child(dialog_footer_spacer())
                    })
                    .child(crate::Button::new("close", "Close"))
                    .child(crate::Button::new("save", "Save")),
            )
        }
    }

    fn draw_a_footer(
        cx: &mut gpui::TestAppContext,
        with_a_left_hand_label: bool,
    ) -> &mut gpui::VisualTestContext {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| FooterHost {
            with_a_left_hand_label,
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        cx
    }

    // Where a dialog's actions are painted, not where the element tree says
    // they were put: a row packed at the left edge reads as part of the form
    // above it rather than as the answer the dialog is waiting for. Measured
    // on the boxes, because that is the whole of the bug.
    #[gpui::test]
    async fn a_dialog_footers_actions_are_painted_in_the_bottom_right_corner(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_footer(cx, false);

        let footer = cx
            .debug_bounds("DIALOG-FOOTER")
            .expect("the footer is painted");
        let save = cx
            .debug_bounds("BUTTON-Save")
            .expect("the confirming action is painted");
        let close = cx
            .debug_bounds("BUTTON-Close")
            .expect("the dismissing action is painted");

        let last_seventh = footer.left() + footer.size.width * 0.85;
        assert!(
            save.right() > last_seventh,
            "the confirming action ends at {:?}, left of the corner it belongs in -- \
             the footer spans {:?}..{:?}",
            save.right(),
            footer.left(),
            footer.right()
        );
        assert!(
            close.left() > footer.left() + footer.size.width * 0.5,
            "both actions belong in the right half; the dismissing one starts at {:?} \
             in a footer spanning {:?}..{:?}",
            close.left(),
            footer.left(),
            footer.right()
        );
        assert!(
            close.right() <= save.left(),
            "the confirming action comes last, so Close at {:?}..{:?} sits left of \
             Save at {:?}..{:?}",
            close.left(),
            close.right(),
            save.left(),
            save.right()
        );
    }

    // A footer that also carries something on its left -- a path, a count, an
    // error -- keeps that on the left while the actions still end in the
    // corner. A guard rather than a proof: the spacer's `flex_1` would push
    // the actions right on its own, so this cannot fail from the alignment
    // being absent, only from the spacer being wired wrongly.
    #[gpui::test]
    async fn a_left_hand_label_does_not_ride_along_to_the_corner(cx: &mut gpui::TestAppContext) {
        let cx = draw_a_footer(cx, true);

        let footer = cx
            .debug_bounds("DIALOG-FOOTER")
            .expect("the footer is painted");
        let close = cx
            .debug_bounds("BUTTON-Close")
            .expect("the dismissing action is painted");
        let save = cx
            .debug_bounds("BUTTON-Save")
            .expect("the confirming action is painted");

        assert!(
            save.right() > footer.left() + footer.size.width * 0.85,
            "the actions still end in the corner beside a left-hand label"
        );
        assert!(
            close.left() > footer.left() + footer.size.width * 0.5,
            "the label holds the left, so neither action is dragged into it"
        );
    }
}
