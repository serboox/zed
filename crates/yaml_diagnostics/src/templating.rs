use std::ops::Range;

/// A text with its Go template actions set aside, and where they were.
pub struct Masked {
    /// The same bytes, with every action replaced by something a YAML reader
    /// can follow. Byte for byte the same length as the text it came from, so
    /// every range found in it is a range in the original.
    pub text: String,
    /// Where the actions were, in that same coordinate space.
    pub actions: Vec<Range<usize>>,
}

impl Masked {
    pub fn covers(&self, range: &Range<usize>) -> bool {
        self.actions
            .iter()
            .any(|action| action.start < range.end && range.start < action.end)
    }
}

/// The same text with every `{{ ... }}` set aside, or `None` when it holds none.
///
/// A chart template is a Go template that produces YAML: until it is rendered
/// the file is not YAML at all, and a reader that insists otherwise reports
/// `name: {{ include "x" . }}` as a broken flow mapping. Setting the actions
/// aside leaves the YAML around them to be read as what it is, so a real
/// mistake in that YAML is still found.
///
/// Two kinds of action, because they mean different things to the shape of the
/// document:
///
/// * An action that is the whole of its line is control flow -- `if`, `range`,
///   `end`, `define`, a comment. It produces no text, so the line becomes
///   blank, which is allowed anywhere.
/// * An action inside a line stands where a value or a key will be, so it
///   becomes a plain scalar of the same width.
///
/// The width is kept in both cases so that a complaint about the YAML lands on
/// the bytes the reader is looking at, with no map back.
pub fn set_actions_aside(text: &str) -> Option<Masked> {
    let actions = actions_in(text);
    if actions.is_empty() {
        return None;
    }

    let mut masked = text.as_bytes().to_vec();
    for action in &actions {
        let blank = line_holds_nothing_else(text, action);
        for byte in &mut masked[action.clone()] {
            if *byte != b'\n' && *byte != b'\r' {
                *byte = if blank { b' ' } else { b'x' };
            }
        }
    }

    Some(Masked {
        // Only ASCII was written over ASCII: every byte that was part of a
        // character wider than one byte was left as it was.
        text: String::from_utf8(masked).ok()?,
        actions,
    })
}

/// Every `{{ ... }}` in the text, outermost first and never overlapping.
///
/// Go's own scanner ends an action at the first `}}` that is not inside a
/// quoted string, and so does this: `{{ printf "}}" }}` is one action, not a
/// broken one. An action that is never closed is not an action -- a file can
/// hold a literal `{{` -- so it is left alone.
fn actions_in(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut actions = Vec::new();
    let mut at = 0;

    while at + 1 < bytes.len() {
        if !(bytes[at] == b'{' && bytes[at + 1] == b'{') {
            at += 1;
            continue;
        }
        match end_of_action(bytes, at + 2) {
            Some(end) => {
                actions.push(at..end);
                at = end;
            }
            None => at += 1,
        }
    }
    actions
}

/// Where the action that opened at `from` ends, past its `}}`.
fn end_of_action(bytes: &[u8], from: usize) -> Option<usize> {
    let mut at = from;
    let mut quote: Option<u8> = None;

    while at < bytes.len() {
        let byte = bytes[at];
        match quote {
            Some(open) => {
                // A backslash escapes the next byte in an interpreted string,
                // but a raw string delimited by backticks has no escapes.
                if open == b'"' && byte == b'\\' {
                    at += 2;
                    continue;
                }
                if byte == open {
                    quote = None;
                }
            }
            None => {
                if matches!(byte, b'"' | b'\'' | b'`') {
                    quote = Some(byte);
                } else if byte == b'}' && bytes.get(at + 1) == Some(&b'}') {
                    return Some(at + 2);
                }
            }
        }
        at += 1;
    }
    None
}

/// Whether the line holding this action holds nothing else that matters.
///
/// Such an action is control flow: it produces no text of its own, so the line
/// it is on has to disappear rather than become a scalar. Several actions on
/// one line -- `{{- if .a }}{{- if .b }}` -- are still only control flow.
fn line_holds_nothing_else(text: &str, action: &Range<usize>) -> bool {
    let bytes = text.as_bytes();
    let start = bytes[..action.start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);
    let end = bytes[action.end..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |newline| action.end + newline);

    let before = &text[start..action.start];
    let after = &text[action.end..end];
    is_only_actions_and_space(before) && is_only_actions_and_space(after)
}

fn is_only_actions_and_space(part: &str) -> bool {
    let mut rest = part.trim();
    while !rest.is_empty() {
        if !rest.starts_with("{{") {
            return false;
        }
        match end_of_action(rest.as_bytes(), 2) {
            Some(end) => rest = rest[end..].trim(),
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn aside(text: &str) -> String {
        set_actions_aside(text)
            .expect("this text holds template actions")
            .text
    }

    #[test]
    fn a_text_without_actions_is_left_alone() {
        assert!(set_actions_aside("name: api\nport: 8080\n").is_none());
    }

    /// The case that started this: an action inside a value made the reader
    /// call the line a broken flow mapping.
    #[test]
    fn an_action_inside_a_line_becomes_a_scalar_of_its_own_width() {
        let masked = aside("name: {{ include \"service.name\" . }}-api\n");
        assert_eq!(masked, "name: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-api\n");
        assert_eq!(
            masked.len(),
            "name: {{ include \"service.name\" . }}-api\n".len()
        );
    }

    /// Control flow produces no text, so the line has to go rather than turn
    /// into a value: left as a scalar, `{{- if .x }}` would read as an entry
    /// with no key.
    #[test]
    fn an_action_that_is_the_whole_line_leaves_a_blank_line() {
        let masked = aside("{{- if .Values.enabled }}\nname: api\n{{- end }}\n");
        assert_eq!(
            masked,
            format!("{}\nname: api\n{}\n", " ".repeat(25), " ".repeat(10))
        );
    }

    #[test]
    fn several_actions_on_one_line_are_still_control_flow() {
        let masked = aside("{{- if .a }}{{- if .b }}\nname: api\n");
        assert_eq!(masked, "                        \nname: api\n");
    }

    /// Go ends an action at the first `}}` that is not inside a string, and a
    /// reader that ends it earlier would leave `\" }}` behind as text.
    #[test]
    fn a_brace_pair_inside_a_string_does_not_end_an_action() {
        let masked = aside("name: {{ printf \"}}\" }}\n");
        assert_eq!(masked, "name: xxxxxxxxxxxxxxxxx\n");
    }

    /// A file may hold a literal `{{` that no `}}` ever closes. It is not an
    /// action, and rewriting it would be rewriting what the reader wrote.
    #[test]
    fn an_unclosed_brace_pair_is_not_an_action() {
        assert!(set_actions_aside("note: this {{ is not closed\n").is_none());
    }

    #[test]
    fn where_the_actions_were_is_reported_in_the_original_coordinates() {
        let text = "name: {{ .Values.name }}\n";
        let masked = set_actions_aside(text).expect("actions");
        assert_eq!(masked.actions, vec![6..24]);
        assert!(masked.covers(&(6..24)));
        assert!(!masked.covers(&(0..4)));
    }

    /// Bytes wider than one are left as they are, so the text stays a string
    /// and the ranges stay true.
    #[test]
    fn text_outside_the_actions_keeps_its_own_bytes() {
        let masked = aside("note: значение\nname: {{ .x }}\n");
        assert!(masked.starts_with("note: значение\n"));
    }
}
