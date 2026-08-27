use std::cmp::Ordering;

use gpui::{AnyElement, IntoElement, Stateful};
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

    /// The gutter above a tab, and the radius of both its top corners and of the
    /// flare where it meets the strip.
    pub fn shoulder() -> Pixels {
        px(6.)
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

        // A tab is a card, not a cell in a table. The old shape -- flush
        // rectangles telling each other apart by which of their four 1px borders
        // was drawn, and by a one-pixel nudge of their padding -- is the single
        // most dating thing about an editor's face. Position no longer changes
        // the shape at all: the selected tab is the one that is filled and
        // raised, and the accent rail on top says which document you are in from
        // across the room.
        let selected = self.selected;
        let card_border = cx.theme().colors().border;
        let card_height = Tab::card_height(cx);
        let content_height = Tab::content_height(cx);
        let content_px = DynamicSpacing::Base04.px(cx);
        let content_gap = DynamicSpacing::Base04.rems(cx);
        let shoulder = Tab::shoulder();
        let strip_bg = cx.theme().colors().tab_bar_background;

        // The selected tab and the buffer under it are one surface, the way a
        // browser draws them: the fill is the same colour as the editor, only the
        // top corners are rounded, nothing separates them along the bottom, and
        // the tab flares outward where it meets the strip so the join reads as a
        // physical continuation rather than a card resting on top.
        let flare = |on_left: bool| {
            div()
                .absolute()
                .bottom_0()
                .w(shoulder)
                .h(shoulder)
                .bg(tab_bg)
                .map(|this| {
                    if on_left {
                        this.left(-shoulder)
                    } else {
                        this.right(-shoulder)
                    }
                })
                .child(
                    // A disc of the strip's own colour, centred on the flare's
                    // outer top corner, carves the concave quarter out of it.
                    div()
                        .absolute()
                        .top(-shoulder)
                        .w(shoulder * 2.)
                        .h(shoulder * 2.)
                        .rounded_full()
                        .bg(strip_bg)
                        .map(|this| {
                            if on_left {
                                this.left(-shoulder)
                            } else {
                                this.left(px(0.))
                            }
                        }),
                )
        };

        self.div
            .h(card_height)
            .mt(shoulder)
            .mx(px(2.))
            // The strip gets a real inset at its ends rather than a tab flush
            // against the window edge.
            .map(|this| match self.position {
                TabPosition::First => this.ml(shoulder),
                TabPosition::Last => this.mr(shoulder),
                TabPosition::Middle(_) => this,
            })
            .rounded_t(px(10.))
            .relative()
            .bg(if selected {
                tab_bg
            } else {
                gpui::transparent_black()
            })
            .hover(move |style| style.bg(tab_hover_bg))
            .active(move |style| style.bg(tab_active_bg))
            .when(selected, move |this| {
                // Border on three sides only. A line along the bottom is exactly
                // what would say "this is a separate box", and a shadow would say
                // it floats -- both are the opposite of what the shape is for.
                this.border_t_1()
                    .border_l_1()
                    .border_r_1()
                    .border_color(card_border)
                    .child(flare(true))
                    .child(flare(false))
            })
            .cursor_pointer()
            .child(
                h_flex()
                    .group("")
                    .relative()
                    .h(content_height)
                    .px(content_px)
                    .gap(content_gap)
                    .text_color(text_color)
                    .child(start_slot)
                    .children(self.children)
                    .child(end_slot),
            )
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
