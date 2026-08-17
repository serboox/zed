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

/// The same after a turn of the wheel. Far shorter than after a drag: a wheel
/// sends the page off gliding, and the only answers worth disbelieving are the
/// ones already on their way when the wheel turned. Past that the page's own
/// word is what keeps the bar with it.
const UNTIL_THE_WHEEL_LANDS: Duration = Duration::from_millis(120);

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

    /// Moves the bar by what the wheel just did, without waiting to be told.
    ///
    /// The page is asked where it stands ten times a second, which is often
    /// enough for a thumb being dragged and far too seldom for a page gliding
    /// under a wheel: the bar arrives in steps while the page moves smoothly.
    /// Here the same distance the wheel sent to the page is applied at once, and
    /// the page's own answer corrects it a moment later.
    pub fn wheeled_by(&self, down_by: f32) {
        let mut at = self.at.borrow_mut();
        let furthest = (at.document - at.view).max(0.);
        at.down = (at.down + down_by).clamp(0., furthest);
        drop(at);
        self.catching_up
            .set(Some(Instant::now() + UNTIL_THE_WHEEL_LANDS));
    }

    /// Whether the page is on the move, and so worth asking where it stands on
    /// every turn of the engine rather than ten times a second. A bar told ten
    /// times a second where a gliding page is arrives in steps behind it.
    pub fn moving(&self) -> bool {
        self.dragging.get()
            || self
                .catching_up
                .get()
                .is_some_and(|until| Instant::now() < until)
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
    fn a_wheel_moves_the_bar_before_the_page_answers() {
        let handle = standing_at(0., 3000., 1000.);
        handle.wheeled_by(250.);

        assert_eq!(
            handle.offset().y,
            px(-250.),
            "the bar waits for nobody: the page is asked ten times a second and \
             the wheel turns far more often than that"
        );
        assert!(
            handle.moving(),
            "a page under the wheel is worth asking about on every turn"
        );

        // An answer already on its way when the wheel turned says where the page
        // was, not where it is going.
        handle.stands_at(PageScroll {
            down: 0.,
            document: 3000.,
            view: 1000.,
        });
        assert_eq!(
            handle.offset().y,
            px(-250.),
            "a stale answer dragged the bar back to where the page had been"
        );

        std::thread::sleep(UNTIL_THE_WHEEL_LANDS + Duration::from_millis(30));
        handle.stands_at(PageScroll {
            down: 400.,
            document: 3000.,
            view: 1000.,
        });
        assert_eq!(
            handle.offset().y,
            px(-400.),
            "once the wheel has landed the page's own word is what the bar shows"
        );
        assert!(
            !handle.moving(),
            "a page that has come to rest is asked about at the slower pace again"
        );
    }

    #[test]
    fn a_wheel_stops_at_both_ends_of_the_page() {
        let handle = standing_at(0., 3000., 1000.);
        handle.wheeled_by(-500.);
        assert_eq!(
            handle.offset().y,
            px(0.),
            "the page does not go above itself"
        );

        handle.wheeled_by(9000.);
        assert_eq!(
            handle.offset().y,
            px(-2000.),
            "nor past its end: 3000 tall with 1000 showing leaves 2000 to scroll"
        );
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
