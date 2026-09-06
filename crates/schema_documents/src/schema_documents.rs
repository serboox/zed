use std::ops::Range;
use std::path::Path;

use collections::HashMap;
use serde::Deserialize;
use serde_json::Value;

pub mod checking;
pub mod completing;
pub mod watching;

pub use checking::{
    Complaint, SOURCE, unreadable_is_one_complaint, validator_for, what_the_schema_said,
};
pub use completing::{
    CursorContext, Place, Segment, Style, Suggestion, Suggestions, suggestions_for,
};
pub use watching::{Reads, recheck_everything, served_by_a_language_server, watch};

/// A document read from a buffer, and where each part of it came from.
///
/// A validator answers in JSON pointers -- `/languages/Rust/tab_size` -- and
/// the editor needs a byte range. Nothing but the text can convert one to the
/// other, so the reading that produces the value records both.
///
/// The same type serves JSON and YAML. A JSON Schema does not care which of
/// the two the reader wrote in, and neither does anything downstream of here:
/// what differs is only how the text becomes a value and how a pointer finds
/// its bytes, and both of those are settled by the time a document exists.
#[derive(Debug)]
pub struct Document {
    pub value: Value,
    spans: Spans,
}

/// Where each pointer's bytes are, filled in by whichever reader produced the
/// document.
#[derive(Debug, Default)]
pub struct Spans {
    /// The whole of the value at each pointer, brackets and quotes included.
    values: HashMap<String, Range<usize>>,
    /// The name of each property, kept under that property's own pointer. A
    /// complaint about a property the schema does not allow belongs on the
    /// name, and the name is not part of the value.
    names: HashMap<String, Range<usize>>,
}

impl Spans {
    pub fn value(&mut self, pointer: &str, range: Range<usize>) {
        self.values.insert(pointer.to_string(), range);
    }

    pub fn name(&mut self, pointer: &str, range: Range<usize>) {
        self.names.insert(pointer.to_string(), range);
    }

    /// Whether a pointer already has a range. A reader that expands one part
    /// of the text into several pointers -- a YAML alias, say -- uses this to
    /// leave the first, outermost range in place rather than overwriting it
    /// from deeper in the expansion.
    pub fn has_value(&self, pointer: &str) -> bool {
        self.values.contains_key(pointer)
    }
}

/// The one thing wrong with a document that could not be read at all.
///
/// One, not many: past the first unbalanced brace nothing further can be
/// said, and saying it anyway would bury the real fault under its
/// consequences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub message: String,
    pub range: Range<usize>,
}

impl Document {
    pub fn new(value: Value, spans: Spans) -> Self {
        Self { value, spans }
    }

    /// The bytes the value at this pointer occupies.
    pub fn value_at(&self, pointer: &str) -> Option<Range<usize>> {
        self.spans.values.get(pointer).cloned()
    }

    /// The bytes this property's name occupies.
    pub fn name_at(&self, pointer: &str) -> Option<Range<usize>> {
        self.spans.names.get(pointer).cloned()
    }

    /// Where to put a complaint about something a value is missing. The
    /// first character, rather than the whole value: an object that lacks a
    /// required property may be the entire file, and underlining the entire
    /// file says nothing about where to fix it.
    ///
    /// The text is needed because a YAML value may start on a character that
    /// is several bytes wide, and a range cutting one in half is not a range
    /// the protocol can be told about.
    pub fn opening_of(&self, text: &str, pointer: &str) -> Option<Range<usize>> {
        let whole = self.value_at(pointer)?;
        let width = text
            .get(whole.clone())
            .and_then(|value| value.chars().next())
            .map_or(1, char::len_utf8);
        Some(whole.start..whole.end.min(whole.start + width))
    }
}

/// The pointer of a property of the value at `parent`, escaped the way RFC
/// 6901 asks and the way the validator's own pointers are escaped, so the two
/// agree on a property named `a/b`.
pub fn pointer_to(parent: &str, name: &str) -> String {
    let mut pointer = String::with_capacity(parent.len() + name.len() + 1);
    pointer.push_str(parent);
    pointer.push('/');
    for character in name.chars() {
        match character {
            '~' => pointer.push_str("~0"),
            '/' => pointer.push_str("~1"),
            character => pointer.push(character),
        }
    }
    pointer
}

/// One rule saying which schema covers which files, as
/// [`json_schema_store::all_schema_file_associations`] writes them. It writes
/// what the language server expects, and this reads the same thing rather
/// than a second list that could disagree with it.
#[derive(Debug, Deserialize)]
pub struct Association {
    #[serde(rename = "fileMatch", default)]
    file_match: Vec<String>,
    url: String,
}

/// The schema that covers this file, or nothing where none does.
///
/// The first rule that matches wins, because the list is written specific
/// first: `tsconfig.json` is also a JSONC file, and the rule naming its own
/// schema stands ahead of the one covering JSONC in general.
pub fn schema_covering(associations: &[Association], path: &Path) -> Option<String> {
    associations
        .iter()
        .find(|association| {
            association
                .file_match
                .iter()
                .any(|pattern| covers(pattern, path))
        })
        .map(|association| association.url.clone())
}

/// Whether one `fileMatch` pattern covers a path, read the way the language
/// server this replaces reads it: a pattern with no separator is about the
/// file's name alone, and one with a separator is about the tail of the path.
/// So `package.json` covers it anywhere, and `zed/settings.json` covers it
/// only under a `zed` directory.
fn covers(pattern: &str, path: &Path) -> bool {
    if !pattern.contains('/') {
        return path
            .file_name()
            .is_some_and(|name| same(pattern, &name.to_string_lossy()));
    }
    let whole = path.to_string_lossy().replace('\\', "/");
    std::iter::once(whole.as_str())
        .chain(whole.match_indices('/').map(|(at, _)| &whole[at + 1..]))
        .any(|tail| same(pattern, tail))
}

/// Whether a pattern is this text exactly, or a glob covering it. A `*` stops
/// at a path separator, so `zed/snippets/*.json` is about that directory and
/// not about a tree below it.
fn same(pattern: &str, subject: &str) -> bool {
    if !pattern.contains(['*', '?', '[', '{']) {
        return pattern == subject;
    }
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher().is_match(subject))
        .unwrap_or(false)
}

/// What a schema said about a text, as diagnostics the editor can show.
pub fn diagnostics_from(text: &str, said: Vec<(Range<usize>, Complaint)>) -> Vec<lsp::Diagnostic> {
    diagnostics_from_source(text, said, SOURCE)
}

/// The same, for a source that is not a schema: a language whose own grammar
/// finds faults attributes them to itself, so that a reader can tell a
/// parser's verdict from a schema's opinion.
pub fn diagnostics_from_source(
    text: &str,
    said: Vec<(Range<usize>, Complaint)>,
    source: &str,
) -> Vec<lsp::Diagnostic> {
    let lines = Lines::of(text);
    said.into_iter()
        .map(|(range, complaint)| lsp::Diagnostic {
            range: lsp::Range {
                start: lines.position_of(text, range.start),
                end: lines.position_of(text, range.end),
            },
            severity: Some(if complaint.is_a_fault {
                lsp::DiagnosticSeverity::ERROR
            } else {
                lsp::DiagnosticSeverity::WARNING
            }),
            source: Some(source.to_string()),
            message: complaint.message,
            ..Default::default()
        })
        .collect()
}

/// Where each line of a text starts, so that a byte offset can be turned into
/// the line and column the protocol wants without walking the text again for
/// every one of them.
struct Lines(Vec<usize>);

impl Lines {
    fn of(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
        Self(starts)
    }

    /// The protocol counts lines from zero and columns in UTF-16 code units,
    /// which is neither the byte offset the reader works in nor the character
    /// count either of them looks like. On the line `  "caf\u{e9} \u{1f980}": 1`
    /// the colon sits at byte 14, at character 10, and at UTF-16 unit 11.
    fn position_of(&self, text: &str, offset: usize) -> lsp::Position {
        let line = self.0.partition_point(|start| *start <= offset).max(1) - 1;
        let start = self.0.get(line).copied().unwrap_or(0);
        let character = text.get(start..offset).unwrap_or("").encode_utf16().count();
        lsp::Position {
            line: line as u32,
            character: character as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn associations(value: serde_json::Value) -> Vec<Association> {
        serde_json::from_value(value).expect("the rules parse")
    }

    /// The real list, in the order the store writes it. `tsconfig.json` is
    /// covered twice -- by its own rule and by the JSONC one, which claims it
    /// as a path suffix -- and the specific rule stands first, so first match
    /// wins is what puts the right schema on it.
    #[test]
    fn the_first_rule_that_covers_a_file_is_the_one_that_wins() {
        let rules = associations(json!([
            {"fileMatch": ["tsconfig.json"], "url": "zed://schemas/tsconfig"},
            {"fileMatch": ["*.jsonc", "tsconfig.json"], "url": "zed://schemas/jsonc"},
        ]));
        assert_eq!(
            schema_covering(&rules, Path::new("/project/tsconfig.json")).as_deref(),
            Some("zed://schemas/tsconfig")
        );
        assert_eq!(
            schema_covering(&rules, Path::new("/project/a.jsonc")).as_deref(),
            Some("zed://schemas/jsonc")
        );
    }

    /// A pattern with no separator is about the file's name, so it covers the
    /// file wherever it is. One with a separator is about the tail of the
    /// path, so it does not cover a file that merely shares the name.
    #[test]
    fn a_named_path_covers_only_that_path_and_a_bare_name_covers_it_anywhere() {
        let rules = associations(json!([
            {"fileMatch": ["zed/settings.json"], "url": "zed://schemas/settings"},
            {"fileMatch": ["package.json"], "url": "zed://schemas/package_json"},
        ]));

        for path in [
            "/home/reader/.config/zed/settings.json",
            "/somewhere/else/zed/settings.json",
        ] {
            assert_eq!(
                schema_covering(&rules, Path::new(path)).as_deref(),
                Some("zed://schemas/settings"),
                "{path}"
            );
        }
        assert_eq!(
            schema_covering(&rules, Path::new("/project/settings.json")),
            None,
            "not under a `zed` directory, so not this schema"
        );
        assert_eq!(
            schema_covering(&rules, Path::new("/deep/inside/a/tree/package.json")).as_deref(),
            Some("zed://schemas/package_json")
        );
    }

    /// A file no rule covers gets nothing at all. Most JSON is like this --
    /// fixtures, lockfiles, data -- and inventing a schema for it would put
    /// warnings on files nobody ever described.
    #[test]
    fn a_file_no_rule_covers_gets_no_schema_rather_than_a_guess() {
        let rules = associations(json!([
            {"fileMatch": ["package.json"], "url": "zed://schemas/package_json"},
        ]));
        for path in ["/project/data.json", "/project/fixtures/a.json", "/a"] {
            assert_eq!(schema_covering(&rules, Path::new(path)), None, "{path}");
        }
    }

    /// The rules the store writes today name JSON files and nothing else, so
    /// a YAML file falls through every one of them. That is what makes the
    /// YAML checking silent in practice until a rule names a YAML file: the
    /// checking is right, and there is simply nothing for it to check
    /// against.
    #[test]
    fn nothing_the_store_writes_today_covers_a_yaml_file() {
        let rules = associations(json!([
            {"fileMatch": ["tsconfig.json"], "url": "zed://schemas/tsconfig"},
            {"fileMatch": ["package.json"], "url": "zed://schemas/package_json"},
            {"fileMatch": ["*.jsonc", "*.json"], "url": "zed://schemas/jsonc"},
        ]));
        for path in [
            "/project/.github/workflows/ci.yaml",
            "/project/docker-compose.yml",
            "/project/package.yaml",
        ] {
            assert_eq!(schema_covering(&rules, Path::new(path)), None, "{path}");
        }

        // And a rule that does name one reaches it, so nothing but the list
        // stands between a YAML file and its schema.
        let rules = associations(json!([
            {"fileMatch": ["*.yaml", "*.yml"], "url": "zed://schemas/whatever"},
        ]));
        assert_eq!(
            schema_covering(&rules, Path::new("/project/docker-compose.yml")).as_deref(),
            Some("zed://schemas/whatever")
        );
    }

    #[test]
    fn a_glob_covers_what_it_should_and_a_directory_of_them_too() {
        let rules = associations(json!([
            {"fileMatch": ["*.jsonc", "zed/snippets/*.json"], "url": "zed://schemas/jsonc"},
        ]));
        for path in [
            "/project/a.jsonc",
            "/home/reader/.config/zed/snippets/rust.json",
        ] {
            assert!(schema_covering(&rules, Path::new(path)).is_some(), "{path}");
        }
        assert_eq!(schema_covering(&rules, Path::new("/project/a.json")), None);
    }

    /// The protocol wants UTF-16 code units, and a file with an emoji in it
    /// disagrees with every other way of counting. Reading one as another
    /// puts every diagnostic on that line into the wrong column.
    #[test]
    fn a_byte_offset_becomes_a_line_and_a_utf16_column() {
        let text = "{\n  \"caf\u{e9} \u{1f980}\": 1\n}\n";
        let colon = text.find(':').expect("the colon");

        let before = &text[2..colon];
        assert_eq!(before.len(), 14, "bytes");
        assert_eq!(before.chars().count(), 10, "characters");
        assert_eq!(before.encode_utf16().count(), 11, "UTF-16 units");

        let lines = Lines::of(text);
        assert_eq!(lines.position_of(text, 0), lsp::Position::new(0, 0));
        assert_eq!(
            lines.position_of(text, colon),
            lsp::Position::new(1, 11),
            "the protocol's own unit -- not 14, and not 10 either"
        );
    }

    /// An offset at or past the end of the text lands at the end of it. The
    /// buffer can change under a check that is already running.
    #[test]
    fn an_offset_past_the_end_lands_at_the_end() {
        let text = "{}\n";
        let lines = Lines::of(text);
        assert_eq!(lines.position_of(text, 3), lsp::Position::new(1, 0));
        assert_eq!(lines.position_of(text, 9_000), lsp::Position::new(1, 0));
        assert_eq!(
            Lines::of("").position_of("", 0),
            lsp::Position::new(0, 0),
            "and an empty text has a first line like any other"
        );
    }

    /// A value that starts on a character several bytes wide still gets a
    /// whole character marked. A range cutting one in half is not a range
    /// the protocol can be told about, and the column it produces is wrong.
    #[test]
    fn the_opening_of_a_value_is_a_whole_character_however_wide_it_is() {
        let text = "ключ:\n  a: 1\n";
        let mut spans = Spans::default();
        spans.value("", 0..text.len());
        let document = Document::new(Value::Null, spans);
        let opening = document.opening_of(text, "").expect("the whole document");
        assert_eq!(opening, 0..2, "`к` is two bytes wide");
        assert!(text.is_char_boundary(opening.end));
    }
}
