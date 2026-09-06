use std::ops::Range;

use collections::HashMap;
use schema_documents::{Document, Spans, Unreadable, pointer_to};
use serde_json::{Map, Value};
use tree_sitter::{Node, Parser, TreeCursor};

/// As deep as a document may nest. The same limit the JSON reader keeps, and
/// for the same reason: a walk over text nobody vouched for runs out of stack
/// rather than out of patience, and a stack overflow aborts the process
/// instead of unwinding.
const DEEPEST_NESTING: usize = 128;

/// Every document a YAML stream holds, or the one fault that stopped the
/// reading.
///
/// A stream is several documents where `---` separates them, and each is
/// checked against the schema on its own -- which is what the schema means:
/// it describes a document, and a file holding three of them holds three
/// documents that each have to fit it.
pub fn read(text: &str) -> Result<Vec<Document>, Unreadable> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_yaml::LANGUAGE.into())
        .is_err()
    {
        return Err(Unreadable {
            message: "the YAML grammar would not load".to_string(),
            range: 0..0,
        });
    }
    let Some(tree) = parser.parse(text, None) else {
        return Err(Unreadable {
            message: "this file is too large to read as YAML".to_string(),
            range: 0..0,
        });
    };

    // One fault, not many. Past the first place the grammar lost the thread
    // the tree is not the reader's document any more, and every further
    // complaint is about text nobody wrote.
    if let Some(fault) = first_fault(&tree.root_node(), text) {
        return Err(fault);
    }

    let mut documents = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for document in root.named_children(&mut cursor) {
        if document.kind() != "document" {
            continue;
        }
        let Some(content) = content_of(&document) else {
            // `---` with nothing after it. There is no document here to have
            // an opinion about, in the way an empty JSON file holds none.
            continue;
        };
        let mut reading = Reading {
            text,
            // Anchors are document-scoped: an anchor set in the first
            // document of a stream is not in scope in the second.
            anchors: HashMap::default(),
            spans: Spans::default(),
        };
        let value = reading.node(&content, "", 0);
        documents.push(Document::new(value, reading.spans));
    }
    Ok(documents)
}

/// The first place the grammar could not follow the text, walked in document
/// order so that it is the first one the reader would see.
fn first_fault(root: &Node<'_>, text: &str) -> Option<Unreadable> {
    if !root.has_error() && !root.is_error() {
        return None;
    }
    let mut cursor: TreeCursor<'_> = root.walk();
    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            return Some(fault_at(&node, text));
        }
        // Only a subtree that holds a fault is worth descending into.
        if node.has_error() && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return None;
            }
        }
    }
}

fn fault_at(node: &Node<'_>, text: &str) -> Unreadable {
    let range = node.byte_range();
    let message = if node.is_missing() {
        format!("`{}` is missing here", node.kind())
    } else {
        "this is not YAML the reader can follow".to_string()
    };
    // A missing node has no width at all, and a mark with no width is a mark
    // the reader cannot see.
    let end = if range.is_empty() {
        text[range.start..]
            .chars()
            .next()
            .map_or(range.start, |character| range.start + character.len_utf8())
    } else {
        range.end
    };
    Unreadable {
        message,
        range: range.start..end,
    }
}

/// What a `document`, `block_node` or `flow_node` actually holds, with the
/// anchor, tag and comments that decorate it set aside.
fn content_of<'tree>(node: &Node<'tree>) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|child| {
        !matches!(
            child.kind(),
            "anchor"
                | "tag"
                | "comment"
                | "yaml_directive"
                | "tag_directive"
                | "reserved_directive"
        )
    })
}

fn anchor_of(node: &Node<'_>, text: &str) -> Option<String> {
    let mut cursor = node.walk();
    let anchor = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "anchor")?;
    let mut inner = anchor.walk();
    let name = anchor
        .named_children(&mut inner)
        .find(|child| child.kind() == "anchor_name")?;
    text.get(name.byte_range()).map(str::to_string)
}

struct Reading<'a> {
    text: &'a str,
    anchors: HashMap<String, Value>,
    spans: Spans,
}

impl<'a> Reading<'a> {
    fn slice(&self, node: &Node<'_>) -> &'a str {
        self.text.get(node.byte_range()).unwrap_or("")
    }

    /// The bytes a node occupies, without the whitespace a block container's
    /// range runs on to include. A block mapping ends at the start of the
    /// line after it, and a mark that reaches into the next line reads as
    /// covering a line the complaint is not about.
    fn place_of(&self, node: &Node<'_>) -> Range<usize> {
        let range = node.byte_range();
        let written = self.slice(node).trim_end().len();
        range.start..range.start + written
    }

    /// The value a node holds, recording where each pointer under it came
    /// from as it goes.
    fn node(&mut self, node: &Node<'_>, pointer: &str, depth: usize) -> Value {
        if depth > DEEPEST_NESTING {
            return Value::Null;
        }
        match node.kind() {
            "block_node" | "flow_node" => {
                let anchor = anchor_of(node, self.text);
                let Some(content) = content_of(node) else {
                    return Value::Null;
                };
                let value = self.node(&content, pointer, depth);
                if let Some(name) = anchor {
                    self.anchors.insert(name, value.clone());
                }
                value
            }
            "alias" => self.alias(node, pointer, depth),
            "block_mapping" | "flow_mapping" => self.mapping(node, pointer, depth),
            "block_sequence" | "flow_sequence" => self.sequence(node, pointer, depth),
            // A `flow_pair` standing where a value belongs is the one-entry
            // mapping of `[a: 1]`, which YAML allows inside a sequence.
            "flow_pair" => self.mapping(node, pointer, depth),
            _ => {
                let value = self.scalar(node);
                let place = self.place_of(node);
                self.spans.value(pointer, place);
                value
            }
        }
    }

    /// What an alias stands for.
    ///
    /// An anchor has to be written before the alias that names it, so the
    /// value is already known by the time this runs and there is no way for
    /// an alias to stand for something containing itself.
    ///
    /// An alias naming an anchor nobody wrote records no range at all, for
    /// itself or for anything under it. Nothing the schema says about it can
    /// then be placed, so nothing is said -- which is what a reader halfway
    /// through typing `*ba` should get, rather than a file marked wrong on
    /// every keystroke.
    fn alias(&mut self, node: &Node<'_>, pointer: &str, depth: usize) -> Value {
        let mut cursor = node.walk();
        let name = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "alias_name")
            .map(|name| self.slice(&name).to_string());
        let Some(value) = name.and_then(|name| self.anchors.get(&name).cloned()) else {
            return Value::Null;
        };
        // Everything the alias brings in came from one place in this text,
        // and that place is where a complaint about any of it belongs.
        let place = self.place_of(node);
        self.record_everything(pointer, &value, place, depth);
        value
    }

    fn record_everything(
        &mut self,
        pointer: &str,
        value: &Value,
        range: Range<usize>,
        depth: usize,
    ) {
        if depth > DEEPEST_NESTING || self.spans.has_value(pointer) {
            return;
        }
        self.spans.value(pointer, range.clone());
        match value {
            Value::Object(entries) => {
                for (name, inner) in entries {
                    let inner_pointer = pointer_to(pointer, name);
                    self.spans.name(&inner_pointer, range.clone());
                    self.record_everything(&inner_pointer, inner, range.clone(), depth + 1);
                }
            }
            Value::Array(items) => {
                for (index, inner) in items.iter().enumerate() {
                    let inner_pointer = format!("{pointer}/{index}");
                    self.record_everything(&inner_pointer, inner, range.clone(), depth + 1);
                }
            }
            _ => {}
        }
    }

    fn mapping(&mut self, node: &Node<'_>, pointer: &str, depth: usize) -> Value {
        let place = self.place_of(node);
        self.spans.value(pointer, place);
        let mut read = Map::new();
        let mut merges = Vec::new();

        // A `flow_pair` standing on its own is the one-entry mapping of
        // `[a: 1]`, so it is its own only pair rather than holding a list of
        // them.
        if node.kind() == "flow_pair" {
            self.pair(node, pointer, &mut read, &mut merges, depth);
        } else {
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            for child in &children {
                match child.kind() {
                    "block_mapping_pair" | "flow_pair" => {
                        self.pair(child, pointer, &mut read, &mut merges, depth);
                    }
                    // A bare node inside `{}` is the key of `{a}`, whose
                    // value is nothing.
                    "flow_node" => {
                        if let Some(name) = self.key_name(child) {
                            let inner = pointer_to(pointer, &name);
                            let place = self.place_of(child);
                            self.spans.name(&inner, place);
                            read.insert(name, Value::Null);
                        }
                    }
                    _ => {}
                }
            }
        }

        for merge in merges {
            self.merge_into(&merge, pointer, &mut read, depth);
        }
        Value::Object(read)
    }

    /// Reads one key and its value into the mapping being built. A `<<` is
    /// set aside rather than read: a merge is applied after every key written
    /// out in full, so that an explicit key always wins over a merged one
    /// however the two are ordered.
    fn pair<'tree>(
        &mut self,
        pair: &Node<'tree>,
        pointer: &str,
        read: &mut Map<String, Value>,
        merges: &mut Vec<Node<'tree>>,
        depth: usize,
    ) {
        let Some(key) = pair.child_by_field_name("key") else {
            return;
        };
        // A key that is not a scalar -- `? [a, b]` -- has no name a JSON
        // pointer could carry, so neither the schema nor this can say
        // anything about it.
        let Some(name) = self.key_name(&key) else {
            return;
        };
        let value = pair.child_by_field_name("value");
        if name == "<<" {
            merges.extend(value);
            return;
        }
        let inner = pointer_to(pointer, &name);
        let place = self.place_of(&content_of(&key).unwrap_or(key));
        self.spans.name(&inner, place);
        let value = match value {
            Some(value) => self.node(&value, &inner, depth + 1),
            None => {
                // `key:` with nothing after it is a null value. It has no
                // bytes of its own, so the pair's own bytes stand for it.
                let place = self.place_of(pair);
                self.spans.value(&inner, place);
                Value::Null
            }
        };
        read.insert(name, value);
    }

    /// Applies one `<<` to a mapping. The value is a mapping, or a sequence
    /// of them, and a key already written out in full is never replaced.
    fn merge_into(
        &mut self,
        merge: &Node<'_>,
        pointer: &str,
        read: &mut Map<String, Value>,
        depth: usize,
    ) {
        if depth > DEEPEST_NESTING {
            return;
        }
        let range = self.place_of(merge);
        let merged = self.node(merge, &format!("{pointer}/<<"), depth + 1);
        let sources = match merged {
            Value::Object(entries) => vec![Value::Object(entries)],
            Value::Array(items) => items,
            _ => return,
        };
        for source in sources {
            let Value::Object(entries) = source else {
                continue;
            };
            for (name, value) in entries {
                if read.contains_key(&name) {
                    continue;
                }
                let inner = pointer_to(pointer, &name);
                self.spans.name(&inner, range.clone());
                self.record_everything(&inner, &value, range.clone(), depth + 1);
                read.insert(name, value);
            }
        }
    }

    fn sequence(&mut self, node: &Node<'_>, pointer: &str, depth: usize) -> Value {
        let place = self.place_of(node);
        self.spans.value(pointer, place);
        let mut read = Vec::new();
        let mut cursor = node.walk();
        for item in node.named_children(&mut cursor) {
            let inner = format!("{pointer}/{}", read.len());
            match item.kind() {
                "block_sequence_item" => {
                    let Some(held) = content_of(&item) else {
                        // A `-` with nothing after it is a null item.
                        let place = self.place_of(&item);
                        self.spans.value(&inner, place);
                        read.push(Value::Null);
                        continue;
                    };
                    read.push(self.node(&held, &inner, depth + 1));
                }
                "comment" => continue,
                _ => read.push(self.node(&item, &inner, depth + 1)),
            }
        }
        Value::Array(read)
    }

    /// The name a key is written under, for keys a JSON pointer can carry.
    ///
    /// A key that is not a string in JSON still has to be one to be a
    /// property, so a number, a boolean or a null key is named by what the
    /// reader wrote.
    fn key_name(&self, key: &Node<'_>) -> Option<String> {
        let inner = content_of(key).unwrap_or(*key);
        if !matches!(
            inner.kind(),
            "plain_scalar" | "double_quote_scalar" | "single_quote_scalar" | "block_scalar"
        ) {
            return None;
        }
        Some(match self.scalar(&inner) {
            Value::String(name) => name,
            _ => self.slice(&inner).to_string(),
        })
    }

    fn scalar(&self, node: &Node<'_>) -> Value {
        match node.kind() {
            "plain_scalar" => match content_of(node) {
                Some(inner) => self.plain(&inner),
                None => Value::String(self.slice(node).to_string()),
            },
            "double_quote_scalar" => Value::String(unescape_double_quoted(self.slice(node))),
            "single_quote_scalar" => Value::String(unescape_single_quoted(self.slice(node))),
            "block_scalar" => Value::String(block_scalar(self.slice(node))),
            "alias" => Value::Null,
            _ => self.plain(node),
        }
    }

    fn plain(&self, node: &Node<'_>) -> Value {
        let text = self.slice(node);
        match node.kind() {
            "null_scalar" => Value::Null,
            "boolean_scalar" => Value::Bool(matches!(text, "true" | "True" | "TRUE")),
            "integer_scalar" => integer(text),
            "float_scalar" => float(text),
            _ => Value::String(text.to_string()),
        }
    }
}

fn integer(text: &str) -> Value {
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, text.strip_prefix('+').unwrap_or(text)),
    };
    let parsed = if let Some(hex) = digits.strip_prefix("0x") {
        i64::from_str_radix(hex, 16).ok()
    } else if let Some(octal) = digits.strip_prefix("0o") {
        i64::from_str_radix(octal, 8).ok()
    } else {
        digits.parse::<i64>().ok()
    };
    match parsed {
        Some(value) => Value::Number((sign * value).into()),
        // Too large for an integer. A number the schema can still weigh is
        // better than nothing, and every one of these is far past the point
        // where the last digits mattered.
        None => float(text),
    }
}

fn float(text: &str) -> Value {
    match text
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
    {
        Some(number) => Value::Number(number),
        // `.inf` and `.nan` are numbers YAML has and JSON has not, so no JSON
        // Schema can describe them and nothing here can either.
        None => Value::Null,
    }
}

fn unescape_single_quoted(text: &str) -> String {
    // One quote off each end, not every quote: `''''` is a scalar holding a
    // single quote, and trimming both of its inner quotes away would leave
    // nothing.
    inside(text, '\'').replace("''", "'")
}

fn inside(text: &str, quote: char) -> &str {
    text.strip_prefix(quote)
        .and_then(|rest| rest.strip_suffix(quote))
        .unwrap_or(text)
}

fn unescape_double_quoted(text: &str) -> String {
    let inner = inside(text, '"');
    if !inner.contains('\\') {
        return inner.to_string();
    }
    let mut read = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            read.push(character);
            continue;
        }
        match characters.next() {
            Some('0') => read.push('\0'),
            Some('a') => read.push('\u{7}'),
            Some('b') => read.push('\u{8}'),
            Some('t' | '\t') => read.push('\t'),
            Some('n') => read.push('\n'),
            Some('v') => read.push('\u{b}'),
            Some('f') => read.push('\u{c}'),
            Some('r') => read.push('\r'),
            Some('e') => read.push('\u{1b}'),
            Some('N') => read.push('\u{85}'),
            Some('_') => read.push('\u{a0}'),
            Some('L') => read.push('\u{2028}'),
            Some('P') => read.push('\u{2029}'),
            // A backslash at the end of a line joins it to the next one.
            Some('\n') => {}
            Some('x') => read.push(hex(&mut characters, 2)),
            Some('u') => read.push(hex(&mut characters, 4)),
            Some('U') => read.push(hex(&mut characters, 8)),
            Some(other) => read.push(other),
            None => {}
        }
    }
    read
}

/// What a numeric escape stands for. One that is not a character stands for
/// the replacement character: what it is worth is a question for whoever
/// wrote it, not for a schema.
fn hex(characters: &mut std::str::Chars<'_>, digits: usize) -> char {
    let mut read = String::with_capacity(digits);
    for _ in 0..digits {
        match characters.next() {
            Some(digit) => read.push(digit),
            None => break,
        }
    }
    u32::from_str_radix(&read, 16)
        .ok()
        .and_then(char::from_u32)
        .unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// The text a `|` or `>` block holds.
///
/// The header says which of the two it is, whether to keep the newlines the
/// text ends with (`+`), drop them (`-`) or keep one, and may name the
/// indentation outright; where it does not, the first line that has any sets
/// it.
///
/// Folding joins the lines of a `>` block: a single break between two lines
/// becomes a space, and a run of blank lines becomes one newline fewer than
/// the breaks it is made of. A line indented past the block keeps its own
/// line, and so does the line after it.
fn block_scalar(text: &str) -> String {
    let mut lines = text.lines();
    let header = lines.next().unwrap_or("");
    let folded = header.starts_with('>');
    let keep = header.contains('+');
    let strip = header.contains('-');
    let named_indent = header
        .chars()
        .find(char::is_ascii_digit)
        .and_then(|digit| digit.to_digit(10))
        .map(|digit| digit as usize);

    let body: Vec<&str> = lines.collect();
    let indent = named_indent.unwrap_or_else(|| {
        body.iter()
            .find(|line| !line.trim().is_empty())
            .map_or(0, |line| line.len() - line.trim_start().len())
    });

    let mut read = String::new();
    let mut written_anything = false;
    let mut blank_lines = 0usize;
    let mut the_line_before_was_more_indented = false;
    for line in &body {
        let content = if line.len() >= indent {
            &line[indent..]
        } else {
            line.trim_start()
        };
        if content.trim().is_empty() {
            blank_lines += 1;
            continue;
        }
        let more_indented = content.starts_with([' ', '\t']);
        if written_anything {
            let breaks = blank_lines + 1;
            if folded && breaks == 1 && !more_indented && !the_line_before_was_more_indented {
                read.push(' ');
            } else if folded {
                read.push_str(&"\n".repeat(breaks.saturating_sub(1).max(1)));
            } else {
                read.push_str(&"\n".repeat(breaks));
            }
        }
        read.push_str(content);
        written_anything = true;
        blank_lines = 0;
        the_line_before_was_more_indented = more_indented;
    }

    if !written_anything {
        return String::new();
    }
    if strip {
        read
    } else if keep {
        // The break that ended the last line, and every blank line after it.
        read.push_str(&"\n".repeat(blank_lines + 1));
        read
    } else {
        read.push('\n');
        read
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn one(text: &str) -> Document {
        let mut documents = read(text).expect("this reads");
        assert_eq!(documents.len(), 1, "one document");
        documents.remove(0)
    }

    fn marked<'a>(text: &'a str, document: &Document, pointer: &str) -> &'a str {
        let range = document
            .value_at(pointer)
            .unwrap_or_else(|| panic!("no range for {pointer}"));
        &text[range]
    }

    fn named<'a>(text: &'a str, document: &Document, pointer: &str) -> &'a str {
        let range = document
            .name_at(pointer)
            .unwrap_or_else(|| panic!("no name range for {pointer}"));
        &text[range]
    }

    #[test]
    fn a_block_document_reads_to_the_value_it_describes() {
        let text = "\
name: zed
tab_size: 2
ratio: 1.5
vim_mode: true
missing:
theme: One Dark
tags:
  - a
  - 2
nested:
  key: value
";
        assert_eq!(
            one(text).value,
            json!({
                "name": "zed",
                "tab_size": 2,
                "ratio": 1.5,
                "vim_mode": true,
                "missing": null,
                "theme": "One Dark",
                "tags": ["a", 2],
                "nested": {"key": "value"},
            })
        );
    }

    #[test]
    fn flow_style_reads_to_the_same_value_as_the_json_it_looks_like() {
        let text = "a: {b: 1, c: [x, y]}\n";
        assert_eq!(one(text).value, json!({"a": {"b": 1, "c": ["x", "y"]}}));
    }

    /// The span is the point of reading it this way. A value's range covers
    /// the value and nothing else, and a key's name is kept apart from it,
    /// because a complaint about a key belongs on the key.
    #[test]
    fn every_value_and_every_key_keeps_the_bytes_it_came_from() {
        let text = "name: zed\ntags:\n  - a\n  - 20\n";
        let document = one(text);

        assert_eq!(marked(text, &document, "/name"), "zed");
        assert_eq!(named(text, &document, "/name"), "name");
        assert_eq!(marked(text, &document, "/tags"), "- a\n  - 20");
        assert_eq!(marked(text, &document, "/tags/0"), "a");
        assert_eq!(marked(text, &document, "/tags/1"), "20");
        assert_eq!(named(text, &document, "/tags"), "tags");
    }

    /// A quoted scalar is worth what it says, and is written where the quotes
    /// are: the value drops them and the range keeps them, because the quotes
    /// are part of what the reader would have to change.
    #[test]
    fn a_quoted_scalar_drops_its_quotes_from_the_value_and_keeps_them_in_the_range() {
        let text = "a: \"two\\nlines\"\nb: 'it''s'\nc: \"caf\\u00e9\"\n";
        let document = one(text);
        assert_eq!(
            document.value,
            json!({"a": "two\nlines", "b": "it's", "c": "caf\u{e9}"})
        );
        assert_eq!(marked(text, &document, "/a"), "\"two\\nlines\"");
        assert_eq!(marked(text, &document, "/b"), "'it''s'");

        // One quote comes off each end, not every quote there is: `''''`
        // holds a single quote and `''` holds nothing.
        assert_eq!(
            one("a: ''''\nb: ''\nc: \"\"\n").value,
            json!({"a": "'", "b": "", "c": ""})
        );
    }

    #[test]
    fn a_block_scalar_is_kept_literally_and_a_folded_one_is_joined() {
        let text = "\
literal: |
  line one
  line two
folded: >
  line one
  line two
stripped: |-
  only
";
        let document = one(text);
        assert_eq!(
            document.value,
            json!({
                "literal": "line one\nline two\n",
                "folded": "line one line two\n",
                "stripped": "only",
            })
        );
    }

    /// Folding is not simply joining every line with a space: a blank line
    /// stands for a paragraph break and a line indented past the block keeps
    /// its own line. A `>` block read as a `|` block with spaces would change
    /// what the file says.
    #[test]
    fn folding_keeps_the_breaks_a_folded_block_is_meant_to_keep() {
        let text = "\
paragraphs: >
  one

  two
indented: >
  flowing
   kept
  flowing again
";
        assert_eq!(
            one(text).value,
            json!({
                "paragraphs": "one\ntwo\n",
                "indented": "flowing\n kept\nflowing again\n",
            })
        );
    }

    /// A key that has no value written after it is null, and the pair's own
    /// bytes stand for the value that is not there -- so a complaint about it
    /// still lands on the line the reader wrote.
    #[test]
    fn a_key_with_nothing_after_it_is_null_and_still_has_a_place() {
        let text = "empty:\nother: 1\n";
        let document = one(text);
        assert_eq!(document.value, json!({"empty": null, "other": 1}));
        assert_eq!(marked(text, &document, "/empty"), "empty:");
    }

    /// An anchor names a value, and an alias stands for it. What the alias
    /// brings in is checked like anything else, and every complaint about it
    /// lands on the alias -- which is the only place in this text the reader
    /// could change.
    #[test]
    fn an_alias_stands_for_what_its_anchor_named_and_is_marked_where_it_is_written() {
        let text = "base: &base\n  a: 1\n  b: 2\ncopy: *base\n";
        let document = one(text);
        assert_eq!(
            document.value,
            json!({"base": {"a": 1, "b": 2}, "copy": {"a": 1, "b": 2}})
        );
        assert_eq!(marked(text, &document, "/copy"), "*base");
        assert_eq!(
            marked(text, &document, "/copy/a"),
            "*base",
            "there is nowhere else in this text to point at"
        );
        assert_eq!(
            marked(text, &document, "/base/a"),
            "1",
            "and the anchored value itself is still marked where it was written"
        );
    }

    /// An anchor has to be written before the alias naming it, so an alias
    /// can never stand for something holding itself. Nothing here recurses,
    /// and the unresolved alias is simply nothing.
    #[test]
    fn an_alias_naming_an_anchor_that_holds_it_resolves_to_nothing_rather_than_recurring() {
        let text = "a: &a\n  inner: *a\n";
        let document = one(text);
        assert_eq!(document.value, json!({"a": {"inner": null}}));
    }

    /// A reader halfway through typing `*ba` has written an alias naming
    /// nothing. It records no range, so nothing the schema says about it can
    /// be placed and nothing is said -- rather than the file turning red on
    /// every keystroke.
    #[test]
    fn an_alias_naming_nothing_is_left_unchecked_rather_than_marked() {
        let text = "copy: *nobody\n";
        let document = one(text);
        assert_eq!(document.value, json!({"copy": null}));
        assert_eq!(document.value_at("/copy"), None);
    }

    /// A merge brings in the keys of another mapping, and a key written out
    /// in full always wins over a merged one however the two are ordered.
    #[test]
    fn a_merge_key_brings_in_what_it_names_and_never_replaces_what_is_written() {
        let text = "base: &base\n  a: 1\n  b: 2\nderived:\n  <<: *base\n  b: 3\n";
        let document = one(text);
        assert_eq!(
            document.value,
            json!({
                "base": {"a": 1, "b": 2},
                "derived": {"b": 3, "a": 1},
            }),
            "`<<` is not a property of the result"
        );
        assert_eq!(
            named(text, &document, "/derived/a"),
            "*base",
            "a merged key is written where the merge is"
        );
        assert_eq!(marked(text, &document, "/derived/b"), "3");
    }

    #[test]
    fn a_merge_of_several_mappings_takes_the_first_that_names_a_key() {
        let text = "\
one: &one
  a: 1
two: &two
  a: 2
  b: 2
merged:
  <<: [*one, *two]
";
        assert_eq!(
            one(text).value.pointer("/merged").cloned(),
            Some(json!({"a": 1, "b": 2}))
        );
    }

    /// A stream is several documents, and the schema describes a document --
    /// so each is read on its own, with its own ranges and its own anchors.
    #[test]
    fn a_stream_holds_one_document_per_separator_and_anchors_do_not_cross_them() {
        let text = "a: &x 1\nb: *x\n---\nc: *x\n";
        let documents = read(text).expect("this reads");
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].value, json!({"a": 1, "b": 1}));
        assert_eq!(
            documents[1].value,
            json!({"c": null}),
            "an anchor from the document before is out of scope"
        );
        let key = documents[1]
            .name_at("/c")
            .expect("the second document's key");
        assert_eq!(
            key.start,
            text.find("c: *x").expect("the second document"),
            "and the second document's ranges are into the same text"
        );
        assert_eq!(&text[key], "c");
    }

    #[test]
    fn a_text_holding_no_document_reads_to_no_documents_rather_than_a_fault() {
        for text in ["", "  \n ", "# still thinking\n", "---\n"] {
            assert!(read(text).expect("this reads").is_empty(), "{text:?}");
        }
    }

    /// One fault, at the place it is. A file mid-edit is unreadable most of
    /// the time, and a pile of consequences buries the cause.
    #[test]
    fn a_document_that_will_not_read_is_one_fault_where_the_reading_stopped() {
        let text = "a: 1\n  b: 2\nc: 3\n";
        let unreadable = read(text).expect_err("this does not read");
        assert_eq!(unreadable.range.start, 0);
        assert!(!unreadable.range.is_empty(), "a mark that can be seen");

        let text = "a: \"unterminated\nb: 2\n";
        let unreadable = read(text).expect_err("this does not read");
        assert_eq!(&text[unreadable.range], "\"unterminated");
    }

    /// Nesting deep enough to run the walk out of stack is capped instead,
    /// because a stack overflow aborts the process rather than unwinding.
    #[test]
    fn nesting_deeper_than_the_walk_takes_is_capped_rather_than_overflowing() {
        fn depth_of(value: &Value) -> usize {
            match value {
                Value::Object(entries) => 1 + entries.values().map(depth_of).max().unwrap_or(0),
                _ => 0,
            }
        }

        let deep: String = (0..DEEPEST_NESTING + 32)
            .map(|level| format!("{}a:\n", "  ".repeat(level)))
            .collect();
        let document = one(&deep);
        assert!(
            depth_of(&document.value) <= DEEPEST_NESTING + 1,
            "the walk stops at its own cap rather than following the document"
        );

        // And what is nested only a little still reads, so the cap is not
        // simply refusing everything.
        let shallow = format!("a: {}1{}\n", "[".repeat(8), "]".repeat(8));
        assert!(one(&shallow).value_at("/a/0/0").is_some());
    }

    #[test]
    fn a_key_that_is_not_a_string_is_named_by_what_the_reader_wrote() {
        let text = "1: one\ntrue: yes\n\"quoted\": three\n";
        let document = one(text);
        assert_eq!(
            document.value,
            json!({"1": "one", "true": "yes", "quoted": "three"})
        );
        assert_eq!(named(text, &document, "/quoted"), "\"quoted\"");
    }

    /// A key holding a slash is one key, not a path. The validator escapes it
    /// in its own pointers, and a reader that did not would look the range up
    /// under the wrong name and mark the wrong bytes.
    #[test]
    fn a_key_holding_a_slash_is_escaped_the_way_the_validator_escapes_it() {
        let text = "a/b: 1\n";
        let document = one(text);
        assert_eq!(marked(text, &document, "/a~1b"), "1");
    }

    /// YAML has numbers JSON has not, and no JSON Schema can describe one.
    #[test]
    fn a_number_json_cannot_hold_is_read_as_nothing() {
        assert_eq!(
            one("a: .inf\nb: .nan\nc: 0x10\n").value,
            json!({"a": null, "b": null, "c": 16})
        );
    }
}
