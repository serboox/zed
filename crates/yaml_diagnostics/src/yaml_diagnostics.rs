use gpui::{App, actions};
use jsonschema::Validator;
use schema_documents::{
    Reads, diagnostics_from, unreadable_is_one_complaint, what_the_schema_said,
};

mod completing;
mod reading;

pub use reading::read;

actions!(
    yaml_diagnostics,
    [
        /// Checks this YAML buffer against the schema that covers it and
        /// shows what does not fit, without a language server.
        Validate
    ]
);

/// The id these diagnostics are filed under. One apart from every other
/// source that is not a server: the SQL validator, the Rust compiler, the Go
/// one, ruff and JSON.
const YAML_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1005);

static YAML: Reads = Reads {
    languages: &["YAML"],
    server_id: YAML_SERVER_ID,
    diagnostics: diagnostics_for,
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

/// Everything the schema has to say about a YAML text, as diagnostics the
/// editor can show.
///
/// A stream holding several documents is several documents to check, and each
/// one is checked on its own: the schema describes a document, so a file with
/// three of them has three chances to disagree with it.
///
/// A text that will not read yields the one fault that stopped it, not a
/// schema complaint per line -- past the place the grammar lost the thread
/// the value is not the reader's value any more, and everything the schema
/// would say about it is about a document nobody wrote.
pub fn diagnostics_for(text: &str, validator: &Validator) -> Vec<lsp::Diagnostic> {
    let said = match read(text) {
        Ok(documents) => documents
            .iter()
            .flat_map(|document| what_the_schema_said(text, document, validator))
            .collect(),
        Err(unreadable) => vec![unreadable_is_one_complaint(unreadable)],
    };
    diagnostics_from(text, said)
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

    /// A file mid-edit is unreadable most of the time, and the schema has an
    /// opinion about every part of the half-document that results. One fault
    /// where the reader is typing is the whole of what is useful.
    #[test]
    fn a_document_that_will_not_read_is_one_fault_and_not_a_pile_of_schema_complaints() {
        let mut said = checked("name: zed\n  tab_size: 2\ntheme: One Dark\n");
        assert_eq!(said.len(), 1, "{said:?}");
        let diagnostic = said.remove(0);
        assert_eq!(
            diagnostic.severity,
            Some(lsp::DiagnosticSeverity::ERROR),
            "and it is the kind that stops work"
        );
        assert_eq!(diagnostic.range.start, lsp::Position::new(0, 0));
        assert_ne!(
            diagnostic.range.start, diagnostic.range.end,
            "a visible mark"
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
