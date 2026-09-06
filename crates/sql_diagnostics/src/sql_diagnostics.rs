use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use sqruff_lib::core::config::{ConfigLoader, FluffConfig};
use sqruff_lib::core::linter::core::Linter;
use sqruff_lib::core::linter::linted_file::LintedFile;
use sqruff_lib_core::dialects::init::DialectKind;
use sqruff_lib_core::errors::SQLBaseError;

mod watching;

pub use watching::{Lint, init};

/// The files sqruff reads a project's configuration from, in the order it
/// reads them within one directory.
///
/// `.sqlfluff` is first, and is the same file `sqlfluff` reads in a pipeline:
/// a project that lints there needs no second configuration for this to use
/// its dialect and its rules.
pub const CONFIGURATION_FILES: [&str; 5] = [
    ".sqlfluff",
    ".sqruff",
    ".sqruff.ini",
    "pyproject.toml",
    "sqruff.toml",
];

/// What the linter is told the buffer is called. It reaches nothing but the
/// report's own path field, which this crate does not read.
const LINTED_AS: &str = "buffer.sql";

/// The templater's own code, which is not a rule that fired and not something
/// to show a reader: with no templater configured it means a `{{ }}` this
/// editor was never going to render.
const TEMPLATER: &str = "TMP";

/// The prefix of a line sqruff refuses to read.
///
/// `FluffConfig::process_raw_file_for_config` scans every linted file for a
/// line starting with this and hands it to `process_inline_config`, which is
/// `panic!("Not implemented")`. A file carrying one is left alone rather than
/// linted, because the alternative is a panic on a save.
const INLINE_CONFIGURATION: &str = "-- sqlfluff";

/// Whether the linter is asked for the parse failures it finds.
///
/// It is not. The SQL console's own validator already reports a statement
/// that will not parse, against the dialect of the connection the reader
/// would run it on, which is better information than a file's configuration
/// can give. Asked this way sqruff has nothing at all to say about SQL it
/// cannot read, which is exactly the wanted silence.
const ASK_FOR_PARSE_FAILURES: bool = false;

/// What a project's own configuration settles: which SQL its files are
/// written in, and which of sqruff's rules it has adopted.
pub struct Configured {
    linter: Linter,
}

impl Configured {
    /// The configuration this stack of files describes, or nothing at all.
    ///
    /// Nothing is the deliberate answer wherever the project has not said
    /// which SQL it writes. A dialect is not a detail here: read as ANSI, a
    /// MySQL file reports a confident list of findings about syntax MySQL
    /// spells differently, and a reader who is shown that once stops reading
    /// any of it. So an absent dialect, and one this build of sqruff does not
    /// know, both mean silence rather than a guess.
    ///
    /// The files are loaded in the order given -- root first, the file's own
    /// directory last -- which is the order sqruff itself loads them in, so
    /// the nearer file overrides the further one.
    pub fn from_files(paths: &[PathBuf]) -> Option<Self> {
        Self::from_files_asking(paths, ASK_FOR_PARSE_FAILURES)
    }

    fn from_files_asking(paths: &[PathBuf], for_parse_failures: bool) -> Option<Self> {
        if paths.is_empty() {
            return None;
        }
        without_taking_the_thread_down(|| {
            // An empty map of the library's own map type, which cannot be
            // named from here without pinning its hash map crate as a second
            // dependency on the same version.
            let mut configs = ConfigLoader::try_from_source("", None).ok()?;
            for path in paths {
                if let Err(error) = ConfigLoader::try_load_config_file(path, &mut configs) {
                    log::debug!("reading {}: {error}", path.display());
                    return None;
                }
            }
            let named = configs
                .get("core")?
                .as_map()?
                .get("dialect")?
                .as_string()?
                .to_owned();
            // Checked here rather than left to the library: an unknown name
            // reaches `DialectKind::from_str(..).unwrap()` inside
            // `FluffConfig::new`, and a project that pins a dialect this
            // build has no grammar for must be quiet, not fatal.
            DialectKind::from_str(&named).ok()?;
            let config = FluffConfig::new(configs, None, None);
            Linter::new(config, None, None, for_parse_failures)
                .map(|linter| Self { linter })
                .map_err(|error| log::debug!("configuring sqruff for {named}: {error}"))
                .ok()
        })
        .flatten()
    }

    /// The dialect this configuration settled on, which decides what sqruff
    /// reads as SQL at all before any rule is considered.
    pub fn dialect(&self) -> &'static str {
        self.linter.config().dialect_kind().name()
    }

    /// Every rule finding in this text, as diagnostics the editor can show.
    ///
    /// Syntax is not among them, and that is the point: the SQL console's own
    /// validator already reports a statement that will not parse, against the
    /// dialect of the connection the reader is running it on. Reporting it
    /// twice, in two wordings, teaches a reader to trust neither.
    pub fn findings_in(&self, sql: &str) -> Vec<lsp::Diagnostic> {
        self.everything_sqruff_said(sql)
            .iter()
            .filter_map(|violation| as_diagnostic(violation, sql))
            .collect()
    }

    /// What the linter reported, before anything is dropped. Every call into
    /// sqruff goes through here, so the two texts it will not survive are
    /// refused in one place.
    fn everything_sqruff_said(&self, sql: &str) -> Vec<SQLBaseError> {
        if sql
            .lines()
            .any(|line| line.starts_with(INLINE_CONFIGURATION))
        {
            return Vec::new();
        }
        if self.is_too_large(sql) {
            return Vec::new();
        }
        without_taking_the_thread_down(|| {
            self.linter
                .lint_string(sql, Some(LINTED_AS.to_owned()), false)
                .map_err(|error| log::debug!("linting SQL: {error}"))
                .ok()
        })
        .flatten()
        .map(LintedFile::into_violations)
        .unwrap_or_default()
    }

    /// Whether the project's own size limit puts this text out of scope.
    ///
    /// The limit is the project's `large_file_skip_byte_limit`, which the
    /// library does not enforce itself, so a pipeline running sqruff over a
    /// dump this size reports nothing about it and neither does this.
    fn is_too_large(&self, sql: &str) -> bool {
        let limit = without_taking_the_thread_down(|| {
            self.linter
                .config()
                .get("large_file_skip_byte_limit", "core")
                .as_int()
                .unwrap_or_default()
        })
        .unwrap_or_default();
        limit > 0 && sql.len() > limit as usize
    }
}

/// One finding, at a place the editor can put it, or nothing where it is not
/// this source's to report.
///
/// A violation with no rule behind it is a parse or lex failure -- the
/// library files those with `rule: None` and an empty slice -- and the SQL
/// console's validator reports those already.
fn as_diagnostic(violation: &SQLBaseError, sql: &str) -> Option<lsp::Diagnostic> {
    let rule = violation.rule.as_ref()?;
    if rule.code == TEMPLATER {
        return None;
    }
    Some(lsp::Diagnostic {
        range: lsp::Range {
            start: utf16_position_at(sql, violation.source_slice.start),
            end: utf16_position_at(sql, violation.source_slice.end),
        },
        // A rule that fired is not an error. The statement runs; the reader
        // gets to decide whether the project's style is worth the edit.
        severity: Some(lsp::DiagnosticSeverity::WARNING),
        code: Some(lsp::NumberOrString::String(rule.code.to_owned())),
        source: Some("sqruff".to_owned()),
        message: what_it_said(violation),
        ..Default::default()
    })
}

/// The whole of what one finding says: its own text, and whether the linter
/// would fix it. Whether a fix exists is the reader's next question after
/// what is wrong, and `sqruff fix` is what answers it.
fn what_it_said(violation: &SQLBaseError) -> String {
    if violation.fixable {
        format!("{}\nfix: available", violation.description)
    } else {
        violation.description.clone()
    }
}

/// The position a byte offset falls at, counted the way the protocol counts:
/// lines from zero, and characters as UTF-16 code units.
///
/// Three units are in play and they disagree. On the line
/// `select café, "🦀🔥" from t` the comma sits at byte 12, at character 11 and
/// at UTF-16 unit 11; the closing quote sits at byte 22, at character 17 and
/// at UTF-16 unit 19. sqruff's `source_slice` is in bytes -- it is used to
/// slice the source string itself -- while its `line_pos` counts characters,
/// and the protocol asks for the UTF-16 unit. Only the text can convert
/// between them, which is why this takes the text.
///
/// An offset past the end, or inside a character, lands at the nearest
/// boundary at or before it rather than panicking.
fn utf16_position_at(sql: &str, offset: usize) -> lsp::Position {
    let mut at = offset.min(sql.len());
    while at > 0 && !sql.is_char_boundary(at) {
        at -= 1;
    }
    let before = sql.get(..at).unwrap_or("");
    let line_starts_at = before.rfind('\n').map_or(0, |newline| newline + 1);
    lsp::Position {
        line: before.matches('\n').count() as u32,
        character: before
            .get(line_starts_at..)
            .unwrap_or("")
            .encode_utf16()
            .count() as u32,
    }
}

/// The configuration files that cover a file, from the root of the project
/// down to the file's own directory.
///
/// A file outside the project has none: the walk stops at the root rather
/// than climbing into a home directory, where a configuration for somebody
/// else's project would decide what this one is told.
pub fn configuration_covering(
    file: &Path,
    root: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let Some(directory) = file.parent() else {
        return Vec::new();
    };
    let mut directories = Vec::new();
    let mut walking = Some(directory);
    while let Some(current) = walking {
        if !current.starts_with(root) {
            directories.clear();
            break;
        }
        directories.push(current);
        if current == root {
            break;
        }
        walking = current.parent();
    }
    directories.reverse();
    directories
        .into_iter()
        .flat_map(|directory| CONFIGURATION_FILES.map(|name| directory.join(name)))
        .filter(|candidate| exists(candidate))
        .collect()
}

/// One call into the library, with a panic in it turned into no answer.
///
/// The library asserts on input a project can hold: an unparseable
/// configuration file, a `load_macros_from_path` it has not implemented, an
/// inline `-- sqlfluff` directive. The two of those this crate can see coming
/// are refused before they are reached; this is for the ones it cannot. No
/// findings is a smaller failure than a report of somebody else's crash.
fn without_taking_the_thread_down<T>(work: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(AssertUnwindSafe(work))
        .map_err(|_| log::warn!("sqruff gave up on a SQL buffer"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A project's own `.sqlfluff` -- the same file `sqlfluff` reads in a
    /// pipeline -- as the one-file stack that covers its SQL.
    fn project(named: &str) -> Vec<PathBuf> {
        vec![
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test_data")
                .join(named)
                .join(".sqlfluff"),
        ]
    }

    fn configured(named: &str) -> Configured {
        Configured::from_files(&project(named))
            .unwrap_or_else(|| panic!("{named} names a dialect sqruff knows"))
    }

    /// Every finding rendered as one line, so a failure shows the whole
    /// report rather than the first field that differs.
    fn reported(findings: &[lsp::Diagnostic]) -> Vec<String> {
        findings
            .iter()
            .map(|finding| {
                let code = match &finding.code {
                    Some(lsp::NumberOrString::String(code)) => code.clone(),
                    other => format!("{other:?}"),
                };
                format!(
                    "{code} {}:{}-{}:{} {}",
                    finding.range.start.line,
                    finding.range.start.character,
                    finding.range.end.line,
                    finding.range.end.character,
                    finding.message.replace('\n', " / "),
                )
            })
            .collect()
    }

    fn nothing() -> Vec<String> {
        Vec::new()
    }

    /// The central promise: a rule the project has adopted fires, and the
    /// reader is shown it on the text it is about, with the code that names
    /// it and a severity that does not paint the file red.
    #[test]
    fn a_rule_the_project_adopted_becomes_a_diagnostic_where_it_fired() {
        let findings = configured("keywords_ansi").findings_in("select 1 from t\n");

        assert_eq!(
            reported(&findings),
            vec![
                "CP01 0:0-0:6 Keywords must be upper case. / fix: available".to_owned(),
                "CP01 0:9-0:13 Keywords must be upper case. / fix: available".to_owned(),
            ]
        );
        assert_eq!(
            findings[0].severity,
            Some(lsp::DiagnosticSeverity::WARNING),
            "a rule that fired is not an error: the statement still runs"
        );
        assert_eq!(findings[0].source.as_deref(), Some("sqruff"));
    }

    /// A statement that fits the project's rules reports nothing.
    #[test]
    fn a_statement_the_projects_rules_are_content_with_reports_nothing() {
        let findings = configured("keywords_ansi").findings_in("SELECT 1 FROM t\n");
        assert_eq!(reported(&findings), nothing());
    }

    /// The whole of the overlap with the SQL console's own validator, which
    /// reports a statement that will not parse against the dialect of the
    /// connection it would run on. The first half asks sqruff for its parse
    /// failures and it has one; the second half is the configuration this
    /// crate actually uses, under which sqruff says nothing about that text at
    /// all. One mistake, underlined once.
    #[test]
    fn a_statement_that_will_not_parse_is_left_to_the_sql_validator() {
        let broken = "select from from where\n";

        let asked = Configured::from_files_asking(&project("ansi"), true)
            .expect("ansi is a dialect sqruff knows");
        let failures = asked.everything_sqruff_said(broken);
        assert!(
            failures.iter().any(|violation| violation.rule.is_none()),
            "asked for them, sqruff does report the parse failure: {failures:#?}"
        );
        assert!(
            failures
                .iter()
                .all(|violation| as_diagnostic(violation, broken).is_none()),
            "and not one of them would be shown from here: {failures:#?}"
        );

        let findings = configured("ansi").findings_in(broken);
        assert_eq!(
            reported(&findings),
            nothing(),
            "unasked, sqruff has nothing to say about SQL it cannot read"
        );
    }

    /// The dialect comes from the project and nowhere else. The same text is
    /// MySQL that parses and reports a rule, and ANSI that does not parse and
    /// so reports no rule at all -- which is why a project that has not said
    /// which it writes is told nothing.
    #[test]
    fn the_dialect_is_the_projects_own_and_not_the_librarys_default() {
        let backticks = "select `count` from `orders`\n";

        let mysql = configured("keywords_mysql");
        let ansi = configured("keywords_ansi");
        assert_eq!(mysql.dialect(), "mysql");
        assert_eq!(ansi.dialect(), "ansi", "which is the library's default");

        assert_eq!(
            reported(&mysql.findings_in(backticks)),
            vec![
                "CP01 0:0-0:6 Keywords must be upper case. / fix: available".to_owned(),
                "CP01 0:15-0:19 Keywords must be upper case. / fix: available".to_owned(),
            ]
        );
        assert_eq!(
            reported(&ansi.findings_in(backticks)),
            vec!["CP01 0:0-0:6 Keywords must be upper case. / fix: available".to_owned()],
            "read as ANSI the statement stops being SQL at the first backtick, \
             so nothing past the leading keyword is read or reported at all"
        );
    }

    /// A project that has not said which SQL it writes is told nothing at
    /// all. Guessing ANSI would report a page of findings about syntax the
    /// project spells correctly for its own database.
    #[test]
    fn a_project_that_names_no_dialect_is_not_linted_at_all() {
        assert!(Configured::from_files(&project("no_dialect")).is_none());
    }

    /// A dialect this build has no grammar for is the same answer, and has to
    /// be: the name reaches an `unwrap` inside the library.
    #[test]
    fn a_dialect_the_library_does_not_know_is_not_linted_at_all() {
        assert!(Configured::from_files(&project("unknown_dialect")).is_none());
    }

    /// No configuration file is no configuration.
    #[test]
    fn no_configuration_file_at_all_is_not_linted() {
        assert!(Configured::from_files(&[]).is_none());
    }

    /// sqruff holds a finding's place twice over, in two units: `source_slice`
    /// in bytes and `line_pos` in characters. The protocol wants a third,
    /// UTF-16 code units. Reading one as another puts every finding on a line
    /// with an accent or an emoji in the wrong column.
    ///
    /// The first half counts the same place three ways. The second half is the
    /// evidence for which of them sqruff reports: the same statement twice,
    /// differing only in one accented letter, so the byte offsets after it
    /// shift by exactly the one extra byte while the character columns do not
    /// move at all.
    #[test]
    fn a_byte_offset_becomes_a_utf16_one_and_not_a_byte_or_character_one() {
        let line = "select café, \"🦀🔥\" from t";
        let quote = line.rfind('"').expect("the closing quote");

        // Three counts of the same place, all different.
        assert_eq!(line[..quote].len(), 23, "bytes");
        assert_eq!(line[..quote].chars().count(), 16, "characters");
        assert_eq!(line[..quote].encode_utf16().count(), 18, "UTF-16 units");
        assert_eq!(
            utf16_position_at(line, quote),
            lsp::Position::new(0, 18),
            "the protocol's own unit -- not 23, which is what sqruff reports"
        );

        let configured = configured("ansi");
        let plain = configured.everything_sqruff_said("select 'cafe' , 1 from t");
        let accented = configured.everything_sqruff_said("select 'café' , 1 from t");
        assert_eq!(
            plain.iter().map(|one| one.rule_code()).collect::<Vec<_>>(),
            accented
                .iter()
                .map(|one| one.rule_code())
                .collect::<Vec<_>>(),
            "one accented letter changes nothing about what fired"
        );
        let after_the_word: Vec<(&SQLBaseError, &SQLBaseError)> = plain
            .iter()
            .zip(accented.iter())
            .filter(|(one, _)| one.source_slice.start > 12)
            .collect();
        assert!(
            !after_the_word.is_empty(),
            "something has to fire after the word for this to say anything: {plain:#?}"
        );
        for (plain, accented) in after_the_word {
            assert_eq!(
                accented.source_slice.start,
                plain.source_slice.start + 1,
                "the slice moved by the one extra byte, so it is counted in bytes"
            );
            assert_eq!(
                accented.line_pos, plain.line_pos,
                "while `line_pos` did not move, so it is counted in characters"
            );
        }
    }

    /// A file the reader has fixed reports nothing, and an empty report is
    /// what clears the underlining: the editor keeps what it was last given,
    /// so nothing to say is a different thing from saying nothing.
    #[test]
    fn a_file_that_was_fixed_reports_nothing_rather_than_its_old_findings() {
        let configured = configured("keywords_ansi");
        assert!(!configured.findings_in("select 1 from t\n").is_empty());
        assert_eq!(
            reported(&configured.findings_in("SELECT 1 FROM t\n")),
            nothing(),
            "an empty report, which is what tells the editor to drop the old one"
        );
    }

    /// A file carrying an inline directive is left alone. The library hands
    /// every such line to a function that is `panic!("Not implemented")`, and
    /// a save must not be able to reach it.
    #[test]
    fn an_inline_configuration_directive_is_left_alone_rather_than_linted() {
        let findings =
            configured("keywords_ansi").findings_in("-- sqlfluff:dialect:mysql\nselect 1 from t\n");
        assert_eq!(reported(&findings), nothing());
    }

    /// A file past the project's own size limit is skipped, as it is by a
    /// pipeline running sqruff over the same tree.
    #[test]
    fn a_file_past_the_projects_size_limit_is_skipped() {
        let configured = configured("keywords_ansi");
        let long = format!("select 1 from t\n{}", "-- padding\n".repeat(2_000));
        assert!(long.len() > 20_000, "past the project's default limit");
        assert_eq!(reported(&configured.findings_in(&long)), nothing());
    }

    /// The stack is the root's configuration first and the file's own
    /// directory last, which is the order the nearer file needs in order to
    /// override the further one.
    #[test]
    fn the_configuration_covering_a_file_is_the_stack_from_the_root_down() {
        let found = configuration_covering(
            Path::new("/project/etl/load.sql"),
            Path::new("/project"),
            |candidate| {
                candidate == Path::new("/project/.sqlfluff")
                    || candidate == Path::new("/project/etl/.sqruff")
            },
        );
        assert_eq!(
            found,
            vec![
                PathBuf::from("/project/.sqlfluff"),
                PathBuf::from("/project/etl/.sqruff"),
            ]
        );
    }

    /// Nothing above the project is read. A configuration in a home directory
    /// belongs to whatever else is kept there, and would otherwise decide what
    /// this project is told about its own SQL.
    #[test]
    fn no_configuration_above_the_project_is_read() {
        assert_eq!(
            configuration_covering(
                Path::new("/project/load.sql"),
                Path::new("/project"),
                |_| { true }
            ),
            CONFIGURATION_FILES
                .iter()
                .map(|name| PathBuf::from("/project").join(name))
                .collect::<Vec<_>>()
        );
        assert!(
            configuration_covering(
                Path::new("/elsewhere/load.sql"),
                Path::new("/project"),
                |_| true
            )
            .is_empty(),
            "a file outside the project has no configuration to read"
        );
    }

    /// An offset past the end, or inside a character, lands at a boundary
    /// rather than panicking: the text may have changed since it was measured.
    #[test]
    fn an_offset_past_the_end_lands_at_the_end() {
        assert_eq!(
            utf16_position_at("select 1\n", 9_000),
            lsp::Position::new(1, 0)
        );
        assert_eq!(utf16_position_at("", 7), lsp::Position::new(0, 0));
        assert_eq!(utf16_position_at("a 🦀 b", 4), lsp::Position::new(0, 2));
        assert_eq!(utf16_position_at("a 🦀 b", 6), lsp::Position::new(0, 4));
    }
}
