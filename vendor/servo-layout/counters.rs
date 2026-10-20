use css_counters::{CounterScopes, CounterValues, LIST_ITEM};
use layout_api::{LayoutElement, LayoutNode};
use rustc_hash::FxHashMap;
use script::layout_dom::{ServoLayoutElement, ServoLayoutNode};
use servo_arc::Arc as ServoArc;
use style::context::SharedStyleContext;
use style::dom::OpaqueNode;
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use style::values::generics::counters::{Content, ContentItem};
use web_atoms::{local_name, ns};

/// What every counter of a document is where a list marker or generated content
/// asks. They are worked out in one walk through the document in tree order, the
/// first time anything asks, because what a counter is at one place depends on
/// every element before it.
///
/// Only the elements something can ask about are kept: a list item, and an
/// element with a `::before` or `::after` that writes a counter.
#[derive(Default)]
pub(crate) struct DocumentCounters {
    own: FxHashMap<OpaqueNode, CounterValues>,
    before: FxHashMap<OpaqueNode, CounterValues>,
    after: FxHashMap<OpaqueNode, CounterValues>,
}

impl DocumentCounters {
    pub(crate) fn compute(
        root: ServoLayoutElement<'_>,
        style_context: &SharedStyleContext,
    ) -> Self {
        let mut walk = Walk {
            style_context,
            scopes: CounterScopes::new(),
            counters: Self::default(),
        };
        walk.element(root.as_node(), root, 1);
        walk.counters
    }

    /// The counters in scope in an element, after it has had its say and before its
    /// children and its `::before`: what its list marker sees.
    pub(crate) fn own(&self, node: OpaqueNode) -> Option<&CounterValues> {
        self.own.get(&node)
    }

    /// The counters in scope in an element's `::before`, which has had its say too.
    pub(crate) fn before(&self, node: OpaqueNode) -> Option<&CounterValues> {
        self.before.get(&node)
    }

    /// The counters in scope after the last child of an element, which is where
    /// its `::after` sits.
    pub(crate) fn after(&self, node: OpaqueNode) -> Option<&CounterValues> {
        self.after.get(&node)
    }
}

struct Walk<'a, 'ctx> {
    style_context: &'a SharedStyleContext<'ctx>,
    scopes: CounterScopes,
    counters: DocumentCounters,
}

impl Walk<'_, '_> {
    fn children(&mut self, parent: ServoLayoutNode<'_>, list_step: i32) {
        for child in parent.dom_children() {
            if let Some(element) = child.as_element() {
                self.element(child, element, list_step);
            }
        }
    }

    fn element(
        &mut self,
        node: ServoLayoutNode<'_>,
        element: ServoLayoutElement<'_>,
        list_step: i32,
    ) {
        let Some(style) = self.styled(element) else {
            return;
        };
        if style.get_box().display.is_none() {
            return;
        }

        let is_html = node.as_html_element().is_some();
        let name = element.local_name();
        let mut children_step = list_step;
        if is_html && *name == local_name!("ol") {
            let reversed = element
                .attribute(&ns!(), &local_name!("reversed"))
                .is_some();
            let step = if reversed { -1 } else { 1 };
            let first = parse_integer(element.attribute_as_str(&ns!(), &local_name!("start")))
                .unwrap_or_else(|| if reversed { list_items_in(node) } else { 1 });
            self.scopes.reset(LIST_ITEM, first.saturating_sub(step));
            children_step = step;
        } else if is_html && (*name == local_name!("ul") || *name == local_name!("menu")) {
            self.scopes.reset(LIST_ITEM, 0);
            children_step = 1;
        }

        let names_list_item = self.apply(&style);
        let is_list_item = style.get_box().display.is_list_item();
        if is_list_item && !names_list_item {
            self.scopes.increment(LIST_ITEM, list_step);
        }
        if is_html && *name == local_name!("li") {
            if let Some(value) =
                parse_integer(element.attribute_as_str(&ns!(), &local_name!("value")))
            {
                self.scopes.set(LIST_ITEM, value);
            }
        }
        if is_list_item {
            self.counters
                .own
                .insert(node.opaque(), self.scopes.snapshot());
        }

        // The pseudo-elements are the first and last children of the element, so a
        // counter one of them makes belongs to the element's children, not to the
        // siblings after the element.
        self.scopes.open_children();
        if let Some(before) = self.pseudo_style(element, PseudoElement::Before) {
            self.apply(&before);
            if writes_counter(&before) {
                self.counters
                    .before
                    .insert(node.opaque(), self.scopes.snapshot());
            }
        }
        self.children(node, children_step);
        if let Some(after) = self.pseudo_style(element, PseudoElement::After) {
            self.apply(&after);
            if writes_counter(&after) {
                self.counters
                    .after
                    .insert(node.opaque(), self.scopes.snapshot());
            }
        }
        self.scopes.close_children();
    }

    /// Lets an element, or a pseudo-element, reset and increment the counters its
    /// style names. Says whether it named the list item counter.
    fn apply(&mut self, style: &ComputedValues) -> bool {
        let counters = style.get_counters();
        for pair in counters.counter_reset.iter() {
            let value = if pair.is_reversed && pair.value == i32::MIN {
                0
            } else {
                pair.value
            };
            self.scopes.reset(&pair.name.0.to_string(), value);
        }
        let mut names_list_item = false;
        for pair in counters.counter_increment.iter() {
            let counter = pair.name.0.to_string();
            names_list_item |= counter == LIST_ITEM;
            self.scopes.increment(&counter, pair.value);
        }
        names_list_item
    }

    /// The element's style, unless it has none: the children of an element that is
    /// not displayed are never styled.
    fn styled(&self, element: ServoLayoutElement<'_>) -> Option<ServoArc<ComputedValues>> {
        let data = element.style_data()?;
        data.element_data.borrow().styles.get_primary()?;
        Some(element.style(self.style_context))
    }

    fn pseudo_style(
        &self,
        element: ServoLayoutElement<'_>,
        pseudo: PseudoElement,
    ) -> Option<ServoArc<ComputedValues>> {
        Some(element.with_pseudo(pseudo)?.style(self.style_context))
    }
}

/// Whether a pseudo-element's content writes a counter.
fn writes_counter(style: &ComputedValues) -> bool {
    match &style.get_counters().content {
        Content::Items(items) => items
            .items
            .iter()
            .any(|item| matches!(item, ContentItem::Counter(..) | ContentItem::Counters(..))),
        Content::Normal | Content::None => false,
    }
}

fn parse_integer(attribute: Option<&str>) -> Option<i32> {
    attribute?.trim().parse().ok()
}

/// How many list items a list has, as an `ol` with `reversed` and no `start` counts
/// down from.
fn list_items_in(list: ServoLayoutNode<'_>) -> i32 {
    list.dom_children()
        .filter(|child| {
            child
                .as_html_element()
                .is_some_and(|item| *item.local_name() == local_name!("li"))
        })
        .count() as i32
}
