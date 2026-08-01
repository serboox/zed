use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{Bounds, Pixels, Point, point, px, size};
use ui::ScrollableHandle;

/// Where the page stands, as the page itself reports it, in its own CSS pixels.
#[derive(Clone, Copy, Debug, Default)]
pub struct PageScroll {
    /// How far down the page has been scrolled.
    pub down: f32,
    /// How tall the whole document is.
    pub document: f32,
    /// How much of it is on screen.
    pub view: f32,
}

/// The scrollbar's side of a page the editor does not scroll itself.
///
/// A live page scrolls inside the engine, so there is no gpui scroll container
/// to hang a scrollbar on -- putting one there scrolled everything twice. This
/// stands in for it: the page says where it stands, and a drag of the thumb asks
/// the page to go somewhere, which the preview passes on when it next turns the
/// engine over.
/// How long after a drag the page's own word on where it stands is still
/// disbelieved. An answer asked for before the drag arrives after it, and taking
/// it would drag the thumb back to where the page was rather than where the
/// reader has put it.
const UNTIL_THE_PAGE_CATCHES_UP: Duration = Duration::from_millis(400);

#[derive(Clone, Default)]
pub struct PageScrollHandle {
    at: Rc<RefCell<PageScroll>>,
    /// Where a drag of the thumb has asked the page to go, until it is passed on.
    asked_for: Rc<Cell<Option<f32>>>,
    /// Whether the thumb is being dragged now.
    dragging: Rc<Cell<bool>>,
    /// Until when the page's word on where it stands is not to be taken.
    catching_up: Rc<Cell<Option<Instant>>>,
}

impl PageScrollHandle {
    /// Records what the page last said about itself. How tall it is and how much
    /// shows are always taken; where it stands is not, while the reader is
    /// dragging the thumb or the page has yet to catch up with a drag.
    pub fn stands_at(&self, said: PageScroll) {
        let mut at = self.at.borrow_mut();
        at.document = said.document;
        at.view = said.view;
        let leading = self.dragging.get()
            || self
                .catching_up
                .get()
                .is_some_and(|until| Instant::now() < until);
        if !leading {
            at.down = said.down;
        }
    }

    /// Where a drag of the thumb wants the page, if anywhere. Taken, because it
    /// is only worth asking the page once.
    pub fn take_request(&self) -> Option<f32> {
        self.asked_for.take()
    }

    /// Whether the page has anything to scroll at all.
    pub fn scrollable(&self) -> bool {
        let at = self.at.borrow();
        at.document > at.view + 1.
    }
}

impl ScrollableHandle for PageScrollHandle {
    fn max_offset(&self) -> Point<Pixels> {
        let at = self.at.borrow();
        point(Pixels::ZERO, px((at.document - at.view).max(0.)))
    }

    /// gpui counts a scrolled-down view as a negative offset, the way a scrolled
    /// container's contents sit above their box.
    fn offset(&self) -> Point<Pixels> {
        point(Pixels::ZERO, px(-self.at.borrow().down))
    }

    fn set_offset(&self, point: Point<Pixels>) {
        let mut at = self.at.borrow_mut();
        let furthest = (at.document - at.view).max(0.);
        let down = (-f32::from(point.y)).clamp(0., furthest);
        // The thumb follows the hand at once. Waiting for the page to be asked,
        // to answer, and to be asked again would let go of the thumb under the
        // pointer and drag it back to where the page was.
        at.down = down;
        self.asked_for.set(Some(down));
        self.catching_up
            .set(Some(Instant::now() + UNTIL_THE_PAGE_CATCHES_UP));
    }

    fn drag_started(&self) {
        self.dragging.set(true);
    }

    fn drag_ended(&self) {
        self.dragging.set(false);
        self.catching_up
            .set(Some(Instant::now() + UNTIL_THE_PAGE_CATCHES_UP));
    }

    fn viewport(&self) -> Bounds<Pixels> {
        let at = self.at.borrow();
        Bounds::new(point(px(0.), px(0.)), size(Pixels::ZERO, px(at.view)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standing_at(down: f32, document: f32, view: f32) -> PageScrollHandle {
        let handle = PageScrollHandle::default();
        handle.stands_at(PageScroll {
            down,
            document,
            view,
        });
        handle
    }

    #[test]
    fn the_thumb_shows_where_the_page_stands() {
        let handle = standing_at(300., 2000., 800.);
        assert_eq!(handle.max_offset().y, px(1200.));
        // Scrolled down is a negative offset, as it is for anything gpui scrolls
        // itself; a thumb driven the other way runs backwards.
        assert_eq!(handle.offset().y, px(-300.));
        assert_eq!(handle.viewport().size.height, px(800.));
        assert!(handle.scrollable());
    }

    #[test]
    fn a_page_that_fits_has_nothing_to_scroll() {
        let handle = standing_at(0., 500., 800.);
        assert_eq!(handle.max_offset().y, px(0.));
        assert!(!handle.scrollable());
    }

    #[test]
    fn a_drag_asks_the_page_to_go_where_the_thumb_went() {
        let handle = standing_at(0., 2000., 800.);
        handle.set_offset(point(px(0.), px(-450.)));
        assert_eq!(handle.take_request(), Some(450.));
        // Asked once: the page is only worth telling the same thing once.
        assert_eq!(handle.take_request(), None);
    }

    #[test]
    fn a_drag_past_either_end_stays_on_the_page() {
        let handle = standing_at(0., 2000., 800.);
        handle.set_offset(point(px(0.), px(-9000.)));
        assert_eq!(handle.take_request(), Some(1200.));
        handle.set_offset(point(px(0.), px(600.)));
        assert_eq!(handle.take_request(), Some(0.));
    }
}
