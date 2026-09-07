use gpui::{App, actions};
use jsonschema::Validator;
use schema_documents::{Reads, diagnostics_from, diagnostics_from_source};

mod checking;
mod reading;

pub use checking::{what_the_reader_said, what_the_schema_said};
pub use reading::read;
pub use schema_documents::validator_for;

actions!(
    json_diagnostics,
    [
        /// Checks this JSON buffer for faults and against the schema that
        /// covers it, and shows what does not fit, without a language
        /// server.
        Validate
    ]
);

/// The id these diagnostics are filed under. One apart from every other
/// source that is not a server: the SQL validator, the Rust compiler, the Go
/// one and ruff.
const JSON_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1004);

/// What a fault found here is attributed to, shown beside each one. The
/// reader says these rather than a schema, so they do not carry the schema's
/// own source.
const SOURCE: &str = "json";

static JSON: Reads = Reads {
    languages: &["JSON", "JSONC"],
    server_id: JSON_SERVER_ID,
    diagnostics: diagnostics_for,
    faults: Some(faults),
    associations: json_schema_store::all_schema_file_associations,
    fetch: json_schema_store::handle_schema_request,
};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _, cx| {
        let project = workspace.project().clone();
        let watching = schema_documents::watch(&JSON, workspace, cx);
        workspace.register_action(move |_, _: &Validate, _, cx| {
            schema_documents::recheck_everything(&JSON, &watching, &project, cx);
        });
    })
    .detach();
}

/// Every fault the reader finds in this text.
///
/// This is what a JSON or JSONC file gets where no schema covers it, which is
/// most of them: a lock file, a `tsconfig` in a project nobody registered a
/// schema for, a fixture. A file that will not parse is the mistake worth
/// showing whether or not anything describes what it should hold.
pub fn faults(text: &str) -> Vec<lsp::Diagnostic> {
    diagnostics_from_source(text, what_the_reader_said(text), SOURCE)
}

/// Everything the schema has to say about this text, as diagnostics the
/// editor can show.
///
/// A text that will not read yields nothing here: [`faults`] has already said
/// where the reading stopped, and the schema's opinion of the half document
/// behind that is about a document nobody wrote.
pub fn diagnostics_for(text: &str, validator: &Validator) -> Vec<lsp::Diagnostic> {
    diagnostics_from(text, what_the_schema_said(text, validator))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    /// End to end over the shape of schema this editor actually serves, with
    /// the range asserted rather than the count: this is the whole promise,
    /// and a diagnostic in the wrong place keeps it in name only.
    #[test]
    fn a_wrong_value_becomes_a_diagnostic_on_the_bytes_it_is_about() {
        let validator = validator_for(&json!({
            "type": "object",
            "properties": { "tab_size": { "type": "integer" } },
        }))
        .expect("the schema builds");

        let text = "{\n  \"tab_size\": \"two\"\n}\n";
        let diagnostics = diagnostics_for(text, &validator);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].range,
            lsp::Range {
                start: lsp::Position::new(1, 14),
                end: lsp::Position::new(1, 19),
            },
            "`\"two\"` on the second line and nothing around it"
        );
        assert_eq!(
            diagnostics[0].severity,
            Some(lsp::DiagnosticSeverity::WARNING)
        );
        assert_eq!(diagnostics[0].source.as_deref(), Some("json-schema"));
    }

    /// The report a file gets when nothing describes what it should hold,
    /// which is nearly every JSON file in a project. Asked for the way the
    /// harness asks for it, because the wiring is the thing that was missing:
    /// the reader has always found this fault and nobody ever asked it.
    fn report_without_a_schema(text: &str) -> Vec<lsp::Diagnostic> {
        let faults = JSON
            .faults
            .expect("JSON reports the faults its own reader finds");
        faults(text)
    }

    /// A file mid-edit, which is what a file is most of the time, and one no
    /// schema covers. The fault is an error rather than a warning: nothing
    /// the reader wrote past it is being read at all.
    #[test]
    fn a_file_the_reader_could_not_read_says_so_even_where_no_schema_covers_it() {
        let report = report_without_a_schema("{\n  \"a\": ,\n}\n");
        assert_eq!(report.len(), 1, "{report:?}");
        assert_eq!(report[0].severity, Some(lsp::DiagnosticSeverity::ERROR));
        assert_eq!(report[0].source.as_deref(), Some("json"));
        assert_eq!(
            report[0].range,
            lsp::Range {
                start: lsp::Position::new(1, 7),
                end: lsp::Position::new(1, 8),
            }
        );
    }

    /// The other half of the promise: a file that reads is left alone. A
    /// warning on a working file no schema covers would be worse than saying
    /// nothing at all.
    #[test]
    fn a_file_that_reads_and_no_schema_covers_gets_an_empty_report() {
        for text in [
            "{\n  \"a\": 1\n}\n",
            "{\n  // the name\n  \"a\": 1,\n}\n",
            "",
            "  \n ",
            "// still thinking\n",
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
        assert!(
            !report_without_a_schema("{\n  \"a\": ,\n}\n").is_empty(),
            "the value is missing"
        );

        let report = report_without_a_schema("{\n  \"a\": 1\n}\n");
        assert!(
            report.is_empty(),
            "an empty report, which is what replaces the last one: {report:?}"
        );
    }

    /// A file the reader rejected gets the reader's fault and no schema
    /// complaints at all: the schema would otherwise have an opinion about
    /// every part of the half document a broken file leaves behind.
    #[test]
    fn a_file_the_reader_rejected_gets_no_schema_complaints() {
        let validator = validator_for(&json!({"type": "object", "required": ["name"]}))
            .expect("the schema builds");
        let broken = "{\n  \"a\": ,\n}\n";
        let said = diagnostics_for(broken, &validator);
        assert!(said.is_empty(), "{said:?}");
        assert!(
            !report_without_a_schema(broken).is_empty(),
            "the fault is still reported"
        );
    }

    /// The reader counts bytes, and the protocol is told UTF-16 code units.
    /// On a line holding a two-byte character and an emoji, bytes, characters
    /// and code units are three different numbers, and reading one as another
    /// puts every mark on that line in the wrong column.
    #[test]
    fn the_reader_counts_bytes_and_the_protocol_is_told_utf16_code_units() {
        let text = "{\n  \"caf\u{e9} \u{1f980}\": ,\n}\n";
        let line_start = 2;
        let stray = text.find(',').expect("the `,` a value should have been");

        let before = &text[line_start..stray];
        assert_eq!(before.len(), 16, "bytes");
        assert_eq!(before.chars().count(), 12, "characters");
        assert_eq!(before.encode_utf16().count(), 13, "UTF-16 units");

        let said = what_the_reader_said(text);
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].0.start, stray, "the reader named the byte");

        let report = report_without_a_schema(text);
        assert_eq!(
            report[0].range.start,
            lsp::Position::new(1, 13),
            "the protocol's own unit -- not 16, and not 12 either: {report:?}"
        );
    }
}
