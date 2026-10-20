/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use atomic_refcell::AtomicRefCell;
use base64::Engine as _;
use cssparser::Parser;
use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::{JSContext, NoGC};
use js::rust::HandleObject;
use layout_api::SVGElementData;
use servo_url::ServoUrl;
use style::attr::AttrValue;
use style::media_queries::MediaList as StyleMediaList;
use style::parser::ParserContext;
use style::stylesheets::CustomMediaEvaluator;
use style::stylesheets::Origin;
use style::values::specified::LengthPercentage;
use style_traits::{ParsingMode, ToCss};
use svg_page_style::{
    Condition, flatten_conditional_rules, insert_style_into_drawing, resolve_page_style,
    style_applies_to_screen,
};
use uuid::Uuid;
use xml5ever::serialize::TraversalScope;

use crate::dom::bindings::codegen::Bindings::CSSBinding::CSSMethods;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, LayoutDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::css::css::CSS;
use crate::dom::document::Document;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::html::htmllinkelement::HTMLLinkElement;
use crate::dom::html::htmlstyleelement::HTMLStyleElement;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::media::medialist::MediaList;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{
    ChildrenMutation, CloneChildrenFlag, Node, NodeDamage, NodeTraits, UnbindContext,
};
use crate::dom::svg::pagestylematcher::PageRuleMatcher;
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;

#[dom_struct]
pub(crate) struct SVGSVGElement {
    svggraphicselement: SVGGraphicsElement,
    #[no_trace]
    uuid: Uuid,
    // The XML source of subtree rooted at this SVG element, serialized into
    // a base64 encoded `data:` url. This is cached to avoid recomputation
    // on each layout and must be invalidated when the subtree changes.
    #[no_trace]
    cached_serialized_data_url: AtomicRefCell<Option<Result<ServoUrl, ()>>>,
    /// The media queries the page's stylesheets asked when the drawing was serialised, and what
    /// each answered, so that the drawing is serialised again when one of them answers otherwise.
    #[no_trace]
    media_conditions: AtomicRefCell<Vec<(String, bool)>>,
}

impl SVGSVGElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGSVGElement {
        SVGSVGElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            uuid: Uuid::new_v4(),
            cached_serialized_data_url: Default::default(),
            media_conditions: Default::default(),
        }
    }

    #[cfg_attr(crown, allow(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGSVGElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGSVGElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    pub(crate) fn serialize_and_cache_subtree(&self, cx: &mut js::context::JSContext) {
        let document_fragment = self.owner_document().CreateDocumentFragment(cx);
        let cloned_node = Node::clone(
            cx,
            self.upcast(),
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        if document_fragment
            .upcast::<Node>()
            .AppendChild(cx, &cloned_node)
            .is_err()
        {
            error!("Unable to clone SVG tree");
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        }

        let computed_color = self
            .upcast::<Element>()
            .style_left_by_the_last_layout()
            .map(|style| style.clone_color().to_css_string());
        let mut conditions = Vec::new();
        let page_css =
            flatten_conditional_rules(&self.page_style_text(), &mut |condition| match condition {
                Condition::Media(query) => {
                    let holds = self.media_condition_holds(query);
                    conditions.push((query.to_string(), holds));
                    holds
                }
                Condition::Supports(query) => {
                    CSS::Supports_(&self.owner_window(), DOMString::from(query))
                }
            });
        *self.media_conditions.borrow_mut() = conditions;
        let page_style = resolve_page_style(&page_css, computed_color.as_deref());
        let matcher = PageRuleMatcher::new(&self.owner_document(), &page_style);
        matcher.mark(cx, self.upcast(), &cloned_node);

        self.process_use_elements(cx, &cloned_node, &matcher);

        let Ok(xml_source) = cloned_node.xml_serialize(TraversalScope::IncludeNode) else {
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        };

        let xml_source: String = xml_source.into();
        let xml_source = insert_style_into_drawing(&xml_source, &matcher.drawing_style());
        let base64_encoded_source = base64::engine::general_purpose::STANDARD.encode(xml_source);
        let data_url = format!("data:image/svg+xml;base64,{base64_encoded_source}");
        match ServoUrl::parse(&data_url) {
            Ok(url) => *self.cached_serialized_data_url.borrow_mut() = Some(Ok(url)),
            Err(error) => error!("Unable to parse serialized SVG data url: {error}"),
        };
    }

    /// What the page's own `<style>` elements say. The drawing is rasterised on its
    /// own, apart from the page, so none of it reaches the drawing unless it is
    /// handed over as text: the engine's style system drops the properties of
    /// SVG that it does not draw itself, and the raw text is the only place they
    /// are still to be read.
    fn page_style_text(&self) -> String {
        let document = self.owner_document();
        let mut text = String::new();
        for imported in document.imported_sheet_texts() {
            text.push_str(&imported);
            text.push('\n');
        }
        for node in document
            .upcast::<Node>()
            .traverse_preorder(ShadowIncluding::No)
        {
            if let Some(style) = node.downcast::<HTMLStyleElement>() {
                let element = style.upcast::<Element>();
                let type_attribute = element.get_string_attribute(&local_name!("type"));
                let media = element.get_string_attribute(&local_name!("media"));
                if style_applies_to_screen(&type_attribute.str(), &media.str()) {
                    text.push_str(&node.child_text_content().str());
                    text.push('\n');
                }
            } else if let Some(link) = node.downcast::<HTMLLinkElement>() {
                if link.get_stylesheet().is_none() || link.is_effectively_disabled() {
                    continue;
                }
                let media = link
                    .upcast::<Element>()
                    .get_string_attribute(&local_name!("media"));
                if style_applies_to_screen("", &media.str()) {
                    text.push_str(&link.source_text());
                    text.push('\n');
                }
            }
        }
        text
    }

    fn process_use_elements(
        &self,
        cx: &mut JSContext,
        root_node: &Node,
        matcher: &PageRuleMatcher,
    ) {
        for node in root_node.traverse_preorder(ShadowIncluding::No) {
            if let Some(element) = node.downcast::<Element>()
                && element.local_name() == &local_name!("use")
            {
                self.process_single_use_element(cx, element, root_node, matcher)
            }
        }
    }

    fn process_single_use_element(
        &self,
        cx: &mut JSContext,
        use_element: &Element,
        root_node: &Node,
        matcher: &PageRuleMatcher,
    ) {
        let href = use_element.get_string_attribute(&local_name!("href"));
        let Some(id_string) = href.str().strip_prefix("#").map(DOMString::from) else {
            return;
        };

        let document = self.upcast::<Node>().owner_doc();
        let Some(referenced_element) = document.GetElementById(cx, id_string) else {
            return;
        };
        let referenced_node = referenced_element.upcast::<Node>();

        // Don't use this node if it doesn't have an `<svg>` ancestor.
        if !referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| ancestor.is::<SVGSVGElement>())
        {
            return;
        };

        // Don't use this node if it already exists within the same `<svg>` element.
        if referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| *ancestor == self.upcast())
        {
            return;
        };

        let cloned_node = Node::clone(
            cx,
            referenced_node,
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        matcher.mark(cx, referenced_node, &cloned_node);
        let _ = root_node.AppendChild(cx, &cloned_node);
    }

    fn media_condition_holds(&self, condition: &str) -> bool {
        let window = self.owner_window();
        let media_list: StyleMediaList = MediaList::parse_media_list(condition, &window);
        media_list.evaluate(
            window.layout().device(),
            self.owner_document().quirks_mode(),
            &mut CustomMediaEvaluator::none(),
        )
    }

    /// A media feature of the page may have changed, such as its width or its theme. A
    /// drawing made with a media query that answers otherwise now has to be made again.
    pub(crate) fn media_may_have_changed(&self, no_gc: &NoGC) {
        let answers_otherwise = self
            .media_conditions
            .borrow()
            .iter()
            .any(|(condition, was)| self.media_condition_holds(condition) != *was);
        if answers_otherwise {
            self.invalidate_cached_serialized_subtree_and_rasterization_result(no_gc);
        }
    }

    /// The drawing was serialised with the page's style as it was then. When that
    /// changes it has to be serialised again, or it goes on showing the old one.
    pub(crate) fn page_style_changed(&self, no_gc: &NoGC) {
        if self.cached_serialized_data_url.borrow().is_some() {
            self.invalidate_cached_serialized_subtree_and_rasterization_result(no_gc);
        }
    }

    fn invalidate_cached_serialized_subtree_and_rasterization_result(&self, no_gc: &NoGC) {
        let owner_window = self.owner_window();
        owner_window
            .image_cache()
            .evict_rasterized_image(&self.uuid);
        if let Some(Ok(url)) = &*self.cached_serialized_data_url.borrow() {
            owner_window.layout_mut().remove_cached_image(url);
            owner_window.image_cache().evict_completed_image(
                url,
                owner_window.origin().immutable(),
                &None,
            );
        }

        *self.cached_serialized_data_url.borrow_mut() = None;
        self.upcast::<Node>().dirty(no_gc, NodeDamage::Other);
    }
}

impl<'dom> LayoutDom<'dom, SVGSVGElement> {
    pub(crate) fn data(self) -> SVGElementData<'dom> {
        let svg_id = self.unsafe_get().uuid;
        let element = self.upcast::<Element>();
        let width = element.get_attr_for_layout(&ns!(), &local_name!("width"));
        let height = element.get_attr_for_layout(&ns!(), &local_name!("height"));
        let view_box = element.get_attr_for_layout(&ns!(), &local_name!("viewBox"));
        SVGElementData {
            source: self
                .unsafe_get()
                .cached_serialized_data_url
                .borrow()
                .clone(),
            width,
            height,
            view_box,
            svg_id,
        }
    }
}

impl VirtualMethods for SVGSVGElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }

    fn attribute_mutated(
        &self,
        cx: &mut js::context::JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn attribute_affects_presentational_hints(&self, attr: AttrRef<'_>) -> bool {
        match attr.local_name() {
            &local_name!("width") | &local_name!("height") => true,
            _ => self
                .super_type()
                .unwrap()
                .attribute_affects_presentational_hints(attr),
        }
    }

    fn parse_plain_attribute(&self, name: &LocalName, value: DOMString) -> AttrValue {
        match *name {
            local_name!("width") | local_name!("height") => {
                let value = &value.str();
                let parser = &mut Parser::new(value);
                let doc = self.owner_document();
                let url = doc.url().into_url().into();
                let context = ParserContext::new(
                    Origin::Author,
                    &url,
                    None,
                    ParsingMode::ALLOW_UNITLESS_LENGTH,
                    doc.quirks_mode(),
                    /* namespaces = */ Default::default(),
                    None,
                    None,
                    /* attr_taint = */ Default::default(),
                );
                let val = LengthPercentage::parse_quirky(
                    &context,
                    parser,
                    style::values::specified::AllowQuirks::Always,
                );
                AttrValue::LengthPercentage(value.to_string(), val.ok())
            }
            _ => self
                .super_type()
                .unwrap()
                .parse_plain_attribute(name, value),
        }
    }

    fn children_changed(&self, cx: &mut JSContext, mutation: &ChildrenMutation) {
        if let Some(super_type) = self.super_type() {
            super_type.children_changed(cx, mutation);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn unbind_from_tree(&self, cx: &mut js::context::JSContext, context: &UnbindContext<'_>) {
        if let Some(s) = self.super_type() {
            s.unbind_from_tree(cx, context);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }
}
