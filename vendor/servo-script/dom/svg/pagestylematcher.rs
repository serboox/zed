use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use html5ever::{LocalName, ns};
use js::context::JSContext;
use layout_api::with_layout_state;
use selectors::parser::Component;
use style::attr::AttrValue;
use style::context::QuirksMode;
use style::dom_apis;
use style::selector_parser::{SelectorImpl, SelectorParser};
use style::stylesheets::UrlExtraData;
use svg_page_style::{
    MARKER_ATTRIBUTE, MatchedRule, emit_matched_rules, marker_value, split_selector_list,
    style_rules,
};

use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot, LayoutDom, ToLayout};
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::Node;
use crate::layout_dom::ServoDangerousStyleElement;

const MOST_PARSED_SELECTORS_KEPT: usize = 20_000;

/// Selector-against-element tests made for one drawing, at the most. A page with
/// thousands of broad rules and a drawing of thousands of elements would otherwise
/// keep the script thread busy for seconds; what is not matched by then is left out.
const MOST_MATCHES_PER_DRAWING: usize = 1_000_000;

/// What an element must have for a selector to match it at all, read off the
/// selector's last compound: a match needs every one of these.
#[derive(Default)]
struct Required {
    id: Option<String>,
    class: Option<String>,
    local_name: Option<String>,
}

struct ParsedSelector {
    list: selectors::SelectorList<SelectorImpl>,
    specificity: u32,
    required: Required,
}

thread_local! {
    static PARSED_SELECTORS: RefCell<HashMap<String, Option<Rc<ParsedSelector>>>> =
        RefCell::new(HashMap::new());
}

fn parse_selector(text: &str, url_data: &UrlExtraData) -> Option<Rc<ParsedSelector>> {
    PARSED_SELECTORS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(parsed) = cache.get(text) {
            return parsed.clone();
        }
        let parsed = SelectorParser::parse_author_origin_no_namespace(text, url_data)
            .ok()
            .and_then(|list| {
                let selector = list.slice().first()?;
                let mut required = Required::default();
                for component in selector.iter() {
                    match component {
                        Component::ID(identifier) => required.id = Some(identifier.to_string()),
                        Component::Class(identifier) => {
                            required.class = Some(identifier.to_string())
                        }
                        Component::LocalName(local_name) => {
                            required.local_name = Some(local_name.lower_name.to_string())
                        }
                        _ => {}
                    }
                }
                let specificity = selector.specificity();
                Some(Rc::new(ParsedSelector {
                    list,
                    specificity,
                    required,
                }))
            });
        if cache.len() >= MOST_PARSED_SELECTORS_KEPT {
            cache.clear();
        }
        cache.insert(text.to_string(), parsed.clone());
        parsed
    })
}

/// One selector of a style rule's list, with what is needed of the rule. Each selector
/// of a list is its own rule for the cascade, because how specific a rule is for an
/// element is how specific the selector that matched it is.
struct PreparedRule {
    order: usize,
    layer: Option<usize>,
    block: Rc<str>,
    selector: Rc<ParsedSelector>,
}

/// The page's style rules, matched by the page's own selector engine against the
/// elements of an inline SVG. The drawing is rasterised apart from the page, where
/// no selector can see an ancestor, a sibling or a pseudo-class, so each element a
/// rule matches is marked with the rule, and the drawing is handed rules that
/// ask for the mark.
pub(crate) struct PageRuleMatcher {
    rules: Vec<PreparedRule>,
    quirks_mode: QuirksMode,
    matched: RefCell<HashSet<usize>>,
    matches_made: Cell<usize>,
}

impl PageRuleMatcher {
    pub(crate) fn new(document: &Document, css: &str) -> Self {
        let url_data = UrlExtraData(document.url().get_arc());
        let rules = style_rules(css)
            .into_iter()
            .enumerate()
            .flat_map(|(order, rule)| {
                let block: Rc<str> = Rc::from(rule.block.trim());
                split_selector_list(rule.selectors)
                    .into_iter()
                    .filter_map(|selector| parse_selector(selector, &url_data))
                    .map(|selector| PreparedRule {
                        order,
                        layer: rule.layer,
                        block: block.clone(),
                        selector,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Self {
            rules,
            quirks_mode: document.quirks_mode(),
            matched: RefCell::default(),
            matches_made: Cell::new(0),
        }
    }

    /// Marks the elements of `clone_root`, a copy of `original_root`'s subtree, with the
    /// rules that match the elements they were copied from.
    pub(crate) fn mark(&self, cx: &mut JSContext, original_root: &Node, clone_root: &Node) {
        let originals = elements_of(original_root);
        let clones = elements_of(clone_root);
        if originals.len() != clones.len() || originals.is_empty() {
            return;
        }

        let mut ids = HashSet::new();
        let mut classes = HashSet::new();
        let mut local_names = HashSet::new();
        for element in &originals {
            if let Some(id) = element.get_id() {
                ids.insert(id.to_string());
            }
            let class_attribute = element.get_string_attribute(&html5ever::local_name!("class"));
            for class in class_attribute.str().split_ascii_whitespace() {
                classes.insert(class.to_string());
            }
            local_names.insert(element.local_name().to_ascii_lowercase().to_string());
        }

        let mut markers_per_element = vec![Vec::new(); originals.len()];
        let mut matched = self.matched.borrow_mut();
        with_layout_state(|| {
            for (marker, rule) in self.rules.iter().enumerate() {
                let required = &rule.selector.required;
                let possible = required.id.as_ref().is_none_or(|id| ids.contains(id))
                    && required
                        .class
                        .as_ref()
                        .is_none_or(|class| classes.contains(class))
                    && required
                        .local_name
                        .as_ref()
                        .is_none_or(|name| local_names.contains(name));
                if !possible {
                    continue;
                }
                for (index, element) in originals.iter().enumerate() {
                    if self.matches_made.get() >= MOST_MATCHES_PER_DRAWING {
                        log::warn!(
                            "a page's rules are too many for an inline SVG, the rest are left out"
                        );
                        return;
                    }
                    self.matches_made.set(self.matches_made.get() + 1);
                    if self.matches(element, &rule.selector) {
                        markers_per_element[index].push(marker);
                        matched.insert(marker);
                    }
                }
            }
        });
        drop(matched);

        let attribute = LocalName::from(MARKER_ATTRIBUTE);
        for (clone, markers) in clones.iter().zip(markers_per_element) {
            if markers.is_empty() {
                // A drawing may carry the attribute itself, and a copy of it would
                // be taken for a mark.
                if clone.has_attribute(&attribute) {
                    clone.remove_attribute(cx, &ns!(), &attribute);
                }
            } else {
                clone.set_attribute(cx, &attribute, AttrValue::String(marker_value(&markers)));
            }
        }
    }

    #[allow(unsafe_code)]
    fn matches(&self, element: &Element, selector: &ParsedSelector) -> bool {
        // SAFETY: `traced` is unrooted, but a reference to `element` is held, so it
        // is not freed while the layout element is in use.
        let traced = Dom::from_ref(element);
        let layout_element: LayoutDom<'_, Element> = unsafe { traced.to_layout() };
        dom_apis::element_matches(
            &ServoDangerousStyleElement::from(layout_element.upcast()),
            &selector.list,
            self.quirks_mode,
        )
    }

    /// The rules that matched something, aimed at the marks `mark` made.
    pub(crate) fn drawing_style(&self) -> String {
        let matched = self.matched.borrow();
        let rules: Vec<MatchedRule> = matched
            .iter()
            .map(|&marker| MatchedRule {
                marker,
                specificity: self.rules[marker].selector.specificity,
                order: self.rules[marker].order,
                layer: self.rules[marker].layer,
                block: self.rules[marker].block.to_string(),
            })
            .collect();
        emit_matched_rules(&rules)
    }
}

fn elements_of(root: &Node) -> Vec<DomRoot<Element>> {
    root.traverse_preorder(ShadowIncluding::No)
        .filter_map(DomRoot::downcast::<Element>)
        .collect()
}
