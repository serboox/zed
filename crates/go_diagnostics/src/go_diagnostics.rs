use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

mod fixing;
mod watching;

pub use watching::{Check, init};

/// One diagnostic a Go tool reported, at a place the editor can put it.
///
/// The range is in the units the protocol means by a character -- UTF-16 code
/// units -- because that is what this editor's own conversion reads. Go counts
/// something else; see [`utf16_position_of`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    /// Absolute, so a caller does not have to remember where the tool ran.
    pub path: PathBuf,
    pub diagnostic: lsp::Diagnostic,
    /// The fixes the analyzer that found this handed over with it, in the
    /// order it listed them. The compiler supplies none: `go build` reports
    /// only text.
    pub fixes: Vec<Fix>,
}

/// A fix an analyzer wrote itself, offered word for word as it gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    /// The analyzer's own words for it -- "Replace 2006-02-01 with
    /// 2006-01-02".
    pub title: String,
    pub replacements: Vec<Replacement>,
}

/// One piece of text an analyzer asked to be put somewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    /// Absolute, like [`Reported::path`]: a fix may reach a file other than
    /// the one the finding is in.
    pub path: PathBuf,
    /// In UTF-16 code units, like [`Reported`]'s range and for the same
    /// reason.
    pub range: lsp::Range,
    /// Empty where the analyzer asked for the range to be deleted, which is a
    /// fix like any other and not the absence of one.
    pub new_text: String,
    /// The text that was in `range` when the analyzer measured it. A caller
    /// compares it against what is there now: a file edited since the run has
    /// moved every offset in the report, and a replacement made against
    /// moved offsets overwrites something the analyzer never looked at.
    pub replaced: String,
}

/// What `go build -json` writes: one JSON object per line, of which only the
/// `build-output` ones carry text, and that text is the compiler's ordinary
/// error output rather than anything structured.
#[derive(Deserialize)]
struct BuildEvent {
    #[serde(rename = "Action")]
    action: String,
    #[serde(rename = "Output", default)]
    output: String,
}

/// Reads what the Go compiler reported into diagnostics the editor can show,
/// with no language server anywhere.
///
/// `ran_in` is the directory `go` was run in, because the paths in its output
/// are relative to that and to nothing else. `read` supplies a file's text,
/// which is needed and not optional: Go gives a byte column, the protocol
/// wants UTF-16 code units, and only the text itself can convert one to the
/// other.
///
/// A file `read` cannot supply is skipped rather than guessed at. Putting a
/// diagnostic on the wrong line is worse than not showing it.
pub fn what_the_compiler_reported(
    output: &str,
    ran_in: &Path,
    read: impl Fn(&Path) -> Option<String>,
) -> Vec<Reported> {
    // The events frame the compiler's output but do not split it into
    // messages, so the text is put back together before it is read. One
    // message can arrive as several events, and one event can carry several
    // lines.
    let mut said = String::new();
    for line in output.lines() {
        let Ok(event) = serde_json::from_str::<BuildEvent>(line) else {
            continue;
        };
        if event.action == "build-output" {
            said.push_str(&event.output);
        }
    }

    let mut reported: Vec<Reported> = Vec::new();
    // Go builds each package that imports a broken one as well, and reports
    // the same error under each. Identical twice over is once.
    let mut already: HashSet<(PathBuf, lsp::Range, String)> = HashSet::new();
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    for line in said.lines() {
        // The name of the package a batch of errors belongs to -- not an
        // error, and not about a place in the code.
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        // The compiler indents the detail under an error rather than
        // repeating the place, so an indented line belongs to whatever was
        // reported last. Some of that detail names a place of its own -- the
        // earlier declaration under a redeclaration, the earlier case under a
        // duplicate one -- and that place is kept as a place. The rest,
        // "have (int)" under a type error, has no place to be kept at and
        // stays part of what was said.
        if line.starts_with([' ', '\t']) {
            let Some(last) = reported.last_mut() else {
                continue;
            };
            match a_place_of_its_own(line, ran_in, &mut texts, &read) {
                Some(place) => last
                    .diagnostic
                    .related_information
                    .get_or_insert_with(Vec::new)
                    .push(place),
                None => {
                    last.diagnostic.message.push('\n');
                    last.diagnostic.message.push_str(line.trim_start());
                }
            }
            continue;
        }
        let Some((file, row, column, message)) = a_place_and_what_was_said(line) else {
            continue;
        };
        let path = beneath(ran_in, file);
        let text = texts
            .entry(path.clone())
            .or_insert_with(|| read(&path))
            .as_deref();
        let Some(text) = text else {
            continue;
        };
        let range = a_range(text, row, column, None);
        if !already.insert((path.clone(), range, message.to_string())) {
            continue;
        }
        reported.push(Reported {
            path,
            fixes: Vec::new(),
            diagnostic: lsp::Diagnostic {
                range,
                severity: Some(lsp::DiagnosticSeverity::ERROR),
                source: Some("go build".to_string()),
                message: message.to_string(),
                ..Default::default()
            },
        });
    }
    reported
}

/// The place an indented detail line points at, where it points at one.
///
/// `./main.go:3:6: other declaration of helper` is a second place the error is
/// about, and the reader's next move is to go and look at it. Folded into the
/// message it becomes a line number to find by hand; kept as a place, the
/// editor can jump to it.
///
/// Nothing at all for a detail line that names no place, which is most of
/// them, and nothing for one whose file cannot be read: only that file's text
/// converts Go's byte column into the protocol's.
fn a_place_of_its_own(
    line: &str,
    ran_in: &Path,
    texts: &mut HashMap<PathBuf, Option<String>>,
    read: &impl Fn(&Path) -> Option<String>,
) -> Option<lsp::DiagnosticRelatedInformation> {
    let (file, row, column, message) = a_place_and_what_was_said(line.trim_start())?;
    if message.is_empty() {
        return None;
    }
    let message = message.to_string();
    let path = beneath(ran_in, file);
    let uri = lsp::Uri::from_file_path(&path).ok()?;
    let text = text_of(texts, &path, read)?;
    Some(lsp::DiagnosticRelatedInformation {
        location: lsp::Location {
            uri,
            range: a_range(text, row, column, None),
        },
        message,
    })
}

/// What `go vet -json` writes: a tree of package to analyzer to either a list
/// of findings or `{"error": ...}` where the analyzer itself failed.
#[derive(Deserialize)]
struct VetTree(HashMap<String, HashMap<String, serde_json::Value>>);

#[derive(Deserialize)]
struct Finding {
    /// `file:line:column`, and the file is absolute in everything `go vet`
    /// has been seen to write.
    posn: String,
    #[serde(default)]
    end: Option<String>,
    message: String,
    /// Alternative ways to fix this one finding, of which at most one may be
    /// applied. Absent from the analyzers that compute none, which is most of
    /// them.
    #[serde(default)]
    suggested_fixes: Vec<SuggestedFix>,
}

#[derive(Deserialize)]
struct SuggestedFix {
    #[serde(default)]
    message: String,
    #[serde(default)]
    edits: Vec<SuggestedEdit>,
}

/// One piece of text an analyzer asked to put over `start..end`, which are
/// absolute byte offsets into `filename`. `new` is empty where it asked for a
/// deletion, and `start` equals `end` where it asked for an insertion.
#[derive(Deserialize)]
struct SuggestedEdit {
    filename: String,
    start: usize,
    end: usize,
    new: String,
}

/// Reads what `go vet` reported into diagnostics the editor can show.
///
/// The stream is not pure JSON: `go` prints the name of each package it vets
/// alongside the tool's own output, so the trees sit in ordinary text. Each
/// one is found and read on its own, and anything between them is passed
/// over.
pub fn what_vet_reported(
    output: &str,
    ran_in: &Path,
    read: impl Fn(&Path) -> Option<String>,
) -> Vec<Reported> {
    let mut reported = Vec::new();
    let mut already: HashSet<(PathBuf, lsp::Range, String)> = HashSet::new();
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    for tree in every_tree_in(output) {
        for analyzers in tree.0.into_values() {
            for (analyzer, found) in analyzers {
                // An analyzer that failed reports an object rather than a
                // list. There is no place in the code to put it.
                let Ok(findings) = serde_json::from_value::<Vec<Finding>>(found) else {
                    continue;
                };
                for finding in findings {
                    let Some((file, row, column)) = a_place(&finding.posn) else {
                        continue;
                    };
                    let path = beneath(ran_in, file);
                    let text = texts
                        .entry(path.clone())
                        .or_insert_with(|| read(&path))
                        .as_deref();
                    let Some(text) = text else {
                        continue;
                    };
                    let ends_at = finding
                        .end
                        .as_deref()
                        .and_then(a_place)
                        .filter(|(also, _, _)| beneath(ran_in, also) == path)
                        .map(|(_, row, column)| (row, column));
                    let range = a_range(text, row, column, ends_at);
                    if !already.insert((path.clone(), range, finding.message.clone())) {
                        continue;
                    }
                    let fixes = fixes_in(&finding, ran_in, &mut texts, &read);
                    reported.push(Reported {
                        path,
                        fixes,
                        diagnostic: lsp::Diagnostic {
                            range,
                            severity: Some(lsp::DiagnosticSeverity::WARNING),
                            code: Some(lsp::NumberOrString::String(analyzer.clone())),
                            source: Some("go vet".to_string()),
                            message: finding.message,
                            ..Default::default()
                        },
                    });
                }
            }
        }
    }
    reported
}

/// Every fix this finding handed over that this reader will pass on.
///
/// `go vet` grades nothing: unlike the Rust compiler's suggestions, an
/// analyzer's fix arrives with no word on how sure of it the analyzer is. So
/// the only grade available is the contract every analyzer writes to, which
/// says a fix's edits "must not overlap, nor contain edits for other
/// packages" and that an edit's end must not precede its start. A fix that
/// breaks that is malformed rather than merely uncertain, and is refused
/// whole.
///
/// Refused whole, and not edit by edit, because a fix's edits are halves of
/// one change: `stringintconv` asks for a `rune(` and its closing `)` as two
/// edits, and applying one without the other leaves the file broken.
///
/// A fix with no message is refused as well -- what the reader picks from a
/// menu is the title, and an unnamed entry says nothing about what it will
/// do -- and so is one with no edits, which would do nothing at all.
fn fixes_in(
    finding: &Finding,
    ran_in: &Path,
    texts: &mut HashMap<PathBuf, Option<String>>,
    read: &impl Fn(&Path) -> Option<String>,
) -> Vec<Fix> {
    finding
        .suggested_fixes
        .iter()
        .filter_map(|suggested| one_fix(suggested, ran_in, texts, read))
        .collect()
}

fn one_fix(
    suggested: &SuggestedFix,
    ran_in: &Path,
    texts: &mut HashMap<PathBuf, Option<String>>,
    read: &impl Fn(&Path) -> Option<String>,
) -> Option<Fix> {
    if suggested.message.is_empty() || suggested.edits.is_empty() || overlap(&suggested.edits) {
        return None;
    }
    let mut replacements = Vec::with_capacity(suggested.edits.len());
    for edit in &suggested.edits {
        if edit.end < edit.start {
            return None;
        }
        let path = beneath(ran_in, &edit.filename);
        let text = text_of(texts, &path, read)?;
        // A range the text cannot be sliced by has been measured against a
        // different file than the one being read, and an empty string here
        // would be indistinguishable from the analyzer asking for an
        // insertion.
        let replaced = text.get(edit.start..edit.end)?.to_string();
        let range = lsp::Range {
            start: utf16_position_at(text, edit.start),
            end: utf16_position_at(text, edit.end),
        };
        replacements.push(Replacement {
            path,
            range,
            new_text: edit.new.clone(),
            replaced,
        });
    }
    Some(Fix {
        title: suggested.message.clone(),
        replacements,
    })
}

/// Whether any two of a fix's edits in the same file cover overlapping text.
/// Two insertions at one point are not an overlap: neither replaces
/// anything, and both can be written in the order they were given.
fn overlap(edits: &[SuggestedEdit]) -> bool {
    let mut by_file: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
    for edit in edits {
        by_file
            .entry(edit.filename.as_str())
            .or_default()
            .push((edit.start, edit.end));
    }
    by_file.into_values().any(|mut ranges| {
        ranges.sort_unstable();
        ranges
            .windows(2)
            .any(|pair| matches!(pair, [(_, before), (after, _)] if after < before))
    })
}

/// A file's text, read at most once however many edits point into it.
fn text_of<'a>(
    texts: &'a mut HashMap<PathBuf, Option<String>>,
    path: &Path,
    read: &impl Fn(&Path) -> Option<String>,
) -> Option<&'a str> {
    texts
        .entry(path.to_path_buf())
        .or_insert_with(|| read(path))
        .as_deref()
}

/// The position an absolute byte offset falls at, counted the way the
/// protocol counts: lines from zero, and characters as UTF-16 code units.
///
/// An offset past the end of the text lands at the end of it, and one inside
/// a character lands on the boundary before it, rather than panicking on a
/// file the tool saw and the editor has since changed.
fn utf16_position_at(text: &str, byte: usize) -> lsp::Position {
    let mut at = byte.min(text.len());
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

/// Every JSON tree in a stream that also holds text that is not JSON.
///
/// A tree starts at a `{` that starts a line, which is where both the
/// indented and the compact form of the printer put it. Whatever follows is
/// read as far as one value goes, and the search resumes after it -- so a
/// package name between two trees is passed over rather than ending the
/// reading.
fn every_tree_in(output: &str) -> Vec<VetTree> {
    let mut trees = Vec::new();
    let mut from = 0usize;
    while from < output.len() {
        let Some(start) = output[from..]
            .match_indices('{')
            .map(|(at, _)| from + at)
            .find(|at| *at == 0 || output.as_bytes()[at - 1] == b'\n')
        else {
            break;
        };
        let mut reading =
            serde_json::Deserializer::from_str(&output[start..]).into_iter::<VetTree>();
        match reading.next() {
            Some(Ok(tree)) => {
                let read_to = reading.byte_offset();
                trees.push(tree);
                from = start + read_to.max(1);
            }
            _ => from = start + 1,
        }
    }
    trees
}

/// Where a `file:line:column: message` line points, and what it says.
///
/// Split at the first colon that a line number follows, so a Windows path
/// keeps its drive letter and a message keeps its own colons.
fn a_place_and_what_was_said(line: &str) -> Option<(&str, u32, Option<u32>, &str)> {
    for (at, _) in line.match_indices(':') {
        if at == 0 {
            continue;
        }
        let Some((row, rest)) = leading_number(&line[at + 1..]) else {
            continue;
        };
        let (column, rest) = match rest.strip_prefix(':').and_then(leading_number) {
            Some((column, rest)) => (Some(column), rest),
            None => (None, rest),
        };
        let Some(message) = rest.strip_prefix(": ") else {
            continue;
        };
        return Some((&line[..at], row, column, message));
    }
    None
}

/// Where a bare `file:line:column` points. Split from the right, because the
/// file name is the part that may hold colons of its own.
fn a_place(posn: &str) -> Option<(&str, u32, Option<u32>)> {
    let (rest, column) = posn.rsplit_once(':')?;
    let column: u32 = column.parse().ok()?;
    let (file, row) = rest.rsplit_once(':')?;
    let row: u32 = row.parse().ok()?;
    (!file.is_empty()).then_some((file, row, Some(column)))
}

fn leading_number(text: &str) -> Option<(u32, &str)> {
    let digits = text.len()
        - text
            .trim_start_matches(|at: char| at.is_ascii_digit())
            .len();
    let (number, rest) = text.split_at(digits);
    Some((number.parse().ok()?, rest))
}

fn beneath(ran_in: &Path, file: &str) -> PathBuf {
    let file = Path::new(file);
    let joined = if file.is_absolute() {
        file.to_path_buf()
    } else {
        ran_in.join(file)
    };
    // Go writes `./main.go` for a file beside the module root and `sub/sub.go`
    // for one below it, and the editor keys diagnostics by path -- so the same
    // file named two ways must not become two files. `..` is left where it is,
    // since resolving it without the filesystem would follow a symlink wrong.
    joined
        .components()
        .filter(|part| !matches!(part, std::path::Component::CurDir))
        .collect()
}

/// The range a diagnostic covers, given where Go says it starts and, for the
/// analyzers that supply one, where it ends.
///
/// Go's compiler supplies only a start. A range of no width underlines
/// nothing, so it is widened to the word the column lands on -- and to the
/// rest of the line where it lands on punctuation, since there is no word
/// there to take.
fn a_range(
    text: &str,
    row: u32,
    column: Option<u32>,
    ends_at: Option<(u32, Option<u32>)>,
) -> lsp::Range {
    let start = utf16_position_of(text, row, column);
    if let Some((row, column)) = ends_at {
        let end = utf16_position_of(text, row, column);
        if (end.line, end.character) > (start.line, start.character) {
            return lsp::Range { start, end };
        }
    }
    let line = line_of(text, row).unwrap_or("");
    let from = byte_column_within(line, column);
    let rest = &line[from..];
    let word = rest
        .find(|at: char| !at.is_alphanumeric() && at != '_')
        .unwrap_or(rest.len());
    let to = if word == 0 { rest.len() } else { word };
    lsp::Range {
        start,
        end: lsp::Position {
            line: start.line,
            character: start.character + rest[..to].encode_utf16().count() as u32,
        },
    }
}

/// The position Go's own line and column fall at, counted the way the
/// protocol counts: lines from zero, and characters as UTF-16 code units.
///
/// Three units are in play and they disagree. On the line
/// `fmt.Println(описание, "🦀🔥"); fmt.Printf(` the last `(` sits at byte 55,
/// at character 41, and at UTF-16 unit 43. Go reports the byte; the protocol
/// asks for the UTF-16 unit. Only the text can convert between them, which is
/// why this takes the text.
///
/// A line or column past the end of the text lands at the end of what there
/// is, rather than panicking on a file the tool saw and the editor has since
/// changed.
fn utf16_position_of(text: &str, row: u32, column: Option<u32>) -> lsp::Position {
    let line = row.saturating_sub(1);
    let Some(found) = line_of(text, row) else {
        return lsp::Position { line, character: 0 };
    };
    let up_to = byte_column_within(found, column);
    lsp::Position {
        line,
        character: found[..up_to].encode_utf16().count() as u32,
    }
}

fn line_of(text: &str, row: u32) -> Option<&str> {
    let line = row.checked_sub(1)?;
    text.split('\n')
        .nth(line as usize)
        .map(|found| found.strip_suffix('\r').unwrap_or(found))
}

/// Go counts a column in bytes from one. A column past the end of the line
/// lands at its end, and one that falls inside a character belongs to that
/// character rather than splitting it.
fn byte_column_within(line: &str, column: Option<u32>) -> usize {
    let mut at = column.unwrap_or(1).saturating_sub(1) as usize;
    at = at.min(line.len());
    while at > 0 && !line.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Real output, captured from `go build -json ./...` under Go 1.25 over a
    /// module with a type error in one package and an unused variable and an
    /// undefined name in another. Kept verbatim rather than written by hand:
    /// every field this reader depends on is one the tool actually emits, in
    /// the shape it actually emits it.
    const REAL_BUILD: &str = include_str!("../test_data/go-build.json");

    /// Real output, captured from `go vet -json ./...` under Go 1.25. It is
    /// the tool's *standard error*, which is where `go` passes the analyzers'
    /// JSON through, mixed with the package names it prints itself.
    const REAL_VET: &str = include_str!("../test_data/go-vet.txt");

    /// Real output, captured from `go build -json ./...` under Go 1.27 over a
    /// module holding a duplicated switch case. The compiler indents the
    /// earlier case under the error and gives it a place of its own, and that
    /// place is on a line holding an emoji -- so Go's byte column, the
    /// characters on the line and the protocol's UTF-16 units are three
    /// different numbers.
    const DUPLICATE_BUILD: &str = include_str!("../test_data/go-build-duplicate.json");
    const DUPLICATE_SOURCE: &str = include_str!("../test_data/go-build-duplicate.source");

    /// Real output from the same command over a module whose analyzers do
    /// suggest fixes -- `timeformat` and `stringintconv` -- captured together
    /// with the file itself, so that every offset in it can be checked
    /// against the text it was measured on.
    const WITH_FIXES: &str = include_str!("../test_data/go-vet-fixes.txt");
    const WITH_FIXES_SOURCE: &str = include_str!("../test_data/go-vet-fixes.source");

    fn over_the_fixture_with_fixes(output: &str) -> Vec<Reported> {
        what_vet_reported(output, Path::new("/src"), |path| {
            (path == Path::new("/src/main.go")).then(|| WITH_FIXES_SOURCE.to_string())
        })
    }

    fn fixes_named(reported: &[Reported], analyzer: &str) -> Vec<Fix> {
        reported
            .iter()
            .filter(|one| {
                one.diagnostic.code == Some(lsp::NumberOrString::String(analyzer.to_string()))
            })
            .flat_map(|one| one.fixes.clone())
            .collect()
    }

    /// The file `REAL_BUILD` was captured over.
    fn built_file() -> String {
        concat!(
            "package main\n",
            "\n",
            "import \"fmt\"\n",
            "\n",
            "func main() {\n",
            "\tfmt.Printf(\"%d\\n\", \"not a number\")\n",
            "\tvar unusedValue int\n",
            "\tописание := \"переменная\"\n",
            "\tfmt.Println(описание, \"\u{1f980}\u{1f525}\", missingHelper())\n",
            "}\n",
        )
        .to_string()
    }

    /// The file `REAL_VET` was captured over.
    fn vetted_file() -> String {
        concat!(
            "package main\n",
            "\n",
            "import \"fmt\"\n",
            "\n",
            "func main() {\n",
            "\tописание := \"переменная\"\n",
            "\tfmt.Println(описание, \"\u{1f980}\u{1f525}\"); fmt.Printf(\"%d\\n\", описание)\n",
            "}\n",
        )
        .to_string()
    }

    fn broken_package() -> String {
        "package sub\n\nfunc Broken() int {\n\treturn \"a string\"\n}\n".to_string()
    }

    fn read_the_captured_module(path: &Path) -> Option<String> {
        match path.to_str()? {
            "/project/main.go" => Some(built_file()),
            "/project/sub/sub.go" => Some(broken_package()),
            "/src/main.go" => Some(vetted_file()),
            _ => None,
        }
    }

    #[test]
    fn every_error_in_a_real_build_is_read_with_its_file_and_place() {
        let reported =
            what_the_compiler_reported(REAL_BUILD, Path::new("/project"), read_the_captured_module);

        assert_eq!(
            reported.len(),
            3,
            "one per compiler error; found {:?}",
            reported
                .iter()
                .map(|one| &one.diagnostic.message)
                .collect::<Vec<_>>()
        );

        let broken = &reported[0];
        assert_eq!(broken.path, Path::new("/project/sub/sub.go"));
        assert_eq!(
            broken.diagnostic.severity,
            Some(lsp::DiagnosticSeverity::ERROR)
        );
        assert_eq!(broken.diagnostic.source.as_deref(), Some("go build"));
        assert_eq!(broken.diagnostic.range.start.line, 3);
        // `\treturn "a string"` -- the string starts at byte 8, column 9.
        assert_eq!(broken.diagnostic.range.start.character, 8);
        assert!(
            broken.diagnostic.message.starts_with("cannot use"),
            "{}",
            broken.diagnostic.message
        );

        let unused = &reported[1];
        assert_eq!(unused.path, Path::new("/project/main.go"));
        assert_eq!(unused.diagnostic.range.start.line, 6);
        assert_eq!(
            unused.diagnostic.message,
            "declared and not used: unusedValue"
        );
        assert_eq!(
            unused.diagnostic.range.end.character - unused.diagnostic.range.start.character,
            "unusedValue".len() as u32,
            "the name is underlined, not a point between two characters"
        );
    }

    /// Go reports a column in bytes, the protocol counts UTF-16 code units,
    /// and the characters on the line are a third number again. Reading one
    /// as another puts every diagnostic on a line with an emoji in the wrong
    /// column.
    #[test]
    fn a_byte_column_becomes_a_utf16_one_on_a_line_that_is_not_ascii() {
        let text = built_file();
        let line = text.lines().nth(8).expect("the ninth line");
        let inside = line.find("missingHelper").expect("the undefined name");

        // Three counts of the same place, all different.
        assert_eq!(line[..inside].len(), 43, "bytes");
        assert_eq!(line[..inside].chars().count(), 29, "characters");
        assert_eq!(line[..inside].encode_utf16().count(), 31, "UTF-16 units");

        let reported =
            what_the_compiler_reported(REAL_BUILD, Path::new("/project"), read_the_captured_module);
        let undefined = reported
            .iter()
            .find(|one| one.diagnostic.message.contains("missingHelper"))
            .expect("the undefined name is reported");
        assert_eq!(undefined.diagnostic.range.start.line, 8);
        assert_eq!(
            undefined.diagnostic.range.start.character, 31,
            "the protocol's own unit -- not 43, which is the column Go reports"
        );
        assert_eq!(
            undefined.diagnostic.range.end.character, 44,
            "and the whole of the name is underlined"
        );
    }

    /// The JSON `go vet` writes does not arrive alone: `go` prints the name of
    /// each package it vets around it, so the tree has to be found in text
    /// that is not JSON.
    #[test]
    fn a_finding_is_read_out_of_the_text_go_prints_around_it() {
        assert!(
            REAL_VET.contains("# example.com/m"),
            "the fixture is only meaningful while go still prints package names alongside"
        );

        let reported = what_vet_reported(REAL_VET, Path::new("/src"), read_the_captured_module);

        assert_eq!(reported.len(), 1, "{reported:?}");
        let found = &reported[0];
        assert_eq!(found.path, Path::new("/src/main.go"));
        assert_eq!(
            found.diagnostic.severity,
            Some(lsp::DiagnosticSeverity::WARNING),
            "vet reports a suspicion, not a build failure"
        );
        assert_eq!(found.diagnostic.source.as_deref(), Some("go vet"));
        assert_eq!(
            found.diagnostic.code,
            Some(lsp::NumberOrString::String("printf".to_string())),
            "the analyzer that said it"
        );
        assert!(
            found.diagnostic.message.contains("wrong type string"),
            "{}",
            found.diagnostic.message
        );

        // The verb vet points at, on a line whose first half is Cyrillic and
        // two emoji. Three counts of the same place, all different.
        let text = vetted_file();
        let line = text.lines().nth(6).expect("the seventh line");
        let inside = line.rfind("%d").expect("the format verb");
        assert_eq!(line[..inside].len(), 56, "bytes");
        assert_eq!(line[..inside].chars().count(), 42, "characters");
        assert_eq!(line[..inside].encode_utf16().count(), 44, "UTF-16 units");
        assert_eq!(found.diagnostic.range.start.line, 6);
        assert_eq!(
            found.diagnostic.range.start.character, 44,
            "the protocol's own unit -- not 56, which is the column vet reports"
        );
    }

    /// A tool that is not installed, a module that will not load, a crash
    /// report -- none of it is JSON, and none of it may take the editor down.
    #[test]
    fn output_that_is_not_what_was_expected_is_passed_over_rather_than_read() {
        let nonsense = [
            "",
            "go: cannot find main module",
            "{",
            "{\"unclosed\": ",
            "not json at all\n{}\nnor this",
            r#"{"ImportPath":"m","Action":"build-fail"}"#,
            r#"{"m":{"printf":{"error":"analyzer failed"}}}"#,
            r#"{"m":{"printf":[{"posn":"nowhere","message":"no place"}]}}"#,
            r#"{"m":{"printf":[{"posn":"/src/main.go:oops:1","message":"no line"}]}}"#,
            "\u{1f980}",
        ];
        for output in nonsense {
            assert!(
                what_the_compiler_reported(output, Path::new("/project"), read_the_captured_module)
                    .is_empty(),
                "read a diagnostic out of {output:?}"
            );
            assert!(
                what_vet_reported(output, Path::new("/src"), read_the_captured_module).is_empty(),
                "read a finding out of {output:?}"
            );
        }
    }

    /// A place past the end of the file lands at the end of what there is.
    /// The tool measured its columns against a file the editor may have
    /// changed since, and a panic in a diagnostics reader would take the
    /// editor down over a stale count.
    #[test]
    fn a_place_past_the_end_lands_at_the_end() {
        assert_eq!(
            utf16_position_of("package main\n", 9_000, Some(9_000)),
            lsp::Position::new(8_999, 0)
        );
        assert_eq!(
            utf16_position_of("package main\n", 1, Some(9_000)),
            lsp::Position::new(0, 12)
        );
        assert_eq!(utf16_position_of("", 1, None), lsp::Position::new(0, 0));
        // Inside a character rather than between two: the column belongs to
        // the character it falls in.
        assert_eq!(
            utf16_position_of("\u{1f980}x\n", 1, Some(3)),
            lsp::Position::new(0, 0)
        );
    }

    /// A file the tool saw and the editor cannot read is skipped. The columns
    /// are only meaningful against the text they were measured on, and a
    /// diagnostic on the wrong line is worse than one not shown.
    #[test]
    fn a_file_that_cannot_be_read_is_skipped_rather_than_placed_by_guess() {
        assert!(what_the_compiler_reported(REAL_BUILD, Path::new("/project"), |_| None).is_empty());
        assert!(what_vet_reported(REAL_VET, Path::new("/src"), |_| None).is_empty());
    }

    /// A detail line that names a place of its own is kept as a place. The
    /// earlier case is where the reader has to look next, and a line number
    /// buried in the message is one they have to find by hand.
    #[test]
    fn a_detail_line_that_names_a_place_is_kept_as_a_place_rather_than_as_text() {
        let line = DUPLICATE_SOURCE
            .lines()
            .nth(5)
            .expect("the case holding the emoji");
        let inside = line.rfind("\"dup\"").expect("the value said twice");

        // Three counts of the same place, all different.
        assert_eq!(line[..inside].len(), 18, "bytes");
        assert_eq!(line[..inside].chars().count(), 12, "characters");
        assert_eq!(line[..inside].encode_utf16().count(), 14, "UTF-16 units");

        let reported = what_the_compiler_reported(DUPLICATE_BUILD, Path::new("/project"), |path| {
            (path == Path::new("/project/main.go")).then(|| DUPLICATE_SOURCE.to_string())
        });
        assert_eq!(reported.len(), 1, "{reported:?}");
        let duplicate = &reported[0];

        assert_eq!(
            duplicate.diagnostic.message,
            "duplicate case \"dup\" (constant of type string) in expression switch",
            "the earlier case is a place now, not a line of text"
        );
        assert_eq!(
            duplicate.diagnostic.range,
            lsp::Range {
                start: lsp::Position::new(6, 6),
                end: lsp::Position::new(6, 12),
            },
            "the case the compiler filed the error under, unmoved"
        );
        assert_eq!(
            duplicate.diagnostic.related_information,
            Some(vec![lsp::DiagnosticRelatedInformation {
                location: lsp::Location {
                    uri: lsp::Uri::from_file_path("/project/main.go")
                        .expect("an absolute path is a file URI"),
                    range: lsp::Range {
                        start: lsp::Position::new(5, 14),
                        end: lsp::Position::new(5, 20),
                    },
                },
                message: "previous case".to_string(),
            }]),
            "the protocol's own unit -- not the 18 bytes Go counted from"
        );
    }

    /// Which errors a real build reports and where each one points. Keeping a
    /// detail line as a place must not add, drop or move one of them, and an
    /// error with no such detail must carry no related place at all.
    #[test]
    fn which_errors_are_reported_and_where_they_point_is_unchanged() {
        let reported =
            what_the_compiler_reported(REAL_BUILD, Path::new("/project"), read_the_captured_module);
        let placed: Vec<(&Path, lsp::Range, &str)> = reported
            .iter()
            .map(|one| {
                (
                    one.path.as_path(),
                    one.diagnostic.range,
                    one.diagnostic.message.as_str(),
                )
            })
            .collect();
        assert_eq!(
            placed,
            vec![
                (
                    Path::new("/project/sub/sub.go"),
                    lsp::Range {
                        start: lsp::Position::new(3, 8),
                        end: lsp::Position::new(3, 18),
                    },
                    "cannot use \"a string\" (untyped string constant) as int value in return statement",
                ),
                (
                    Path::new("/project/main.go"),
                    lsp::Range {
                        start: lsp::Position::new(6, 5),
                        end: lsp::Position::new(6, 16),
                    },
                    "declared and not used: unusedValue",
                ),
                (
                    Path::new("/project/main.go"),
                    lsp::Range {
                        start: lsp::Position::new(8, 31),
                        end: lsp::Position::new(8, 44),
                    },
                    "undefined: missingHelper",
                ),
            ]
        );
        for one in &reported {
            assert_eq!(one.diagnostic.related_information, None, "{one:?}");
        }
    }

    /// The compiler indents the detail under a type error rather than
    /// repeating the place, so those lines belong to the message above them
    /// and not to a line of their own.
    #[test]
    fn the_detail_indented_under_a_message_stays_with_it() {
        let stream = format!(
            "{}\n{}\n{}\n",
            r##"{"ImportPath":"m","Action":"build-output","Output":"# m\n"}"##,
            r#"{"ImportPath":"m","Action":"build-output","Output":"./main.go:6:2: cannot use x as string value\n"}"#,
            r#"{"ImportPath":"m","Action":"build-output","Output":"\thave (int)\n\twant (string)\n"}"#,
        );
        let reported =
            what_the_compiler_reported(&stream, Path::new("/project"), |_| Some(built_file()));
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert_eq!(
            reported[0].diagnostic.message,
            "cannot use x as string value\nhave (int)\nwant (string)"
        );
    }

    /// An analyzer's suggested fix is read under the analyzer's own name for
    /// it, placed in the protocol's own unit, and carrying the text it
    /// expects to replace. Go reports a byte offset, the protocol wants
    /// UTF-16 code units, and the characters on the line are a third number
    /// again.
    #[test]
    fn a_suggested_fix_is_read_with_its_own_words_and_placed_in_utf16_units() {
        let line = WITH_FIXES_SOURCE
            .lines()
            .nth(9)
            .expect("the line the format string is on");
        let format = line.find("2006-02-01").expect("the format string");

        // Three counts of the same place, all different.
        assert_eq!(line[..format].len(), 58, "bytes");
        assert_eq!(line[..format].chars().count(), 47, "characters");
        assert_eq!(line[..format].encode_utf16().count(), 48, "UTF-16 units");

        let reported = over_the_fixture_with_fixes(WITH_FIXES);
        let fixes = fixes_named(&reported, "timeformat");
        assert_eq!(fixes.len(), 1, "{fixes:?}");
        assert_eq!(fixes[0].title, "Replace 2006-02-01 with 2006-01-02");
        assert_eq!(fixes[0].replacements.len(), 1);
        let replacement = &fixes[0].replacements[0];
        assert_eq!(replacement.path, Path::new("/src/main.go"));
        assert_eq!(
            replacement.range,
            lsp::Range {
                start: lsp::Position::new(9, 48),
                end: lsp::Position::new(9, 58),
            },
            "the protocol's own unit -- not the 58 bytes Go counts to get there"
        );
        assert_eq!(replacement.new_text, "2006-01-02");
        assert_eq!(replacement.replaced, "2006-02-01");
    }

    /// One finding's alternatives are read as separate fixes, each whole. The
    /// two edits of `stringintconv`'s second alternative are halves of one
    /// change, and both belong to it.
    #[test]
    fn each_alternative_is_one_fix_however_many_places_it_touches() {
        let fixes = fixes_named(&over_the_fixture_with_fixes(WITH_FIXES), "stringintconv");
        let shape: Vec<(&str, usize)> = fixes
            .iter()
            .map(|fix| (fix.title.as_str(), fix.replacements.len()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("Format the number as a decimal", 1),
                ("Convert a single rune to a string", 2),
            ]
        );
        // A pure insertion: it replaces nothing, and both halves are one.
        assert_eq!(fixes[1].replacements[0].new_text, "rune(");
        assert_eq!(fixes[1].replacements[0].replaced, "");
        assert_eq!(fixes[1].replacements[1].new_text, ")");
    }

    /// `go vet` grades nothing, so the only grade this reader has is the
    /// contract every analyzer writes to: a fix's edits must not overlap, an
    /// edit's end must not precede its start, and a fix the reader cannot
    /// name or that changes nothing is no fix. A fix that breaks any of that
    /// is refused whole -- half of a two-part change applied alone would
    /// leave the file broken.
    #[test]
    fn a_fix_that_breaks_the_analyzer_contract_is_refused_whole() {
        const TIMEFORMAT: &str = "Replace 2006-02-01 with 2006-01-02";
        const TWO_EDITS: &str = "Convert a single rune to a string";
        let refused: [(&str, String, &[&str]); 4] = [
            (
                "an overlap",
                WITH_FIXES.replace("\"end\": 198,", "\"end\": 205,"),
                &[TWO_EDITS],
            ),
            (
                "an end before its start",
                WITH_FIXES.replace("\"end\": 155,", "\"end\": 5,"),
                &[TIMEFORMAT],
            ),
            ("no name", WITH_FIXES.replace(TIMEFORMAT, ""), &[TIMEFORMAT]),
            (
                // The real arrays move to a field nothing reads, leaving
                // every fix with nothing to apply.
                "no edits",
                WITH_FIXES.replace("\"edits\": [", "\"edits\": [], \"unread\": ["),
                &[TIMEFORMAT, TWO_EDITS],
            ),
        ];
        for (why, output, gone) in refused {
            assert_ne!(output, WITH_FIXES, "{why}: the fixture shape has moved");
            let reported = over_the_fixture_with_fixes(&output);
            assert!(
                !reported.is_empty(),
                "{why}: the findings themselves must survive"
            );
            let named: Vec<String> = reported
                .iter()
                .flat_map(|one| one.fixes.iter().map(|fix| fix.title.clone()))
                .collect();
            for refused in gone {
                assert!(
                    !named.iter().any(|title| title == refused),
                    "{why}: {named:?}"
                );
            }
        }
    }

    /// A byte offset past the end of the text lands at the end of what there
    /// is, and one inside a character on the boundary before it. The report
    /// was measured against a file the editor may have changed since, and a
    /// panic in a diagnostics reader would take the editor down over a stale
    /// count.
    #[test]
    fn a_byte_offset_past_the_end_lands_at_the_end() {
        assert_eq!(
            utf16_position_at("package main\n", 9_000),
            lsp::Position::new(1, 0)
        );
        assert_eq!(
            utf16_position_at("a\n\u{1f980}b\n", 3),
            lsp::Position::new(1, 0),
            "the second line, before the emoji"
        );
        assert_eq!(
            utf16_position_at("a\n\u{1f980}b\n", 5),
            lsp::Position::new(1, 0),
            "inside the emoji, so on the boundary before it"
        );
        assert_eq!(
            utf16_position_at("a\n\u{1f980}b\n", 6),
            lsp::Position::new(1, 2),
            "past the emoji, which is two UTF-16 units and four bytes"
        );
        assert_eq!(utf16_position_at("", 0), lsp::Position::new(0, 0));
    }
}
