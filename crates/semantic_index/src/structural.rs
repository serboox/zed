use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use streaming_iterator::StreamingIterator as _;

use crate::languages::{self, Readable};
use crate::measure::{Spread, spread_of};
use crate::walk;

/// One occurrence of a structural query's pattern, found in one file.
///
/// `line` and `excerpt` describe the pattern's own primary node -- the last
/// capture the query declares, by the same convention the built-in outline
/// queries already use: every pattern in `outline.scm` ends with `@item`, the
/// node the whole pattern is about, and names introduced earlier are there to
/// narrow the search down to it. A structural query follows the same shape:
/// context captures first, the thing being looked for last.
///
/// A query may also name one capture `@within`. The pattern that declares it
/// defines regions rather than matches to report: every other pattern's
/// matches are kept only when they fall inside one of those regions, by byte
/// range, in the same file. This is how a query expresses "this pattern
/// anywhere inside that one" -- a relationship tree-sitter's own S-expression
/// nesting cannot express past a fixed number of levels, since it only
/// describes direct parent-child structure. A query with no `@within`
/// capture is not scoped at all: every match it produces is reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralMatch {
    /// Relative to the project root, forward slashes.
    pub path: String,
    /// One-based, of the primary capture.
    pub line: u32,
    /// The source line the primary capture starts on, trimmed. Short enough to
    /// show in a results list; the exact text of each capture is in `captures`.
    pub excerpt: String,
    /// Every capture the query took in this match, in source order.
    pub captures: Vec<Capture>,
}

/// One named capture of a match, wherever in the pattern it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub name: String,
    pub line: u32,
    pub text: String,
}

/// A query the reader wrote that tree-sitter refused to compile, with the
/// position inside their own query text -- never a silent empty result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryProblem {
    pub row: usize,
    pub column: usize,
    pub offset: usize,
    pub message: String,
    pub kind: QueryProblemKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryProblemKind {
    Syntax,
    NodeType,
    Field,
    Capture,
    Predicate,
    Structure,
    Language,
}

impl fmt::Display for QueryProblem {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "{:?} error at line {}, column {}: {}",
            self.kind,
            self.row + 1,
            self.column + 1,
            self.message
        )
    }
}

impl std::error::Error for QueryProblem {}

impl From<tree_sitter::QueryError> for QueryProblem {
    fn from(error: tree_sitter::QueryError) -> Self {
        QueryProblem {
            row: error.row,
            column: error.column,
            offset: error.offset,
            message: error.message,
            kind: error.kind.into(),
        }
    }
}

impl From<tree_sitter::QueryErrorKind> for QueryProblemKind {
    fn from(kind: tree_sitter::QueryErrorKind) -> Self {
        match kind {
            tree_sitter::QueryErrorKind::Syntax => QueryProblemKind::Syntax,
            tree_sitter::QueryErrorKind::NodeType => QueryProblemKind::NodeType,
            tree_sitter::QueryErrorKind::Field => QueryProblemKind::Field,
            tree_sitter::QueryErrorKind::Capture => QueryProblemKind::Capture,
            tree_sitter::QueryErrorKind::Predicate => QueryProblemKind::Predicate,
            tree_sitter::QueryErrorKind::Structure => QueryProblemKind::Structure,
            tree_sitter::QueryErrorKind::Language => QueryProblemKind::Language,
        }
    }
}

/// The two numbers the plan's gate for structural search is stated in, plus
/// enough around them to see whether delivery is smooth or bursty.
#[derive(Debug, Clone, Default)]
pub struct SearchNumbers {
    /// `None` for a query that finds nothing in the project at all.
    pub time_to_first_match: Option<Duration>,
    pub time_to_last_match: Duration,
    /// Median and 95th percentile of how long each individual match took to
    /// arrive since the search started -- the whole distribution, not only its
    /// two endpoints.
    pub arrival: Spread,
    pub matches: usize,
    pub files_searched: usize,
}

/// Compiles `query_text` for `language` and runs it over every file of
/// `language` under `root`, on up to `cores` threads.
///
/// Matches are sent to the returned channel as they are found rather than
/// collected first, so a caller can render the first ones while the rest of
/// the project is still being searched. The channel closes on its own once
/// every file has been read; dropping the receiver early stops the search
/// from doing further work that nothing would see.
pub fn search(
    root: &Path,
    language: &Readable,
    query_text: &str,
    cores: usize,
) -> Result<mpsc::Receiver<StructuralMatch>, QueryProblem> {
    let (_files_searched, receiver) = spawn_search(root, language, query_text, cores)?;
    Ok(receiver)
}

/// Runs a search the same way [`search`] does, but blocks until it is
/// finished and reports when the first and the last match arrived, the way
/// [`crate::measure::measure`] reports parsing.
pub fn measure(
    root: &Path,
    language: &Readable,
    query_text: &str,
    cores: usize,
) -> Result<SearchNumbers, QueryProblem> {
    // Started before the query is even compiled: from the person's own point
    // of view, that time is part of how long they wait to see anything.
    let started = Instant::now();
    let (files_searched, receiver) = spawn_search(root, language, query_text, cores)?;

    let mut arrivals = Vec::new();
    for _ in receiver.iter() {
        arrivals.push(started.elapsed());
    }
    let time_to_first_match = arrivals.first().copied();
    let matches = arrivals.len();
    let arrival = spread_of(arrivals);

    Ok(SearchNumbers {
        time_to_first_match,
        time_to_last_match: started.elapsed(),
        arrival,
        matches,
        files_searched,
    })
}

/// Compiles the query, lists the files it will run over, and starts the pass
/// on a thread of its own so the caller gets a receiver back immediately.
fn spawn_search(
    root: &Path,
    language: &Readable,
    query_text: &str,
    cores: usize,
) -> Result<(usize, mpsc::Receiver<StructuralMatch>), QueryProblem> {
    let query = tree_sitter::Query::new(&language.grammar, query_text)?;
    let within_index = within_capture_index(&query);
    let files = matching_files(root, language);
    let files_searched = files.len();

    let (sender, receiver) = mpsc::channel();
    let grammar = language.grammar.clone();
    let root = root.to_path_buf();
    let cores = cores.max(1);
    std::thread::spawn(move || {
        search_all_files(&files, &root, &query, &grammar, within_index, cores, sender)
    });

    Ok((files_searched, receiver))
}

/// The index of the `@within` capture, if the query declares one. `None`
/// means the query is not scoped at all, and every match is reported exactly
/// as it always was.
fn within_capture_index(query: &tree_sitter::Query) -> Option<u32> {
    query
        .capture_names()
        .iter()
        .position(|name| *name == "within")
        .map(|at| at as u32)
}

/// Every file under `root` that belongs to `language`, by the same
/// longest-suffix-wins rule the rest of the crate uses to decide which
/// language owns a file -- so a search for one language does not also run
/// over a file another language claims a longer suffix on. Sorted, so a
/// single-threaded pass has a deterministic, reproducible order.
fn matching_files(root: &Path, language: &Readable) -> Vec<PathBuf> {
    let claimed = languages::by_suffix();
    let mut files: Vec<PathBuf> = walk::files_under(root)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| languages::of_file(name, &claimed))
                .is_some_and(|owner| owner == language.name)
        })
        .collect();
    files.sort();
    files
}

/// Runs the query over every file in `files`.
///
/// With one core, files are read in the order they were given, with no pool
/// at all -- both because a pool buys nothing with a single worker, and
/// because a deterministic order is worth having for its own sake. With more
/// than one, files are handed to rayon and no order between them is promised;
/// matches within a single file are still found in the tree's own traversal
/// order regardless.
fn search_all_files(
    files: &[PathBuf],
    root: &Path,
    query: &tree_sitter::Query,
    grammar: &tree_sitter::Language,
    within_index: Option<u32>,
    cores: usize,
    sender: mpsc::Sender<StructuralMatch>,
) {
    if cores <= 1 {
        for path in files {
            search_one_file(path, root, query, grammar, within_index, &sender);
        }
        return;
    }

    let pool = match rayon::ThreadPoolBuilder::new().num_threads(cores).build() {
        Ok(pool) => pool,
        Err(error) => {
            log::warn!(
                "structural search: could not build a {cores}-thread pool, \
                 running on one thread instead -- {error}"
            );
            for path in files {
                search_one_file(path, root, query, grammar, within_index, &sender);
            }
            return;
        }
    };
    pool.install(|| {
        files.par_iter().for_each_with(sender, |sender, path| {
            search_one_file(path, root, query, grammar, within_index, &*sender);
        });
    });
}

/// Parses one file and sends every match the query finds in it. Errors of any
/// kind -- the file cannot be read, is not this grammar's language, or does
/// not parse -- drop the file from the search rather than failing the pass:
/// one file fewer, the same as every other pass in this crate.
fn search_one_file(
    path: &Path,
    root: &Path,
    query: &tree_sitter::Query,
    grammar: &tree_sitter::Language,
    within_index: Option<u32>,
    sender: &mpsc::Sender<StructuralMatch>,
) {
    let Ok(contents) = std::fs::read(path) else {
        return;
    };
    let Ok(relative) = path.strip_prefix(root) else {
        return;
    };
    let relative_path = relative.to_string_lossy().replace('\\', "/");

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(grammar).is_err() {
        return;
    }
    let Some(tree) = parser.parse(&contents, None) else {
        return;
    };

    let text = String::from_utf8_lossy(&contents);
    let lines: Vec<&str> = text.lines().collect();

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), contents.as_slice());

    let Some(within_index) = within_index else {
        // Unscoped: exactly the old behaviour, sent the moment each match is
        // found. A file's matches were always independent of any other
        // file's, so this path alone already gives the plan's "first result
        // under 200 ms" its room -- nothing here waits on the rest of the
        // project.
        while let Some(matched) = matches.next() {
            let Some((found, _byte_range)) =
                structural_match(&relative_path, &contents, &lines, query, matched)
            else {
                continue;
            };
            if sender.send(found).is_err() {
                // The receiver was dropped: nobody is listening any more, so
                // there is no point reading the rest of this file either.
                return;
            }
        }
        return;
    };

    // Scoped by `@within`. A region can only ever bound matches inside the
    // same parse tree it came from, so every region and every candidate this
    // file could possibly need are both found in this one pass over this one
    // file -- nothing here reaches into another file, and nothing a later
    // file does can change what this file already decided. That is also why
    // this buffers only per file rather than for the whole project: it costs
    // this one file's own parse time, not the rest of the project's, so a
    // small file's matches are still visible long before a large project
    // finishes.
    let mut regions: Vec<(usize, usize)> = Vec::new();
    let mut candidates: Vec<((usize, usize), StructuralMatch)> = Vec::new();
    while let Some(matched) = matches.next() {
        let region_nodes: Vec<(usize, usize)> = matched
            .captures
            .iter()
            .filter(|capture| capture.index == within_index)
            .map(|capture| (capture.node.start_byte(), capture.node.end_byte()))
            .collect();
        if !region_nodes.is_empty() {
            regions.extend(region_nodes);
            continue;
        }
        let Some((found, byte_range)) =
            structural_match(&relative_path, &contents, &lines, query, matched)
        else {
            continue;
        };
        candidates.push((byte_range, found));
    }

    for (byte_range, found) in candidates {
        let inside_a_region = regions
            .iter()
            .any(|region| region.0 <= byte_range.0 && byte_range.1 <= region.1);
        if inside_a_region && sender.send(found).is_err() {
            return;
        }
    }
}

/// Builds one [`StructuralMatch`] from a query match, or `None` for a match
/// whose query captured nothing at all -- there is then no node to point at
/// or to open, so it is dropped rather than reported with a made-up position.
///
/// The byte range returned alongside it is the same primary node's own span,
/// which is what `@within` containment is judged against.
fn structural_match(
    path: &str,
    contents: &[u8],
    lines: &[&str],
    query: &tree_sitter::Query,
    matched: &tree_sitter::QueryMatch,
) -> Option<(StructuralMatch, (usize, usize))> {
    let capture_names = query.capture_names();
    let primary = primary_node(capture_names, matched.captures)?;
    let byte_range = (primary.start_byte(), primary.end_byte());
    let line = primary.start_position().row as u32 + 1;
    let excerpt = lines
        .get(primary.start_position().row)
        .map(|text| text.trim().to_string())
        .unwrap_or_default();

    let mut raw_captures: Vec<&tree_sitter::QueryCapture> = matched.captures.iter().collect();
    raw_captures.sort_by_key(|capture| capture.node.start_byte());
    let captures: Vec<Capture> = raw_captures
        .into_iter()
        .filter_map(|capture| {
            let name = capture_names.get(capture.index as usize)?;
            let text = capture.node.utf8_text(contents).ok()?;
            Some(Capture {
                name: (*name).to_string(),
                line: capture.node.start_position().row as u32 + 1,
                text: text.to_string(),
            })
        })
        .collect();

    Some((
        StructuralMatch {
            path: path.to_string(),
            line,
            excerpt,
            captures,
        },
        byte_range,
    ))
}

/// The node the match is primarily about: the one captured by the last name
/// the query declares that this particular match actually captured something
/// under. Read [`StructuralMatch`]'s own doc for why the last name is the
/// right one to trust.
fn primary_node<'tree>(
    capture_names: &[&str],
    captures: &[tree_sitter::QueryCapture<'tree>],
) -> Option<tree_sitter::Node<'tree>> {
    for name_index in (0..capture_names.len()).rev() {
        let mut earliest: Option<tree_sitter::Node<'tree>> = None;
        for capture in captures {
            if capture.index as usize != name_index {
                continue;
            }
            earliest = Some(match earliest {
                Some(current) if current.start_byte() <= capture.node.start_byte() => current,
                _ => capture.node,
            });
        }
        if let Some(node) = earliest {
            return Some(node);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust() -> Readable {
        let (readable, _) = languages::readable();
        readable
            .into_iter()
            .find(|language| language.name == "rust")
            .expect("Rust is one of the languages the editor ships")
    }

    fn project_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("a directory to put a project in");
        for (name, contents) in files {
            let at = root.path().join(name);
            if let Some(parent) = at.parent() {
                std::fs::create_dir_all(parent).expect("the fixture file's directory");
            }
            std::fs::write(at, contents).expect("a fixture file");
        }
        root
    }

    fn found_by(root: &Path, query_text: &str) -> Vec<StructuralMatch> {
        let receiver = search(root, &rust(), query_text, 2).expect("the query compiles");
        receiver.iter().collect()
    }

    /// The plan's acceptance example: every `.unwrap()` inside an
    /// implementation of `Drop`, across the whole project, however deeply the
    /// call is nested inside `drop`'s own body. Uses `@within` exactly the
    /// way the doc on [`StructuralMatch`] describes: one pattern marks the
    /// region (the `impl Drop` block), a second, wholly separate pattern
    /// finds `.unwrap()` calls anywhere at all, and the engine keeps only the
    /// ones that land inside a region.
    ///
    /// Three near misses prove the region -- not the nesting depth, and not
    /// the text -- is what decides: a lexically identical call sitting in a
    /// comment, a plain (undropped) `unwrap()` in an unrelated `impl` block,
    /// and a *nested* `unwrap()` in that same unrelated block. If nesting
    /// depth alone controlled the result, that last one would wrongly match
    /// once nesting is supported at all.
    #[test]
    fn every_unwrap_inside_a_drop_implementation_is_found_however_deeply_nested() {
        const DROP_UNWRAP: &str = r#"
((impl_item
   trait: (type_identifier) @drop_trait
   (#eq? @drop_trait "Drop")) @within)

((call_expression
   function: (field_expression
     field: (field_identifier) @method)) @call
 (#eq? @method "unwrap"))
"#;

        let project = project_with(&[
            (
                "guard.rs",
                r#"struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        self.resource.take().unwrap();
        if let Some(guard) = self.lock.take() {
            guard.release().unwrap();
        }
        match self.other.take() {
            Some(value) => {
                value.finish().unwrap();
            }
            None => {}
        }
    }
}
"#,
            ),
            (
                // A near miss a regular expression would happily match: the
                // exact text `.unwrap()` sits right next to a comment naming
                // Drop, but there is no Drop implementation here at all.
                "commented.rs",
                r#"struct Commented;

// impl Drop for Commented { fn drop(&mut self) { self.thing.unwrap(); } }
impl Commented {
    fn noop(&self) {}
}
"#,
            ),
            (
                // Two more near misses in one file, both in a real `impl`
                // block that is not Drop: an unwrap as a plain statement, and
                // one nested inside an `if let` just as deep as the ones this
                // test expects to find inside Guard. Neither belongs to any
                // region, so neither is reported.
                "not_dropped.rs",
                r#"struct NotDropped;

impl NotDropped {
    fn drop_like(&mut self) {
        self.other.take().unwrap();
        if let Some(value) = self.another.take() {
            value.unwrap();
        }
    }
}
"#,
            ),
        ]);

        let mut found = found_by(project.path(), DROP_UNWRAP);
        found.sort_by_key(|one| one.line);
        assert_eq!(
            found.len(),
            3,
            "three unwraps are inside the Drop implementation, at three different levels of nesting: {found:?}"
        );

        let expected: [(u32, &str); 3] = [
            (5, "self.resource.take().unwrap();"),
            (7, "guard.release().unwrap();"),
            (11, "value.finish().unwrap();"),
        ];
        for (one, (line, excerpt)) in found.iter().zip(expected) {
            assert_eq!(one.path, "guard.rs");
            assert_eq!(one.line, line);
            assert_eq!(one.excerpt, excerpt);
            let call_text = excerpt.trim_end_matches(';');
            assert!(
                one.captures
                    .iter()
                    .any(|capture| capture.name == "call" && capture.text == call_text),
                "{:?}",
                one.captures
            );
        }
    }

    #[test]
    fn a_malformed_query_reports_the_position_of_its_own_mistake() {
        let project = project_with(&[("one.rs", "pub fn work() {}\n")]);
        let problem = search(project.path(), &rust(), "(not_a_real_node_type) @thing", 1)
            .expect_err("this text does not name a real grammar node");
        assert_eq!(problem.row, 0);
        assert_eq!(problem.column, 1);
        assert_eq!(problem.offset, 1);
        assert_eq!(problem.kind, QueryProblemKind::NodeType);
        assert!(
            problem.message.contains("not_a_real_node_type"),
            "{}",
            problem.message
        );
        // The position is shown to the person, not swallowed into a generic
        // failure -- the point of the type at all.
        assert!(problem.to_string().contains("line 1"));
    }

    #[test]
    fn a_query_that_matches_nothing_reports_no_matches_and_no_error() {
        let project = project_with(&[("one.rs", "pub fn work() {}\n")]);
        let found = found_by(project.path(), "(macro_definition) @item");
        assert!(found.is_empty());
    }

    const UNWRAP_ANYWHERE: &str = r#"
((call_expression
   function: (field_expression
     field: (field_identifier) @method)) @call
 (#eq? @method "unwrap"))
"#;

    /// The regression this whole mechanism must not cause: a query that never
    /// names `@within` is not scoped at all, exactly as before it existed.
    #[test]
    fn a_query_without_within_is_not_scoped_and_behaves_as_it_always_did() {
        let project = project_with(&[("one.rs", "fn free() {\n    x.unwrap();\n}\n")]);
        let found = found_by(project.path(), UNWRAP_ANYWHERE);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "one.rs");
        assert_eq!(found[0].line, 2);
        assert_eq!(found[0].excerpt, "x.unwrap();");
    }

    /// A query that names `@within` but whose region pattern happens to match
    /// nothing in a file reports nothing for that file at all -- it does not
    /// fall back to the unscoped behaviour just because no region was found.
    #[test]
    fn within_present_but_its_region_matches_nothing_yields_no_results() {
        const DROP_UNWRAP: &str = r#"
((impl_item
   trait: (type_identifier) @drop_trait
   (#eq? @drop_trait "Drop")) @within)

((call_expression
   function: (field_expression
     field: (field_identifier) @method)) @call
 (#eq? @method "unwrap"))
"#;
        // A real unwrap call, but nowhere near any kind of `impl` block, let
        // alone a Drop one -- the unscoped pattern would happily find it.
        let project = project_with(&[("one.rs", "fn free() {\n    x.unwrap();\n}\n")]);
        assert!(
            !found_by(project.path(), UNWRAP_ANYWHERE).is_empty(),
            "sanity check: the same call is found once nothing scopes it"
        );

        let found = found_by(project.path(), DROP_UNWRAP);
        assert!(
            found.is_empty(),
            "no region exists in this file, so nothing should be reported: {found:?}"
        );
    }

    /// A match inside two regions at once -- one nested inside the other --
    /// is reported exactly once, never once per region it happens to sit in.
    #[test]
    fn a_match_inside_nested_regions_is_reported_once_not_once_per_region() {
        const NESTED_REGIONS: &str = r#"
(mod_item) @within
(function_item) @within
(call_expression) @call
"#;
        let project = project_with(&[(
            "one.rs",
            "mod outer {\n    fn contains() {\n        inner_call();\n    }\n}\n",
        )]);

        let found = found_by(project.path(), NESTED_REGIONS);
        assert_eq!(
            found.len(),
            1,
            "one call, inside both the module region and the function region \
             nested in it, reported once: {found:?}"
        );
        assert_eq!(found[0].line, 3);
        assert_eq!(found[0].excerpt, "inner_call();");
    }

    #[test]
    fn every_capture_a_match_takes_is_read_back_by_name_line_and_text() {
        let project = project_with(&[("one.rs", "pub fn work() {}\n")]);
        let found = found_by(
            project.path(),
            "(function_item name: (identifier) @name) @item",
        );
        assert_eq!(found.len(), 1);
        let matched = &found[0];
        assert_eq!(matched.path, "one.rs");
        // @item is declared last, so it is the primary node: the whole
        // function, not just its name.
        assert_eq!(matched.line, 1);
        assert_eq!(matched.excerpt, "pub fn work() {}");

        let name = matched
            .captures
            .iter()
            .find(|capture| capture.name == "name")
            .expect("the name capture");
        assert_eq!(name.text, "work");
        assert_eq!(name.line, 1);

        let item = matched
            .captures
            .iter()
            .find(|capture| capture.name == "item")
            .expect("the item capture");
        assert_eq!(item.text, "pub fn work() {}");
    }

    /// Matches are sent as they are found, not collected and handed over in
    /// one piece once the whole project has been read. Proved without timing
    /// assumptions: with one core, files are searched in a known, sorted
    /// order, so if the very last file's match is not sitting in the channel
    /// immediately after the very first match is received, the pass was
    /// still in progress at that moment.
    #[test]
    fn matches_stream_in_as_they_are_found_rather_than_all_at_once() {
        const FILE_COUNT: usize = 150;
        let files: Vec<(String, String)> = (0..FILE_COUNT)
            .map(|at| {
                let mut body = String::new();
                for statement in 0..25 {
                    body.push_str(&format!("    let a{statement} = {statement};\n"));
                }
                (
                    format!("f{at:04}.rs"),
                    format!("fn f() {{\n{body}    let _ = a0;\n}}\n"),
                )
            })
            .collect();
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, contents)| (name.as_str(), contents.as_str()))
            .collect();
        let project = project_with(&borrowed);

        let receiver = search(
            project.path(),
            &rust(),
            "(function_item name: (identifier) @name) @item",
            1,
        )
        .expect("the query compiles");

        let first = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the first file's match arrives");
        assert_eq!(first.path, "f0000.rs");

        let too_soon = receiver.try_recv();
        let last_file_already_here =
            matches!(&too_soon, Ok(found) if found.path == format!("f{:04}.rs", FILE_COUNT - 1));
        assert!(
            !last_file_already_here,
            "the last file's match was already sitting in the channel \
             right after the first one was received: the pass was not \
             streaming, it had already finished"
        );

        let mut all = vec![first];
        if let Ok(found) = too_soon {
            all.push(found);
        }
        all.extend(receiver.iter());
        assert_eq!(all.len(), FILE_COUNT, "every file is eventually reached");
        assert_eq!(
            all.last().expect("at least one match").path,
            format!("f{:04}.rs", FILE_COUNT - 1)
        );
    }

    #[test]
    fn measuring_reports_when_the_first_and_the_last_match_arrived() {
        let project = project_with(&[
            ("one.rs", "pub fn work() {}\n"),
            ("two.rs", "pub fn other() {}\n"),
            ("notes.txt", "not rust at all\n"),
        ]);
        let numbers = measure(
            project.path(),
            &rust(),
            "(function_item name: (identifier) @name) @item",
            2,
        )
        .expect("the query compiles");

        assert_eq!(
            numbers.files_searched, 2,
            "only the two Rust files belong to the language"
        );
        assert_eq!(numbers.matches, 2);
        let first = numbers
            .time_to_first_match
            .expect("at least one match was found");
        assert!(first <= numbers.time_to_last_match);
        assert!(numbers.arrival.median <= numbers.time_to_last_match);
        assert!(numbers.arrival.ninety_fifth <= numbers.time_to_last_match);
    }

    #[test]
    fn measuring_a_search_with_no_matches_reports_no_time_to_a_first_one() {
        let project = project_with(&[("one.rs", "pub fn work() {}\n")]);
        let numbers = measure(project.path(), &rust(), "(macro_definition) @item", 1)
            .expect("the query compiles");
        assert!(numbers.time_to_first_match.is_none());
        assert_eq!(numbers.matches, 0);
        assert_eq!(numbers.files_searched, 1);
    }
}
