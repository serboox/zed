use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use rayon::prelude::*;
use streaming_iterator::StreamingIterator as _;

use crate::languages::{self, Readable};
use crate::measure::{Spread, as_memory, as_time, spread_of};
use crate::walk;

/// How much further than Rust's own cost, per megabyte of source, a language
/// may run before the plan calls it a badly written query. Public so the
/// binary that gates on this reuses the same number [`Report`] prints beside.
pub const OVER_RUST_BY: f64 = 2.0;

/// What one readable language cost to read.
#[derive(Debug, Clone, Default)]
pub struct LanguageCost {
    pub name: String,
    pub files: usize,
    pub bytes: u64,
    pub symbols: usize,
    /// Parsing every one of this language's files and running its outline
    /// query over them, on the pass's own thread pool.
    pub took: Duration,
    /// The distribution `took` is made of, file by file -- the tail is what
    /// shows up a single pathological file the total alone would hide.
    pub parsing_a_file: Spread,
    /// `took` scaled to what one megabyte of this language's own source would
    /// cost. `Duration::ZERO` for a language with no bytes to scale by,
    /// rather than a division the caller has to guard against.
    pub per_megabyte: Duration,
}

impl LanguageCost {
    /// Whether this language costs strictly more than [`OVER_RUST_BY`] times
    /// `rust_per_megabyte` -- the plan's own gate for this step. A language at
    /// exactly the multiple is still inside the gate; the plan says "dearer
    /// than that", not "dearer or equal".
    pub fn costs_over_twice(&self, rust_per_megabyte: Duration) -> bool {
        self.per_megabyte.as_secs_f64() > rust_per_megabyte.as_secs_f64() * OVER_RUST_BY
    }
}

/// Every readable language's own cost, in the order [`crate::languages::readable`]
/// returns them.
pub struct Report {
    pub languages: Vec<LanguageCost>,
}

impl Report {
    fn rust_per_megabyte(&self) -> Option<Duration> {
        self.languages
            .iter()
            .find(|language| language.name == "rust")
            .map(|language| language.per_megabyte)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rust_per_megabyte = self.rust_per_megabyte();
        for language in &self.languages {
            let over = rust_per_megabyte.is_some_and(|rust| language.costs_over_twice(rust));
            writeln!(
                out,
                "{:<12} {:>5} files  {:>10}  {:>11} a megabyte  {:>7} symbols  \
                 parsing a file: median {:>9} 95th {:>9}{}",
                language.name,
                language.files,
                as_memory(language.bytes),
                as_time(language.per_megabyte),
                language.symbols,
                as_time(language.parsing_a_file.median),
                as_time(language.parsing_a_file.ninety_fifth),
                if over {
                    "  <- over twice Rust's cost"
                } else {
                    ""
                },
            )?;
        }
        Ok(())
    }
}

/// Measures every language [`crate::languages::readable`] returns, each on
/// its own timed pass over its own files, sharing one thread pool across all
/// of them.
///
/// One walk of the project, not one per language: files are read from disk
/// once and sorted into which language claims each, the same longest-suffix
/// rule the rest of the crate uses, and only then is each language's own
/// share parsed and timed -- so the "how long the pass took" this reports is
/// a real, attributable duration for that language alone, not a guess at how
/// a shared, interleaved pass would have split.
pub fn measure_all(root: &Path, cores: usize) -> Result<Report> {
    let (readable, refused) = languages::readable();
    anyhow::ensure!(
        !readable.is_empty(),
        "no built-in language has an outline query to run"
    );
    for trouble in &refused {
        log::warn!("outline query left out of the per-language measurement -- {trouble}");
    }

    let claimed = languages::suffixes_of(&readable);
    let mut by_language: Vec<Vec<PathBuf>> = readable.iter().map(|_| Vec::new()).collect();
    for path in walk::files_under(root) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(at) = languages::claimant(name, &claimed) else {
            continue;
        };
        let Some(files) = by_language.get_mut(at) else {
            continue;
        };
        files.push(path);
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cores.max(1))
        .build()
        .context("building the pool the pass runs on")?;

    let languages = readable
        .iter()
        .zip(by_language)
        .map(|(language, files)| measure_one(language, &files, &pool))
        .collect();

    Ok(Report { languages })
}

fn measure_one(language: &Readable, files: &[PathBuf], pool: &rayon::ThreadPool) -> LanguageCost {
    let started = Instant::now();
    let per_file: Vec<(Duration, u64, usize)> = pool.install(|| {
        files
            .par_iter()
            .filter_map(|path| read_one(path, language))
            .collect()
    });
    let took = started.elapsed();

    let bytes: u64 = per_file.iter().map(|(_, bytes, _)| *bytes).sum();
    let symbols: usize = per_file.iter().map(|(_, _, symbols)| *symbols).sum();
    let parsing_a_file = spread_of(per_file.iter().map(|(parsing, _, _)| *parsing).collect());

    LanguageCost {
        name: language.name.clone(),
        files: per_file.len(),
        bytes,
        symbols,
        took,
        parsing_a_file,
        per_megabyte: per_megabyte_cost(took, bytes),
    }
}

/// Parses one file and runs its outline query over it, the same way
/// [`crate::measure::measure`] costs a file -- parsing alone, not the file
/// read that comes before it. `None` for a file that cannot be read or
/// parsed at all: one file fewer in the language's own numbers, not a reason
/// to abandon the pass.
fn read_one(path: &Path, language: &Readable) -> Option<(Duration, u64, usize)> {
    let source = std::fs::read(path).ok()?;
    let bytes = source.len() as u64;

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language.grammar).ok()?;
    let started = Instant::now();
    let tree = parser.parse(&source, None)?;
    let parsing = started.elapsed();

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut symbols = 0usize;
    let mut matches = cursor.matches(&language.outline, tree.root_node(), source.as_slice());
    while matches.next().is_some() {
        symbols += 1;
    }

    Some((parsing, bytes, symbols))
}

fn per_megabyte_cost(took: Duration, bytes: u64) -> Duration {
    if bytes == 0 {
        return Duration::ZERO;
    }
    let megabytes = bytes as f64 / (1024. * 1024.);
    Duration::from_secs_f64(took.as_secs_f64() / megabytes)
}

/// Whether a capture the outline query yields is a real declaration, for the
/// languages where it yields something else besides.
///
/// The outline query is the editor's own -- shared with the outline panel,
/// go-to-symbol, and everything else that reads `crate::definitions::in_file`
/// -- and it is not the index's place to change what the editor already
/// agrees is a definition. What it is the index's place to do is decide,
/// once, which of that query's own captures are worth keeping in a project-
/// wide symbol index rather than in a single file's own outline, where a
/// nested `let` or an object literal's own key is legitimately useful to see
/// listed beside the function that holds it.
///
/// For every language but TypeScript, TSX and JavaScript this is `true`
/// unconditionally: reading `crates/go/outline.scm`, `python/outline.scm`,
/// `c/outline.scm` and `cpp/outline.scm` end to end found nothing of the
/// same shape -- Go's own `var`/`const` capture is already anchored to
/// `source_file`, so it has no nested-in-a-function variant to begin with.
///
/// TypeScript, TSX and JavaScript are one case, not three: `tsx/outline.scm`
/// is byte-identical to `typescript/outline.scm` (verified: same checksum),
/// and JavaScript's own `config.toml` names `tsx` as the grammar it is parsed
/// with and ships no `outline.scm` of its own, so it inherits the same
/// query. Whatever is noise in one is noise in all three, which is why this
/// function is keyed by three names rather than one.
pub fn is_declaration(language: &str, defined: tree_sitter::Node) -> bool {
    if !matches!(language, "typescript" | "tsx" | "javascript") {
        return true;
    }
    match defined.kind() {
        // An object literal's own `key: value` entry. The query's own
        // comment already calls these "Object properties"; nothing else in
        // outline.scm captures a `pair` node, so the kind alone is enough.
        "pair" => false,
        // The `describe(...)`/`it(...)`/`test(...)` wrapper a test file
        // opens with, captured by name so the runner's own suite and case
        // titles show up in an outline. A project-wide symbol index has no
        // use for "does the thing" as a findable name, and `call_expression`
        // is not the kind of any other capture in this query, so nothing
        // else is caught by dropping it.
        "call_expression" => false,
        // A method defined inside an object literal -- `{ connect() {} }` --
        // uses the very same `method_definition` kind a real class method
        // does; the query itself tells the two apart by which node the
        // method sits in, `(object (method_definition ...))` against
        // `(class_body (method_definition ...))`, so the filter has to ask
        // the same question rather than trust the kind alone. Grouped with
        // `pair` under "object literal properties": a method attached to a
        // value is the same kind of member-of-a-value noise as a key is.
        "method_definition" => !is_object_literal_method(defined),
        // A `let`/`const` binding. The query captures this identically --
        // same kind, `variable_declarator` for a plain name and `identifier`
        // (or, for a destructured object key with no renaming,
        // `shorthand_property_identifier_pattern`) for a destructured one --
        // whether it sits at the top of the file, right after `export`, or
        // inside a function body several calls deep. Kind alone cannot tell
        // a module-level constant worth finding from a loop counter, so this
        // one case looks past the captured node to where it actually lives.
        "variable_declarator" | "identifier" | "shorthand_property_identifier_pattern" => {
            !nested_inside_a_block(defined)
        }
        _ => true,
    }
}

/// Whether `defined` -- a `method_definition` node -- sits inside an object
/// literal (`(object (method_definition ...))`) rather than a class body
/// (`(class_body (method_definition ...))`). A `method_definition`'s parent
/// is always exactly one of those two node kinds in this grammar, so one
/// direct parent check is enough; no need to walk further.
fn is_object_literal_method(defined: tree_sitter::Node) -> bool {
    defined
        .parent()
        .is_some_and(|parent| parent.kind() == "object")
}

/// Whether `defined` -- a `variable_declarator`, or the identifier-shaped
/// node a destructuring pattern captures -- belongs to a `let`/`const` whose
/// own `lexical_declaration` sits inside a `statement_block`, rather than
/// directly under `program` or under an `export_statement`.
///
/// The three query patterns this distinguishes -- top-level, exported, and
/// nested -- put `lexical_declaration` at a different depth below `defined`
/// depending on how deep the destructuring pattern nests (a plain name is one
/// hop up; a rest element inside an array pattern is several), so this walks
/// up looking for it by kind rather than assuming a fixed number of parents.
fn nested_inside_a_block(defined: tree_sitter::Node) -> bool {
    let mut current = defined;
    while let Some(parent) = current.parent() {
        if parent.kind() == "lexical_declaration" {
            return parent
                .parent()
                .is_some_and(|grandparent| grandparent.kind() == "statement_block");
        }
        current = parent;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_language() -> Readable {
        let (readable, _) = languages::readable();
        readable
            .into_iter()
            .find(|language| language.name == "rust")
            .expect("Rust is one of the languages the editor ships")
    }

    fn typescript_language() -> Readable {
        let (readable, _) = languages::readable();
        readable
            .into_iter()
            .find(|language| language.name == "typescript")
            .expect("TypeScript is one of the languages the editor ships")
    }

    /// The index of a capture by name, the same small lookup
    /// [`crate::definitions::in_file`] uses; kept here rather than shared,
    /// since that function's own copy is private to its module.
    fn capture_index(query: &tree_sitter::Query, name: &str) -> Option<u32> {
        query
            .capture_names()
            .iter()
            .position(|capture| *capture == name)
            .map(|at| at as u32)
    }

    /// Runs `language`'s own outline query over `source` exactly the way
    /// [`crate::definitions::in_file`] does, and -- when `filtered` is true --
    /// through [`is_declaration`] right where the wiring this module hands
    /// off would put it: after the primary `@item` node is found, before it
    /// is ever turned into anything that leaves this function. Returns each
    /// kept match's own grammar kind alongside its name, so a test can tell
    /// two same-named things apart by what kind of node found them.
    fn defined_in(source: &str, language: &Readable, filtered: bool) -> Vec<(String, String)> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language.grammar)
            .expect("the grammar loads");
        let tree = parser.parse(source, None).expect("the fixture parses");

        let item = capture_index(&language.outline, "item").expect("the query captures @item");
        let name = capture_index(&language.outline, "name").expect("the query captures @name");

        let mut found = Vec::new();
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&language.outline, tree.root_node(), source.as_bytes());
        while let Some(matched) = matches.next() {
            let Some(defined) = matched
                .captures
                .iter()
                .find(|capture| capture.index == item)
                .map(|capture| capture.node)
            else {
                continue;
            };
            if filtered && !is_declaration(&language.name, defined) {
                continue;
            }
            let named: Vec<&str> = matched
                .captures
                .iter()
                .filter(|capture| capture.index == name)
                .filter_map(|capture| capture.node.utf8_text(source.as_bytes()).ok())
                .collect();
            if named.is_empty() {
                continue;
            }
            found.push((defined.kind().to_string(), named.join(" ")));
        }
        found
    }

    /// A real function, a real class with a real method, an object literal
    /// with properties and a method of its own, a `let` nested inside a
    /// function body, and a test-runner call -- one fixture holding every
    /// shape the plan names as noise, plus every shape that is not.
    const A_TYPESCRIPT_FILE: &str = r#"
function realFunction() {
    return 1;
}

class RealClass {
    method() {
        let nested = 1;
        return nested;
    }
}

const config = {
    host: "localhost",
    port: 8080,
    connect() {
        return true;
    },
};

describe("a suite", () => {
    it("does the thing", () => {
        return true;
    });
});
"#;

    #[test]
    fn declarations_are_kept_and_the_structural_captures_are_filtered() {
        let mut names: Vec<String> = defined_in(A_TYPESCRIPT_FILE, &typescript_language(), true)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        // Sorted before comparing: the query's own cross-pattern match order
        // is not something this test should have to assume.
        names.sort();
        let mut expected = vec!["realFunction", "RealClass", "method", "config"];
        expected.sort();
        assert_eq!(
            names, expected,
            "kept exactly the function, the class, its method, and the \
             top-level const -- nothing nested, no object property, no \
             object literal method, no test wrapper: {names:?}"
        );
    }

    /// The same fixture without the filter finds more -- proof the filter
    /// does something, not only that it compiles and returns a plausible
    /// answer.
    #[test]
    fn without_the_filter_the_same_fixture_yields_more() {
        let filtered = defined_in(A_TYPESCRIPT_FILE, &typescript_language(), true);
        let unfiltered = defined_in(A_TYPESCRIPT_FILE, &typescript_language(), false);
        assert!(
            unfiltered.len() > filtered.len(),
            "filtered {filtered:?}, unfiltered {unfiltered:?}"
        );
        assert_eq!(filtered.len(), 4, "{filtered:?}");
        // The six the filter takes out, one of each noise shape the fixture
        // carries: the nested `let`, both object properties, the object
        // literal's own method, and both test-runner call titles.
        assert_eq!(unfiltered.len(), 10, "{unfiltered:?}");
        let unfiltered_names: Vec<&str> =
            unfiltered.iter().map(|(_, name)| name.as_str()).collect();
        for dropped in [
            "nested",
            "host",
            "port",
            "connect",
            "a suite",
            "does the thing",
        ] {
            assert!(
                unfiltered_names.contains(&dropped),
                "{dropped} should still be there unfiltered: {unfiltered_names:?}"
            );
        }
    }

    /// Nothing that is a real declaration in Rust is dropped: the filter is a
    /// TypeScript-family concern, and for every other language
    /// `is_declaration` answers `true` before it ever looks at a kind.
    #[test]
    fn a_rust_fixture_is_unaffected_by_the_filter() {
        const A_RUST_FILE: &str = "pub struct Thing {\n    field: u32,\n}\n\npub fn work() {\n    let local = 1;\n    local\n}\n\npub const LIMIT: u32 = 10;\n";
        let filtered = defined_in(A_RUST_FILE, &rust_language(), true);
        let unfiltered = defined_in(A_RUST_FILE, &rust_language(), false);
        assert_eq!(
            filtered.len(),
            unfiltered.len(),
            "filtered {filtered:?}, unfiltered {unfiltered:?}"
        );
        assert_eq!(filtered, unfiltered);
    }

    fn cost(files: usize, bytes: u64, symbols: usize, took: Duration) -> LanguageCost {
        LanguageCost {
            name: "test".to_string(),
            files,
            bytes,
            symbols,
            took,
            parsing_a_file: Spread::default(),
            per_megabyte: per_megabyte_cost(took, bytes),
        }
    }

    #[test]
    fn per_megabyte_scales_the_whole_pass_to_one_megabyte_of_source() {
        let one_megabyte = 1024 * 1024;
        let language = cost(1, one_megabyte, 10, Duration::from_millis(500));
        assert_eq!(language.per_megabyte, Duration::from_millis(500));

        let half_megabyte = cost(1, one_megabyte / 2, 5, Duration::from_millis(250));
        assert_eq!(half_megabyte.per_megabyte, Duration::from_millis(500));
    }

    #[test]
    fn a_language_with_no_files_costs_nothing_and_answers_rather_than_panics() {
        let language = cost(0, 0, 0, Duration::ZERO);
        assert_eq!(language.files, 0);
        assert_eq!(language.per_megabyte, Duration::ZERO);
        assert_eq!(language.parsing_a_file, Spread::default());
    }

    #[test]
    fn a_language_with_files_but_zero_bytes_does_not_divide_by_zero() {
        // Every file present read as empty: an edge a real project will not
        // produce, but one `per_megabyte_cost` still has to answer rather
        // than divide by zero for.
        let language = cost(3, 0, 0, Duration::from_millis(10));
        assert_eq!(language.per_megabyte, Duration::ZERO);
    }

    #[test]
    fn exactly_twice_rust_is_still_inside_the_gate() {
        let rust = Duration::from_millis(100);
        let mut language = cost(1, 1024 * 1024, 1, Duration::ZERO);
        language.per_megabyte = Duration::from_millis(200);
        assert!(
            !language.costs_over_twice(rust),
            "exactly twice is not over twice"
        );
    }

    #[test]
    fn just_over_twice_rust_is_outside_the_gate() {
        let rust = Duration::from_millis(100);
        let mut language = cost(1, 1024 * 1024, 1, Duration::ZERO);
        language.per_megabyte = Duration::from_micros(200_001);
        assert!(
            language.costs_over_twice(rust),
            "one microsecond over twice has to trip it"
        );
    }

    #[test]
    fn well_under_twice_rust_is_inside_the_gate() {
        let rust = Duration::from_millis(100);
        let mut language = cost(1, 1024 * 1024, 1, Duration::ZERO);
        language.per_megabyte = Duration::from_millis(120);
        assert!(!language.costs_over_twice(rust));
    }
}
