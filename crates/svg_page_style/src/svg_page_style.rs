const CDATA_END: &str = "]]>";
const CDATA_END_SPLIT: &str = "]]]]><![CDATA[>";

/// Whether a `<style>` element with these `type` and `media` attributes applies to
/// a drawing on a screen. A type other than CSS is not a stylesheet at all, and a
/// media query is read for its type only: a condition such as `(max-width: 900px)`
/// is not evaluated, so only a query that names `print` or `speech`, or denies
/// `screen` or `all`, is turned away.
pub fn style_applies_to_screen(type_attribute: &str, media: &str) -> bool {
    let type_attribute = type_attribute.trim();
    if !type_attribute.is_empty() && !type_attribute.eq_ignore_ascii_case("text/css") {
        return false;
    }
    let media = media.trim().to_ascii_lowercase();
    media.is_empty() || media.split(',').any(query_applies_to_screen)
}

fn query_applies_to_screen(query: &str) -> bool {
    // An empty entry in a list of queries is a query that matches nothing.
    if query.trim().is_empty() {
        return false;
    }
    let mut words = query.split_ascii_whitespace().peekable();
    if words.peek() == Some(&"only") {
        words.next();
    }
    let negated = words.peek() == Some(&"not");
    if negated {
        words.next();
    }
    let media_type: String = words
        .next()
        .unwrap_or_default()
        .chars()
        .take_while(|character| character.is_ascii_alphabetic() || *character == '-')
        .collect();
    let names_screen = matches!(media_type.as_str(), "" | "all" | "screen");
    names_screen != negated
}

/// Puts `css` in a `<style>` element at the start of the root element of a drawing
/// that was serialised on its own, so that the rules of the page it came from apply
/// to it. Rules in a drawing's own `<style>` elements follow and win on ties.
/// A drawing whose root element cannot be found is returned as it was.
pub fn insert_style_into_drawing(drawing: &str, css: &str) -> String {
    if css.trim().is_empty() {
        return drawing.to_owned();
    }
    let Some(start_tag) = root_start_tag(drawing) else {
        return drawing.to_owned();
    };
    let style = format!(
        "<style type=\"text/css\"><![CDATA[{}]]></style>",
        css.replace(CDATA_END, CDATA_END_SPLIT)
    );
    let (head, tail) = drawing.split_at(start_tag.end);
    match head.strip_suffix("/>") {
        Some(open) => format!("{open}>{style}</{}>{tail}", start_tag.name),
        None => format!("{head}{style}{tail}"),
    }
}

struct StartTag<'a> {
    name: &'a str,
    /// Offset just past the closing `>`.
    end: usize,
}

/// The start tag of the first element, skipping a declaration, comments and
/// whitespace before it. A `>` inside a quoted attribute value does not end it.
fn root_start_tag(drawing: &str) -> Option<StartTag<'_>> {
    let mut at = 0;
    loop {
        let rest = &drawing[at..];
        let trimmed = rest.trim_start();
        at += rest.len() - trimmed.len();
        if let Some(after) = trimmed.strip_prefix("<?") {
            at += 2 + after.find("?>")? + 2;
        } else if let Some(after) = trimmed.strip_prefix("<!--") {
            at += 4 + after.find("-->")? + 3;
        } else {
            break;
        }
    }

    let rest = &drawing[at..];
    let after_bracket = rest.strip_prefix('<')?;
    let name_length = after_bracket.find(|character: char| {
        character.is_whitespace() || character == '>' || character == '/'
    })?;
    if name_length == 0 {
        return None;
    }
    let name = &after_bracket[..name_length];

    let mut quote = None;
    for (offset, character) in rest.char_indices() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if open == character => quote = None,
            (None, '>') => {
                return Some(StartTag {
                    name,
                    end: at + offset + 1,
                });
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "svg .box { fill: #0f0 }";

    #[test]
    fn a_rule_goes_inside_the_root_element() {
        let drawing = "<svg width=\"10\"><rect class=\"box\"></rect></svg>";
        assert_eq!(
            insert_style_into_drawing(drawing, RULE),
            "<svg width=\"10\"><style type=\"text/css\"><![CDATA[svg .box { fill: #0f0 }]]></style>\
             <rect class=\"box\"></rect></svg>"
        );
    }

    #[test]
    fn a_bracket_inside_an_attribute_value_is_not_the_end_of_the_tag() {
        let drawing = "<svg aria-label=\"a > b\" data-x='c > d'><g></g></svg>";
        let inserted = insert_style_into_drawing(drawing, RULE);
        assert!(
            inserted.starts_with("<svg aria-label=\"a > b\" data-x='c > d'><style"),
            "{inserted}"
        );
    }

    #[test]
    fn an_empty_root_element_is_opened_and_closed() {
        assert_eq!(
            insert_style_into_drawing("<svg width=\"10\"/>", RULE),
            "<svg width=\"10\"><style type=\"text/css\"><![CDATA[svg .box { fill: #0f0 }]]></style></svg>"
        );
    }

    #[test]
    fn a_declaration_and_comments_before_the_root_are_skipped() {
        let drawing = "<?xml version=\"1.0\"?>\n<!-- made by hand -->\n<svg><g></g></svg>";
        let inserted = insert_style_into_drawing(drawing, RULE);
        assert!(
            inserted.contains("<svg><style type=\"text/css\">"),
            "{inserted}"
        );
        assert!(inserted.starts_with("<?xml"), "{inserted}");
    }

    #[test]
    fn the_end_of_a_cdata_section_inside_the_css_cannot_close_it() {
        let inserted = insert_style_into_drawing("<svg></svg>", "a { content: \"]]>\" }");
        assert!(inserted.contains("]]]]><![CDATA[>"), "{inserted}");
        assert_eq!(inserted.matches(CDATA_END).count(), 2, "{inserted}");
    }

    #[test]
    fn nothing_is_inserted_without_css_or_without_a_root() {
        assert_eq!(
            insert_style_into_drawing("<svg></svg>", "  \n"),
            "<svg></svg>"
        );
        assert_eq!(insert_style_into_drawing("no markup", RULE), "no markup");
        assert_eq!(insert_style_into_drawing("<svg", RULE), "<svg");
    }

    #[test]
    fn only_screen_styles_apply_to_a_drawing_on_screen() {
        assert!(style_applies_to_screen("", ""));
        assert!(style_applies_to_screen("", "all"));
        assert!(style_applies_to_screen("", "screen"));
        assert!(style_applies_to_screen("", "(max-width: 900px)"));
        assert!(style_applies_to_screen("", "print, screen"));
        assert!(style_applies_to_screen(
            "",
            "only screen and (min-width: 1px)"
        ));
        assert!(!style_applies_to_screen("", "print"));
        assert!(!style_applies_to_screen("", " PRINT "));
        assert!(!style_applies_to_screen("", "speech"));
        assert!(!style_applies_to_screen("", "print, speech"));
        assert!(!style_applies_to_screen("", "print and (color)"));
    }

    #[test]
    fn whitespace_between_the_words_of_a_query_is_any_whitespace() {
        assert!(style_applies_to_screen("", "only\tscreen"));
        assert!(style_applies_to_screen("", "not\nprint"));
        assert!(!style_applies_to_screen("", "not\tscreen"));
    }

    #[test]
    fn an_empty_entry_in_a_list_matches_nothing() {
        assert!(!style_applies_to_screen("", "print,"));
        assert!(!style_applies_to_screen("", ","));
        assert!(style_applies_to_screen("", "screen,"));
        assert!(style_applies_to_screen("", "   "));
    }

    #[test]
    fn a_negated_query_applies_to_a_screen_unless_it_denies_one() {
        assert!(style_applies_to_screen("", "not print"));
        assert!(style_applies_to_screen("", "not speech"));
        assert!(!style_applies_to_screen("", "not screen"));
        assert!(!style_applies_to_screen("", "not all"));
    }

    #[test]
    fn only_css_is_a_stylesheet() {
        assert!(style_applies_to_screen("text/css", ""));
        assert!(style_applies_to_screen(" TEXT/CSS ", ""));
        assert!(!style_applies_to_screen("text/less", ""));
        assert!(!style_applies_to_screen("text/x-unknown", "screen"));
    }
}
