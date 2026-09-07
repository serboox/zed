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
    App, BoxShadow, Hsla, InteractiveElement, IntoElement as _, MouseButton, ParentElement, Pixels,
    PromptLevel, Styled, Window, px, rgb,
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

/// The two hues a chart's series are drawn in, and no others.
///
/// Not the two accents: those carry "focal" and "danger", and a line saying how
/// much memory a build is holding is neither -- reusing them would make every
/// chart read as an alarm. Red with green is out for the same reason it is out
/// everywhere: roughly one man in twelve cannot separate that pair, and it is
/// the pair everyone reaches for first. Cyan and violet stay apart for every
/// kind of colour vision, and they are far enough apart in lightness (a light
/// hue against a mid-dark one) that the pair still reads once the screen is
/// desaturated -- which is the test a hue pair has to pass, since colour alone
/// is never allowed to carry a quantity here.
pub fn series_processor() -> Hsla {
    rgb(0x00e5ff).into()
}

pub fn series_memory() -> Hsla {
    rgb(0x9a5cff).into()
}

/// How many steps [`ramp`] has.
pub const RAMP_STEPS: usize = 5;

/// Where a value sits on a sequential ramp of the memory hue: `0.` for the
/// smallest of a set of bars, `1.` for the largest.
///
/// One hue getting lighter, never a spread of different hues: a set of bars
/// sorted by size is ordered data, and a rainbow over ordered data reads as
/// categories that have nothing to do with each other. Quantised to
/// [`RAMP_STEPS`] steps because a continuous ramp over a handful of bars gives
/// neighbours a difference nobody can see, while five steps stay apart in
/// lightness as well as in saturation.
pub fn ramp(fraction: f32) -> Hsla {
    let steps = [
        rgb(0x2a1a45),
        rgb(0x452a6e),
        rgb(0x603c99),
        rgb(0x7d4ec6),
        rgb(0x9a5cff),
    ];
    let last = RAMP_STEPS - 1;
    let step = (fraction.clamp(0., 1.) * last as f32).round() as usize;
    steps[step.min(last)].into()
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
/// the workspace, the size it opens at, and the grips that let the reader
/// change that size or carry the window somewhere else.
///
/// One call rather than the eight lines each window used to carry, because
/// eight lines repeated sixty-eight times is how sixty-eight windows end up
/// eight different shapes. `overflow_hidden` belongs to the shell and not to
/// the caller: a child that paints past a rounded corner is what makes the
/// radius look like a mistake.
///
/// `name` is what the window is called, and doubles as what its size and place
/// are remembered under -- so a window reopened in the same session comes back
/// the way it was left. It only has to be stable, not the visible title: a
/// window whose heading changes with its state (the dev container's does)
/// passes one name for the whole run of states.
pub fn dialog_shell(name: impl Into<crate::SharedString>, window: &Window, cx: &App) -> gpui::Div {
    use crate::StyledExt as _;
    let name = name.into();
    let viewport = window.viewport_size();
    let this = placed(&name, window);
    let held_size = placement_of(&this, cx);
    // A size the reader chose in a larger editor is still held to the editor
    // there is now. Without this, shrinking the editor window leaves the
    // dialog's footer -- and the action it is waiting for -- outside it.
    let chosen = held_size.size.map(|size| {
        gpui::size(
            held(size.width, DIALOG_MIN_WIDTH, viewport.width),
            held(size.height, DIALOG_MIN_HEIGHT, viewport.height - DROPPED_BY),
        )
    });
    let width = chosen.map_or_else(|| dialog_default_width(viewport), |size| size.width);

    gpui::div()
        .flex()
        .flex_col()
        .w(width)
        // A height only once the reader has asked for one. Until then the
        // window is as tall as what is in it and no taller, which is what keeps
        // a two-line confirmation from opening as tall as a form.
        .when_some(chosen, |shell, size| shell.h(size.height))
        .when(chosen.is_none(), |shell| {
            shell.max_h(dialog_default_max_height(viewport))
        })
        // Where the window has been carried to, as an offset from where the
        // layout would have put it. An offset rather than an absolute place, so
        // a window that has never been moved still lands wherever whatever
        // opened it decides -- the modal layer centres it -- and a window that
        // has been moved keeps that relationship when the editor is resized.
        .left(held_size.moved_by.x)
        .top(held_size.moved_by.y)
        .overflow_hidden()
        // The same step of the elevation ramp the pickers float at, rather
        // than the surface and shadow written out again here: that ramp
        // already decides the near-black fill, the raised border a modal gets
        // instead of a menu's dim one, the corner radius and the shadow. Two
        // places deciding it is how a window comes to have a dim border
        // beside a picker's raised one.
        .elevation_3(cx)
        // Carrying the window by its naming row is decided here rather than in
        // [`dialog_header`], because a dialog is free to build its own heading
        // and several do. What the row is, from here, is the top
        // [`HEADER_BAND`] of the surface.
        .on_mouse_down(MouseButton::Left, {
            let name = name.clone();
            move |event: &gpui::MouseDownEvent, window: &mut Window, cx: &mut App| {
                let this = placed(&name, window);
                let Some(was) = painted_bounds(&this, cx) else {
                    return;
                };
                if event.position.y - was.top() > HEADER_BAND {
                    return;
                }
                if event.click_count >= 2 {
                    forget_placement(&this, cx);
                } else {
                    start_drag(
                        this,
                        Pinned::InTheMiddle,
                        Grip::Move,
                        event.position,
                        was,
                        cx,
                    );
                }
                window.refresh();
            }
        })
        .child(dialog_drag_watcher(name.clone()))
        .children(dialog_grips(&name, Pinned::InTheMiddle))
        .debug_selector(|| "DIALOG-SHELL".to_string())
}

/// How a floating surface takes part in being carried and resized.
///
/// A container rather than four arguments, because every call site sets a
/// different two of them and a four-argument call says nothing about which.
#[derive(Debug, Clone)]
pub struct Floating {
    name: crate::SharedString,
    pinned: Pinned,
    top_at: Pixels,
    own: gpui::Point<Pixels>,
    edges_resize_it: bool,
    placed_by_its_host: bool,
}

/// Which side of the workspace holds the surface in place, and how far in.
///
/// It decides two things at once: which inset the offset is applied to, and
/// where the edge that was *not* grabbed ends up. A centred window that grows
/// by ten pixels has each of its edges move out by five on its own; a surface
/// pinned by one edge keeps that edge exactly where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pinned {
    /// Centred, the way the modal layer places a dialog or a picker.
    InTheMiddle,
    /// Its left edge held this far from the left of what contains it.
    ByItsLeft(Pixels),
    /// Its right edge held this far from the right.
    ByItsRight(Pixels),
}

impl Pinned {
    /// How much of what the surface grew is given back to the offset when the
    /// right edge is dragged. The left edge takes the remainder, so one number
    /// says both.
    fn given_back(self) -> f32 {
        match self {
            Pinned::InTheMiddle => 0.5,
            Pinned::ByItsLeft(_) => 0.,
            Pinned::ByItsRight(_) => 1.,
        }
    }
}

impl Floating {
    /// A surface pinned by its left edge at the very left, whose own edges
    /// resize it.
    ///
    /// `name` is what its size and place are remembered under, so it has to be
    /// the same string every time this surface is opened -- not a heading that
    /// names the row or column it happens to be about.
    pub fn named(name: impl Into<crate::SharedString>) -> Self {
        Self {
            name: name.into(),
            pinned: Pinned::ByItsLeft(px(0.)),
            top_at: px(0.),
            own: gpui::Point::default(),
            edges_resize_it: true,
            placed_by_its_host: false,
        }
    }

    /// Which side already holds the surface, and how far in. Passed rather than
    /// overwritten, because two `left` calls on one element mean the second
    /// wins and the first silently does nothing -- and a surface pinned by its
    /// right edge that is given a `left` jumps across the pane.
    pub fn pinned(mut self, pinned: Pinned) -> Self {
        self.pinned = pinned;
        self
    }

    /// How far down its container the surface already starts.
    pub fn top_at(mut self, top_at: Pixels) -> Self {
        self.top_at = top_at;
        self
    }

    /// A displacement the caller works out for itself and still wants -- a
    /// picker recentres itself as it is resized. Added to what the reader
    /// carried the surface by rather than replaced by it.
    pub fn carried_from(mut self, own: gpui::Point<Pixels>) -> Self {
        self.own = own;
        self
    }

    /// For a surface whose place its host applies rather than the surface
    /// itself: the editor lays its popovers out as root elements, and the
    /// offsets a root element asks for are dropped by whatever draws it. Such a
    /// host reads [`carried_by`] and adds it to the origin it draws at, so this
    /// says "add the strip and remember the place, but do not set the insets".
    pub fn placed_by_its_host(mut self) -> Self {
        self.placed_by_its_host = true;
        self
    }

    /// For a surface that decides its own size and keeps its own edges -- a
    /// picker resizes itself and persists the result. It takes the carrying and
    /// nothing else.
    pub fn keeping_its_own_size(mut self) -> Self {
        self.edges_resize_it = false;
        self
    }
}

/// Lets a floating surface -- a popup, a menu, a picker -- be carried somewhere
/// else, and unless it keeps its own size, resized by its edges.
///
/// What it is carried by is a strip along its top edge rather than the whole
/// top band a dialog is picked up by, because a floating surface has no naming
/// row to spare: the top of a picker is its query field, and an editor's own
/// mouse handling does not stop a press from reaching an ancestor -- a band
/// there would carry the surface off every time the reader dragged across the
/// query to select it.
///
/// Call it last, around the finished surface: the size the reader chose has to
/// be set after whatever size the caller sets, or the caller's own size wins on
/// every frame and the drag appears to do nothing.
pub fn floating<E>(how: Floating, surface: E, window: &Window, cx: &App) -> E
where
    E: Styled + ParentElement + InteractiveElement + gpui::IntoElement,
{
    let Floating {
        name,
        pinned,
        top_at,
        own,
        edges_resize_it,
        placed_by_its_host,
    } = how;
    let this = placed(&name, window);
    let placement = placement_of(&this, cx);
    let carried = placement.moved_by;
    let viewport = window.viewport_size();
    let size = placement.size.filter(|_| edges_resize_it).map(|size| {
        gpui::size(
            held(size.width, DIALOG_MIN_WIDTH, viewport.width),
            held(size.height, DIALOG_MIN_HEIGHT, viewport.height),
        )
    });

    surface
        .when(!placed_by_its_host, |surface| {
            surface
                .map(|surface| match pinned {
                    Pinned::InTheMiddle => surface.left(own.x + carried.x),
                    Pinned::ByItsLeft(inset) => surface.left(inset + own.x + carried.x),
                    Pinned::ByItsRight(inset) => surface.right(inset + own.x - carried.x),
                })
                .top(top_at + own.y + carried.y)
                .when_some(size, |surface, size| surface.w(size.width).h(size.height))
        })
        // Room for the strip that carries it, so the strip covers a margin of
        // the surface's own rather than the first row of what is in it. Without
        // it, a press meant for the search field at the top of a popup grabs
        // the surface instead -- the strip is drawn last and answers first.
        //
        // Not for a surface its host places: the host measures such a surface
        // as a root element, and padding added here is padding it measures --
        // a one-line popover would come out `CARRY_STRIP` taller with its text
        // pushed down, and the height it reports decides whether it is placed
        // above or below the caret. The strip lies over the first row there
        // instead, which is prose in both surfaces that are placed this way.
        .when(!placed_by_its_host, |surface| surface.pt(CARRY_STRIP))
        .child(dialog_drag_watcher(name.clone()))
        .when(edges_resize_it, |surface| {
            surface.children(dialog_grips(&name, pinned))
        })
        .child(gpui::deferred(carry_strip(name, pinned)).with_priority(1))
}

/// The width a dialog opens at:/// The width a dialog opens at: most of the editor's width rather than a fixed
/// box, because the fixed box was 760px whether the editor was 1280px wide or
/// 3840px, and a form with two columns in it had to scroll in both directions
/// while three quarters of the screen stood empty.
///
/// Held between a floor and a ceiling all the same. The floor is what the
/// widest dialog's own contents need; the ceiling is there because a line of
/// text 3000px long is unreadable, and a dialog is mostly lines of text.
pub fn dialog_default_width(viewport: gpui::Size<Pixels>) -> Pixels {
    let room = (viewport.width - MARGIN * 2.).max(DIALOG_MIN_WIDTH);
    (viewport.width * 0.66)
        .clamp(DIALOG_WIDTH, DIALOG_WIDEST)
        .min(room)
}

/// How tall a dialog may grow before its middle scrolls instead.
///
/// Most of the editor's height, less the room the modal layer drops a window by
/// from the top: a dialog that asks for the whole height is one whose footer --
/// and with it the button the dialog is waiting to be pressed -- is below the
/// bottom edge of the window.
pub fn dialog_default_max_height(viewport: gpui::Size<Pixels>) -> Pixels {
    let room = (viewport.height - DROPPED_BY - MARGIN).max(DIALOG_MIN_HEIGHT);
    (viewport.height * 0.72).min(room)
}

/// The width a dialog opens no narrower than, and no wider than.
pub const DIALOG_WIDTH: Pixels = px(760.);
pub const DIALOG_WIDEST: Pixels = px(1180.);

/// The floor a resize is held to. Small enough to tuck a window out of the way,
/// large enough that the naming row and the way out of it are still there.
pub const DIALOG_MIN_WIDTH: Pixels = px(360.);
pub const DIALOG_MIN_HEIGHT: Pixels = px(180.);

/// Room kept between a dialog and the edge of the editor when it opens.
const MARGIN: Pixels = px(24.);

/// How far down the workspace the modal layer drops a window it opens.
const DROPPED_BY: Pixels = px(80.);

/// How much of a window that has been carried away must still be on screen.
/// Enough that its naming row can be grabbed and it can be carried back.
const KEPT_ON_SCREEN: Pixels = px(120.);

/// How much of the top of the surface counts as the naming row for the purpose
/// of picking the window up. A little more than the row is tall, since the row
/// above a divider is what the reader is aiming at.
const HEADER_BAND: Pixels = px(40.);

/// How tall the strip along the top edge that carries a floating surface is.
/// Thin, since it sits over the surface's own top padding, but no thinner than
/// the edges the same surface is resized by.
const CARRY_STRIP: Pixels = px(10.);

/// How wide the strip along each edge is that resizes the window, and how far
/// into the window a corner reaches.
const GRIP: Pixels = px(6.);
const CORNER: Pixels = px(16.);

/// Which edges of a window a drag moves. `Move` moves all four, which is what
/// carrying the window somewhere else is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grip {
    Move,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Grip {
    fn moves_left(self) -> bool {
        matches!(
            self,
            Grip::Move | Grip::Left | Grip::TopLeft | Grip::BottomLeft
        )
    }

    fn moves_right(self) -> bool {
        matches!(
            self,
            Grip::Move | Grip::Right | Grip::TopRight | Grip::BottomRight
        )
    }

    fn moves_top(self) -> bool {
        matches!(
            self,
            Grip::Move | Grip::Top | Grip::TopLeft | Grip::TopRight
        )
    }

    fn moves_bottom(self) -> bool {
        matches!(
            self,
            Grip::Move | Grip::Bottom | Grip::BottomLeft | Grip::BottomRight
        )
    }
}

/// Where a window stands and how big it is, once the reader has said. `None`
/// for the size means nobody has said yet, and the window is as big as
/// [`dialog_default_width`] and its own contents make it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Placement {
    moved_by: gpui::Point<Pixels>,
    size: Option<gpui::Size<Pixels>>,
}

/// Which dialog, in which editor window. The editor window is part of it
/// because two editor windows can each have a dialog of the same name open --
/// two commit windows, say -- and they are two windows the reader places
/// separately, not one window remembered twice.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Placed {
    in_window: gpui::WindowId,
    name: crate::SharedString,
}

fn placed(name: &crate::SharedString, window: &Window) -> Placed {
    Placed {
        in_window: window.window_handle().window_id(),
        name: name.clone(),
    }
}

/// A drag in progress: which dialog, which of its edges, where the pointer
/// started, and what the window was when it started. Measured from the start
/// rather than accumulated per event, so a drag that outruns the frame rate
/// does not drift.
struct Dragged {
    of: Placed,
    pinned: Pinned,
    grip: Grip,
    from_pointer: gpui::Point<Pixels>,
    from: Placement,
    was: gpui::Bounds<Pixels>,
}

#[derive(Default)]
struct Dialogs {
    placements: std::collections::HashMap<Placed, Placement>,
    /// Where each window was last painted. A resize has to start from the size
    /// the window actually is, which for a window nobody has resized yet is
    /// whatever its contents came to -- a number only the frame knows.
    painted: std::collections::HashMap<Placed, gpui::Bounds<Pixels>>,
    dragged: Option<Dragged>,
}

/// Holds a number between two bounds without assuming which of them is the
/// larger. `clamp` panics when the low bound is above the high one, and in an
/// editor window narrower than a dialog's own floor -- which the reader is free
/// to drag it to -- that is exactly what these two are.
fn held(value: Pixels, at_least: Pixels, at_most: Pixels) -> Pixels {
    value.max(at_least).min(at_most.max(at_least))
}

impl gpui::Global for Dialogs {}

/// Where a surface has been carried to, as an offset from where its own layout
/// would put it. Exposed so a test can tell a drag that never started from one
/// that started and had its offset measured away by whatever places the
/// surface.
pub fn carried_by(name: &crate::SharedString, window: &Window, cx: &App) -> gpui::Point<Pixels> {
    placement_of(&placed(name, window), cx).moved_by
}

/// Whether the reader has given this window a size of its own by dragging one
/// of its edges.
///
/// A dialog that opens narrower or wider than the shared default has to stop
/// imposing that width once this is true, or the width it sets undoes the drag
/// on the very next frame.
pub fn dialog_was_resized(name: &crate::SharedString, window: &Window, cx: &App) -> bool {
    placement_of(&placed(name, window), cx).size.is_some()
}

fn placement_of(of: &Placed, cx: &App) -> Placement {
    cx.try_global::<Dialogs>()
        .and_then(|dialogs| dialogs.placements.get(of).copied())
        .unwrap_or_default()
}

fn painted_bounds(of: &Placed, cx: &App) -> Option<gpui::Bounds<Pixels>> {
    cx.try_global::<Dialogs>()
        .and_then(|dialogs| dialogs.painted.get(of).copied())
}

fn forget_placement(of: &Placed, cx: &mut App) {
    cx.default_global::<Dialogs>().placements.remove(of);
}

fn start_drag(
    of: Placed,
    pinned: Pinned,
    grip: Grip,
    from_pointer: gpui::Point<Pixels>,
    was: gpui::Bounds<Pixels>,
    cx: &mut App,
) {
    let from = placement_of(&of, cx);
    cx.default_global::<Dialogs>().dragged = Some(Dragged {
        of,
        pinned,
        grip,
        from_pointer,
        from,
        was,
    });
}

/// Answers where the drag in progress has taken the window, and reports whether
/// anything about it changed -- which is what decides whether the frame is
/// redrawn.
fn drag_to(
    of: &Placed,
    pointer: gpui::Point<Pixels>,
    viewport: gpui::Size<Pixels>,
    cx: &mut App,
) -> bool {
    let Some((pinned, grip, from, was, from_pointer)) = cx
        .try_global::<Dialogs>()
        .and_then(|dialogs| dialogs.dragged.as_ref())
        .filter(|dragged| &dragged.of == of)
        .map(|dragged| {
            (
                dragged.pinned,
                dragged.grip,
                dragged.from,
                dragged.was,
                dragged.from_pointer,
            )
        })
    else {
        return false;
    };

    let moved = pointer - from_pointer;
    let mut offset = from.moved_by;
    let mut width = was.size.width;
    let mut height = was.size.height;

    // Holding the edge that was not grabbed is what the pinning decides.
    let given_back = pinned.given_back();

    // How large the surface may grow before the edge being dragged leaves the
    // screen. The edge that is *not* being dragged stays where it was painted,
    // so it is that edge the room is measured from -- and measuring it from the
    // start of the drag is what makes it hold: `was` is the one set of bounds in
    // this function that the drag is not changing.
    let room_across = if grip.moves_right() && !grip.moves_left() {
        viewport.width - was.left().max(px(0.))
    } else if grip.moves_left() && !grip.moves_right() {
        was.right().min(viewport.width)
    } else {
        viewport.width
    };
    let room_down = if grip.moves_bottom() && !grip.moves_top() {
        viewport.height - was.top().max(px(0.))
    } else if grip.moves_top() && !grip.moves_bottom() {
        was.bottom().min(viewport.height)
    } else {
        viewport.height
    };
    if grip.moves_left() && grip.moves_right() {
        offset.x += moved.x;
    } else if grip.moves_right() {
        width = held(was.size.width + moved.x, DIALOG_MIN_WIDTH, room_across);
        offset.x += (width - was.size.width) * given_back;
    } else if grip.moves_left() {
        width = held(was.size.width - moved.x, DIALOG_MIN_WIDTH, room_across);
        // The left edge is the one being dragged, so it moves by the whole of
        // what the surface grew when nothing recentres it, and by half when the
        // layout moves the other edge out as well.
        offset.x -= (width - was.size.width) * (1. - given_back);
    }

    // Vertically the layer does not centre -- it drops the window a fixed way
    // down -- so the top edge is the anchored one and only a drag on the top
    // edge itself has to compensate.
    if grip.moves_top() && grip.moves_bottom() {
        offset.y += moved.y;
    } else if grip.moves_bottom() {
        height = held(was.size.height + moved.y, DIALOG_MIN_HEIGHT, room_down);
    } else if grip.moves_top() {
        height = held(was.size.height - moved.y, DIALOG_MIN_HEIGHT, room_down);
        offset.y += was.size.height - height;
    }

    // Carrying is held to the screen, so a surface cannot be carried where it
    // could not be reached to be carried back. Resizing is not: what a resize
    // is held to is the size floor and the viewport, and working back the
    // untouched place from painted bounds does not hold while the size those
    // bounds were painted at is the thing changing.
    if grip == Grip::Move {
        let natural = was.origin - from.moved_by;
        let left = held(
            natural.x + offset.x,
            KEPT_ON_SCREEN - width,
            viewport.width - KEPT_ON_SCREEN,
        );
        let top = held(
            natural.y + offset.y,
            px(0.),
            viewport.height - KEPT_ON_SCREEN,
        );
        offset.x = left - natural.x;
        offset.y = top - natural.y;
    }

    let placement = Placement {
        moved_by: offset,
        // Carrying a window does not decide its height: a dialog only moved
        // stays as tall as its contents.
        size: if grip == Grip::Move {
            from.size
        } else {
            Some(gpui::size(width, height))
        },
    };

    let dialogs = cx.default_global::<Dialogs>();
    if dialogs.placements.get(of) == Some(&placement) {
        return false;
    }
    dialogs.placements.insert(of.clone(), placement);
    true
}

fn end_drag(of: &Placed, cx: &mut App) -> bool {
    let dialogs = cx.default_global::<Dialogs>();
    if dialogs
        .dragged
        .as_ref()
        .is_some_and(|dragged| &dragged.of == of)
    {
        dialogs.dragged = None;
        return true;
    }
    false
}

/// Records where the window was painted, and listens for the rest of a drag.
///
/// The listeners are the window's rather than the shell's own, because a drag
/// that makes a window bigger is a drag whose pointer is outside the window --
/// and a move heard by the element the drag started on is not heard once the
/// pointer has left it. Registered while drawing, which is how a frame-scoped
/// listener is registered.
fn dialog_drag_watcher(name: crate::SharedString) -> impl gpui::IntoElement {
    gpui::canvas(
        {
            let name = name.clone();
            move |bounds, window: &mut Window, cx: &mut App| {
                cx.default_global::<Dialogs>()
                    .painted
                    .insert(placed(&name, window), bounds);
            }
        },
        move |_bounds, _, window, _cx| {
            let dragged = name.clone();
            window.on_mouse_event(
                move |event: &gpui::MouseMoveEvent, phase, window: &mut Window, cx: &mut App| {
                    if phase != gpui::DispatchPhase::Bubble || !event.dragging() {
                        return;
                    }
                    let this = placed(&dragged, window);
                    if drag_to(&this, event.position, window.viewport_size(), cx) {
                        window.refresh();
                    }
                },
            );
            let released = name;
            window.on_mouse_event(
                move |_: &gpui::MouseUpEvent, phase, window: &mut Window, cx: &mut App| {
                    if phase == gpui::DispatchPhase::Bubble {
                        let this = placed(&released, window);
                        if end_drag(&this, cx) {
                            window.refresh();
                        }
                    }
                },
            );
        },
    )
    .absolute()
    .size_full()
}

/// The strip along the top edge that carries a floating surface.
///
/// It records its own bounds rather than reading the ones the surface recorded,
/// because two surfaces can share one name on purpose -- the hover stack is a
/// diagnostic above a docs popover, carried together -- and the one that
/// painted last would otherwise hand its bounds to the other one's drag. The
/// strip spans the surface, so its own bounds answer everything a carry asks:
/// where the surface starts and how wide it is.
fn carry_strip(name: crate::SharedString, pinned: Pinned) -> impl gpui::IntoElement {
    let mine: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<Pixels>>>> = Default::default();
    gpui::div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(CARRY_STRIP)
        .occlude()
        .cursor_grab()
        .debug_selector(|| "CARRY-STRIP".to_string())
        .child(
            gpui::canvas(
                {
                    let mine = mine.clone();
                    move |bounds, _window, _cx| mine.set(Some(bounds))
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .on_mouse_down(MouseButton::Left, {
            move |event: &gpui::MouseDownEvent, window: &mut Window, cx: &mut App| {
                let this = placed(&name, window);
                let Some(was) = mine.get() else {
                    return;
                };
                if event.click_count >= 2 {
                    forget_placement(&this, cx);
                } else {
                    start_drag(this, pinned, Grip::Move, event.position, was, cx);
                }
                cx.stop_propagation();
                window.refresh();
            }
        })
}

/// The strips along the edges and the squares in the corners that resize the
/// window.
///
/// Drawn after everything the caller puts in the window -- `deferred` -- because
/// a grip a form is painted over is a grip the pointer never reaches. They are
/// inside the surface rather than straddling its edge, so that a press on one
/// is a press on the window: the modal layer reads a press outside the window
/// as "the reader is done with this" and closes it.
fn dialog_grips(name: &crate::SharedString, pinned: Pinned) -> Vec<gpui::AnyElement> {
    let edges: [(Grip, fn(gpui::Div) -> gpui::Div); 8] = [
        (Grip::Top, |grip| {
            grip.top_0().left_0().right_0().h(GRIP).cursor_row_resize()
        }),
        (Grip::Bottom, |grip| {
            grip.bottom_0()
                .left_0()
                .right_0()
                .h(GRIP)
                .cursor_row_resize()
        }),
        (Grip::Left, |grip| {
            grip.left_0().top_0().bottom_0().w(GRIP).cursor_col_resize()
        }),
        (Grip::Right, |grip| {
            grip.right_0()
                .top_0()
                .bottom_0()
                .w(GRIP)
                .cursor_col_resize()
        }),
        (Grip::TopLeft, |grip| {
            grip.top_0().left_0().size(CORNER).cursor_nwse_resize()
        }),
        (Grip::TopRight, |grip| {
            grip.top_0().right_0().size(CORNER).cursor_nesw_resize()
        }),
        (Grip::BottomLeft, |grip| {
            grip.bottom_0().left_0().size(CORNER).cursor_nesw_resize()
        }),
        (Grip::BottomRight, |grip| {
            grip.bottom_0().right_0().size(CORNER).cursor_nwse_resize()
        }),
    ];

    edges
        .into_iter()
        .map(|(grip, place)| {
            let name = name.clone();
            gpui::deferred(
                place(gpui::div().absolute())
                    .occlude()
                    .debug_selector(move || format!("DIALOG-GRIP-{grip:?}"))
                    .on_mouse_down(MouseButton::Left, {
                        move |event: &gpui::MouseDownEvent, window: &mut Window, cx: &mut App| {
                            let this = placed(&name, window);
                            let Some(was) = painted_bounds(&this, cx) else {
                                return;
                            };
                            start_drag(this, pinned, grip, event.position, was, cx);
                            cx.stop_propagation();
                            window.refresh();
                        }
                    }),
            )
            // Above the corners of the window's own contents, and above the
            // edge grips where a corner overlaps one.
            .with_priority(1)
            .into_any_element()
        })
        .collect()
}

/// The row a dialog names itself on: the title at the left, and room after it
/// for whatever the window keeps at the right -- which is the way out.
///
/// The spacer is part of the helper so that the close control lands in the
/// corner without every caller remembering to push it there.
pub fn dialog_header(name: impl Into<crate::SharedString>, cx: &App) -> gpui::Div {
    header_row().child(dialog_title(name, cx)).child(after())
}

/// The naming row with a mark before the name, the way the reference window
/// puts an icon left of its title.
///
/// It is a slot rather than something a caller appends, because a child added
/// to [`dialog_header`] lands after the room that pushes the way out into the
/// corner -- so an icon added that way arrives on the right, where it reads as
/// a control rather than as what the window is about. That is what happened to
/// the connection prompt's mark, and the two flags that chose it went unread
/// until the compiler said so.
pub fn dialog_header_marked(
    mark: impl gpui::IntoElement,
    name: impl Into<crate::SharedString>,
    cx: &App,
) -> gpui::Div {
    header_row()
        .child(mark)
        .child(dialog_title(name, cx))
        .child(after())
}

fn header_row() -> gpui::Div {
    gpui::div()
        .flex()
        .flex_row()
        .flex_none()
        .w_full()
        .px_3()
        .py_2()
        .gap_2()
        .items_center()
        .debug_selector(|| "DIALOG-HEADER".to_string())
}

/// The room between the name and whatever the window keeps at its right end.
fn after() -> gpui::Div {
    gpui::div().flex_1()
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

/// The floor every labelled action in a dialog footer is sized to, so a row of
/// answers reads as one row. Button height is already fixed at
/// [`SEGMENT_HEIGHT`]; width is what varies, and a "OK" half the width of the
/// "Cancel" beside it reads as two kinds of control rather than two answers to
/// the same question. Three times the control height is the width Fluent, the
/// GNOME HIG and Qt all land near.
pub const DIALOG_ACTION_MIN_WIDTH: Pixels = px(84.);

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
                    .child(crate::Button::new("close", "Close").min_width(DIALOG_ACTION_MIN_WIDTH))
                    .child(crate::Button::new("save", "Save").min_width(DIALOG_ACTION_MIN_WIDTH)),
            )
        }
    }

    struct ActionWidthHost;

    impl Render for ActionWidthHost {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div().w(gpui::px(600.0)).h(gpui::px(120.0)).child(
                dialog_footer()
                    .child(crate::Button::new("ok", "OK").min_width(DIALOG_ACTION_MIN_WIDTH))
                    .child(
                        crate::Button::new("cancel", "Cancel").min_width(DIALOG_ACTION_MIN_WIDTH),
                    )
                    .child(
                        crate::Button::new("replace", "Replace every occurrence")
                            .min_width(DIALOG_ACTION_MIN_WIDTH),
                    ),
            )
        }
    }

    // Labels differ in length; the actions they name must not. Measured on the
    // painted boxes, because a row where one answer is half the width of the
    // one beside it reads as two kinds of control rather than one row of
    // answers. The long label is here so the floor cannot be mistaken for a
    // fixed width that truncates.
    #[gpui::test]
    async fn dialog_actions_are_painted_the_same_width(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| ActionWidthHost);
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let short = cx
            .debug_bounds("BUTTON-OK")
            .expect("the short action is painted");
        let medium = cx
            .debug_bounds("BUTTON-Cancel")
            .expect("the second action is painted");
        let long = cx
            .debug_bounds("BUTTON-Replace every occurrence")
            .expect("the long action is painted");

        assert_eq!(
            short.size.width, medium.size.width,
            "two short labels must give two actions of one width, not {:?} beside {:?}",
            short.size.width, medium.size.width
        );
        assert_eq!(
            short.size.width, DIALOG_ACTION_MIN_WIDTH,
            "a short label is widened to the floor, not left at its own {:?}",
            short.size.width
        );
        assert!(
            long.size.width > DIALOG_ACTION_MIN_WIDTH,
            "the floor is a minimum, not a fixed width: a long label was cut to {:?}",
            long.size.width
        );
        assert_eq!(
            short.size.height, long.size.height,
            "actions keep one height as well as one width"
        );
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

    /// The one window every test below draws. Named here because it is also the
    /// key its size and place are remembered under, and a test that resizes it
    /// has to be able to ask about the same window afterwards.
    const TEST_DIALOG: crate::SharedString = crate::SharedString::new_static("Edit Connection");

    struct DialogHost {
        body_is_taller_than_the_window: bool,
    }

    impl Render for DialogHost {
        fn render(
            &mut self,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> impl gpui::IntoElement {
            let tall = if self.body_is_taller_than_the_window {
                gpui::px(2000.0)
            } else {
                gpui::px(80.0)
            };
            let shell = dialog_shell(TEST_DIALOG, window, cx)
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
                        .child(
                            crate::Button::new("cancel", "Cancel")
                                .min_width(DIALOG_ACTION_MIN_WIDTH),
                        )
                        .child(
                            crate::Button::new("save", "Save").min_width(DIALOG_ACTION_MIN_WIDTH),
                        ),
                );

            // Stood where the workspace's modal layer stands a window it
            // opens: dropped `DROPPED_BY` down the workspace, centred across
            // it, in a column of no height of its own. The placement is not
            // incidental to what these tests measure -- a window centred by
            // its parent moves both its edges when it grows, which is the
            // whole reason a drag on one edge has to compensate -- and the
            // shell cannot be the window's own root here, because the root
            // element's offsets are the window's own and are not applied.
            gpui::div().absolute().size_full().child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .h(gpui::px(0.))
                    .top(DROPPED_BY)
                    .items_center()
                    .child(gpui::div().flex().flex_row().child(shell)),
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
        // A known editor size, so what the window opens at is a number these
        // tests can name rather than whatever the test platform defaults to.
        // Wide enough that the shared default is not the floor: a test run in
        // a 1024px editor cannot tell "most of the width" from the fixed 760px
        // box it replaced.
        cx.simulate_resize(gpui::size(gpui::px(1920.), gpui::px(1200.)));
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

        let ceiling = cx.update(|window, _| dialog_default_max_height(window.viewport_size()));
        assert!(
            shell.size.height <= ceiling + gpui::px(0.5),
            "a body of 2000px grew the window to {:?}, past the {:?} it may reach",
            shell.size.height,
            ceiling
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

    struct FloatingHost;

    impl Render for FloatingHost {
        fn render(
            &mut self,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> impl gpui::IntoElement {
            let surface = gpui::div()
                .id("floating-surface")
                .debug_selector(|| "FLOATING-SURFACE".to_string())
                .absolute()
                .w(gpui::px(300.0))
                .child(
                    gpui::div()
                        .debug_selector(|| "FLOATING-FIRST-ROW".to_string())
                        .w_full()
                        .h(gpui::px(24.0))
                        .child("a field at the very top"),
                );
            gpui::div().absolute().size_full().child(floating(
                Floating::named("Test Popup").top_at(gpui::px(40.)),
                surface,
                window,
                cx,
            ))
        }
    }

    fn draw_a_floating_surface(cx: &mut gpui::TestAppContext) -> &mut gpui::VisualTestContext {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| FloatingHost);
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(800.)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        cx
    }

    fn redraw(cx: &mut gpui::VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    fn shell_bounds(cx: &mut gpui::VisualTestContext) -> gpui::Bounds<Pixels> {
        cx.debug_bounds("DIALOG-SHELL")
            .expect("the shell is painted")
    }

    fn drag(cx: &mut gpui::VisualTestContext, from: gpui::Point<Pixels>, to: gpui::Point<Pixels>) {
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        redraw(cx);
        // Two steps rather than one, because a drag the reader makes is many
        // moves and each is answered from where the drag started -- a window
        // that drifted would arrive somewhere else.
        let midway = gpui::point((from.x + to.x) / 2., (from.y + to.y) / 2.);
        cx.simulate_mouse_move(midway, MouseButton::Left, gpui::Modifiers::default());
        redraw(cx);
        cx.simulate_mouse_move(to, MouseButton::Left, gpui::Modifiers::default());
        redraw(cx);
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        redraw(cx);
    }

    // A window opens at most of the editor's width rather than at a fixed box.
    // The complaint this answers: a form with two columns of variables in it
    // opened 760px wide with three quarters of the screen left empty.
    #[gpui::test]
    async fn a_window_opens_at_most_of_the_editors_width(cx: &mut gpui::TestAppContext) {
        let cx = draw_a_dialog(cx, false);

        let viewport = cx.update(|window, _| window.viewport_size());
        let shell = shell_bounds(cx);

        assert!(
            (shell.size.width - dialog_default_width(viewport)).abs() <= gpui::px(1.),
            "a window in a {:?} editor opened {:?} wide, not the {:?} the shared default asks \
             for",
            viewport,
            shell.size.width,
            dialog_default_width(viewport)
        );
        assert!(
            shell.size.width > DIALOG_WIDTH + gpui::px(100.),
            "a window in a {:?} editor opened {:?} wide, which is still the fixed {:?} box it \
             replaced",
            viewport,
            shell.size.width,
            DIALOG_WIDTH
        );
    }

    // The window is carried by its naming row, and arrives where it was taken.
    #[gpui::test]
    async fn the_window_is_carried_by_its_naming_row(cx: &mut gpui::TestAppContext) {
        let cx = draw_a_dialog(cx, false);

        let before = shell_bounds(cx);
        let grab = gpui::point(before.center().x, before.top() + gpui::px(12.));
        let carried = gpui::point(grab.x - gpui::px(90.), grab.y + gpui::px(70.));
        drag(cx, grab, carried);

        let after = shell_bounds(cx);
        assert!(
            (after.left() - (before.left() - gpui::px(90.))).abs() <= gpui::px(1.)
                && (after.top() - (before.top() + gpui::px(70.))).abs() <= gpui::px(1.),
            "the window was carried 90px left and 70px down from {:?} and arrived at {:?}",
            before.origin,
            after.origin
        );
        assert!(
            (after.size.width - before.size.width).abs() <= gpui::px(1.)
                && (after.size.height - before.size.height).abs() <= gpui::px(1.),
            "carrying the window also changed its size, from {:?} to {:?}",
            before.size,
            after.size
        );
    }

    // Dragging an edge resizes the window and holds the opposite edge still.
    // The opposite edge is the half of this that is easy to get wrong: the
    // modal layer centres the window, so a window that grows moves both its
    // edges outward unless the drag compensates for it.
    #[gpui::test]
    async fn dragging_the_right_edge_widens_the_window_and_holds_its_left_edge(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_dialog(cx, false);

        let before = shell_bounds(cx);
        let grab = gpui::point(before.right() - gpui::px(2.), before.center().y);
        drag(cx, grab, gpui::point(grab.x + gpui::px(120.), grab.y));

        let after = shell_bounds(cx);
        assert!(
            (after.size.width - (before.size.width + gpui::px(120.))).abs() <= gpui::px(2.),
            "dragging the right edge 120px out took the width from {:?} to {:?}",
            before.size.width,
            after.size.width
        );
        assert!(
            (after.left() - before.left()).abs() <= gpui::px(2.),
            "the left edge moved from {:?} to {:?} while the right edge was being dragged",
            before.left(),
            after.left()
        );
    }

    #[gpui::test]
    async fn dragging_the_left_edge_widens_the_window_and_holds_its_right_edge(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_dialog(cx, false);

        let before = shell_bounds(cx);
        let grab = gpui::point(before.left() + gpui::px(2.), before.center().y);
        drag(cx, grab, gpui::point(grab.x - gpui::px(100.), grab.y));

        let after = shell_bounds(cx);
        assert!(
            (after.size.width - (before.size.width + gpui::px(100.))).abs() <= gpui::px(2.),
            "dragging the left edge 100px out took the width from {:?} to {:?}",
            before.size.width,
            after.size.width
        );
        assert!(
            (after.right() - before.right()).abs() <= gpui::px(2.),
            "the right edge moved from {:?} to {:?} while the left edge was being dragged",
            before.right(),
            after.right()
        );
    }

    // A window made taller keeps its footer inside it: the height the drag
    // asks for is the height of the whole window, not of its middle.
    #[gpui::test]
    async fn dragging_the_bottom_edge_makes_the_window_taller_and_keeps_the_actions_inside(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_dialog(cx, true);

        let before = shell_bounds(cx);
        let grab = gpui::point(before.center().x, before.bottom() - gpui::px(2.));
        drag(cx, grab, gpui::point(grab.x, grab.y + gpui::px(150.)));

        let after = shell_bounds(cx);
        let save = cx
            .debug_bounds("BUTTON-Save")
            .expect("the confirming action is painted");
        assert!(
            (after.size.height - (before.size.height + gpui::px(150.))).abs() <= gpui::px(2.),
            "dragging the bottom edge 150px down took the height from {:?} to {:?}, which is \
             not the height the drag asked for",
            before.size.height,
            after.size.height
        );
        assert!(
            save.bottom() <= after.bottom() + gpui::px(0.5),
            "the confirming action ends at {:?}, below a window ending at {:?}",
            save.bottom(),
            after.bottom()
        );
    }

    // The floor a resize is held to. Without it a window can be dragged down to
    // nothing, and a window with no naming row left cannot be grabbed to be
    // dragged back.
    #[gpui::test]
    async fn a_window_cannot_be_dragged_narrower_than_the_floor(cx: &mut gpui::TestAppContext) {
        let cx = draw_a_dialog(cx, false);

        let before = shell_bounds(cx);
        let grab = gpui::point(before.right() - gpui::px(2.), before.center().y);
        drag(
            cx,
            grab,
            gpui::point(before.left() - gpui::px(200.), grab.y),
        );

        let after = shell_bounds(cx);
        assert!(
            (after.size.width - DIALOG_MIN_WIDTH).abs() <= gpui::px(2.),
            "dragged past its own left edge, the window came to {:?} rather than stopping at \
             the {:?} floor",
            after.size.width,
            DIALOG_MIN_WIDTH
        );
    }

    // The way back: a window whose size has been dragged into a corner is
    // given its own size back by double-clicking the row it is carried by.
    #[gpui::test]
    async fn double_clicking_the_naming_row_gives_the_window_its_size_back(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_dialog(cx, false);

        let before = shell_bounds(cx);
        let grab = gpui::point(before.right() - gpui::px(2.), before.center().y);
        drag(cx, grab, gpui::point(grab.x - gpui::px(250.), grab.y));
        let narrowed = shell_bounds(cx);
        assert!(
            narrowed.size.width < before.size.width - gpui::px(200.),
            "the window was not narrowed first, so this test proves nothing: {:?} to {:?}",
            before.size.width,
            narrowed.size.width
        );

        let row = gpui::point(narrowed.center().x, narrowed.top() + gpui::px(12.));
        cx.simulate_event(gpui::MouseDownEvent {
            position: row,
            modifiers: gpui::Modifiers::default(),
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        redraw(cx);
        cx.simulate_mouse_up(row, MouseButton::Left, gpui::Modifiers::default());
        redraw(cx);

        let after = shell_bounds(cx);
        assert!(
            (after.size.width - before.size.width).abs() <= gpui::px(1.),
            "the window came back {:?} wide rather than the {:?} it opened at",
            after.size.width,
            before.size.width
        );
    }

    // A size chosen in a large editor is not kept when the editor becomes
    // small: the window has to fit the editor it is in, or its footer -- and
    // the action the dialog is waiting for -- is outside the window.
    #[gpui::test]
    async fn a_remembered_size_does_not_outgrow_a_shrunken_editor(cx: &mut gpui::TestAppContext) {
        let cx = draw_a_dialog(cx, false);

        let before = shell_bounds(cx);
        let grab = gpui::point(before.right() - gpui::px(2.), before.center().y);
        drag(cx, grab, gpui::point(grab.x + gpui::px(300.), grab.y));
        let widened = shell_bounds(cx);
        assert!(
            widened.size.width > before.size.width + gpui::px(200.),
            "the window was not widened first, so this test proves nothing: {:?} to {:?}",
            before.size.width,
            widened.size.width
        );

        cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(700.)));
        redraw(cx);

        let after = shell_bounds(cx);
        assert!(
            after.size.width <= gpui::px(900.) + gpui::px(1.),
            "the window kept the {:?} it was given in a wider editor, in an editor now 900px \
             wide",
            after.size.width
        );
    }

    // Two editor windows each have their own copy of a window of the same name
    // -- two commit windows are two windows -- and placing one does not place
    // the other.
    #[gpui::test]
    async fn two_editor_windows_place_their_own_copy_of_a_window(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let first = cx.add_window(|_window, _cx| DialogHost {
            body_is_taller_than_the_window: false,
        });
        let second = cx.add_window(|_window, _cx| DialogHost {
            body_is_taller_than_the_window: false,
        });

        let before = {
            let mut one = gpui::VisualTestContext::from_window(first.into(), cx);
            one.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(1000.)));
            redraw(&mut one);
            let before = shell_bounds(&mut one);
            let grab = gpui::point(before.center().x, before.top() + gpui::px(12.));
            drag(
                &mut one,
                grab,
                gpui::point(grab.x - gpui::px(200.), grab.y + gpui::px(100.)),
            );
            let moved = shell_bounds(&mut one);
            assert!(
                (moved.left() - (before.left() - gpui::px(200.))).abs() <= gpui::px(1.),
                "the first window was not carried, so this test proves nothing: {:?} to {:?}",
                before.origin,
                moved.origin
            );
            before
        };

        let mut other = gpui::VisualTestContext::from_window(second.into(), cx);
        other.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(1000.)));
        redraw(&mut other);
        let untouched = shell_bounds(&mut other);
        assert!(
            (untouched.left() - before.left()).abs() <= gpui::px(1.)
                && (untouched.top() - before.top()).abs() <= gpui::px(1.),
            "carrying the window in one editor also carried the one in the other: {:?} against \
             the {:?} it opened at",
            untouched.origin,
            before.origin
        );
    }

    // A resize is held to the screen the same way carrying is, and the case
    // that shows it is a surface already carried down the pane: the edge being
    // dragged is the one that can leave the screen, and the room it has is
    // measured from the edge that is staying put -- not from the whole editor,
    // which is what a surface still at the top of the pane cannot tell apart.
    #[gpui::test]
    async fn a_resize_cannot_push_a_carried_surface_past_the_last_row(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_floating_surface(cx);

        let viewport = cx.update(|window, _| window.viewport_size());
        let strip = cx
            .debug_bounds("CARRY-STRIP")
            .expect("the strip is painted");
        let grab = strip.center();
        drag(cx, grab, gpui::point(grab.x, grab.y + gpui::px(360.)));

        let carried = cx
            .debug_bounds("FLOATING-SURFACE")
            .expect("the surface is painted");
        assert!(
            carried.top() > gpui::px(300.),
            "the surface was not carried down first, so this test proves nothing: it is at {:?}",
            carried.origin
        );

        let edge = gpui::point(carried.center().x, carried.bottom() - gpui::px(2.));
        drag(cx, edge, gpui::point(edge.x, edge.y + viewport.height * 2.));

        let after = cx
            .debug_bounds("FLOATING-SURFACE")
            .expect("the surface is still painted");
        assert!(
            after.bottom() <= viewport.height + gpui::px(2.),
            "dragged far past the last row, a surface whose top is at {:?} ends at {:?} in an \
             editor {:?} tall",
            carried.top(),
            after.bottom(),
            viewport.height
        );
    }

    // The strip that carries a floating surface covers a margin of the
    // surface's own, not the first row of what is in it: a press meant for the
    // field at the top of a popup must reach the field.
    #[gpui::test]
    async fn the_carry_strip_does_not_cover_the_first_row_of_content(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_floating_surface(cx);

        let strip = cx
            .debug_bounds("CARRY-STRIP")
            .expect("the strip is painted");
        let first = cx
            .debug_bounds("FLOATING-FIRST-ROW")
            .expect("the first row of content is painted");

        assert!(
            first.top() >= strip.bottom() - gpui::px(0.5),
            "the strip covers {:?}..{:?} and the first row starts at {:?}, under it",
            strip.top(),
            strip.bottom(),
            first.top()
        );
    }

    // An editor window smaller than a dialog's own floor is a shape the reader
    // is free to drag it to, and every bound in the resize has to survive it:
    // the floor above the ceiling is what `clamp` panics on, which is a crash
    // rather than a window of an awkward size.
    #[gpui::test]
    async fn an_editor_smaller_than_the_floor_does_not_bring_the_drag_down(
        cx: &mut gpui::TestAppContext,
    ) {
        let cx = draw_a_dialog(cx, false);

        cx.simulate_resize(gpui::size(gpui::px(320.), gpui::px(140.)));
        redraw(cx);

        // The top edge, because it is the only one of the four still inside an
        // editor this small: a window held to a 360px floor in a 320px editor
        // hangs off both sides, and its bottom is below the last row.
        let before = shell_bounds(cx);
        let grab = gpui::point(before.center().x, before.top() + gpui::px(2.));
        drag(cx, grab, gpui::point(grab.x, grab.y - gpui::px(400.)));

        let after = shell_bounds(cx);
        assert!(
            after.size.height >= DIALOG_MIN_HEIGHT - gpui::px(1.),
            "in a 140px editor the window came to {:?} tall, under the {:?} floor",
            after.size.height,
            DIALOG_MIN_HEIGHT
        );
    }

    // A window carried at the edge of the editor keeps enough of itself on
    // screen to be grabbed and carried back.
    #[gpui::test]
    async fn a_window_cannot_be_carried_off_the_screen(cx: &mut gpui::TestAppContext) {
        let cx = draw_a_dialog(cx, false);

        let viewport = cx.update(|window, _| window.viewport_size());
        let before = shell_bounds(cx);
        let grab = gpui::point(before.center().x, before.top() + gpui::px(12.));
        drag(
            cx,
            grab,
            gpui::point(grab.x + viewport.width * 3., grab.y + viewport.height * 3.),
        );

        let after = shell_bounds(cx);
        assert!(
            after.left() < viewport.width - KEPT_ON_SCREEN + gpui::px(2.)
                && after.top() < viewport.height - KEPT_ON_SCREEN + gpui::px(2.),
            "carried far past the corner, the window came to rest at {:?} in a {:?} editor, \
             where there is nothing left of it to grab",
            after.origin,
            viewport
        );
    }
}
