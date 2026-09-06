use gpui::{App, actions};
use jsonschema::Validator;
use schema_documents::{Reads, diagnostics_from};

mod checking;
mod reading;

pub use checking::what_the_schema_said;
pub use reading::read;
pub use schema_documents::validator_for;

actions!(
    json_diagnostics,
    [
        /// Checks this JSON buffer against the schema that covers it and
        /// shows what does not fit, without a language server.
        Validate
    ]
);

/// The id these diagnostics are filed under. One apart from every other
/// source that is not a server: the SQL validator, the Rust compiler, the Go
/// one and ruff.
const JSON_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1004);

static JSON: Reads = Reads {
    languages: &["JSON", "JSONC"],
    server_id: JSON_SERVER_ID,
    diagnostics: diagnostics_for,
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

/// Everything the schema has to say about this text, as diagnostics the
/// editor can show.
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

    #[test]
    fn a_document_that_will_not_read_becomes_one_error_where_the_reading_stopped() {
        let validator = validator_for(&json!({"type": "object"})).expect("the schema builds");
        let diagnostics = diagnostics_for("{\n  \"a\": ,\n}\n", &validator);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].severity,
            Some(lsp::DiagnosticSeverity::ERROR)
        );
        assert_eq!(
            diagnostics[0].range,
            lsp::Range {
                start: lsp::Position::new(1, 7),
                end: lsp::Position::new(1, 8),
            }
        );
    }
}
