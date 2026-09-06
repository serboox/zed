use std::ops::Range;

use schema_documents::{Document, Spans, Unreadable, pointer_to};
use taplo::dom::{KeyOrIndex, Keys};
use taplo::rowan::TextRange;

/// The document a TOML text holds, and where each part of it came from.
///
/// `None` is a text holding no document at all -- empty, or nothing but
/// blank lines and comments. A file the reader has just created has nothing
/// to check, and a complaint about it would be a complaint about every new
/// file.
pub fn read(text: &str) -> Option<Result<Document, Unreadable>> {
    if nothing_but_space_and_comments(text) {
        return None;
    }
    let parsed = taplo::parser::parse(text);
    if let Some(first) = parsed.errors.first() {
        return Some(Err(Unreadable {
            message: first.message.clone(),
            range: bytes_of(text, first.range),
        }));
    }
    let root = parsed.into_dom();
    // Every value in the tree is one the parser accepted, so this only fails
    // for a document the parser did not reject and the DOM could not
    // represent. There is nothing useful to say about such a file, and
    // saying it as a fault would put an error on a file that reads.
    let value = serde_json::to_value(&root).ok()?;

    let mut spans = Spans::default();
    if let Some(whole) = root.text_ranges(false).next() {
        spans.value("", bytes_of(text, whole));
    }
    for (keys, node) in root.flat_iter() {
        let pointer = pointer_for(&keys);
        if let Some(own) = node.text_ranges(false).next()
            && !spans.has_value(&pointer)
        {
            spans.value(&pointer, bytes_of(text, own));
        }
        // A dotted key and a table header both name the same value, and the
        // name the reader could change is the last step of the path.
        if let Some(KeyOrIndex::Key(key)) = keys.iter().next_back()
            && let Some(named) = key.text_ranges().next()
        {
            spans.name(&pointer, bytes_of(text, named));
        }
    }
    Some(Ok(Document::new(value, spans)))
}

/// The JSON pointer of a path through the document, escaped the way the
/// validator's own pointers are.
fn pointer_for(keys: &Keys) -> String {
    let mut pointer = String::new();
    for step in keys.iter() {
        match step {
            KeyOrIndex::Key(key) => {
                let deeper = pointer_to(&pointer, key.value());
                pointer = deeper;
            }
            KeyOrIndex::Index(index) => {
                pointer.push('/');
                pointer.push_str(&index.to_string());
            }
        }
    }
    pointer
}

/// A range taplo answered with, as bytes of this text.
///
/// Taplo counts in bytes -- its ranges come straight from the lexer's spans
/// -- despite its own documentation calling them character offsets. The
/// clamping is for a buffer that changed under a check already running.
pub fn bytes_of(text: &str, range: TextRange) -> Range<usize> {
    let start = (u32::from(range.start()) as usize).min(text.len());
    let end = (u32::from(range.end()) as usize).clamp(start, text.len());
    start..end
}

/// Whether a text holds no TOML at all.
pub fn nothing_but_space_and_comments(text: &str) -> bool {
    text.lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn document(text: &str) -> Document {
        match read(text) {
            Some(Ok(document)) => document,
            other => panic!("{text:?} should read: {other:?}"),
        }
    }

    fn value_at(text: &str, pointer: &str) -> String {
        let document = document(text);
        let range = document
            .value_at(pointer)
            .unwrap_or_else(|| panic!("{pointer} has bytes"));
        text[range].to_string()
    }

    #[test]
    fn a_text_holding_no_document_reads_as_nothing_rather_than_as_an_error() {
        for text in ["", "  \n\t\n", "# still thinking\n", "\n# a\n# b\n"] {
            assert!(read(text).is_none(), "{text:?}");
        }
    }

    /// The shape this crate exists for. Every pointer the validator could
    /// answer with has to find its own bytes, and a table header has to
    /// reach the value it names rather than the line it is written on.
    #[test]
    fn a_cargo_shaped_file_reads_into_a_value_with_every_part_placed() {
        let text = "[package]\nname = \"zed\"\nedition = \"2024\"\n\n\
                    [dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n\n\
                    [[bin]]\nname = \"zed\"\n";
        let document = document(text);
        assert_eq!(
            document.value["package"]["name"],
            serde_json::json!("zed"),
            "{:?}",
            document.value
        );
        assert_eq!(document.value["dependencies"]["serde"]["version"], "1");
        assert_eq!(document.value["bin"][0]["name"], "zed");

        assert_eq!(value_at(text, "/package/name"), "\"zed\"");
        assert_eq!(
            value_at(text, "/dependencies/serde/features/0"),
            "\"derive\""
        );
        assert_eq!(value_at(text, "/bin/0/name"), "\"zed\"");
    }

    #[test]
    fn a_property_name_is_the_key_the_reader_wrote() {
        let text = "[package]\nname = \"zed\"\n";
        let document = document(text);
        let range = document.name_at("/package/name").expect("the key's bytes");
        assert_eq!(&text[range], "name");
        let range = document.name_at("/package").expect("the header's bytes");
        assert_eq!(&text[range], "package");
    }

    /// A dotted key is the same value as the table header that spells it
    /// out, so it has to reach the same pointer.
    #[test]
    fn a_dotted_key_names_the_same_value_as_a_header_would() {
        let text = "package.name = \"zed\"\n";
        assert_eq!(value_at(text, "/package/name"), "\"zed\"");
    }

    #[test]
    fn a_document_that_will_not_read_says_where_the_reading_stopped() {
        let text = "[package]\nname = \n";
        match read(text) {
            Some(Err(unreadable)) => {
                assert!(!unreadable.message.is_empty());
                assert!(
                    unreadable.range.start >= 10,
                    "on the second line: {unreadable:?}"
                );
            }
            other => panic!("should not read: {other:?}"),
        }
    }

    /// The whole document has a range of its own, which is where a complaint
    /// about a missing top-level key goes.
    #[test]
    fn the_root_of_the_document_has_bytes_of_its_own() {
        let text = "name = \"zed\"\n";
        let document = document(text);
        let opening = document.opening_of(text, "").expect("the whole document");
        assert_eq!(opening, 0..1);
    }
}
