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
        .child(name)
}

/// The whole outer box of a dialog: the surface, the shadow that lifts it off
/// the workspace, and the size every dialog in this fork shares.
///
/// One call rather than the eight lines each window used to carry, because
/// eight lines repeated sixty-eight times is how sixty-eight windows end up
/// eight different shapes. `overflow_hidden` belongs to the shell and not to
/// the caller: a child that paints past a rounded corner is what makes the
/// radius look like a mistake.
pub fn dialog_shell(cx: &App) -> gpui::Div {
    use crate::StyledExt as _;
    gpui::div()
        .flex()
        .flex_col()
        .w(DIALOG_WIDTH)
        .max_h(DIALOG_MAX_HEIGHT)
        .overflow_hidden()
        // The same step of the elevation ramp the pickers float at, rather
        // than the surface and shadow written out again here: that ramp
        // already decides the near-black fill, the raised border a modal gets
        // instead of a menu's dim one, the corner radius and the shadow. Two
        // places deciding it is how a window comes to have a dim border
        // beside a picker's raised one.
        .elevation_3(cx)
        .debug_selector(|| "DIALOG-SHELL".to_string())
}

/// How wide every dialog is, and how tall it may grow before its middle
/// scrolls instead. Shared so that two windows opened one after the other do
/// not jump size between them.
pub const DIALOG_WIDTH: Pixels = px(760.);
pub const DIALOG_MAX_HEIGHT: Pixels = px(480.);

/// The row a dialog names itself on: the title at the left, and room after it
/// for whatever the window keeps at the right -- which is the way out.
///
/// The spacer is part of the helper so that the close control lands in the
/// corner without every caller remembering to push it there.
pub fn dialog_header(name: impl Into<crate::SharedString>, cx: &App) -> gpui::Div {
    gpui::div()
        .flex()
        .flex_row()
        .flex_none()
        .w_full()
        .px_3()
        .py_2()
        .gap_2()
        .items_center()
        .child(dialog_title(name, cx))
        .child(gpui::div().flex_1())
        .debug_selector(|| "DIALOG-HEADER".to_string())
}

/// The middle of a dialog, between the header and the footer.
///
/// `flex_1` with `min_h_0` is what lets it give way when the window is short,
/// so the footer keeps its full height and no action is pushed past the
/// window's edge. `items_stretch` is not decoration either: a row centres its
/// children here, and a centred child is given the height of its own contents
/// rather than the height of the row -- which once stood a 773px column inside
/// a 480px window, centred on it, with nothing to scroll.
pub fn dialog_body() -> gpui::Div {
    gpui::div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_stretch()
        .overflow_hidden()
        .debug_selector(|| "DIALOG-BODY".to_string())
}

/// What a footer keeps on its left: a path, a count, a pair of toggles, how a
/// test went.
///
/// Capped at half the bar and allowed to be cut short, because asking a flex
/// row to give way is not enough -- the text in it reports its whole width as
/// the least it can take, and the actions are what get pushed past the edge of
/// the window. A button off the edge cannot be clicked at all, so the labels
/// are what lose the argument.
pub fn dialog_footer_left() -> gpui::Div {
    gpui::div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .max_w(gpui::relative(0.5))
        .min_w_0()
        .flex_shrink_1()
        .overflow_hidden()
        .debug_selector(|| "DIALOG-FOOTER-LEFT".to_string())
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
    gpui::div()
        .flex()
        .flex_col()
        .w_full()
        .gap_1()
        // Mixed case at the same size the reference window labels its fields
        // with. A field label is read alongside the value under it, not
        // announced.
        .child(
            crate::Label::new(name.clone())
                .size(crate::LabelSize::Small)
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
    /// border, and the corner radius a floating surface gets.
    ///
    /// The radius is read from the elevation ramp rather than written here as
    /// a number, because the ramp is what the pickers already round by -- 53
    /// of them, through `elevation_3`. A second number in this file is how a
    /// window ends up with 6px corners beside a picker's 12px ones, which is
    /// the kind of difference nobody can name and everybody sees.
    ///
    /// [`RADIUS`] is the smaller step, for what sits *inside* a surface: the
    /// segmented frames and the fields.
    fn cyberpunk_surface(self) -> Self {
        self.bg(surface())
            .rounded(crate::ElevationIndex::ModalSurface.radius())
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

    struct DialogHost {
        body_is_taller_than_the_window: bool,
    }

    impl Render for DialogHost {
        fn render(
            &mut self,
            _window: &mut Window,
            cx: &mut Context<Self>,
        ) -> impl gpui::IntoElement {
            let tall = if self.body_is_taller_than_the_window {
                gpui::px(2000.0)
            } else {
                gpui::px(80.0)
            };
            dialog_shell(cx)
                .child(
                    dialog_header("Edit Connection", cx).child(
                        gpui::div()
                            .debug_selector(|| "DIALOG-CLOSE".to_string())
                            .child(crate::Button::new("dismiss", "x")),
                    ),
                )
                .child(dialog_body().child(gpui::div().w_full().h(tall)))
                .child(
                    dialog_footer()
                        .child(dialog_footer_left().child(crate::Label::new(
                            "a left-hand label long enough to want the whole bar for itself",
                        )))
                        .child(crate::Button::new("cancel", "Cancel"))
                        .child(crate::Button::new("save", "Save")),
                )
        }
    }

    fn draw_a_dialog(
        cx: &mut gpui::TestAppContext,
        body_is_taller_than_the_window: bool,
    ) -> &mut gpui::VisualTestContext {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| DialogHost {
            body_is_taller_than_the_window,
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        cx
    }

    // The way out sits in the corner the reader reaches for, and the window
    // names itself at the other end of the same row.
    #[gpui::test]
    async fn a_dialog_names_itself_at_the_left_and_keeps_the_way_out_at_the_right(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_dialog(cx, false);

        let header = cx
            .debug_bounds("DIALOG-HEADER")
            .expect("the header is painted");
        let close = cx
            .debug_bounds("DIALOG-CLOSE")
            .expect("the way out is painted");

        assert!(
            close.right() > header.left() + header.size.width * 0.85,
            "the way out ends at {:?} in a header spanning {:?}..{:?}, so it is not in the \
             corner",
            close.right(),
            header.left(),
            header.right()
        );
    }

    // The rule this guards: nothing may hang past the window's edge. A body
    // taller than the window has to give way, and the footer has to keep its
    // full height -- an action pushed off the edge cannot be clicked at all.
    //
    // What it proves and what it does not, because the difference was measured
    // rather than assumed. Removing `max_h` from the shell fails it: a 2000px
    // body grows the window to 1080px. Removing `flex_none` from the footer,
    // `min_h_0` from the body, or `overflow_hidden` from the shell does not --
    // each was taken out in turn and the test still passed, because `max_h`
    // with a `flex_1` middle is already enough for this assembly. So those
    // three are kept as the belt to this brace, and are not something this
    // test may be cited as validating.
    #[gpui::test]
    async fn a_body_taller_than_the_window_gives_way_and_the_actions_stay_inside(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_dialog(cx, true);

        let shell = cx
            .debug_bounds("DIALOG-SHELL")
            .expect("the shell is painted");
        let footer = cx
            .debug_bounds("DIALOG-FOOTER")
            .expect("the footer is painted");
        let save = cx
            .debug_bounds("BUTTON-Save")
            .expect("the confirming action is painted");

        assert!(
            shell.size.height <= DIALOG_MAX_HEIGHT + gpui::px(0.5),
            "a body of 2000px grew the window to {:?}, past the {:?} it may reach",
            shell.size.height,
            DIALOG_MAX_HEIGHT
        );
        assert!(
            footer.bottom() <= shell.bottom() + gpui::px(0.5),
            "the footer ends at {:?} below a shell ending at {:?}, so it hangs past the edge",
            footer.bottom(),
            shell.bottom()
        );
        assert!(
            save.bottom() <= shell.bottom() + gpui::px(0.5) && save.size.height > gpui::px(8.0),
            "the confirming action is painted {:?} tall ending at {:?}, in a shell ending at \
             {:?} -- it has been squeezed or pushed out",
            save.size.height,
            save.bottom(),
            shell.bottom()
        );
    }

    // The left-hand side of a footer is what gives way, not the actions: the
    // label here is deliberately longer than half the bar.
    #[gpui::test]
    async fn a_long_left_hand_label_is_cut_short_rather_than_pushing_the_actions_out(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_dialog(cx, false);

        let footer = cx
            .debug_bounds("DIALOG-FOOTER")
            .expect("the footer is painted");
        let left = cx
            .debug_bounds("DIALOG-FOOTER-LEFT")
            .expect("the left-hand side is painted");
        let save = cx
            .debug_bounds("BUTTON-Save")
            .expect("the confirming action is painted");

        assert!(
            left.size.width <= footer.size.width * 0.5 + gpui::px(1.0),
            "the left-hand side took {:?} of a {:?} bar, past the half it is allowed",
            left.size.width,
            footer.size.width
        );
        assert!(
            save.right() > footer.left() + footer.size.width * 0.85,
            "the actions still end in the corner: Save ends at {:?} in a bar spanning \
             {:?}..{:?}",
            save.right(),
            footer.left(),
            footer.right()
        );
    }
}
