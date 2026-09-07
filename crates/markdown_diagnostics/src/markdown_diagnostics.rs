use std::panic::AssertUnwindSafe;
use std::path::Path;

use rumdl_lib::config::{Config, RUMDL_CONFIG_FILES, SourcedConfig};
use rumdl_lib::document_run::DocumentRun;
use rumdl_lib::rule::{LintWarning, Rule, Severity};

mod watching;

pub use watching::{Lint, init};

/// The rule numbers `markdownlint` itself defines.
///
/// rumdl answers to the same numbers and adds its own above them, and those
/// extra ones are the whole reason an embedded linter starts arguing with a
/// build: a project whose pipeline runs `markdownlint` has never had an
/// opinion from MD061 or MD077, and a reader who acts on one is fixing
/// something nobody asked about. So a project that has not adopted rumdl by
/// name hears only the rules its own linter could also have raised.
///
/// Taken from `markdownlint`'s own `doc/Rules.md`, which runs MD001 to MD060
/// with the gaps its deprecated rules left. Kept as the rules to keep rather
/// than the rules to drop, so a rule rumdl adds later is silent here until
/// somebody decides otherwise.
const RULES_MARKDOWNLINT_ALSO_HAS: &[&str] = &[
    "MD001", "MD003", "MD004", "MD005", "MD007", "MD009", "MD010", "MD011", "MD012", "MD013",
    "MD014", "MD018", "MD019", "MD020", "MD021", "MD022", "MD023", "MD024", "MD025", "MD026",
    "MD027", "MD028", "MD029", "MD030", "MD031", "MD032", "MD033", "MD034", "MD035", "MD036",
    "MD037", "MD038", "MD039", "MD040", "MD041", "MD042", "MD043", "MD044", "MD045", "MD046",
    "MD047", "MD048", "MD049", "MD050", "MD051", "MD052", "MD053", "MD054", "MD055", "MD056",
    "MD058", "MD059", "MD060",
];

/// What a project says about linting its Markdown.
pub struct Configured {
    pub config: Config,
    /// Whether the project configures rumdl under its own name, rather than
    /// through a `markdownlint` file rumdl can also read or through nothing
    /// at all. A project that named rumdl wants everything rumdl has to say;
    /// see [`RULES_MARKDOWNLINT_ALSO_HAS`] for what the others hear.
    pub adopted_rumdl: bool,
}

/// The configuration covering a file, read from the project's own files.
///
/// rumdl looks for `.rumdl.toml`, `rumdl.toml`, `.config/rumdl.toml` and a
/// `pyproject.toml` holding `[tool.rumdl]`, and also reads the
/// `.markdownlint*` files `markdownlint` itself uses, from the file's own
/// directory upwards to the project root. A configuration that will not load
/// leaves the defaults in place: a reader whose config file is broken is no
/// worse off than one who has none.
pub fn configuration_for(file: &Path, project_root: &Path) -> Configured {
    let found = file
        .parent()
        .and_then(|directory| SourcedConfig::discover_config_for_dir(directory, project_root));
    let Some(found) = found else {
        return Configured {
            config: Config::default(),
            adopted_rumdl: false,
        };
    };
    let adopted_rumdl = names_rumdl(&found);
    match SourcedConfig::load_config_for_path(&found, project_root) {
        Ok(config) => Configured {
            config,
            adopted_rumdl,
        },
        Err(error) => {
            log::debug!("reading {}: {error}", found.display());
            Configured {
                config: Config::default(),
                adopted_rumdl: false,
            }
        }
    }
}

fn names_rumdl(config_file: &Path) -> bool {
    config_file
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| RUMDL_CONFIG_FILES.contains(&name))
}

/// The rules to run over one file: what the configuration asks for, narrowed
/// to what `markdownlint` could also have said unless the project asked for
/// rumdl by name.
pub fn rules_for(configured: &Configured, file: Option<&Path>) -> Vec<Box<dyn Rule>> {
    let every_rule = rumdl_lib::rules::all_rules(&configured.config);
    let mut rules = rumdl_lib::rules::filter_rules(&every_rule, &configured.config.global);
    if !configured.adopted_rumdl {
        rules.retain(|rule| RULES_MARKDOWNLINT_ALSO_HAS.contains(&rule.name()));
    }
    match file {
        Some(file) => rumdl_lib::rules::filter_rules_for_file(&rules, &configured.config, file),
        None => rules,
    }
}

/// Everything the linter has to say about a Markdown text, as diagnostics the
/// editor can show.
///
/// An empty list is an answer and not silence: it is what tells the editor
/// that a file whose fault the reader has just fixed is clean now.
pub fn diagnostics_for(
    text: &str,
    rules: &[Box<dyn Rule>],
    config: &Config,
    file: Option<&Path>,
) -> Vec<lsp::Diagnostic> {
    // rumdl 0.2.66 reads a leading byte order mark as text, so a file that
    // opens with one and then a level 1 heading is told its first line is not
    // a heading (MD041). Cutting the mark off first costs one column on the
    // first line, which `place` adds back.
    let (text, byte_order_mark) = match text.strip_prefix('\u{feff}') {
        Some(after) => (after, 1),
        None => (text, 0),
    };
    let run = DocumentRun::new(text, rules, config);
    let run = match file {
        Some(file) => run.file_path(file),
        None => run,
    };
    // A rule that panics must not take the editor with it. rumdl's own
    // language server guards its rules the same way, which is the reason to
    // do it here rather than a precaution about nothing.
    let analyzed = std::panic::catch_unwind(AssertUnwindSafe(|| run.analyze()));
    let warnings = match analyzed {
        Ok(Ok(analysis)) => analysis.warnings,
        Ok(Err(error)) => {
            log::debug!("linting this Markdown: {error}");
            return Vec::new();
        }
        Err(_) => {
            log::warn!("a Markdown rule panicked; this file goes unlinted");
            return Vec::new();
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    warnings
        .iter()
        .map(|warning| advice_about(warning, &lines, byte_order_mark))
        .collect()
}

/// One finding, phrased the way a linter's opinion deserves.
fn advice_about(warning: &LintWarning, lines: &[&str], byte_order_mark: u32) -> lsp::Diagnostic {
    lsp::Diagnostic {
        range: lsp::Range {
            start: place(lines, warning.line, warning.column, byte_order_mark),
            end: place(lines, warning.end_line, warning.end_column, byte_order_mark),
        },
        severity: Some(how_much_it_matters(warning.severity)),
        code: warning.rule_name.clone().map(lsp::NumberOrString::String),
        source: Some("rumdl".to_string()),
        message: warning.message.clone(),
        ..Default::default()
    }
}

/// How much one finding matters, keeping the distinction rumdl draws and
/// stopping a step short of calling any of it a fault.
///
/// rumdl grades its own rules, and the grades are not interchangeable: a
/// heading that skipped a level or an image with no alt text it calls an
/// error, while an over-indented list item or a fenced block with no language
/// it calls a warning. A reader who cannot tell those apart cannot decide
/// what to fix first, which is the whole use of a grade.
///
/// Every grade still lands below `ERROR`, because nothing rumdl reports stops
/// the document from being read, published or built. So its error becomes a
/// warning and its milder grades stay advice: the order survives, and no
/// finding claims to be something that stops work.
fn how_much_it_matters(severity: Severity) -> lsp::DiagnosticSeverity {
    match severity {
        Severity::Error => lsp::DiagnosticSeverity::WARNING,
        Severity::Warning | Severity::Info => lsp::DiagnosticSeverity::HINT,
    }
}

/// Where a finding sits, counted the way the protocol counts.
///
/// Three units are in play and they disagree. On the line `Тест 🙂🙂 text   `
/// the first trailing space sits at byte 22, at character 12 and at UTF-16
/// code unit 14, all counted from zero. rumdl reports lines and columns from
/// one and counts columns in characters; the protocol wants lines from zero
/// and characters as UTF-16 code units. Only the line's own text can convert
/// one to the other, which is why this takes the lines.
///
/// An end column is one past the last character a finding covers, and a
/// finding on the last line of a document without a trailing newline can name
/// a column past the end of the line; both keep their overshoot rather than
/// being clamped, which is what rumdl's own language server does with them.
fn place(lines: &[&str], line: usize, column: usize, byte_order_mark: u32) -> lsp::Position {
    let line_index = line.saturating_sub(1);
    let characters_before = column.saturating_sub(1);
    let text = lines.get(line_index).copied().unwrap_or("");
    let mut counted = 0;
    let mut units = 0;
    for character in text.chars().take(characters_before) {
        counted += 1;
        units += character.len_utf16() as u32;
    }
    let overshoot = (characters_before - counted) as u32;
    let mark = if line_index == 0 { byte_order_mark } else { 0 };
    lsp::Position {
        line: line_index as u32,
        character: units + overshoot + mark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A document with a heading that climbs two levels at once and a list
    /// item indented by three spaces. Both are things `markdownlint` also
    /// reports, so the finding a reader sees here is one their build would
    /// have raised too.
    const WITH_FINDINGS: &str = "# Title\n\n### Skipped\n\n- one\n   - two\n";

    /// The same document with both faults fixed.
    const FIXED: &str = "# Title\n\n## Skipped\n\n- one\n  - two\n";

    /// A table and a fenced code block, which is where Markdown linters most
    /// often place a finding on the wrong line.
    const TABLE_AND_FENCE: &str = "# Table\n\n| Name | Value |\n| ---- | ----- |\n| a    | 1     |\n\n```rust\nfn main() {}\n```\n";

    /// A line holding Cyrillic text and two emoji, then three trailing
    /// spaces: the one place where a byte, a character and a UTF-16 code unit
    /// are three different numbers.
    const WITH_WIDE_CHARACTERS: &str = "# Heading\n\nТест 🙂🙂 text   \n";

    fn linted(text: &str) -> Vec<lsp::Diagnostic> {
        let configured = Configured {
            config: Config::default(),
            adopted_rumdl: false,
        };
        let rules = rules_for(&configured, None);
        diagnostics_for(text, &rules, &configured.config, None)
    }

    fn said(diagnostics: &[lsp::Diagnostic]) -> Vec<(String, lsp::Range)> {
        diagnostics
            .iter()
            .map(|diagnostic| {
                let code = match &diagnostic.code {
                    Some(lsp::NumberOrString::String(code)) => code.clone(),
                    other => format!("{other:?}"),
                };
                (code, diagnostic.range)
            })
            .collect()
    }

    fn at(line: u32, from: u32, to: u32) -> lsp::Range {
        lsp::Range {
            start: lsp::Position::new(line, from),
            end: lsp::Position::new(line, to),
        }
    }

    /// The whole promise, with the ranges asserted rather than the count: a
    /// finding in the wrong place keeps the promise in name only.
    #[test]
    fn a_document_with_faults_is_read_into_advice_where_the_faults_are() {
        let diagnostics = linted(WITH_FINDINGS);
        assert_eq!(
            said(&diagnostics),
            vec![
                ("MD001".to_string(), at(2, 0, 11)),
                ("MD007".to_string(), at(5, 0, 3)),
            ],
            "the heading on the third line and the list item on the sixth"
        );
        assert_eq!(
            diagnostics[0].message,
            "Expected heading level 2, but found heading level 3"
        );
        assert_eq!(diagnostics[0].source.as_deref(), Some("rumdl"));
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.severity)
                .collect::<Vec<_>>(),
            vec![
                // rumdl grades a heading that skipped a level an error.
                Some(lsp::DiagnosticSeverity::WARNING),
                // and an over-indented list item a warning.
                Some(lsp::DiagnosticSeverity::HINT),
            ]
        );
    }

    /// A heading that skipped a level and a fenced block with no language are
    /// not the same size of problem, and rumdl says so: it grades the first an
    /// error and the second a warning. The two arrive apart.
    #[test]
    fn the_grade_rumdl_gave_a_finding_is_kept_and_nothing_becomes_a_fault() {
        const GRADED: &str = "# Title\n\n### Skipped\n\n```\ncode\n```\n";
        let diagnostics = linted(GRADED);

        // What rumdl itself graded these, read straight from the library so
        // the expectation is not a guess about its rules.
        let configured = Configured {
            config: Config::default(),
            adopted_rumdl: false,
        };
        let rules = rules_for(&configured, None);
        let run = DocumentRun::new(GRADED, &rules, &configured.config);
        let graded: Vec<(Option<String>, Severity)> = run
            .analyze()
            .expect("the document analyses")
            .warnings
            .iter()
            .map(|warning| (warning.rule_name.clone(), warning.severity))
            .collect();
        assert!(
            graded.contains(&(Some("MD001".to_string()), Severity::Error)),
            "{graded:?}"
        );
        assert!(
            graded.contains(&(Some("MD040".to_string()), Severity::Warning)),
            "{graded:?}"
        );

        let severity_of = |wanted: &str| {
            diagnostics
                .iter()
                .find(|diagnostic| {
                    matches!(&diagnostic.code, Some(lsp::NumberOrString::String(code)) if code == wanted)
                })
                .and_then(|diagnostic| diagnostic.severity)
        };
        assert_eq!(
            severity_of("MD001"),
            Some(lsp::DiagnosticSeverity::WARNING),
            "the one rumdl graded an error"
        );
        assert_eq!(
            severity_of("MD040"),
            Some(lsp::DiagnosticSeverity::HINT),
            "the one rumdl graded a warning"
        );
        for diagnostic in &diagnostics {
            assert_ne!(
                diagnostic.severity,
                Some(lsp::DiagnosticSeverity::ERROR),
                "nothing here stops the document being read, published or built"
            );
        }
    }

    /// A document with nothing wrong with it says nothing. Without this the
    /// feature would be measured only by what it finds, and a linter that
    /// complains about correct Markdown is worse than none.
    #[test]
    fn a_document_with_nothing_wrong_reports_nothing() {
        assert_eq!(linted("# Title\n\nA paragraph.\n"), Vec::new());
    }

    /// A table and a fenced code block are read as what they are. Both are
    /// where a Markdown linter's line and column arithmetic usually goes
    /// wrong, and neither is a fault.
    #[test]
    fn a_table_and_a_fenced_block_are_not_faults() {
        assert_eq!(linted(TABLE_AND_FENCE), Vec::new());
    }

    /// A document whose faults have been fixed reports an empty list, not a
    /// shorter one and not the same one. The editor keeps what it was last
    /// given, so this empty list is what clears the last one off the screen.
    #[test]
    fn a_document_that_was_fixed_reports_an_empty_list() {
        assert_eq!(linted(WITH_FINDINGS).len(), 2);
        assert_eq!(linted(FIXED), Vec::new(), "and nothing is left behind");
    }

    /// The three numbers, all of them asserted, because the two that are
    /// wrong are the ones easy to reach for. rumdl counts characters from
    /// one; the protocol counts UTF-16 code units from zero; a byte offset is
    /// a third number again, and is what a regex or a parser hands you.
    #[test]
    fn a_column_is_a_character_count_and_becomes_a_utf16_one() {
        let line = "Тест 🙂🙂 text   ";
        assert!(line.ends_with("   "), "the line ends in three spaces");
        let trailing_spaces_at_character = line.chars().count() - 3;
        let trailing_spaces_at_byte = line.len() - 3;
        let trailing_spaces_at_utf16: u32 = line
            .chars()
            .take(trailing_spaces_at_character)
            .map(|character| character.len_utf16() as u32)
            .sum();
        assert_eq!(
            (
                trailing_spaces_at_byte,
                trailing_spaces_at_character,
                trailing_spaces_at_utf16
            ),
            (22, 12, 14),
            "the same place is byte 22, character 12 and UTF-16 unit 14"
        );

        let diagnostics = linted(WITH_WIDE_CHARACTERS);
        assert_eq!(
            said(&diagnostics),
            vec![("MD009".to_string(), at(2, 14, 17))],
            "the UTF-16 unit and not the byte or the character"
        );
    }

    /// A file that opens with a byte order mark and then a level 1 heading
    /// has a level 1 heading. rumdl 0.2.66 reads the mark as text and says
    /// otherwise, and a reader whose editor writes marks would carry that
    /// finding on every file they own.
    #[test]
    fn a_byte_order_mark_is_neither_a_fault_nor_a_shifted_column() {
        assert_eq!(linted("\u{feff}# Title\n\nA paragraph.\n"), Vec::new());

        let diagnostics = linted("\u{feff}# Title   \n\nA paragraph.\n");
        assert_eq!(
            said(&diagnostics),
            vec![("MD009".to_string(), at(0, 8, 11))],
            "the trailing spaces on the first line, counted past the mark"
        );
    }

    /// The rules rumdl has and `markdownlint` does not are what make an
    /// embedded linter argue with a build, so they wait for a project to ask
    /// for rumdl by name.
    #[test]
    fn rules_markdownlint_does_not_have_wait_for_a_project_to_adopt_rumdl() {
        let by_default = rules_for(
            &Configured {
                config: Config::default(),
                adopted_rumdl: false,
            },
            None,
        );
        let with_rumdl = rules_for(
            &Configured {
                config: Config::default(),
                adopted_rumdl: true,
            },
            None,
        );

        let extra: Vec<&str> = with_rumdl
            .iter()
            .map(|rule| rule.name())
            .filter(|name| !RULES_MARKDOWNLINT_ALSO_HAS.contains(name))
            .collect();
        assert!(
            !extra.is_empty(),
            "rumdl has rules of its own, or this narrowing is about nothing"
        );
        assert!(
            extra.contains(&"MD057"),
            "MD057 among them: it reads the filesystem to check a relative link, \
             which a buffer being written points at before it exists -- {extra:?}"
        );
        assert!(
            by_default.len() < with_rumdl.len(),
            "{} of {} rules kept",
            by_default.len(),
            with_rumdl.len()
        );
        for rule in &by_default {
            assert!(
                RULES_MARKDOWNLINT_ALSO_HAS.contains(&rule.name()),
                "{} is not a rule markdownlint has",
                rule.name()
            );
        }
    }

    /// A rumdl file is the project asking for rumdl; a `markdownlint` file is
    /// the project asking for `markdownlint`, which rumdl can read but must
    /// not answer beyond.
    #[test]
    fn only_a_rumdl_config_file_counts_as_adopting_rumdl() {
        for named in [
            "/project/.rumdl.toml",
            "/project/rumdl.toml",
            "/project/.config/rumdl.toml",
            "/project/pyproject.toml",
        ] {
            assert!(names_rumdl(Path::new(named)), "{named}");
        }
        for other in [
            "/project/.markdownlint.json",
            "/project/.markdownlint-cli2.yaml",
            "/project/markdownlint.yml",
        ] {
            assert!(!names_rumdl(Path::new(other)), "{other}");
        }
    }
}
