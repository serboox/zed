use gpui::{App, Context, Global, Pixels, Point, Size, Subscription, px};

/// How near an edge the pointer has to come for that edge's panels to return.
///
/// A few pixels, as a browser in full screen does it: near enough that reaching
/// for the tabs finds them, far enough that reading the first line of a page does
/// not keep bringing them back.
pub const NEAR_AN_EDGE: Pixels = px(4.);

/// How far the pointer may wander before what came back goes again. Measured
/// from the edge it came from, and larger than the edge itself so that a panel
/// does not go while the pointer is still on it -- one distance for both would
/// make it flicker along the line where it appears, since showing it puts
/// something under the pointer that was not there a moment ago.
const THE_TABS_REACH: Pixels = px(90.);
const A_SIDE_PANEL_REACHES: Pixels = px(340.);
const THE_BOTTOM_REACHES: Pixels = px(260.);

/// How long a panel takes to come back, and to go. Fast enough not to be waited
/// for, slow enough to be seen as a movement rather than a jump.
pub const HOW_LONG_IT_TAKES: std::time::Duration = std::time::Duration::from_millis(140);

/// Which edges are showing what normally sits at them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges {
    pub top: bool,
    pub left: bool,
    pub right: bool,
    pub bottom: bool,
}

/// Whether the window is showing nothing but the document: no tabs, no panels,
/// no status bar -- the way a browser looks in full screen.
///
/// Held here rather than in the workspace because everything that has to hide
/// reads it while it draws, and several of those -- a pane's tab bar, a dock --
/// are drawn from places that have no handle on the workspace.
#[derive(Default)]
struct GlobalOnlyTheDocument {
    on: bool,
    showing: Edges,
}

impl Global for GlobalOnlyTheDocument {}

/// Whether the window is showing nothing but the document.
pub fn only_the_document(cx: &App) -> bool {
    cx.try_global::<GlobalOnlyTheDocument>()
        .is_some_and(|state| state.on)
}

/// Which edges have been fetched back by the pointer.
pub fn showing(cx: &App) -> Edges {
    cx.try_global::<GlobalOnlyTheDocument>()
        .map(|state| state.showing)
        .unwrap_or_default()
}

/// Whether the tabs and the title bar should be drawn: always, unless the window
/// is showing only the document and the pointer is away from the top edge.
pub fn draw_the_top(cx: &App) -> bool {
    !only_the_document(cx) || showing(cx).top
}

pub fn draw_the_left(cx: &App) -> bool {
    !only_the_document(cx) || showing(cx).left
}

pub fn draw_the_right(cx: &App) -> bool {
    !only_the_document(cx) || showing(cx).right
}

pub fn draw_the_bottom(cx: &App) -> bool {
    !only_the_document(cx) || showing(cx).bottom
}

/// Whether what is around the document is being drawn because the pointer
/// fetched it, rather than because it is always there. What comes back that way
/// comes back moving.
pub fn it_was_fetched_back(cx: &App) -> bool {
    only_the_document(cx)
}

pub fn set_only_the_document(on: bool, cx: &mut App) {
    {
        let state = cx.default_global::<GlobalOnlyTheDocument>();
        if state.on == on {
            return;
        }
        state.on = on;
        // Coming back out, everything is where it always was rather than hovering
        // over the page: whatever the pointer was doing is forgotten.
        state.showing = Edges::default();
    }
    // Everything that hides reads this while it draws, and nothing observes it,
    // so nothing would redraw itself.
    cx.refresh_windows();
}

/// Says where the pointer is, in the window's own coordinates, so each edge can
/// fetch back what belongs to it and let it go again.
///
/// Does nothing unless the window is showing only the document, and redraws only
/// when the answer changes -- this is called for every mouse move there is.
pub fn the_pointer_is_at(at: Point<Pixels>, window: Size<Pixels>, cx: &mut App) {
    let (on, showing) = match cx.try_global::<GlobalOnlyTheDocument>() {
        Some(state) => (state.on, state.showing),
        None => return,
    };
    if !on {
        return;
    }
    let now = Edges {
        top: keep_or_fetch(showing.top, at.y, NEAR_AN_EDGE, THE_TABS_REACH),
        left: keep_or_fetch(showing.left, at.x, NEAR_AN_EDGE, A_SIDE_PANEL_REACHES),
        right: keep_or_fetch(
            showing.right,
            window.width - at.x,
            NEAR_AN_EDGE,
            A_SIDE_PANEL_REACHES,
        ),
        bottom: keep_or_fetch(
            showing.bottom,
            window.height - at.y,
            NEAR_AN_EDGE,
            THE_BOTTOM_REACHES,
        ),
    };
    if now == showing {
        return;
    }
    cx.default_global::<GlobalOnlyTheDocument>().showing = now;
    cx.refresh_windows();
}

/// Whether an edge shows what belongs to it: fetched at `near`, kept until the
/// pointer is further than `reach`.
fn keep_or_fetch(showing: bool, from_the_edge: Pixels, near: Pixels, reach: Pixels) -> bool {
    match showing {
        false => from_the_edge <= near,
        true => from_the_edge <= reach,
    }
}

/// Notifies a view whenever the mode or what is showing changes.
pub fn observe_only_the_document<T: 'static>(cx: &mut Context<T>) -> Subscription {
    cx.observe_global::<GlobalOnlyTheDocument>(|_, cx| cx.notify())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, point, size};

    fn a_window() -> Size<Pixels> {
        size(px(1400.), px(900.))
    }

    /// Each edge answers for itself: the top brings the tabs, the left brings the
    /// left panel, and neither brings the other.
    #[gpui::test]
    fn each_edge_fetches_back_only_what_belongs_to_it(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(!only_the_document(cx));
            assert!(draw_the_top(cx) && draw_the_left(cx) && draw_the_bottom(cx));

            set_only_the_document(true, cx);
            assert_eq!(showing(cx), Edges::default(), "nothing is showing at first");
            assert!(!draw_the_top(cx) && !draw_the_left(cx));

            // The top edge brings the tabs, and nothing else.
            the_pointer_is_at(point(px(700.), px(2.)), a_window(), cx);
            assert!(draw_the_top(cx), "the tabs came back");
            assert!(!draw_the_left(cx), "and the left panel did not");
            assert!(!draw_the_bottom(cx));

            // Reading in the middle of the page puts everything away.
            the_pointer_is_at(point(px(700.), px(450.)), a_window(), cx);
            assert_eq!(showing(cx), Edges::default());

            // The left edge brings the left panel alone.
            the_pointer_is_at(point(px(1.), px(450.)), a_window(), cx);
            assert!(draw_the_left(cx));
            assert!(!draw_the_top(cx) && !draw_the_right(cx));

            // The right edge, the right panel.
            the_pointer_is_at(point(px(1399.), px(450.)), a_window(), cx);
            assert!(draw_the_right(cx));
            assert!(!draw_the_left(cx));

            // The bottom edge, what sits along the bottom.
            the_pointer_is_at(point(px(700.), px(899.)), a_window(), cx);
            assert!(draw_the_bottom(cx));
            assert!(!draw_the_top(cx));
        });
    }

    /// What came back stays while the pointer is on it, which is further from the
    /// edge than the distance that fetched it. One distance for both and it would
    /// flicker along the line where it appears.
    #[gpui::test]
    fn what_came_back_stays_while_the_pointer_is_on_it(cx: &mut TestAppContext) {
        cx.update(|cx| {
            set_only_the_document(true, cx);
            the_pointer_is_at(point(px(700.), px(2.)), a_window(), cx);
            assert!(draw_the_top(cx));

            // Down onto the tabs themselves: further than the edge that fetched
            // them, and they stay.
            the_pointer_is_at(point(px(700.), px(40.)), a_window(), cx);
            assert!(draw_the_top(cx));

            // Past them, and they go.
            the_pointer_is_at(point(px(700.), px(300.)), a_window(), cx);
            assert!(!draw_the_top(cx));

            // Leaving the mode puts everything back where it always was.
            the_pointer_is_at(point(px(700.), px(2.)), a_window(), cx);
            assert!(showing(cx).top);
            set_only_the_document(false, cx);
            assert_eq!(showing(cx), Edges::default());
            assert!(draw_the_top(cx) && draw_the_left(cx));
        });
    }
}
