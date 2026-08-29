use std::cmp::Ordering;

use gpui::{
    AnyElement, Bounds, Hsla, IntoElement, PathBuilder, Size, Stateful, canvas, point, size,
};
use smallvec::SmallVec;

use crate::prelude::*;

const START_TAB_SLOT_SIZE: Pixels = px(12.);
const END_TAB_SLOT_SIZE: Pixels = px(14.);

/// The position of a [`Tab`] within a list of tabs.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TabPosition {
    /// The tab is first in the list.
    First,

    /// The tab is in the middle of the list (i.e., it is not the first or last tab).
    ///
    /// The [`Ordering`] is where this tab is positioned with respect to the selected tab.
    Middle(Ordering),

    /// The tab is last in the list.
    Last,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TabCloseSide {
    Start,
    End,
}

#[derive(IntoElement, RegisterComponent)]
pub struct Tab {
    id: ElementId,
    div: Stateful<Div>,
    selected: bool,
    position: TabPosition,
    close_side: TabCloseSide,
    start_slot: Option<AnyElement>,
    end_slot: Option<AnyElement>,
    bg_tint: Option<gpui::Hsla>,
    children: SmallVec<[AnyElement; 2]>,
}

impl Tab {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        Self {
            id: id.clone(),
            div: div()
                .id(id.clone())
                .debug_selector(|| format!("TAB-{}", id)),
            selected: false,
            position: TabPosition::First,
            close_side: TabCloseSide::End,
            start_slot: None,
            end_slot: None,
            bg_tint: None,
            children: SmallVec::new(),
        }
    }

    pub fn position(mut self, position: TabPosition) -> Self {
        self.position = position;
        self
    }

    pub fn close_side(mut self, close_side: TabCloseSide) -> Self {
        self.close_side = close_side;
        self
    }

    pub fn start_slot<E: IntoElement>(mut self, element: impl Into<Option<E>>) -> Self {
        self.start_slot = element.into().map(IntoElement::into_any_element);
        self
    }

    pub fn end_slot<E: IntoElement>(mut self, element: impl Into<Option<E>>) -> Self {
        self.end_slot = element.into().map(IntoElement::into_any_element);
        self
    }

    /// Blends a tint over the tab's background. Used to flag the tab's item
    /// (for example a production database console) with its env color so the
    /// tab matches the tinted editor body.
    pub fn bg_tint(mut self, tint: Option<gpui::Hsla>) -> Self {
        self.bg_tint = tint;
        self
    }

    pub fn content_height(cx: &App) -> Pixels {
        DynamicSpacing::Base32.px(cx) - px(1.)
    }

    /// Height of the strip a row of tabs sits in. A tab is shorter than this by
    /// [`Tab::shoulder`], the gutter above it; below it there is no gutter at
    /// all, because the selected tab has to reach the strip's bottom edge.
    pub fn container_height(cx: &App) -> Pixels {
        DynamicSpacing::Base32.px(cx) + px(8.)
    }

    /// The gutter above a tab. Above it there is strip to see; below it there is
    /// none, because the selected tab has to reach the strip's bottom edge.
    pub fn shoulder() -> Pixels {
        px(6.)
    }

    /// The widest a tab is allowed to be, whatever its name. A browser settles on
    /// the same number, and for the same reason: a strip where one tab takes a
    /// third of the window is a strip you cannot read, and the name is no more
    /// legible for being whole -- what tells two files apart is the start and the
    /// end of the name, which is what the middle ellipsis keeps.
    ///
    /// This is also what turns that ellipsis on: the label asks for the room it
    /// is given, so without a bound there is nothing to truncate against and the
    /// tab simply grows.
    pub fn widest() -> Pixels {
        px(240.)
    }

    /// The radius of the tab's two top corners and of the two feet it stands on
    /// where it meets the strip. One number for both: the silhouette reads as a
    /// single swept shape only when the outward turn at the top and the inward
    /// turn at the bottom are the same size.
    pub fn corner() -> Pixels {
        px(10.)
    }

    /// Height of one tab: the strip minus the gutter above it, so its bottom
    /// edge lands exactly on the strip's bottom edge.
    pub fn card_height(cx: &App) -> Pixels {
        Self::container_height(cx) - Self::shoulder()
    }
}

impl InteractiveElement for Tab {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.div.interactivity()
    }
}

impl StatefulInteractiveElement for Tab {}

impl Toggleable for Tab {
    fn toggle_state(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
}

impl ParentElement for Tab {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for Tab {
    #[allow(refining_impl_trait)]
    fn render(self, _: &mut Window, cx: &mut App) -> Stateful<Div> {
        let (text_color, tab_bg, tab_hover_bg, tab_active_bg) = match self.selected {
            false => (
                cx.theme().colors().text_muted,
                cx.theme().colors().tab_inactive_background,
                cx.theme().colors().ghost_element_hover,
                cx.theme().colors().ghost_element_active,
            ),
            true => (
                cx.theme().colors().text,
                cx.theme().colors().tab_active_background,
                cx.theme().colors().element_hover,
                cx.theme().colors().element_active,
            ),
        };

        let tab_bg = self.bg_tint.map_or(tab_bg, |tint| tab_bg.blend(tint));

        let (start_slot, end_slot) = {
            let start_slot = h_flex()
                .size(START_TAB_SLOT_SIZE)
                .justify_center()
                .children(self.start_slot);

            let end_slot = h_flex()
                .size(END_TAB_SLOT_SIZE)
                .justify_center()
                .children(self.end_slot);

            match self.close_side {
                TabCloseSide::End => (start_slot, end_slot),
                TabCloseSide::Start => (end_slot, start_slot),
            }
        };

        // The selected tab and the surface under it are one piece of material,
        // the way a browser draws them: the same fill, only the top corners
        // rounded, nothing along the bottom to separate them, and two concave
        // feet where the sides meet the strip so the join reads as a
        // continuation rather than a card resting on top. That shape cannot be
        // made of boxes, so it is drawn as one path.
        let selected = self.selected;
        let card_border = cx.theme().colors().border;
        let card_height = Tab::card_height(cx);
        let content_height = Tab::content_height(cx);
        let content_px = DynamicSpacing::Base04.px(cx);
        let content_gap = DynamicSpacing::Base04.rems(cx);
        let shoulder = Tab::shoulder();
        let corner = Tab::corner();
        let face_of = self.id.clone();

        self.div
            .relative()
            .h(card_height)
            .max_w(Tab::widest())
            .mt(shoulder)
            // Room on both sides for the feet, so the selected tab never plants
            // one on the tab beside it.
            .mx(corner)
            // The strip gets a real inset at its ends rather than a tab flush
            // against the window edge.
            .map(|this| match self.position {
                TabPosition::First => this.ml(corner + shoulder),
                TabPosition::Last => this.mr(corner + shoulder),
                TabPosition::Middle(_) => this,
            })
            .rounded_t(corner)
            // The selected tab answers to no hover: it is the surface you are
            // already on, and lighting it under the pointer would say it is
            // something to move to.
            .when(!selected, move |this| {
                this.hover(move |style| style.bg(tab_hover_bg))
                    .active(move |style| style.bg(tab_active_bg))
            })
            .when(selected, move |this| {
                this.child(
                    div()
                        .debug_selector(move || format!("TAB-FACE-{face_of}"))
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(-corner)
                        .right(-corner)
                        .child(
                            canvas(
                                |_, _, _| {},
                                move |bounds, _, window, _| {
                                    paint_tab_silhouette(
                                        bounds,
                                        corner,
                                        tab_bg,
                                        card_border,
                                        window,
                                    );
                                },
                            )
                            .size_full(),
                        ),
                )
            })
            .cursor_pointer()
            .child(
                h_flex()
                    .group("")
                    .relative()
                    .h(content_height)
                    // Told to fill the tab rather than left to size itself to the
                    // name. A row with no width of its own gives the name nothing
                    // to be shortened against, so the name runs on and the bound
                    // above clips the end of the row -- which is the close
                    // button. Filling the tab pushes the shortening down to the
                    // name, where the middle ellipsis is waiting for it.
                    .w_full()
                    .min_w_0()
                    .overflow_hidden()
                    .px(content_px)
                    .gap(content_gap)
                    .text_color(text_color)
                    .child(start_slot)
                    .children(self.children)
                    .child(end_slot),
            )
    }
}

/// Draws the selected tab: a face with two rounded top corners that flares out
/// at the bottom into two concave feet. `bounds` is the tab's own box widened by
/// one foot on either side, and the shape is left open along the bottom so the
/// tab and the surface beneath it share an edge instead of having one drawn
/// between them.
/// The largest corner a box of this size has room for. A tab narrower than four
/// corners, or shorter than two, has none for the shape asked of it, and a path
/// drawn to the full radius there folds through itself rather than coming out
/// small. Panels are laid out at no width at all while they settle, so this is
/// reached in ordinary use, not only at the extremes.
fn corner_that_fits(size: Size<Pixels>, wanted: Pixels) -> Pixels {
    wanted.min(size.width / 4.).min(size.height / 2.)
}

fn paint_tab_silhouette(
    bounds: Bounds<Pixels>,
    corner: Pixels,
    fill: Hsla,
    border: Hsla,
    window: &mut Window,
) {
    // Where a quarter circle's Bezier handles sit along the tangents. A rounder
    // number leaves a flat spot where each turn meets the straight beside it,
    // which is exactly what the eye reads as "drawn by hand".
    const HANDLE: f32 = 0.552_284_75;

    // Returns whether there was room to draw at all. Each path is measured
    // against its own box rather than against the tab's: the edge is drawn half
    // a pixel in, so a radius that just fits the face leaves the edge's straight
    // runs a pixel short and folds the path back through itself.
    let outline = |builder: &mut PathBuilder, inset: Pixels| {
        let left = bounds.origin.x + inset;
        let top = bounds.origin.y + inset;
        let right = bounds.right() - inset;
        let bottom = bounds.bottom();
        let corner = corner_that_fits(size(right - left, bottom - top), corner);
        if corner <= px(0.) {
            return false;
        }
        let handle = corner * HANDLE;
        let face_left = left + corner;
        let face_right = right - corner;

        builder.move_to(point(left, bottom));
        builder.cubic_bezier_to(
            point(face_left, bottom - corner),
            point(left + handle, bottom),
            point(face_left, bottom - corner + handle),
        );
        builder.line_to(point(face_left, top + corner));
        builder.cubic_bezier_to(
            point(face_left + corner, top),
            point(face_left, top + corner - handle),
            point(face_left + corner - handle, top),
        );
        builder.line_to(point(face_right - corner, top));
        builder.cubic_bezier_to(
            point(face_right, top + corner),
            point(face_right - corner + handle, top),
            point(face_right, top + corner - handle),
        );
        builder.line_to(point(face_right, bottom - corner));
        builder.cubic_bezier_to(
            point(right, bottom),
            point(face_right, bottom - corner + handle),
            point(right - handle, bottom),
        );
        true
    };

    let mut face = PathBuilder::fill();
    if outline(&mut face, px(0.)) {
        face.close();
        match face.build() {
            Ok(face) => window.paint_path(face, fill),
            Err(error) => log::warn!("the tab's face could not be drawn: {error}"),
        }
    }

    // Half a pixel in, so the line lands inside the fill rather than half of it
    // on the strip above and half on the editor below.
    let mut edge = PathBuilder::stroke(px(1.));
    if outline(&mut edge, px(0.5)) {
        match edge.build() {
            Ok(edge) => window.paint_path(edge, border),
            Err(error) => log::warn!("the tab's edge could not be drawn: {error}"),
        }
    }
}

impl Component for Tab {
    fn scope() -> ComponentScope {
        ComponentScope::Navigation
    }

    fn description() -> &'static str {
        "A tab component that can be used in a tabbed interface, \
        supporting different positions and states."
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .children(vec![example_group_with_title(
                "Variations",
                vec![
                    single_example(
                        "Default",
                        Tab::new("default").child("Default Tab").into_any_element(),
                    ),
                    single_example(
                        "Selected",
                        Tab::new("selected")
                            .toggle_state(true)
                            .child("Selected Tab")
                            .into_any_element(),
                    ),
                    single_example(
                        "First",
                        Tab::new("first")
                            .position(TabPosition::First)
                            .child("First Tab")
                            .into_any_element(),
                    ),
                    single_example(
                        "Middle",
                        Tab::new("middle")
                            .position(TabPosition::Middle(Ordering::Equal))
                            .child("Middle Tab")
                            .into_any_element(),
                    ),
                    single_example(
                        "Last",
                        Tab::new("last")
                            .position(TabPosition::Last)
                            .child("Last Tab")
                            .into_any_element(),
                    ),
                ],
            )])
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Context, IntoElement, Render, TestAppContext, Window, px, size};

    use super::corner_that_fits;
    use crate::{Tab, TabBar, TabPosition, prelude::*};

    struct StripHost;

    impl Render for StripHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            TabBar::new("strip")
                .child(
                    Tab::new("resting")
                        .position(TabPosition::First)
                        .child("resting"),
                )
                .child(
                    Tab::new("chosen")
                        .position(TabPosition::Last)
                        .toggle_state(true)
                        .child("chosen"),
                )
        }
    }

    struct LongNameHost;

    impl Render for LongNameHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            TabBar::new("strip").child(
                Tab::new("long")
                    .position(TabPosition::First)
                    .toggle_state(true)
                    .end_slot(IconButton::new("close", IconName::Close).icon_size(IconSize::XSmall))
                    .child(
                        h_flex().w_full().min_w_0().overflow_hidden().child(
                            Label::new(
                                "InstrumentsDB_instruments-db-qa_forexpros_com-3822039d.sql",
                            )
                            .truncate_middle()
                            .flex_1(),
                        ),
                    ),
            )
        }
    }

    fn same(left: Pixels, right: Pixels) -> bool {
        (f32::from(left) - f32::from(right)).abs() < 0.5
    }

    fn draw_a_strip(cx: &mut TestAppContext) -> &mut gpui::VisualTestContext {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| StripHost);
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        cx
    }

    // What the reader sees as one piece of material rather than a card resting
    // on a shelf: the chosen tab reaches the strip's bottom edge, and its shape
    // widens by one foot on each side to get there. Measured on the painted
    // boxes, because the seam this replaces was a matter of pixels and not of
    // state.
    #[gpui::test]
    async fn the_chosen_tab_flares_into_the_surface_below_it(cx: &mut TestAppContext) {
        let cx = draw_a_strip(cx);

        let tab = cx.debug_bounds("TAB-chosen").expect("the tab is painted");
        let face = cx
            .debug_bounds("TAB-FACE-chosen")
            .expect("the chosen tab is drawn as a shape");
        let strip = cx
            .debug_bounds("TAB-BAR-strip")
            .expect("the strip is painted");
        let corner = Tab::corner();

        assert!(
            same(face.left(), tab.left() - corner) && same(face.right(), tab.right() + corner),
            "the shape spans {:?}..{:?} around a tab of {:?}..{:?}, so it has no feet",
            face.left(),
            face.right(),
            tab.left(),
            tab.right()
        );
        assert!(
            same(face.bottom(), tab.bottom()) && same(tab.bottom(), strip.bottom()),
            "the tab ends at {:?} and the strip at {:?}: a seam the reader would see",
            tab.bottom(),
            strip.bottom()
        );
    }

    // A foot that landed on the tab beside it would be painted over by that
    // tab's own hover fill, and the join would come apart under the pointer.
    #[gpui::test]
    async fn a_foot_never_lands_on_the_tab_beside_it(cx: &mut TestAppContext) {
        let cx = draw_a_strip(cx);

        let resting = cx.debug_bounds("TAB-resting").expect("the tab is painted");
        let face = cx
            .debug_bounds("TAB-FACE-chosen")
            .expect("the chosen tab is drawn as a shape");

        assert!(
            face.left() >= resting.right(),
            "the shape starts at {:?}, inside the tab that ends at {:?}",
            face.left(),
            resting.right()
        );
    }

    #[gpui::test]
    async fn a_resting_tab_is_no_shape_at_all(cx: &mut TestAppContext) {
        let cx = draw_a_strip(cx);

        assert!(
            cx.debug_bounds("TAB-FACE-resting").is_none(),
            "a tab that is not the chosen one drew itself a shape"
        );
    }

    #[test]
    fn a_box_with_no_room_gets_the_corner_it_has_room_for() {
        let wanted = px(10.);
        assert_eq!(corner_that_fits(size(px(200.), px(34.)), wanted), wanted);
        // Half the height, a quarter of the width: past either the path would
        // fold through itself.
        assert_eq!(corner_that_fits(size(px(200.), px(10.)), wanted), px(5.));
        assert_eq!(corner_that_fits(size(px(12.), px(34.)), wanted), px(3.));
        assert_eq!(corner_that_fits(size(px(0.), px(0.)), wanted), px(0.));
    }

    // A name of any length has to fit the strip: a tab that grows to its title
    // takes a third of the window for one file and leaves the rest unreadable.
    // The bound is also what turns the middle ellipsis on -- the label asks for
    // the room it is given, so with nothing to ask against it never truncates.
    #[gpui::test]
    async fn a_long_name_does_not_make_a_long_tab(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| LongNameHost);
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let tab = cx.debug_bounds("TAB-long").expect("the tab is painted");
        assert!(
            tab.size.width <= Tab::widest() + px(0.5),
            "a tab with a long name painted {:?} wide, past the {:?} it is allowed",
            tab.size.width,
            Tab::widest()
        );
    }
}
