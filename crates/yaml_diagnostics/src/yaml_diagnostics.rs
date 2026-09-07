use std::ops::Range;

use gpui::{App, actions};
use jsonschema::Validator;
use schema_documents::{
    Complaint, Reads, diagnostics_from, diagnostics_from_source, unreadable_is_one_complaint,
    what_the_schema_said,
};

mod completing;
mod reading;

pub use reading::read;

actions!(
    yaml_diagnostics,
    [
        /// Checks this YAML buffer for faults and against the schema that
        /// covers it, and shows what does not fit, without a language
        /// server.
        Validate
    ]
);

/// The id these diagnostics are filed under. One apart from every other
/// source that is not a server: the SQL validator, the Rust compiler, the Go
/// one, ruff and JSON.
const YAML_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1005);

/// What a fault found here is attributed to, shown beside each one. The
/// grammar says these rather than a schema, so they do not carry the schema's
/// own source.
const SOURCE: &str = "yaml";

static YAML: Reads = Reads {
    languages: &["YAML"],
    server_id: YAML_SERVER_ID,
    diagnostics: diagnostics_for,
    faults: Some(faults),
    associations: json_schema_store::all_schema_file_associations,
    fetch: json_schema_store::handle_schema_request,
};

pub fn init(cx: &mut App) {
    completing::init(cx);
    cx.observe_new(|workspace: &mut workspace::Workspace, _, cx| {
        let project = workspace.project().clone();
        let watching = schema_documents::watch(&YAML, workspace, cx);
        workspace.register_action(move |_, _: &Validate, _, cx| {
            schema_documents::recheck_everything(&YAML, &watching, &project, cx);
        });
    })
    .detach();
}

/// Every fault the grammar finds in this text.
///
/// This is what a YAML file gets where no schema covers it, which is nearly
/// every one of them: a `docker-compose.yml` in a project nobody registered a
/// schema for, a Kubernetes manifest, a fixture. A file the grammar cannot
/// follow is the mistake worth showing whether or not anything describes what
/// it should hold.
pub fn faults(text: &str) -> Vec<lsp::Diagnostic> {
    diagnostics_from_source(text, what_the_grammar_said(text), SOURCE)
}

/// The one place the grammar lost the thread, placed on the bytes the reading
/// stopped at.
///
/// One, not many: past the first such place everything further is a
/// consequence of the first mistake rather than a second one.
pub fn what_the_grammar_said(text: &str) -> Vec<(Range<usize>, Complaint)> {
    match read(text) {
        Err(unreadable) => vec![unreadable_is_one_complaint(unreadable)],
        Ok(_) => Vec::new(),
    }
}

/// Everything the schema has to say about a YAML text, as diagnostics the
/// editor can show.
///
/// A stream holding several documents is several documents to check, and each
/// one is checked on its own: the schema describes a document, so a file with
/// three of them has three chances to disagree with it.
///
/// A text that will not read yields nothing here: [`faults`] has already said
/// where the reading stopped, and past that place the value is not the
/// reader's value any more, so everything the schema would say about it is
/// about a document nobody wrote.
pub fn diagnostics_for(text: &str, validator: &Validator) -> Vec<lsp::Diagnostic> {
    let Ok(documents) = read(text) else {
        return Vec::new();
    };
    diagnostics_from(
        text,
        documents
            .iter()
            .flat_map(|document| what_the_schema_said(text, document, validator))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use schema_documents::validator_for;
    use serde_json::{Value, json};

    fn a_schema() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "tab_size": { "type": "integer" },
                "theme": { "enum": ["One Dark", "One Light"] },
                "tags": { "type": "array", "items": { "type": "string" } },
            },
        })
    }

    fn checked_against(schema: &Value, text: &str) -> Vec<lsp::Diagnostic> {
        let validator = validator_for(schema).expect("the schema builds");
        diagnostics_for(text, &validator)
    }

    fn checked(text: &str) -> Vec<lsp::Diagnostic> {
        checked_against(&a_schema(), text)
    }

    /// Where the complaint lands is the whole point: a message with the wrong
    /// range is a squiggle under working YAML. Every case below asserts the
    /// place, not only the count.
    fn only(text: &str) -> (lsp::Range, String) {
        let mut said = checked(text);
        assert_eq!(said.len(), 1, "{said:?}");
        let diagnostic = said.remove(0);
        (diagnostic.range, diagnostic.message)
    }

    fn range(from: (u32, u32), to: (u32, u32)) -> lsp::Range {
        lsp::Range {
            start: lsp::Position::new(from.0, from.1),
            end: lsp::Position::new(to.0, to.1),
        }
    }

    #[test]
    fn a_document_that_satisfies_its_schema_says_nothing() {
        let said = checked("name: zed\ntab_size: 2\ntheme: One Dark\ntags:\n  - a\n");
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn a_value_of_the_wrong_type_is_marked_on_that_value_and_nothing_else() {
        let (where_it_is, message) = only("name: zed\ntab_size: two\n");
        assert_eq!(
            where_it_is,
            range((1, 10), (1, 13)),
            "`two` and not the key"
        );
        assert!(message.contains("integer"), "{message}");
    }

    /// The value is nested, so the pointer the validator answers with is
    /// nested too. A reader that only handled the top level would put this on
    /// the sequence instead of on the item inside it.
    #[test]
    fn a_wrong_value_inside_a_sequence_is_marked_on_the_item_and_not_the_sequence() {
        let (where_it_is, _) = only("name: zed\ntags:\n  - a\n  - 2\n  - c\n");
        assert_eq!(where_it_is, range((3, 4), (3, 5)), "the `2` alone");
    }

    /// The property is missing, so it has no bytes of its own. The first
    /// character of the mapping that should have held it does -- not the
    /// whole mapping, which here is the entire file.
    #[test]
    fn a_missing_required_property_is_marked_on_the_start_of_its_mapping() {
        let (where_it_is, message) = only("tab_size: 2\n");
        assert_eq!(where_it_is, range((0, 0), (0, 1)));
        assert!(message.contains("name"), "{message}");
    }

    /// The validator points this at the mapping and names the property
    /// separately, so placing it takes the extra step. Getting that step
    /// wrong puts the mark on the whole file instead of on the typo.
    #[test]
    fn a_property_the_schema_forbids_is_marked_on_the_key_the_reader_wrote() {
        let (where_it_is, message) = only("name: zed\ntabsize: 2\n");
        assert_eq!(where_it_is, range((1, 0), (1, 7)), "`tabsize` alone");
        assert_eq!(message, "`tabsize` is not a property this schema allows");
    }

    #[test]
    fn a_value_outside_the_schemas_list_is_marked_on_that_value() {
        let (where_it_is, message) = only("name: zed\ntheme: Solarized\n");
        assert_eq!(where_it_is, range((1, 7), (1, 16)));
        assert!(message.contains("One Dark"), "{message}");
    }

    /// The report a file gets when nothing describes what it should hold,
    /// which is nearly every YAML file in a project. Asked for the way the
    /// harness asks for it, because the wiring is the thing that was missing:
    /// the grammar has always found this fault and nobody ever asked it.
    fn report_without_a_schema(text: &str) -> Vec<lsp::Diagnostic> {
        let faults = YAML
            .faults
            .expect("YAML reports the faults its own grammar finds");
        faults(text)
    }

    /// A file mid-edit, which is what a file is most of the time, and one no
    /// schema covers. The fault is an error rather than a warning: nothing
    /// the reader wrote past it is being read at all.
    #[test]
    fn a_file_the_grammar_could_not_follow_says_so_even_where_no_schema_covers_it() {
        let mut report = report_without_a_schema("name: zed\n  tab_size: 2\ntheme: One Dark\n");
        assert_eq!(report.len(), 1, "{report:?}");
        let diagnostic = report.remove(0);
        assert_eq!(
            diagnostic.severity,
            Some(lsp::DiagnosticSeverity::ERROR),
            "and it is the kind that stops work"
        );
        assert_eq!(diagnostic.source.as_deref(), Some("yaml"));
        assert_eq!(diagnostic.range.start, lsp::Position::new(0, 0));
        assert_ne!(
            diagnostic.range.start, diagnostic.range.end,
            "a visible mark"
        );
    }

    /// The other half of the promise: a file that reads is left alone. A
    /// warning on a working file no schema covers would be worse than saying
    /// nothing at all.
    #[test]
    fn a_file_that_reads_and_no_schema_covers_gets_an_empty_report() {
        for text in [
            "name: zed\ntab_size: 2\n",
            "# the name\nname: zed\n",
            "name: zed\n---\nname: other\n",
            "",
            "  \n ",
            "# still thinking\n",
            "---\n",
        ] {
            let report = report_without_a_schema(text);
            assert!(report.is_empty(), "{text:?}: {report:?}");
        }
    }

    /// The whole promise of a report that replaces itself: a file the reader
    /// has just fixed hands back an empty report, and an empty report is what
    /// clears the underlining the mistake left behind. Silence would leave the
    /// old marks on the screen.
    #[test]
    fn a_file_that_was_fixed_hands_back_an_empty_report_rather_than_nothing() {
        let broken = "name: zed\n  tab_size: 2\n";
        assert!(
            !report_without_a_schema(broken).is_empty(),
            "the second line is indented under a scalar"
        );

        let report = report_without_a_schema("name: zed\ntab_size: 2\n");
        assert!(
            report.is_empty(),
            "an empty report, which is what replaces the last one: {report:?}"
        );
    }

    /// A file the grammar rejected gets the grammar's fault and no schema
    /// complaints at all: the schema would otherwise have an opinion about
    /// every part of the half document a broken file leaves behind.
    #[test]
    fn a_file_the_grammar_rejected_gets_no_schema_complaints() {
        let broken = "name: zed\n  tab_size: 2\ntheme: One Dark\n";
        assert!(checked(broken).is_empty(), "{:?}", checked(broken));
        assert!(
            !report_without_a_schema(broken).is_empty(),
            "the fault is still reported"
        );
    }

    /// The grammar counts bytes, and the protocol is told UTF-16 code units.
    /// On a line holding a two-byte character and an emoji, bytes, characters
    /// and code units are three different numbers, and reading one as another
    /// puts every mark on that line in the wrong column.
    #[test]
    fn the_grammar_counts_bytes_and_the_protocol_is_told_utf16_code_units() {
        let text = "caf\u{e9}\u{1f980}: \"unterminated\nb: 2\n";
        let stray = text
            .find('"')
            .expect("the quote that opens the string nothing closes");

        let before = &text[..stray];
        assert_eq!(before.len(), 11, "bytes");
        assert_eq!(before.chars().count(), 7, "characters");
        assert_eq!(before.encode_utf16().count(), 8, "UTF-16 units");

        let said = what_the_grammar_said(text);
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].0.start, stray, "the grammar named the byte");

        let report = report_without_a_schema(text);
        assert_eq!(
            report[0].range.start,
            lsp::Position::new(0, 8),
            "the protocol's own unit -- not 11, and not 7 either: {report:?}"
        );
    }

    /// The schema is not satisfied by any of this, and none of it is said:
    /// there is no document here to have an opinion about yet.
    #[test]
    fn a_text_holding_no_document_says_nothing_rather_than_that_it_is_wrong() {
        for text in ["", "  \n ", "# still thinking\n", "---\n"] {
            assert!(checked(text).is_empty(), "{text:?}");
        }
    }

    /// Comments are not the schema's business. They do not change the value,
    /// so a commented file and the same file without its comments must be
    /// judged identically -- and the surviving complaint must still land on
    /// the right bytes of the commented text.
    #[test]
    fn comments_are_not_schema_violations() {
        let commented = "# the name\nname: zed # still the name\ntab_size: 2\n";
        assert!(checked(commented).is_empty(), "{:?}", checked(commented));

        let (where_it_is, _) = only("# the name\nname: zed\ntab_size: two\n");
        assert_eq!(where_it_is, range((2, 10), (2, 13)));
    }

    /// The schema describes a document, and a stream holds several of them.
    /// Each is measured against it on its own, and a complaint about the
    /// second is placed in the second.
    #[test]
    fn every_document_of_a_stream_is_checked_against_the_schema_on_its_own() {
        let said = checked("name: zed\n---\nname: zed\ntab_size: 2\n");
        assert!(said.is_empty(), "{said:?}");

        let (where_it_is, message) = only("name: zed\n---\nname: 2\n");
        assert_eq!(where_it_is, range((2, 6), (2, 7)));
        assert!(message.contains("string"), "{message}");
    }

    /// A merge is not a property. Reading `<<` as one would put a warning on
    /// every file that shares a block of settings between jobs, which is most
    /// of the YAML anyone writes by hand.
    #[test]
    fn a_merge_key_brings_in_what_it_names_and_is_not_itself_a_property() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "defaults": { "type": "object" },
                "job": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "name": { "type": "string" },
                        "tab_size": { "type": "integer" },
                    },
                },
            },
        });
        let text = "defaults: &d\n  tab_size: two\njob:\n  <<: *d\n  name: zed\n";
        let said = checked_against(&schema, text);
        assert_eq!(
            said.len(),
            1,
            "`<<` itself is not complained about: {said:?}"
        );
        assert_eq!(
            said[0].range,
            range((3, 6), (3, 8)),
            "what the merge brought in is wrong, and `*d` is where to fix it"
        );
        assert!(said[0].message.contains("integer"), "{:?}", said[0].message);
    }

    /// An anchored value that fits and an alias to it that fits are both
    /// silent, and a schema violation reached through an alias is placed on
    /// the alias -- the only place in the text the reader could change.
    #[test]
    fn a_violation_reached_through_an_alias_is_marked_on_the_alias() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": { "type": "object", "properties": { "n": { "type": "integer" } } },
                "b": { "type": "object", "properties": { "n": { "type": "integer" } } },
            },
        });
        let text = "a: &x\n  n: two\nb: *x\n";
        let said = checked_against(&schema, text);
        // The validator answers in its own order, so what matters is which
        // places were marked rather than in which order.
        let places: Vec<lsp::Range> = said.iter().map(|said| said.range).collect();
        assert_eq!(places.len(), 2, "{said:?}");
        assert!(
            places.contains(&range((1, 5), (1, 8))),
            "`two` where it is written: {places:?}"
        );
        assert!(
            places.contains(&range((2, 3), (2, 5))),
            "and `*x` for the copy: {places:?}"
        );
    }
}
