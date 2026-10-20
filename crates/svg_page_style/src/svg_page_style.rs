use std::collections::HashMap;

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

/// What the page's stylesheets say, made fit for a drawing that is rasterised on
/// its own, where nothing computes a custom property or an inherited colour for it:
/// every `var()` outside a string or `url()` is replaced by the value the page's
/// root (`:root`, `html`, `body` or `*`) gives the custom property, or by its
/// fallback, and the colour `currentColor` means is handed to the drawing: the
/// `computed_color` the engine worked out for it, or, without one, a `color` the
/// root gives. A custom property set anywhere else, and a `var()` with neither a
/// value nor a fallback, are left as they were.
pub fn resolve_page_style(css: &str, computed_color: Option<&str>) -> String {
    let css = strip_comments(css);
    let root = RootDeclarations::collect(&css);
    let properties = root.custom_properties();
    let resolved = substitute_variables(&css, &properties, 0);
    let color = match computed_color {
        Some(color) => color.to_string(),
        None => match root.value("color") {
            Some(color) => substitute_variables(&color, &properties, 0),
            None => return resolved,
        },
    };
    if color.contains("var(") || is_css_wide_keyword_or_current_color(&color) {
        return resolved;
    }
    format!("svg {{ color: {color} }}\n{resolved}")
}

const MOST_NESTED_VARIABLES: usize = 8;

fn is_css_wide_keyword_or_current_color(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "inherit" | "initial" | "unset" | "revert" | "revert-layer" | "currentcolor"
    )
}

/// The weight of a declaration in the cascade: `!important` beats all, then a more
/// specific selector, and of two that tie the later rule wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Weight {
    important: bool,
    specificity: u8,
    position: usize,
}

/// What the page's rules say about the two elements every drawing inherits from.
/// The drawing sits somewhere under `body`, so a value `body` has of its own, from
/// `body` or `*`, hides what `html` passes down.
struct RootDeclarations {
    html: HashMap<String, (Weight, String)>,
    body: HashMap<String, (Weight, String)>,
}

impl RootDeclarations {
    fn collect(css: &str) -> Self {
        let mut root = Self {
            html: HashMap::new(),
            body: HashMap::new(),
        };
        let mut position = 0;
        for rule in top_level_rules(css) {
            for selector in rule.selectors.split(',') {
                let selector = selector.trim().to_ascii_lowercase();
                let for_html = specificity_for_html(&selector);
                let for_body = specificity_for_body(&selector);
                if for_html.is_none() && for_body.is_none() {
                    continue;
                }
                for (name, value, important) in declarations(rule.block) {
                    position += 1;
                    for (declared, specificity) in
                        [(&mut root.html, for_html), (&mut root.body, for_body)]
                    {
                        let Some(specificity) = specificity else {
                            continue;
                        };
                        let weight = Weight {
                            important,
                            specificity,
                            position,
                        };
                        let stronger = declared.get(&name).is_none_or(|(held, _)| weight > *held);
                        if stronger {
                            declared.insert(name.clone(), (weight, value.clone()));
                        }
                    }
                }
            }
        }
        root
    }

    fn value(&self, name: &str) -> Option<String> {
        self.body
            .get(name)
            .or_else(|| self.html.get(name))
            .map(|(_, value)| value.clone())
    }

    fn custom_properties(&self) -> HashMap<String, String> {
        self.html
            .iter()
            .chain(self.body.iter())
            .filter(|(name, _)| name.starts_with("--"))
            .map(|(name, _)| (name.clone(), self.value(name).unwrap_or_default()))
            .collect()
    }
}

fn specificity_for_html(selector: &str) -> Option<u8> {
    match selector {
        "*" => Some(0),
        "html" => Some(1),
        ":root" => Some(10),
        "html:root" | ":root:root" => Some(11),
        _ => None,
    }
}

fn specificity_for_body(selector: &str) -> Option<u8> {
    match selector {
        "*" => Some(0),
        "body" => Some(1),
        _ => None,
    }
}

/// The characters of `text` that are not in a string, with the offsets they are at.
/// A backslash makes the character after it part of what it is in, in a string or
/// out of one.
fn outside_strings(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    text.char_indices().filter(move |(_, character)| {
        if escaped {
            escaped = false;
            return false;
        }
        if *character == '\\' {
            escaped = true;
            return false;
        }
        match quote {
            Some(open) => {
                if *character == open {
                    quote = None;
                }
                false
            }
            None if matches!(character, '"' | '\'') => {
                quote = Some(*character);
                false
            }
            None => true,
        }
    })
}

/// The length of the unquoted address `text` starts with, up to and including the
/// `)` that ends it; a backslash makes the `)` after it part of the address.
fn unquoted_url_length(text: &str) -> usize {
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == ')' {
            return index + 1;
        }
    }
    text.len()
}

/// The stylesheet without its comments, each of which is replaced by a space. What
/// looks like a comment inside a string or an unquoted address is part of it.
fn strip_comments(css: &str) -> String {
    let mut stripped = String::with_capacity(css.len());
    let mut index = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut previous: Option<char> = None;
    while let Some(character) = css[index..].chars().next() {
        let rest = &css[index..];
        let mut length = character.len_utf8();
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == open {
                quote = None;
            }
            stripped.push(character);
        } else if matches!(character, '"' | '\'') {
            quote = Some(character);
            stripped.push(character);
        } else if let Some(inside) = rest.strip_prefix("/*") {
            length = 2 + inside.find("*/").map_or(inside.len(), |end| end + 2);
            stripped.push(' ');
        } else if !previous.is_some_and(is_part_of_a_name)
            && starts_with_ignoring_case(rest, "url(")
            && !rest[4..].trim_start().starts_with(['"', '\''])
        {
            length = unquoted_url_length(rest);
            stripped.push_str(&rest[..length]);
        } else {
            stripped.push(character);
        }
        previous = css[index..index + length].chars().next_back();
        index += length;
    }
    stripped
}

struct Rule<'a> {
    selectors: &'a str,
    block: &'a str,
}

/// The style rules of a stylesheet that are not inside an at-rule.
fn top_level_rules(css: &str) -> Vec<Rule<'_>> {
    let mut rules = Vec::new();
    let mut rest = css;
    while let Some(open) = outside_strings(rest)
        .find(|(_, character)| *character == '{')
        .map(|(index, _)| index)
    {
        let Some(length) = balanced_block_length(&rest[open..]) else {
            break;
        };
        // What comes before the last `;` of the prelude belongs to an at-rule
        // without a block, such as `@import`.
        let prelude = rest[..open].rsplit(';').next().unwrap_or_default().trim();
        if !prelude.starts_with('@') && !prelude.is_empty() {
            rules.push(Rule {
                selectors: prelude,
                block: &rest[open + 1..open + length - 1],
            });
        }
        rest = &rest[open + length..];
    }
    rules
}

/// The length of the `{ ... }` block `text` starts with, braces included.
fn balanced_block_length(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, character) in outside_strings(text) {
        match character {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The `name: value` pairs of a declaration block, each with whether it is
/// `!important`. Names are lowercased unless they are custom properties.
fn declarations(block: &str) -> Vec<(String, String, bool)> {
    let mut pairs = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, character) in outside_strings(block) {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                pairs.extend(declaration(&block[start..index]));
                start = index + 1;
            }
            _ => {}
        }
    }
    pairs.extend(declaration(&block[start..]));
    pairs
}

fn declaration(text: &str) -> Option<(String, String, bool)> {
    let (name, value) = text.split_once(':')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let name = if name.starts_with("--") {
        name.to_string()
    } else {
        name.to_ascii_lowercase()
    };
    let value = value.trim();
    let without_flag = value
        .len()
        .checked_sub("important".len())
        .filter(|start| value.is_char_boundary(*start))
        .filter(|start| value[*start..].eq_ignore_ascii_case("important"))
        .and_then(|start| value[..start].trim_end().strip_suffix('!'));
    match without_flag {
        Some(rest) => Some((name, rest.trim_end().to_string(), true)),
        None => Some((name, value.to_string(), false)),
    }
}

fn substitute_variables(text: &str, properties: &HashMap<String, String>, depth: usize) -> String {
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut previous: Option<char> = None;
    while let Some(character) = text[index..].chars().next() {
        let rest = &text[index..];
        let starts_a_function = !previous.is_some_and(is_part_of_a_name);
        let mut length = character.len_utf8();
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == open {
                quote = None;
            }
            output.push(character);
        } else if matches!(character, '"' | '\'') {
            quote = Some(character);
            output.push(character);
        } else if starts_a_function && starts_with_ignoring_case(rest, "url(") {
            // What is inside `url()` is an address, not a value.
            length = matching_parenthesis(&rest[3..]).map_or(1, |close| 3 + close + 1);
            output.push_str(&rest[..length]);
        } else if starts_a_function && starts_with_ignoring_case(rest, "var(") {
            match matching_parenthesis(&rest[3..]) {
                Some(close) => {
                    length = 3 + close + 1;
                    let call = &rest[..length];
                    match resolve_variable(&rest[4..3 + close], properties, depth) {
                        Some(value) => output.push_str(&value),
                        None => output.push_str(call),
                    }
                }
                None => output.push(character),
            }
        } else {
            output.push(character);
        }
        previous = text[index..index + length].chars().next_back();
        index += length;
    }
    output
}

/// The value of the `var()` whose arguments are `arguments`, or `None` when there is
/// neither a value nor a fallback or the chain of variables is too deep.
fn resolve_variable(
    arguments: &str,
    properties: &HashMap<String, String>,
    depth: usize,
) -> Option<String> {
    if depth >= MOST_NESTED_VARIABLES {
        return None;
    }
    let (name, fallback) = match top_level_comma(arguments) {
        Some(comma) => (
            arguments[..comma].trim(),
            Some(arguments[comma + 1..].trim()),
        ),
        None => (arguments.trim(), None),
    };
    properties
        .get(name)
        .map(|value| value.as_str())
        .or(fallback)
        .map(|value| substitute_variables(value, properties, depth + 1))
}

fn starts_with_ignoring_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn is_part_of_a_name(character: char) -> bool {
    character.is_alphanumeric() || character == '-' || character == '_'
}

/// The offset of the `)` that closes the `(` `text` starts with.
fn matching_parenthesis(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, character) in outside_strings(text) {
        match character {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn top_level_comma(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, character) in outside_strings(text) {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return Some(index),
            _ => {}
        }
    }
    None
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

    fn resolve(css: &str) -> String {
        resolve_page_style(css, None)
    }

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

    #[test]
    fn a_variable_the_root_sets_is_replaced_where_it_is_used() {
        let css = ":root { --brand: #4a154b; --gap: 4px } svg .box { fill: var(--brand); stroke-width: var(--gap) }";
        let resolved = resolve(css);
        assert!(resolved.contains("fill: #4a154b"), "{resolved}");
        assert!(resolved.contains("stroke-width: 4px"), "{resolved}");
        assert!(!resolved.contains("var("), "{resolved}");
    }

    #[test]
    fn a_fallback_stands_in_for_a_variable_nobody_set() {
        let resolved =
            resolve("svg .a { fill: var(--missing, #123) } svg .b { fill: var(--missing) }");
        assert!(resolved.contains("fill: #123"), "{resolved}");
        assert!(resolved.contains("fill: var(--missing)"), "{resolved}");
    }

    #[test]
    fn variables_that_use_variables_are_followed_and_a_loop_stops() {
        let css = "html { --a: var(--b); --b: var(--c, #0f0); --x: var(--y); --y: var(--x) } svg path { fill: var(--a); stroke: var(--x) }";
        let resolved = resolve(css);
        assert!(resolved.contains("fill: #0f0"), "{resolved}");
        assert!(resolved.contains("stroke: var("), "{resolved}");
    }

    #[test]
    fn only_the_roots_variables_count_and_at_rules_are_left_alone() {
        let css = ".dark { --brand: red } @media (min-width: 1px) { :root { --brand: blue } } \
                   svg .box { fill: var(--brand, green) }";
        let resolved = resolve(css);
        assert!(resolved.contains("fill: green"), "{resolved}");
        assert!(resolved.contains("@media (min-width: 1px)"), "{resolved}");
    }

    #[test]
    fn the_roots_colour_becomes_the_colour_of_the_drawing() {
        let resolved = resolve("body { color: #223344 } svg text { fill: currentColor }");
        assert!(resolved.starts_with("svg { color: #223344 }"), "{resolved}");
        assert!(resolve("svg text { fill: red }").starts_with("svg text"));
    }

    #[test]
    fn comments_strings_and_semicolons_in_values_do_not_confuse_it() {
        let css = "/* :root { --a: 1 } */ :root { --font: \"a;b\", serif; --n: 2 } svg text { font-family: var(--font); letter-spacing: var(--n) }";
        let resolved = resolve(css);
        assert!(
            resolved.contains("font-family: \"a;b\", serif"),
            "{resolved}"
        );
        assert!(resolved.contains("letter-spacing: 2"), "{resolved}");
        assert!(!resolved.contains("--a"), "{resolved}");
    }

    #[test]
    fn a_comment_marker_inside_a_string_or_an_address_is_not_a_comment() {
        let css =
            "svg text { font-family: \"a/*b*/c\"; fill: url(data:x/*y*/z) } :root { --a: red }";
        let resolved = resolve(css);
        assert!(resolved.contains("\"a/*b*/c\""), "{resolved}");
        assert!(resolved.contains("url(data:x/*y*/z)"), "{resolved}");
    }

    #[test]
    fn an_escaped_parenthesis_does_not_end_an_address() {
        let css = ":root { --x: red } svg { background: url(foo\\)/*var(--x)*/); fill: var(--x) }";
        let resolved = resolve(css);
        assert!(resolved.contains("url(foo\\)/*var(--x)*/)"), "{resolved}");
        assert!(resolved.contains("fill: red"), "{resolved}");
    }

    #[test]
    fn a_var_inside_a_string_or_an_address_is_not_a_variable() {
        let css = ":root { --a: red } svg text { content: \"var(--a)\"; fill: url(var(--a)); stroke: var(--a) }";
        let resolved = resolve(css);
        assert!(resolved.contains("content: \"var(--a)\""), "{resolved}");
        assert!(resolved.contains("fill: url(var(--a))"), "{resolved}");
        assert!(resolved.contains("stroke: red"), "{resolved}");
    }

    #[test]
    fn a_name_that_ends_in_var_is_not_a_function_call() {
        let resolved =
            resolve(":root { --a: red } svg path { fill: myvar(--a); stroke: VAR(--a) }");
        assert!(resolved.contains("fill: myvar(--a)"), "{resolved}");
        assert!(resolved.contains("stroke: red"), "{resolved}");
    }

    #[test]
    fn the_stronger_declaration_of_a_variable_wins() {
        let later = resolve(":root { --a: red } :root { --a: blue } svg .x { fill: var(--a) }");
        assert!(later.contains("fill: blue"), "{later}");

        let more_specific =
            resolve(":root { --a: red } html { --a: blue } svg .x { fill: var(--a) }");
        assert!(more_specific.contains("fill: red"), "{more_specific}");

        let important =
            resolve(":root { --a: red ! IMPORTANT } :root { --a: blue } svg .x { fill: var(--a) }");
        assert!(important.contains("fill: red"), "{important}");
        assert!(!important.contains("important"), "{important}");
    }

    #[test]
    fn what_body_sets_hides_what_html_passes_down() {
        let css = ":root { --a: red; color: #111111 } body { --a: blue; color: #222222 } svg .x { fill: var(--a) }";
        let resolved = resolve(css);
        assert!(resolved.contains("fill: blue"), "{resolved}");
        assert!(resolved.starts_with("svg { color: #222222 }"), "{resolved}");

        let from_html = resolve("html { color: #111111 } body { margin: 0 }");
        assert!(
            from_html.starts_with("svg { color: #111111 }"),
            "{from_html}"
        );

        let everything = resolve("html { color: #111111 } * { color: #333333 }");
        assert!(
            everything.starts_with("svg { color: #333333 }"),
            "{everything}"
        );
    }

    #[test]
    fn the_colour_the_engine_computed_for_the_drawing_wins_over_the_roots() {
        let css = "body { color: #111111 } svg text { fill: currentColor }";
        let computed = resolve_page_style(css, Some("rgb(1, 2, 3)"));
        assert!(
            computed.starts_with("svg { color: rgb(1, 2, 3) }"),
            "{computed}"
        );
        assert_eq!(computed.matches("svg { color:").count(), 1, "{computed}");

        let without_any_root = resolve_page_style("svg text { fill: red }", Some("rgb(1, 2, 3)"));
        assert!(
            without_any_root.starts_with("svg { color: rgb(1, 2, 3) }"),
            "{without_any_root}"
        );

        let deferring = resolve_page_style(css, Some("currentcolor"));
        assert!(deferring.starts_with("body"), "{deferring}");
    }

    #[test]
    fn a_colour_that_only_defers_is_not_handed_to_the_drawing() {
        for deferring in ["inherit", "currentColor", "unset", "var(--nobody)"] {
            let resolved = resolve(&format!(
                "body {{ color: {deferring} }} svg {{ fill: red }}"
            ));
            assert!(resolved.starts_with("body"), "{deferring}: {resolved}");
        }
    }

    #[test]
    fn a_stylesheet_is_read_in_one_pass() {
        let mut css = String::from(":root { --a: red }\n");
        for index in 0..30_000 {
            css.push_str(&format!(
                "svg .c{index} {{ fill: var(--a); stroke: \"x\" }}\n"
            ));
        }
        let resolved = resolve(&css);
        assert!(
            resolved.contains(".c29999 { fill: red;"),
            "{}",
            &resolved[resolved.len() - 80..]
        );
    }

    #[test]
    fn an_unterminated_input_is_returned_without_panicking() {
        let _ = resolve(":root { --a: 1 ");
        let _ = resolve("svg { fill: var(--a");
        let _ = resolve("/* never closed");
        let _ = resolve("}}}{{{");
    }
}
