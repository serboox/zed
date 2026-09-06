use std::ops::Range;

use gpui::{App, actions};
use jsonschema::Validator;
use schema_documents::{Complaint, Reads, diagnostics_from, diagnostics_from_source};

mod completing;
mod formatting;
mod reading;

pub use reading::read;

actions!(
    toml_diagnostics,
    [
        /// Checks this TOML buffer for faults and against the schema that
        /// covers it, and shows what does not fit, without a language
        /// server.
        Validate
    ]
);

/// The id these diagnostics are filed under. One apart from every other
/// source that is not a server: the SQL validator, the Rust compiler, the Go
/// one, ruff, JSON, YAML, oxlint and clang.
const TOML_SERVER_ID: language::LanguageServerId = language::LanguageServerId(usize::MAX - 1008);

/// What a fault found here is attributed to, shown beside each one. The
/// parser says these rather than a schema, so they do not carry the schema's
/// own source.
const SOURCE: &str = "toml";

static TOML: Reads = Reads {
    languages: &["TOML"],
    server_id: TOML_SERVER_ID,
    diagnostics: diagnostics_for,
    faults: Some(faults),
    associations: json_schema_store::all_schema_file_associations,
    fetch: json_schema_store::handle_schema_request,
};

pub fn init(cx: &mut App) {
    completing::init(cx);
    formatting::init(cx);
    cx.observe_new(|workspace: &mut workspace::Workspace, _, cx| {
        let project = workspace.project().clone();
        let watching = schema_documents::watch(&TOML, workspace, cx);
        workspace.register_action(move |_, _: &Validate, _, cx| {
            schema_documents::recheck_everything(&TOML, &watching, &project, cx);
        });
    })
    .detach();
}

/// Every fault the parser and the TOML specification find in this text.
///
/// This is what a TOML file gets where no schema covers it, which is nearly
/// every one of them: `Cargo.toml`, `mise.toml` and `pyproject.toml` are
/// read by the grammar alone, and a file that will not parse is the mistake
/// worth showing whether or not anything describes what it should hold.
pub fn faults(text: &str) -> Vec<lsp::Diagnostic> {
    diagnostics_from_source(text, what_taplo_said(text), SOURCE)
}

/// Everything the schema has to say about this text, as diagnostics the
/// editor can show.
///
/// A text that will not parse yields nothing here: [`faults`] has already
/// said where the parsing stopped, and the schema's opinion of the half
/// document behind that is about a document nobody wrote.
pub fn diagnostics_for(text: &str, validator: &Validator) -> Vec<lsp::Diagnostic> {
    let Some(Ok(document)) = read(text) else {
        return Vec::new();
    };
    diagnostics_from(
        text,
        schema_documents::what_the_schema_said(text, &document, validator),
    )
}

/// Everything wrong with this text that needs no schema to see, placed on
/// the bytes it is about.
///
/// Every parse fault is reported rather than only the first: the parser
/// recovers and keeps going, so a second fault twenty lines down is a
/// separate mistake and not a consequence of the first. The specification's
/// own rules -- a key defined twice, a table that is not a table -- are
/// checked only where the text parsed, because a partial tree's complaints
/// are about a document that was never written.
pub fn what_taplo_said(text: &str) -> Vec<(Range<usize>, Complaint)> {
    if reading::nothing_but_space_and_comments(text) {
        return Vec::new();
    }
    let parsed = taplo::parser::parse(text);
    if !parsed.errors.is_empty() {
        let mut said: Vec<(Range<usize>, Complaint)> = Vec::new();
        for error in &parsed.errors {
            let range = visible(text, reading::bytes_of(text, error.range));
            if said.iter().any(|(already, _)| *already == range) {
                continue;
            }
            said.push((
                range,
                Complaint {
                    message: error.message.clone(),
                    is_a_fault: true,
                },
            ));
        }
        return said;
    }
    let root = parsed.into_dom();
    let Err(faults) = root.validate() else {
        return Vec::new();
    };
    faults.filter_map(|fault| place(text, &fault)).collect()
}

/// Where one of the specification's own rules was broken, and what to say
/// about it.
fn place(text: &str, fault: &taplo::dom::Error) -> Option<(Range<usize>, Complaint)> {
    use taplo::dom::Error as Fault;

    let (range, message) = match fault {
        // The first of the two keys stays as it is: the second is the one
        // the reader wrote last and the one they would delete.
        Fault::ConflictingKeys { key, other } => (
            key.text_ranges().next()?,
            format!("`{}` is already defined in this document", other.value()),
        ),
        Fault::ExpectedTable {
            not_table,
            required_by,
        } => (
            required_by.text_ranges().next()?,
            format!(
                "`{}` is not a table, so `{}` cannot be defined under it",
                not_table.value(),
                required_by.value()
            ),
        ),
        Fault::ExpectedArrayOfTables {
            not_array_of_tables,
            required_by,
        } => (
            required_by.text_ranges().next()?,
            format!(
                "`{}` is not an array of tables, so `{}` cannot be added to it",
                not_array_of_tables.value(),
                required_by.value()
            ),
        ),
        Fault::InvalidEscapeSequence { string } => (
            string.text_range(),
            "this string holds an escape TOML does not define".to_string(),
        ),
        Fault::UnexpectedSyntax { syntax } => (
            syntax.text_range(),
            "TOML does not allow this here".to_string(),
        ),
        // Nothing here queries the document, so nothing here can fail that
        // way.
        Fault::Query(_) => return None,
    };
    Some((
        visible(text, reading::bytes_of(text, range)),
        Complaint {
            message,
            is_a_fault: true,
        },
    ))
}

/// A range wide enough to see. Taplo answers with an empty range for a fault
/// it found inside a token -- a bad escape in a string -- and a diagnostic
/// nothing is drawn under is a diagnostic the reader never finds.
fn visible(text: &str, range: Range<usize>) -> Range<usize> {
    if range.start != range.end {
        return range;
    }
    let width = text
        .get(range.start..)
        .and_then(|rest| rest.chars().next())
        .map_or(0, char::len_utf8);
    range.start..(range.start + width).min(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use schema_documents::validator_for;
    use serde_json::json;

    const CARGO: &str = "[package]\n\
                         name = \"zed\"\n\
                         version = \"0.1.0\"\n\
                         edition = \"2024\"\n\
                         \n\
                         [dependencies]\n\
                         anyhow = \"1\"\n\
                         serde = { version = \"1\", features = [\"derive\"] }\n\
                         \n\
                         [[bin]]\n\
                         name = \"zed\"\n\
                         path = \"src/main.rs\"\n";

    /// The file this crate exists for. Nothing is said about it, because
    /// there is nothing wrong with it -- and a warning on a working
    /// `Cargo.toml` would be worse than saying nothing at all.
    #[test]
    fn a_cargo_shaped_file_that_parses_is_left_alone() {
        assert!(faults(CARGO).is_empty(), "{:?}", faults(CARGO));
    }

    #[test]
    fn a_text_holding_no_toml_says_nothing_rather_than_that_it_is_wrong() {
        for text in ["", "  \n\t\n", "# still thinking\n"] {
            assert!(faults(text).is_empty(), "{text:?}");
        }
    }

    /// A file mid-edit, which is what a file is most of the time. The fault
    /// is an error rather than a warning: nothing the reader wrote below it
    /// is being read at all.
    #[test]
    fn a_file_the_parser_could_not_read_says_so_where_the_reading_stopped() {
        let text = "[package]\nname = \n";
        let said = faults(text);
        assert!(!said.is_empty(), "the value is missing");
        assert_eq!(said[0].severity, Some(lsp::DiagnosticSeverity::ERROR));
        assert_eq!(said[0].source.as_deref(), Some("toml"));
        assert_eq!(said[0].range.start.line, 1, "{said:?}");
        assert_ne!(
            said[0].range.start, said[0].range.end,
            "a mark nothing is drawn under is a mark nobody finds"
        );
    }

    /// The parser accepts this and the specification does not, so it comes
    /// from the second pass rather than the first. Marked on the second
    /// spelling, which is the one to delete.
    #[test]
    fn a_key_defined_twice_is_a_fault_on_the_second_one() {
        let text = "name = \"zed\"\nname = \"other\"\n";
        assert!(
            taplo::parser::parse(text).errors.is_empty(),
            "the parser has no objection -- this is the specification's rule"
        );

        let said = faults(text);
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(
            said[0].range,
            lsp::Range {
                start: lsp::Position::new(1, 0),
                end: lsp::Position::new(1, 4),
            },
            "the second `name` and nothing else"
        );
        assert!(said[0].message.contains("name"), "{}", said[0].message);
    }

    /// A table header written twice is the same mistake spelled differently.
    #[test]
    fn a_table_defined_twice_is_a_fault_on_the_second_header() {
        let text = "[package]\nname = \"zed\"\n\n[package]\nversion = \"1\"\n";
        let said = faults(text);
        assert!(
            said.iter().any(|said| said.range.start.line == 3),
            "the second header is where to fix it: {said:?}"
        );
    }

    /// The whole promise of a report that replaces itself: a file the reader
    /// has just fixed hands back an empty report, and an empty report is
    /// what clears the underlining the mistake left behind. Silence would
    /// leave the old marks on the screen.
    #[test]
    fn a_file_that_was_fixed_hands_back_an_empty_report_rather_than_nothing() {
        let broken = "[package]\nname = \"zed\"\nname = \"zed\"\n";
        assert!(!faults(broken).is_empty(), "the key is written twice");

        let fixed = "[package]\nname = \"zed\"\nversion = \"1\"\n";
        let report = faults(fixed);
        assert!(
            report.is_empty(),
            "an empty report, which is what replaces the last one: {report:?}"
        );
    }

    /// Taplo's own documentation calls these character offsets. They are
    /// bytes, taken straight from the lexer's spans -- and on a line holding
    /// a two-byte character and an emoji, bytes, characters and the
    /// protocol's UTF-16 code units are three different numbers. Reading one
    /// as another puts every mark on that line in the wrong column.
    #[test]
    fn taplo_counts_bytes_and_the_protocol_is_told_utf16_code_units() {
        let text = "a = 1\nb = \"café 🦀\" @\n";
        let line_start = 6;
        let stray = text.find('@').expect("the token TOML has no place for");

        let before = &text[line_start..stray];
        assert_eq!(before.len(), 17, "bytes");
        assert_eq!(before.chars().count(), 13, "characters");
        assert_eq!(before.encode_utf16().count(), 14, "UTF-16 units");

        let earliest = taplo::parser::parse(text)
            .errors
            .iter()
            .map(|error| u32::from(error.range.start()) as usize)
            .min()
            .expect("the stray token is a fault");
        assert_eq!(
            earliest, stray,
            "taplo named the byte, not the character and not the code unit"
        );

        let said = faults(text);
        assert!(
            said.iter()
                .any(|said| said.range.start == lsp::Position::new(1, 14)),
            "the protocol's own unit -- not 17, and not 13 either: {said:?}"
        );
    }

    fn a_schema() -> serde_json::Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "required": ["package"],
            "properties": {
                "package": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "name": { "type": "string" },
                        "edition": { "enum": ["2015", "2018", "2021", "2024"] },
                        "rust-version": { "type": "string" },
                    },
                },
            },
        })
    }

    fn checked(text: &str) -> Vec<lsp::Diagnostic> {
        let validator = validator_for(&a_schema()).expect("the schema builds");
        diagnostics_for(text, &validator)
    }

    #[test]
    fn a_document_that_satisfies_its_schema_says_nothing() {
        let said = checked("[package]\nname = \"zed\"\nedition = \"2024\"\n");
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn a_value_of_the_wrong_type_is_marked_on_that_value_and_nothing_else() {
        let said = checked("[package]\nname = 2\n");
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(
            said[0].range,
            lsp::Range {
                start: lsp::Position::new(1, 7),
                end: lsp::Position::new(1, 8),
            },
            "the `2` alone"
        );
        assert!(said[0].message.contains("string"), "{}", said[0].message);
    }

    /// The validator points this at the table and names the key separately,
    /// so placing it takes the extra step. Getting that step wrong puts the
    /// mark on the whole file instead of on the typo.
    #[test]
    fn a_key_the_schema_forbids_is_marked_on_the_name_the_reader_wrote() {
        let said = checked("[package]\nname = \"zed\"\nrust_version = \"1.90\"\n");
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(
            said[0].range,
            lsp::Range {
                start: lsp::Position::new(2, 0),
                end: lsp::Position::new(2, 12),
            }
        );
        assert_eq!(
            said[0].message,
            "`rust_version` is not a property this schema allows"
        );
    }

    #[test]
    fn a_value_outside_the_schemas_list_is_marked_on_that_value() {
        let said = checked("[package]\nedition = \"2026\"\n");
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].range.start, lsp::Position::new(1, 10));
    }

    /// A file the parser rejected gets the parser's fault and no schema
    /// complaints at all: the schema would otherwise have an opinion about
    /// every part of the half document a broken file leaves behind.
    #[test]
    fn a_file_the_parser_rejected_gets_no_schema_complaints() {
        let broken = "[package]\nname = \n";
        assert!(checked(broken).is_empty(), "{:?}", checked(broken));
        assert!(!faults(broken).is_empty(), "the fault is still reported");
    }
}

/// The one condition that decides whether anything here is said at all.
///
/// A project with a TOML language server running already reports these
/// faults, and saying them again would underline every mistake twice. The
/// shared harness asks this before it publishes anything, so proving both
/// answers proves which files this crate speaks for.
#[cfg(test)]
mod what_a_language_server_silences {
    use std::any::Any;
    use std::path::Path;
    use std::sync::Arc;

    use gpui::{Entity, TestAppContext};
    use language::{Buffer, FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    use super::faults;

    const BROKEN: &str = "[package]\nname = \n";

    fn a_toml_language() -> Arc<Language> {
        Arc::new(Language::new(
            LanguageConfig {
                name: "TOML".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["toml".to_string()],
                    ..Default::default()
                },
                ..LanguageConfig::default()
            },
            None,
        ))
    }

    /// Opened the way a reader's own file is opened, because that is what
    /// starts whatever serves it. Whatever is returned last is only held
    /// alive: letting the buffer's handle or the registration go stops the
    /// server again, and the answer below would then be about nothing.
    async fn a_project_holding_a_broken_cargo_toml(
        with_a_language_server: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Buffer>, Box<dyn Any>) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        cx.executor().allow_parking();

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "Cargo.toml": BROKEN }))
            .await;
        let fs: Arc<dyn fs::Fs> = fs;
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let languages = project.read_with(cx, |project, _| project.languages().clone());
        languages.add(a_toml_language());
        let servers = with_a_language_server
            .then(|| languages.register_fake_lsp("TOML", FakeLspAdapter::default()));

        let (buffer, held) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/project/Cargo.toml"), cx)
            })
            .await
            .expect("the file should open");
        cx.run_until_parked();
        (project, buffer, Box::new((held, servers)))
    }

    /// With no server anywhere -- which is what this fork ships with -- the
    /// fault is ours to report, and there is one to report.
    #[gpui::test]
    async fn a_buffer_no_server_serves_is_ours_to_speak_for(cx: &mut TestAppContext) {
        let (project, buffer, _held) = a_project_holding_a_broken_cargo_toml(false, cx).await;
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer
                .language()
                .map(|language| language.name().as_ref().to_string())),
            Some("TOML".to_string()),
            "the language has to be settled, or the harness would not look at all"
        );
        assert!(
            !cx.update(|cx| schema_documents::served_by_a_language_server(&project, &buffer, cx)),
            "no server is running"
        );
        assert!(!faults(BROKEN).is_empty(), "and the file is broken");
    }

    /// With one running, nothing here is published: the report the reader
    /// sees is the server's alone.
    #[gpui::test]
    async fn a_buffer_a_language_server_serves_gets_nothing_from_here(cx: &mut TestAppContext) {
        let (project, buffer, _held) = a_project_holding_a_broken_cargo_toml(true, cx).await;
        assert!(
            cx.update(|cx| schema_documents::served_by_a_language_server(&project, &buffer, cx)),
            "a server serves this buffer, so this crate stays quiet about it"
        );
    }
}
