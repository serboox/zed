/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use css_counters::{CounterStyleKind, LIST_ITEM, format_counter};
use layout_api::LayoutNode;
use style::counter_style::{CounterStyle, Symbol, SymbolsType};
use style::properties::longhands::list_style_type::computed_value::T as ListStyleType;
use style::values::computed::Image;
use style::values::generics::counters::Content;
use stylo_atoms::atom;

use crate::context::LayoutContext;
use crate::dom_traversal::{
    NodeAndStyleInfo, PseudoElementContentItem, generate_pseudo_element_content,
};
use crate::replaced::ReplacedContents;

/// <https://drafts.csswg.org/css-lists/#content-property>
pub(crate) fn make_marker<'dom>(
    context: &LayoutContext,
    info: &NodeAndStyleInfo<'dom>,
) -> Option<(NodeAndStyleInfo<'dom>, Vec<PseudoElementContentItem>)> {
    let marker_info =
        info.with_pseudo_element(context, style::selector_parser::PseudoElement::Marker)?;
    let style = &marker_info.style;
    let list_style = style.get_list();

    // https://drafts.csswg.org/css-lists/#marker-image
    let marker_image = || match &list_style.list_style_image {
        Image::Url(url) => Some(vec![
            PseudoElementContentItem::Replaced(ReplacedContents::from_image_url(
                marker_info.node,
                context,
                url,
            )?),
            PseudoElementContentItem::Text(" ".into()),
        ]),
        // XXX: Non-None image types unimplemented.
        Image::ImageSet(..)
        | Image::Gradient(..)
        | Image::Image(..)
        | Image::CrossFade(..)
        | Image::PaintWorklet(..)
        | Image::None => None,
        Image::LightDark(..) => unreachable!("light-dark() should be disabled"),
    };

    let content = match &marker_info.style.get_counters().content {
        Content::Items(_) => generate_pseudo_element_content(&marker_info, context),
        Content::None => return None,
        Content::Normal => marker_image().or_else(|| {
            let value = context
                .counters()
                .own(info.node.opaque())
                .map_or(0, |counters| counters.innermost(LIST_ITEM));
            Some(vec![PseudoElementContentItem::Text(marker_string(
                &list_style.list_style_type,
                value,
            )?)])
        })?,
    };

    Some((marker_info, content))
}

fn symbol_to_string(symbol: &Symbol) -> String {
    match symbol {
        Symbol::String(string) => string.to_string(),
        Symbol::Ident(ident) => ident.0.to_string(),
    }
}

/// How the style system's counter style is written, for what this engine can
/// write: decimal for every style it has no rule for.
pub(crate) fn counter_style_kind(counter_style: &CounterStyle) -> CounterStyleKind {
    match counter_style {
        CounterStyle::None => CounterStyleKind::Hidden,
        CounterStyle::String(string) => CounterStyleKind::Glyph(string.to_string()),
        CounterStyle::Name(name) => match name.0.to_string().as_str() {
            "disc" => CounterStyleKind::Glyph("\u{2022}".to_string()), /* "•" */
            "circle" => CounterStyleKind::Glyph("\u{25E6}".to_string()), /* "◦" */
            "square" => CounterStyleKind::Glyph("\u{25AA}".to_string()), /* "▪" */
            "disclosure-open" => CounterStyleKind::Glyph("\u{25BE}".to_string()), /* "▾" */
            "disclosure-closed" => CounterStyleKind::Glyph("\u{25B8}".to_string()), /* "▸" */
            "decimal-leading-zero" => CounterStyleKind::DecimalLeadingZero,
            "lower-alpha" | "lower-latin" => CounterStyleKind::LowerAlpha,
            "upper-alpha" | "upper-latin" => CounterStyleKind::UpperAlpha,
            "lower-roman" => CounterStyleKind::LowerRoman,
            "upper-roman" => CounterStyleKind::UpperRoman,
            "lower-greek" => CounterStyleKind::LowerGreek,
            _ => CounterStyleKind::Decimal,
        },
        CounterStyle::Symbols { ty, symbols } => {
            let symbols: Vec<String> = symbols.0.iter().map(symbol_to_string).collect();
            match ty {
                SymbolsType::Numeric => CounterStyleKind::Numeric(symbols),
                SymbolsType::Alphabetic => CounterStyleKind::Alphabetic(symbols),
                SymbolsType::Symbolic => CounterStyleKind::Symbolic(symbols),
                SymbolsType::Cyclic => CounterStyleKind::Cyclic(symbols),
                SymbolsType::Fixed => CounterStyleKind::Fixed(symbols),
            }
        }
    }
}

/// <https://drafts.csswg.org/css-counter-styles-3/#generate-a-counter>
pub(crate) fn generate_counter_representation(counter_style: &CounterStyle, value: i32) -> String {
    format_counter(value, &counter_style_kind(counter_style))
}

/// <https://drafts.csswg.org/css-lists/#marker-string>
pub(crate) fn marker_string(list_style_type: &ListStyleType, value: i32) -> Option<String> {
    let suffix = match &list_style_type.0 {
        CounterStyle::None => return None,
        CounterStyle::String(string) => return Some(string.to_string()),
        CounterStyle::Name(name) => match name.0 {
            atom!("disc")
            | atom!("circle")
            | atom!("square")
            | atom!("disclosure-open")
            | atom!("disclosure-closed") => " ",
            atom!("hiragana")
            | atom!("hiragana-iroha")
            | atom!("katakana")
            | atom!("katakana-iroha")
            | atom!("cjk-decimal")
            | atom!("cjk-earthly-branch")
            | atom!("cjk-heavenly-stem")
            | atom!("japanese-informal")
            | atom!("japanese-formal")
            | atom!("simp-chinese-informal")
            | atom!("simp-chinese-formal")
            | atom!("trad-chinese-informal")
            | atom!("trad-chinese-formal")
            | atom!("cjk-ideographic") => "\u{3001}", /* "、" */
            atom!("korean-hangul-formal")
            | atom!("korean-hanja-informal")
            | atom!("korean-hanja-formal") => ", ",
            atom!("ethiopic-numeric") => "/ ",
            _ => ". ",
        },
        CounterStyle::Symbols { .. } => " ",
    };
    Some(generate_counter_representation(&list_style_type.0, value) + suffix)
}
