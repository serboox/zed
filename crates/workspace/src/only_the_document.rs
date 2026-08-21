use gpui::{App, Context, Global, Pixels, Subscription, px};

/// How near the top edge the pointer has to come for the chrome to return.
///
/// A few pixels, as a browser in full screen does it: near enough that reaching
/// for the tabs finds them, far enough that reading the first line of a page does
/// not keep bringing them back.
pub const NEAR_THE_TOP: Pixels = px(4.);

/// How tall the chrome is taken to be once it is showing, for deciding when the
/// pointer has left it again. The tab bar and a row of controls come to about
/// this; a few pixels either way only change where the chrome hides itself, and
/// the pointer is on it or on the page well before that.
pub const THE_CHROME_IS_THIS_TALL: Pixels = px(90.);

/// Whether the window is showing nothing but the document: no tabs, no panels, no
/// status bar -- the way a browser looks in full screen.
///
/// Held here rather than in the workspace because everything that has to hide
/// reads it while it draws, and several of those -- a pane's tab bar, a dock --
/// are drawn from places that have no handle on the workspace.
#[derive(Default)]
struct GlobalOnlyTheDocument {
    /// Whether the mode is on at all.
    on: bool,
    /// Whether the pointer has come near the top edge, so the chrome is showing
    /// over the document for as long as it stays there.
    chrome_showing: bool,
}

impl Global for GlobalOnlyTheDocument {}

/// Whether the window is showing nothing but the document.
pub fn only_the_document(cx: &App) -> bool {
    cx.try_global::<GlobalOnlyTheDocument>()
        .is_some_and(|state| state.on)
}

/// Whether what is normally around the document is showing anyway, because the
/// pointer has come to the top edge to fetch it.
pub fn chrome_is_showing(cx: &App) -> bool {
    cx.try_global::<GlobalOnlyTheDocument>()
        .is_some_and(|state| state.chrome_showing)
}

/// Whether a thing that lives around the document should be drawn at all: always,
/// unless the window is showing only the document and the pointer is elsewhere.
pub fn draw_what_surrounds_the_document(cx: &App) -> bool {
    !only_the_document(cx) || chrome_is_showing(cx)
}

pub fn set_only_the_document(on: bool, cx: &mut App) {
    {
        let state = cx.default_global::<GlobalOnlyTheDocument>();
        if state.on == on {
            return;
        }
        state.on = on;
        // Coming back out, the chrome is where it always was rather than
        // hovering over the page: whatever the pointer was doing is forgotten.
        state.chrome_showing = false;
    }
    // Everything that hides reads this while it draws, and nothing observes it,
    // so nothing would redraw itself.
    cx.refresh_windows();
}

/// Says where the pointer is, in the window's own coordinates, so the chrome can
/// come back when it reaches the top edge and go again when it leaves.
///
/// Does nothing unless the window is showing only the document, and redraws only
/// when the answer changes -- this is called for every mouse move there is.
pub fn the_pointer_is_at(y: Pixels, cx: &mut App) {
    let (on, showing) = match cx.try_global::<GlobalOnlyTheDocument>() {
        Some(state) => (state.on, state.chrome_showing),
        None => return,
    };
    if !on {
        return;
    }
    // Hysteresis, and it is the whole trick: coming back needs the very edge, and
    // leaving needs the pointer to be past the chrome. One threshold for both
    // would make the chrome flicker along the line where it appears, since
    // showing it puts something under the pointer that was not there before.
    let now_showing = match showing {
        false => y <= NEAR_THE_TOP,
        true => y <= THE_CHROME_IS_THIS_TALL,
    };
    if now_showing == showing {
        return;
    }
    cx.default_global::<GlobalOnlyTheDocument>().chrome_showing = now_showing;
    cx.refresh_windows();
}

/// Notifies a view whenever the mode or the chrome changes.
pub fn observe_only_the_document<T: 'static>(cx: &mut Context<T>) -> Subscription {
    cx.observe_global::<GlobalOnlyTheDocument>(|_, cx| cx.notify())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// The chrome comes back at the very edge and stays until the pointer is
    /// past it. Both from one threshold and it would flicker: showing the chrome
    /// puts something under the pointer that was not there a moment ago.
    #[gpui::test]
    fn the_chrome_comes_back_at_the_edge_and_leaves_past_itself(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(
                !only_the_document(cx),
                "the window shows everything to begin with"
            );
            assert!(draw_what_surrounds_the_document(cx));

            set_only_the_document(true, cx);
            assert!(only_the_document(cx));
            assert!(!draw_what_surrounds_the_document(cx), "and now it does not");

            // Halfway down the window is reading, not reaching for the tabs.
            the_pointer_is_at(px(400.), cx);
            assert!(!chrome_is_showing(cx));

            // The very edge fetches it.
            the_pointer_is_at(px(2.), cx);
            assert!(chrome_is_showing(cx));
            assert!(draw_what_surrounds_the_document(cx));

            // And it stays while the pointer is on it, which is further down
            // than the edge that fetched it.
            the_pointer_is_at(px(40.), cx);
            assert!(chrome_is_showing(cx));

            // Past it, and it goes.
            the_pointer_is_at(px(200.), cx);
            assert!(!chrome_is_showing(cx));

            // Leaving the mode leaves the chrome where it always was.
            the_pointer_is_at(px(2.), cx);
            assert!(chrome_is_showing(cx));
            set_only_the_document(false, cx);
            assert!(!chrome_is_showing(cx));
            assert!(draw_what_surrounds_the_document(cx));
        });
    }
}
