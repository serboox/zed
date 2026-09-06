use std::ops::Range;
use std::path::{Path, PathBuf};

use miette::Diagnostic as _;

mod watching;

pub use watching::{Check, init};

/// What the compiler had to say about one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// It read the file, resolved everything the file imports, and found
    /// nothing wrong.
    Clean,
    /// It stopped on something the reader wrote.
    Faulted(lsp::Diagnostic),
    /// Nothing can honestly be said about this file. Either an import it
    /// needs is not under any root that could be found, or the fault the
    /// compiler stopped on is in another file -- and past an unresolved
    /// import every type name in this file is unresolvable, so a report would
    /// be a wall of complaints about code that is very likely correct.
    Unsayable,
}

/// The roots a file's imports are looked up under, most specific first.
///
/// The file's own directory, then each directory above it up to and including
/// the worktree root. That covers both conventions a project can hold at
/// once: `import "sibling.proto"` resolves against the file's own directory,
/// and `import "shop/v1/order.proto"` resolves against whichever ancestor the
/// package path hangs off. The standard `google/protobuf/*.proto` files need
/// no root at all -- the compiler carries its own copies.
///
/// Ordered most specific first for a reason beyond taste: the compiler names
/// a file by its path relative to the first root that contains it, and then
/// opens that name against the roots in order. With the nearest root first
/// both steps land on the same file, so a project that holds two files of the
/// same relative name under two roots cannot make the compiler complain that
/// one shadows the other.
///
/// A file that is not under this worktree at all gets its own directory and
/// nothing else: the worktree says nothing about where such a file's imports
/// live.
pub fn import_roots(worktree_root: &Path, file: &Path) -> Vec<PathBuf> {
    let Some(directory) = file.parent() else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for ancestor in directory.ancestors() {
        roots.push(ancestor.to_path_buf());
        if ancestor == worktree_root {
            return roots;
        }
    }
    vec![directory.to_path_buf()]
}

/// Compiles one file with the protobuf compiler and reports what it said.
///
/// `text` must be the text the compiler will read from disk, because every
/// place the compiler reports is an offset into it. Reading it separately
/// after the compiler has run would risk placing a diagnostic by the offsets
/// of one version of a file on the lines of another.
pub fn check(file: &Path, roots: &[PathBuf], text: &str) -> Checked {
    let mut compiler = match protox::Compiler::new(roots) {
        Ok(compiler) => compiler,
        Err(trouble) => {
            log::debug!("no compiler for {}: {trouble}", file.display());
            return Checked::Unsayable;
        }
    };
    // Nothing here reads the descriptor set, and building the source info for
    // it is the most expensive part of a compile.
    match compiler.include_source_info(false).open_file(file) {
        Ok(_) => Checked::Clean,
        Err(trouble) => what_it_said(&trouble, file, text),
    }
}

/// What to publish for a file that has just been checked.
///
/// Always a report, and an empty one where there is nothing to say. The
/// editor keeps whatever it was last given until it is given something else,
/// so a file whose fault the reader has just fixed needs an empty report --
/// which is a different thing from silence.
pub fn what_to_tell(checked: Checked) -> Vec<lsp::Diagnostic> {
    match checked {
        Checked::Faulted(diagnostic) => vec![diagnostic],
        Checked::Clean | Checked::Unsayable => Vec::new(),
    }
}

fn what_it_said(trouble: &protox::Error, file: &Path, text: &str) -> Checked {
    // An import the compiler could not find, or a file that is not under any
    // root: from here on every type name in the file is unresolvable, and the
    // hundred complaints that follow would all be about the missing root
    // rather than about the code.
    if trouble.is_file_not_found() {
        return Checked::Unsayable;
    }
    // A file that could not be opened at all -- a permission, a file deleted
    // between the save and the read. Not the reader's mistake, and not
    // something to underline in their source.
    if trouble.is_io() {
        return Checked::Unsayable;
    }
    if !is_about(trouble.file(), file) {
        return Checked::Unsayable;
    }
    let place = where_it_is(trouble).unwrap_or_else(|| the_end_of(text));
    Checked::Faulted(lsp::Diagnostic {
        range: lsp::Range {
            start: utf16_position_at(text, place.start),
            end: utf16_position_at(text, place.end),
        },
        severity: Some(lsp::DiagnosticSeverity::ERROR),
        source: Some("protox".to_string()),
        message: what_it_says(trouble),
        ..Default::default()
    })
}

/// Whether a fault the compiler reported is about this file rather than about
/// one it imports.
///
/// The compiler names a file by its path relative to the root it was found
/// under, so the question is whether the file's own path ends with that name.
/// Compared by whole path components, so `order.proto` does not answer for
/// `reorder.proto`.
fn is_about(reported_in: Option<&str>, file: &Path) -> bool {
    reported_in.is_some_and(|name| file.ends_with(Path::new(name)))
}

/// The bytes a fault is about.
///
/// The compiler carries its places as `miette` labels, and a label's offset
/// is counted in bytes from the start of the file it named -- not in
/// characters, and not in the UTF-16 code units the protocol asks for. The
/// test over a line holding an accent and an emoji is what says so.
fn where_it_is(trouble: &protox::Error) -> Option<Range<usize>> {
    let label = trouble.labels()?.next()?;
    Some(label.offset()..label.offset().saturating_add(label.len()))
}

/// Where to put a fault the compiler gave no place for.
///
/// `expected ..., but reached end of file` is the one a reader will actually
/// meet -- a message whose closing brace is missing -- and the end of the
/// file is where it happened. The last line that holds anything is marked
/// rather than the very last byte, because a range of no width draws nothing.
fn the_end_of(text: &str) -> Range<usize> {
    let ends_at = text.trim_end().len();
    let starts_at = text
        .get(..ends_at)
        .and_then(|before| before.rfind('\n'))
        .map_or(0, |newline| newline + 1);
    starts_at..ends_at
}

/// The whole of what one fault says: its own text, and the advice it came
/// with. The advice is the reader's next question after what is wrong, and
/// the compiler keeps it in a separate field an editor would otherwise drop.
fn what_it_says(trouble: &protox::Error) -> String {
    let mut said = trouble.to_string();
    if let Some(help) = trouble.help() {
        said.push_str("\nhelp: ");
        said.push_str(&help.to_string());
    }
    said
}

/// The position a byte offset falls at, counted the way the protocol counts:
/// lines from zero, and characters as UTF-16 code units.
///
/// Three units are in play and they disagree. On the line
/// `  string café = "🦀🔥";` the `=` sits at byte 16, at character 15, and at
/// UTF-16 unit 15 -- and past the emoji the last two part company as well.
/// The compiler reports the byte; the protocol asks for the UTF-16 unit. Only
/// the text can convert between them, which is why this takes the text.
///
/// An offset past the end, or inside a character, lands on the nearest
/// boundary at or before it, rather than panicking on a file the compiler
/// read and the editor has since changed.
fn utf16_position_at(text: &str, offset: usize) -> lsp::Position {
    let mut at = offset.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    let before = text.get(..at).unwrap_or("");
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A project on disk, because the compiler resolves imports by opening
    /// files: a fixture held in memory would test a code path nobody runs.
    struct AProject {
        root: tempfile::TempDir,
    }

    impl AProject {
        fn new() -> Self {
            AProject {
                root: tempfile::tempdir().expect("a temporary directory"),
            }
        }

        fn holding(&self, relative: &str, source: &str) -> PathBuf {
            let path = self.root.path().join(relative);
            if let Some(directory) = path.parent() {
                std::fs::create_dir_all(directory).expect("the directory is created");
            }
            std::fs::write(&path, source).expect("the file is written");
            path
        }

        fn checked(&self, file: &Path) -> Checked {
            let text = std::fs::read_to_string(file).expect("the file reads back");
            check(file, &import_roots(self.root.path(), file), &text)
        }
    }

    fn faulted(checked: Checked) -> lsp::Diagnostic {
        match checked {
            Checked::Faulted(diagnostic) => diagnostic,
            other => panic!("expected a fault, got {other:?}"),
        }
    }

    #[test]
    fn a_file_the_compiler_accepts_says_nothing() {
        let project = AProject::new();
        let file = project.holding(
            "shop/v1/order.proto",
            "syntax = \"proto3\";\n\npackage shop.v1;\n\nmessage Order {\n  string order_id = 1;\n}\n",
        );
        assert_eq!(project.checked(&file), Checked::Clean);
    }

    /// A fault the parser finds, placed on the token that stopped it.
    #[test]
    fn a_syntax_error_is_a_fault_on_the_token_that_stopped_the_parser() {
        let project = AProject::new();
        let file = project.holding(
            "order.proto",
            "syntax = \"proto3\";\n\nmessage Order {\n  string order_id = ;\n}\n",
        );
        let diagnostic = faulted(project.checked(&file));
        assert_eq!(
            diagnostic.severity,
            Some(lsp::DiagnosticSeverity::ERROR),
            "a file that will not parse is not a suggestion"
        );
        assert_eq!(diagnostic.source.as_deref(), Some("protox"));
        assert_eq!(
            diagnostic.range.start.line, 3,
            "the fourth line, counted from zero: {diagnostic:?}"
        );
        assert!(
            diagnostic.message.contains("but found ';'"),
            "{}",
            diagnostic.message
        );
    }

    /// A fault only the second half of a compile can find: the file parses,
    /// and the name it uses does not exist. This is the whole reason for
    /// embedding the compiler rather than reading the parse tree.
    #[test]
    fn a_name_that_does_not_exist_is_a_fault_the_parser_alone_would_not_find() {
        let project = AProject::new();
        let file = project.holding(
            "order.proto",
            "syntax = \"proto3\";\n\nmessage Order {\n  Missing thing = 1;\n}\n",
        );
        let diagnostic = faulted(project.checked(&file));
        assert_eq!(diagnostic.severity, Some(lsp::DiagnosticSeverity::ERROR));
        assert!(
            diagnostic.message.contains("Missing"),
            "the fault has to name what is missing: {diagnostic:?}"
        );
        assert_eq!(
            diagnostic.range.start.line, 3,
            "on the line that used the name: {diagnostic:?}"
        );
    }

    /// The compiler resolves imports against the roots, so an import written
    /// the way the package path reads is compiled rather than refused.
    #[test]
    fn an_import_resolved_under_a_root_above_the_file_is_compiled() {
        let project = AProject::new();
        project.holding(
            "shop/v1/currency.proto",
            "syntax = \"proto3\";\n\npackage shop.v1;\n\nenum Currency {\n  CURRENCY_UNSPECIFIED = 0;\n}\n",
        );
        let file = project.holding(
            "shop/v1/order.proto",
            "syntax = \"proto3\";\n\npackage shop.v1;\n\nimport \"shop/v1/currency.proto\";\n\nmessage Order {\n  Currency currency = 1;\n}\n",
        );
        assert_eq!(project.checked(&file), Checked::Clean);
    }

    /// The standard descriptors come with the compiler, so a file that
    /// imports one needs no root of its own for it.
    #[test]
    fn an_import_of_a_standard_file_needs_no_root() {
        let project = AProject::new();
        let file = project.holding(
            "order.proto",
            "syntax = \"proto3\";\n\nimport \"google/protobuf/timestamp.proto\";\n\nmessage Order {\n  google.protobuf.Timestamp placed_at = 1;\n}\n",
        );
        assert_eq!(project.checked(&file), Checked::Clean);
    }

    /// An import that resolves nowhere makes every type name in the file
    /// unresolvable. Reporting that would be a screenful of complaints about
    /// code that is very likely correct, so nothing is said at all.
    #[test]
    fn an_import_that_resolves_nowhere_says_nothing_rather_than_a_wall_of_complaints() {
        let project = AProject::new();
        let file = project.holding(
            "order.proto",
            "syntax = \"proto3\";\n\nimport \"nowhere/currency.proto\";\n\nmessage Order {\n  Currency currency = 1;\n}\n",
        );
        assert_eq!(project.checked(&file), Checked::Unsayable);
    }

    /// A fault in an imported file is that file's business. Reporting it on
    /// the file that imports it would underline an import statement the
    /// reader wrote correctly.
    #[test]
    fn a_fault_in_an_imported_file_is_not_reported_on_the_file_that_imports_it() {
        let project = AProject::new();
        project.holding("currency.proto", "syntax = \"proto3\";\n\nenum Currency {\n");
        let file = project.holding(
            "order.proto",
            "syntax = \"proto3\";\n\nimport \"currency.proto\";\n\nmessage Order {\n  Currency currency = 1;\n}\n",
        );
        assert_eq!(project.checked(&file), Checked::Unsayable);
    }

    /// A file whose closing brace is missing still reports something. The
    /// compiler gives this one no place at all, so it is placed by
    /// [`the_end_of`], and the alternative -- dropping it -- would leave the
    /// one fault a reader meets most often invisible.
    #[test]
    fn a_file_that_ends_mid_message_still_reports_a_fault() {
        let project = AProject::new();
        let file = project.holding(
            "order.proto",
            "syntax = \"proto3\";\n\nmessage Order {\n  string order_id = 1;\n",
        );
        let diagnostic = faulted(project.checked(&file));
        assert_eq!(diagnostic.severity, Some(lsp::DiagnosticSeverity::ERROR));
        assert!(
            diagnostic.message.contains("end of file"),
            "{diagnostic:?}"
        );
        assert!(
            diagnostic.range.start.line <= 3,
            "somewhere in the file the reader wrote: {diagnostic:?}"
        );
    }

    /// The compiler counts a place in bytes, the protocol counts it in UTF-16
    /// code units, and a reader looking at the line counts characters. All
    /// three disagree on the same place, and reading one as another puts every
    /// diagnostic on a line holding an accent or an emoji in the wrong column.
    #[test]
    fn a_byte_offset_becomes_a_utf16_column_and_not_a_byte_or_character_one() {
        let source = "syntax = \"proto3\";\n\nmessage Order {\n  /* café 🦀🔥 */ @\n}\n";
        let line = source.lines().nth(3).expect("the café line");
        let stray = line.find('@').expect("the token the lexer refuses");

        // Three counts of the same place, all different.
        assert_eq!(line[..stray].len(), 23, "bytes");
        assert_eq!(line[..stray].chars().count(), 16, "characters");
        assert_eq!(line[..stray].encode_utf16().count(), 18, "UTF-16 units");

        let project = AProject::new();
        let file = project.holding("order.proto", source);
        let diagnostic = faulted(project.checked(&file));
        assert_eq!(
            diagnostic.range.start,
            lsp::Position::new(3, 18),
            "the protocol's own unit -- not 23, which is the byte the compiler \
             reports, and not 16, which is the character a reader counts: {diagnostic:?}"
        );
    }

    /// A file whose fault the reader has just fixed needs an empty report.
    /// The editor keeps whatever it was last given, so saying nothing would
    /// leave the fault on the screen after it is gone.
    #[test]
    fn a_file_that_has_been_fixed_is_told_it_is_clean_rather_than_left_alone() {
        let project = AProject::new();
        let file = project.holding(
            "order.proto",
            "syntax = \"proto3\";\n\nmessage Order {\n  string order_id = ;\n}\n",
        );
        assert_eq!(what_to_tell(project.checked(&file)).len(), 1);

        let file = project.holding(
            "order.proto",
            "syntax = \"proto3\";\n\nmessage Order {\n  string order_id = 1;\n}\n",
        );
        assert_eq!(
            what_to_tell(project.checked(&file)),
            Vec::new(),
            "an empty report, which is what takes the fault down"
        );
    }

    /// A file whose imports cannot be judged reports an empty list too, not
    /// silence: otherwise a fault from before the import broke would stay up
    /// for the rest of the session.
    #[test]
    fn a_file_nothing_can_be_said_about_reports_an_empty_list() {
        assert_eq!(what_to_tell(Checked::Unsayable), Vec::new());
    }

    #[test]
    fn the_roots_run_from_the_files_own_directory_up_to_the_worktree() {
        let root = Path::new("/project");
        assert_eq!(
            import_roots(root, Path::new("/project/shop/v1/order.proto")),
            vec![
                PathBuf::from("/project/shop/v1"),
                PathBuf::from("/project/shop"),
                PathBuf::from("/project"),
            ],
            "nearest first, so the compiler names the file by its shortest name"
        );
        assert_eq!(
            import_roots(root, Path::new("/project/order.proto")),
            vec![PathBuf::from("/project")]
        );
    }

    /// A file the worktree does not hold gets its own directory and nothing
    /// else: the worktree says nothing about where such a file's imports are.
    #[test]
    fn a_file_outside_the_worktree_gets_only_its_own_directory() {
        assert_eq!(
            import_roots(Path::new("/project"), Path::new("/tmp/scratch/order.proto")),
            vec![PathBuf::from("/tmp/scratch")]
        );
    }

    /// An offset past the end of the text, or inside a character, lands on
    /// the nearest boundary at or before it. The compiler measured them
    /// against a file the editor may have changed since.
    #[test]
    fn a_place_past_the_end_or_inside_a_character_lands_on_a_boundary() {
        assert_eq!(
            utf16_position_at("syntax = \"proto3\";\n", 9_000),
            lsp::Position::new(1, 0)
        );
        assert_eq!(utf16_position_at("", 0), lsp::Position::new(0, 0));
        assert_eq!(utf16_position_at("", 7), lsp::Position::new(0, 0));
        // Inside the four bytes of the crab, which begins at byte 2.
        assert_eq!(utf16_position_at("a 🦀 b", 4), lsp::Position::new(0, 2));
        // And just past it, where its two UTF-16 units have been counted.
        assert_eq!(utf16_position_at("a 🦀 b", 6), lsp::Position::new(0, 4));
    }

    #[test]
    fn the_end_of_a_text_is_the_last_line_that_holds_anything() {
        assert_eq!(the_end_of("message Order {\n\n\n"), 0..15);
        assert_eq!(the_end_of("a\nbb\n"), 2..4);
        assert_eq!(the_end_of(""), 0..0);
        assert_eq!(the_end_of("\n\n"), 0..0);
    }

    #[test]
    fn a_fault_is_matched_to_a_file_by_whole_path_components() {
        let file = Path::new("/project/shop/v1/order.proto");
        assert!(is_about(Some("shop/v1/order.proto"), file));
        assert!(is_about(Some("order.proto"), file));
        assert!(
            !is_about(Some("reorder.proto"), file),
            "a name is not a suffix of another name"
        );
        assert!(!is_about(Some("v1/other.proto"), file));
        assert!(!is_about(None, file));
    }
}
