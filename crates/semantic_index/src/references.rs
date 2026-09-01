use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use streaming_iterator::StreamingIterator as _;

use crate::against_the_server::{
    Comparison, Identity, Priming, QueryAnswers, Server, ServerRefused, compare,
};
use crate::definitions::Definition;
use crate::languages::{self, Readable};
use crate::measure::{Spread, as_time, spread_of};
use crate::walk;

/// The references query this measurement runs, compiled once per call to
/// [`measure`] and reused for the whole project.
const QUERY_TEXT: &str = include_str!("rust_references.scm");

/// A name found at an exact position: row and column both zero-based, as
/// tree-sitter and the LSP protocol both count them.
///
/// `Definition` and [`Identity`](crate::against_the_server::Identity) keep
/// only a one-based line, which is enough to show a person where something
/// is but not enough to point a language server at one specific occurrence
/// among several on the same line -- which is what sampling a symbol to ask
/// `textDocument/references` about needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NamedAt {
    path: String,
    name: String,
    row: u32,
    column: u32,
}

/// Every reference the query finds under a project, grouped by the exact
/// name captured -- the only thing this index can group by, since it
/// resolves no scope at all. Two unrelated symbols that happen to share a
/// name are indistinguishable to it, which is the reason this measurement
/// and its error catalogue exist in the first place.
struct ReferenceIndex {
    by_name: HashMap<String, Vec<Definition>>,
}

impl ReferenceIndex {
    fn references_to(&self, name: &str) -> Vec<Definition> {
        self.by_name.get(name).cloned().unwrap_or_default()
    }
}

/// Everything a project-wide pass over the references query found: the
/// references themselves, and the extra context the error catalogue's
/// classification needs to explain a divergence rather than merely count it.
struct ProjectScan {
    /// Every symbol the outline query defines, kept for two reasons: sampling
    /// symbols to ask about, and telling whether a name is ambiguous (defined
    /// more than once in the project) when a divergence is classified.
    defined: Vec<NamedAt>,
    index: ReferenceIndex,
    /// Line ranges, one-based inclusive, of every `macro_invocation` per file
    /// -- a reference living only inside one is invisible to this grammar.
    macro_spans: HashMap<String, Vec<(u32, u32)>>,
    /// Line ranges of every `use_declaration` per file -- this query does not
    /// capture an imported name as a reference at all (see `rust_references.scm`).
    use_spans: HashMap<String, Vec<(u32, u32)>>,
    /// Line ranges of every item carrying a `#[cfg(...)]` attribute per file
    /// -- the grammar parses every branch unconditionally; rust-analyzer does
    /// not.
    cfg_gated_spans: HashMap<String, Vec<(u32, u32)>>,
    /// Every name bound as a `let`, a function parameter, or a closure
    /// parameter anywhere in the project.
    local_bindings: HashSet<String>,
}

/// The Rust language the rest of the crate already knows how to read, found
/// the same way [`crate::symbols::build`] and [`crate::structural::search`]
/// do.
fn rust_language() -> Result<Readable> {
    let (readable, _) = languages::readable();
    readable
        .into_iter()
        .find(|language| language.name == "rust")
        .context("Rust is not one of the languages the editor ships an outline query for")
}

/// The index of a capture by name, or `None` where the query has no such
/// capture. The same small, obviously correct lookup
/// [`crate::definitions::in_file`] uses; kept here rather than shared,
/// because that function's own copy is private to its module.
fn capture_index(query: &tree_sitter::Query, name: &str) -> Option<u32> {
    query
        .capture_names()
        .iter()
        .position(|capture| *capture == name)
        .map(|at| at as u32)
}

/// Every file under `root` that belongs to `rust`, the same longest-suffix
/// rule the rest of the crate uses to decide which language owns a file.
fn rust_files_under(root: &Path, rust: &Readable) -> Vec<PathBuf> {
    let claimed = languages::by_suffix();
    walk::files_under(root)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| languages::of_file(name, &claimed))
                .is_some_and(|owner| owner == rust.name)
        })
        .collect()
}

/// Every symbol the outline query defines in one already-parsed file, with
/// the exact position of its own name.
fn defined_symbols_in(
    path: &str,
    contents: &[u8],
    outline: &tree_sitter::Query,
    tree: &tree_sitter::Tree,
) -> Vec<NamedAt> {
    let Some(name_index) = capture_index(outline, "name") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(outline, tree.root_node(), contents);
    while let Some(matched) = matches.next() {
        for capture in matched.captures {
            if capture.index != name_index {
                continue;
            }
            let Ok(text) = capture.node.utf8_text(contents) else {
                continue;
            };
            let start = capture.node.start_position();
            found.push(NamedAt {
                path: path.to_string(),
                name: text.to_string(),
                row: start.row as u32,
                column: start.column as u32,
            });
        }
    }
    found
}

/// Every symbol the outline query defines under `root`. A pass distinct from
/// [`crate::symbols::build`]'s: that one exists to persist what it finds and
/// keeps only the line a person reads, not the column a language server
/// position needs. This one keeps nothing on disk and answers to nothing but
/// this measurement -- and its own timing is the plan's "a pass over
/// definitions" half of the cost ratio this step is gated on.
fn defined_symbols_pass(root: &Path, rust: &Readable) -> Vec<NamedAt> {
    let mut found = Vec::new();
    for path in rust_files_under(root, rust) {
        let Ok(contents) = std::fs::read(&path) else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative_path = relative.to_string_lossy().replace('\\', "/");
        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&rust.grammar).is_err() {
            continue;
        }
        let Some(tree) = parser.parse(&contents, None) else {
            continue;
        };
        found.extend(defined_symbols_in(
            &relative_path,
            &contents,
            &rust.outline,
            &tree,
        ));
    }
    found
}

/// Every occurrence `query` finds in one already-parsed file: any capture
/// whose name starts with `reference.`, whichever of the query's patterns it
/// came from.
fn occurrences_in(
    path: &str,
    contents: &[u8],
    query: &tree_sitter::Query,
    tree: &tree_sitter::Tree,
) -> Vec<NamedAt> {
    let capture_names = query.capture_names();
    let mut found = Vec::new();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), contents);
    while let Some(matched) = matches.next() {
        for capture in matched.captures {
            let Some(name) = capture_names.get(capture.index as usize) else {
                continue;
            };
            if !name.starts_with("reference.") {
                continue;
            }
            let Ok(text) = capture.node.utf8_text(contents) else {
                continue;
            };
            let start = capture.node.start_position();
            found.push(NamedAt {
                path: path.to_string(),
                name: text.to_string(),
                row: start.row as u32,
                column: start.column as u32,
            });
        }
    }
    found
}

/// The classification-support spans and names one file's own tree yields, on
/// the side, while it is parsed for the references pass anyway.
#[derive(Default)]
struct FileSpans {
    macro_invocations: Vec<(u32, u32)>,
    use_declarations: Vec<(u32, u32)>,
    cfg_gated: Vec<(u32, u32)>,
    local_bindings: HashSet<String>,
}

/// Whether `attribute_item`'s own attribute is a bare `#[cfg(...)]` --
/// `cfg_attr` is deliberately not included: unlike `cfg`, it does not remove
/// the item it decorates when its predicate is false, so it does not belong
/// in the same bucket as one that does.
fn is_cfg_attribute(attribute_item: tree_sitter::Node, contents: &[u8]) -> bool {
    let Some(attribute) = attribute_item.named_child(0) else {
        return false;
    };
    let Some(name) = attribute.named_child(0) else {
        return false;
    };
    name.kind() == "identifier" && name.utf8_text(contents) == Ok("cfg")
}

/// The item an attribute applies to: its next named sibling, skipping over
/// any other attributes or comments stacked between the two.
fn item_gated_by(attribute_item: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut sibling = attribute_item.next_named_sibling();
    while let Some(node) = sibling {
        let skip_over = matches!(
            node.kind(),
            "attribute_item" | "line_comment" | "block_comment"
        );
        if !skip_over {
            return Some(node);
        }
        sibling = node.next_named_sibling();
    }
    None
}

/// Records `pattern`'s own name as a local binding, where the pattern is
/// simple enough to have one: a bare identifier, or one wrapped in `mut`.
/// Destructuring patterns (tuples, structs, references) are deliberately not
/// unpacked -- a best-effort heuristic does not need to be an exhaustive
/// pattern matcher to be honest about what it does cover.
fn record_binding_name(pattern: tree_sitter::Node, contents: &[u8], into: &mut FileSpans) {
    let identifier = match pattern.kind() {
        "identifier" => Some(pattern),
        "mut_pattern" => pattern
            .named_child(0)
            .filter(|child| child.kind() == "identifier"),
        _ => None,
    };
    if let Some(identifier) = identifier
        && let Ok(text) = identifier.utf8_text(contents)
    {
        into.local_bindings.insert(text.to_string());
    }
}

/// Walks the whole tree once, collecting everything [`FileSpans`] needs.
fn walk_for_classification(node: tree_sitter::Node, contents: &[u8], into: &mut FileSpans) {
    match node.kind() {
        "macro_invocation" => {
            into.macro_invocations.push((
                node.start_position().row as u32,
                node.end_position().row as u32,
            ));
        }
        "use_declaration" => {
            into.use_declarations.push((
                node.start_position().row as u32,
                node.end_position().row as u32,
            ));
        }
        "attribute_item" => {
            if is_cfg_attribute(node, contents)
                && let Some(gated) = item_gated_by(node)
            {
                into.cfg_gated.push((
                    gated.start_position().row as u32,
                    gated.end_position().row as u32,
                ));
            }
        }
        "let_declaration" | "parameter" => {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                record_binding_name(pattern, contents, into);
            }
        }
        "closure_parameters" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                record_binding_name(child, contents, into);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_classification(child, contents, into);
    }
}

/// A reference position turned into the index's own `Definition` shape --
/// `kind` is left empty: a reference's grammar node kind (`identifier`,
/// `field_identifier`, and so on) is not the vocabulary `Definition::kind`
/// otherwise carries, and is not something the comparison needs.
fn definition_of(named: &NamedAt) -> Definition {
    Definition {
        path: named.path.clone(),
        name: named.name.clone(),
        kind: String::new(),
        line: named.row + 1,
        language: "rust".to_string(),
    }
}

/// Runs the references query and the classification walk over every Rust
/// file under `root`, and folds the result together with `defined` -- already
/// collected and timed on its own by [`defined_symbols_pass`] -- into one
/// [`ProjectScan`].
///
/// A match at a position `defined` already names as a declaration is
/// dropped, to match `textDocument/references`'s own `includeDeclaration:
/// false`. A match found by more than one of the query's own patterns at the
/// exact same position -- a method call's field expression is, structurally,
/// also a field access -- is kept once, not once per pattern that described
/// it.
fn scan_references(
    root: &Path,
    rust: &Readable,
    references_query: &tree_sitter::Query,
    defined: Vec<NamedAt>,
) -> ProjectScan {
    let mut raw_occurrences = Vec::new();
    let mut macro_spans: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    let mut use_spans: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    let mut cfg_gated_spans: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    let mut local_bindings: HashSet<String> = HashSet::new();

    for path in rust_files_under(root, rust) {
        let Ok(contents) = std::fs::read(&path) else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative_path = relative.to_string_lossy().replace('\\', "/");

        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&rust.grammar).is_err() {
            continue;
        }
        let Some(tree) = parser.parse(&contents, None) else {
            continue;
        };

        raw_occurrences.extend(occurrences_in(
            &relative_path,
            &contents,
            references_query,
            &tree,
        ));

        let mut spans = FileSpans::default();
        walk_for_classification(tree.root_node(), &contents, &mut spans);
        if !spans.macro_invocations.is_empty() {
            macro_spans.insert(relative_path.clone(), spans.macro_invocations);
        }
        if !spans.use_declarations.is_empty() {
            use_spans.insert(relative_path.clone(), spans.use_declarations);
        }
        if !spans.cfg_gated.is_empty() {
            cfg_gated_spans.insert(relative_path.clone(), spans.cfg_gated);
        }
        local_bindings.extend(spans.local_bindings);
    }

    let declared: HashSet<(String, u32, u32)> = defined
        .iter()
        .map(|symbol| (symbol.path.clone(), symbol.row, symbol.column))
        .collect();
    let mut by_name: HashMap<String, Vec<Definition>> = HashMap::new();
    let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
    for occurrence in raw_occurrences {
        let key = (occurrence.path.clone(), occurrence.row, occurrence.column);
        if declared.contains(&key) || !seen.insert(key) {
            continue;
        }
        by_name
            .entry(occurrence.name.clone())
            .or_default()
            .push(definition_of(&occurrence));
    }

    ProjectScan {
        defined,
        index: ReferenceIndex { by_name },
        macro_spans,
        use_spans,
        cfg_gated_spans,
        local_bindings,
    }
}

/// Picks up to `count` of `symbols` at an even stride, so the sample is the
/// same on every run over the same project -- the same idea
/// [`crate::against_the_server::sample_queries`] uses, adapted to carry a
/// symbol's position along with its name, which asking a language server
/// about one specific symbol needs and a bare name does not give. Unlike
/// that function, nothing here is deduplicated by name: two definitions that
/// happen to share a name are exactly one of the cases this measurement
/// exists to see, not noise to collapse away before sampling.
fn sample_symbols(symbols: &[NamedAt], count: usize) -> Vec<NamedAt> {
    if symbols.is_empty() || count == 0 {
        return Vec::new();
    }
    let stride = (symbols.len() / count).max(1);
    let mut picked = Vec::with_capacity(count.min(symbols.len()));
    let mut at = 0;
    while picked.len() < count && at < symbols.len() {
        picked.push(symbols[at].clone());
        at += stride;
    }
    picked
}

/// `matched` over everything the index reported, `matched` plus
/// [`Comparison::the_indexs_extra_findings`]. `1.0` where the index reported
/// nothing at all: there is then no wrong answer among its output, which is
/// the standard convention for precision over an empty result set, the same
/// vacuous-truth shape [`compare`]'s own `recall` already uses for the
/// opposite case.
fn precision_of(comparison: &Comparison) -> f64 {
    let index_findings = comparison.matched + comparison.the_indexs_extra_findings;
    if index_findings == 0 {
        1.0
    } else {
        comparison.matched as f64 / index_findings as f64
    }
}

/// Why a divergence happened, named honestly rather than forced into a
/// bucket that does not fit. Order is the order buckets print in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DivergenceReason {
    /// The name also names another definition somewhere in the project: a
    /// call, a type use, a field access or a macro invocation naming it
    /// cannot be told apart, by text alone, from one naming the other.
    NameSharedByAnotherDefinition,
    /// The name is also bound as a local variable or a parameter somewhere in
    /// the project.
    NameSharedByALocalBinding,
    /// The reference sits inside an item carrying a `#[cfg(...)]` attribute:
    /// the grammar parses every branch of a conditional compilation
    /// unconditionally, while rust-analyzer resolves only the one that is
    /// actually active.
    BehindACfgAttribute,
    /// The reference lives only inside a macro invocation's own argument
    /// tokens, which this grammar does not parse as expressions at all -- an
    /// interpolated `format!` argument is the common real example.
    OnlyInsideAMacroInvocation,
    /// The reference is a `use` import of the name: `rust_references.scm`
    /// deliberately does not capture one, since an imported name is a plain
    /// `identifier` in this grammar, not the `type_identifier` the rest of
    /// the type-use pattern relies on.
    ReferencedOnlyViaAnImport,
    /// The match fell inside a comment or a string literal -- a bucket this
    /// measurement expects to stay empty, and proves so with a test: the
    /// grammar never decomposes either into identifier nodes, so no pattern
    /// here can structurally match inside one.
    InsideACommentOrAString,
    /// None of the above.
    Unclassified,
}

impl fmt::Display for DivergenceReason {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            DivergenceReason::NameSharedByAnotherDefinition => {
                "the name also names another definition in the project"
            }
            DivergenceReason::NameSharedByALocalBinding => {
                "the name is also bound as a local variable or a parameter somewhere in the project"
            }
            DivergenceReason::BehindACfgAttribute => {
                "the reference sits behind a #[cfg(...)] the grammar parses but \
                 rust-analyzer does not activate"
            }
            DivergenceReason::OnlyInsideAMacroInvocation => {
                "the reference lives only inside a macro invocation's own argument tokens"
            }
            DivergenceReason::ReferencedOnlyViaAnImport => {
                "the reference is a `use` import, which this query does not capture"
            }
            DivergenceReason::InsideACommentOrAString => {
                "the match fell inside a comment or a string literal"
            }
            DivergenceReason::Unclassified => {
                "none of the above -- not forced into a bucket that does not fit"
            }
        };
        write!(out, "{text}")
    }
}

/// One divergence, classified.
#[derive(Debug, Clone)]
pub struct ClassifiedDivergence {
    pub query: String,
    pub identity: Identity,
    pub reason: DivergenceReason,
}

/// Every divergence [`compare`] reported, classified into named buckets --
/// the plan's own words: a list of what the index gets wrong, not a general
/// "there is some error".
pub struct ErrorCatalogue {
    pub missed: Vec<ClassifiedDivergence>,
    pub extra: Vec<ClassifiedDivergence>,
}

fn line_falls_in(
    spans: &HashMap<String, Vec<(u32, u32)>>,
    path: &str,
    one_based_line: u32,
) -> bool {
    let Some(list) = spans.get(path) else {
        return false;
    };
    let row = one_based_line.saturating_sub(1);
    list.iter().any(|(start, end)| *start <= row && row <= *end)
}

/// Classifies a symbol the index found but the server did not.
fn classify_extra(scan: &ProjectScan, name: &str, identity: &Identity) -> DivergenceReason {
    let definitions_sharing_the_name = scan
        .defined
        .iter()
        .filter(|symbol| symbol.name == name)
        .count();
    if definitions_sharing_the_name > 1 {
        return DivergenceReason::NameSharedByAnotherDefinition;
    }
    if scan.local_bindings.contains(name) {
        return DivergenceReason::NameSharedByALocalBinding;
    }
    if line_falls_in(&scan.cfg_gated_spans, &identity.path, identity.line) {
        return DivergenceReason::BehindACfgAttribute;
    }
    DivergenceReason::Unclassified
}

/// Classifies a symbol the server found but the index did not.
fn classify_missed(scan: &ProjectScan, identity: &Identity) -> DivergenceReason {
    if line_falls_in(&scan.macro_spans, &identity.path, identity.line) {
        return DivergenceReason::OnlyInsideAMacroInvocation;
    }
    if line_falls_in(&scan.use_spans, &identity.path, identity.line) {
        return DivergenceReason::ReferencedOnlyViaAnImport;
    }
    DivergenceReason::Unclassified
}

impl ErrorCatalogue {
    fn build(scan: &ProjectScan, comparison: &Comparison) -> Self {
        let mut missed = Vec::new();
        let mut extra = Vec::new();
        for divergence in &comparison.divergent_queries {
            for identity in &divergence.the_server_found_and_the_index_missed {
                missed.push(ClassifiedDivergence {
                    query: divergence.query.clone(),
                    identity: identity.clone(),
                    reason: classify_missed(scan, identity),
                });
            }
            for identity in &divergence.the_index_found_and_the_server_did_not {
                extra.push(ClassifiedDivergence {
                    query: divergence.query.clone(),
                    identity: identity.clone(),
                    reason: classify_extra(scan, &divergence.query, identity),
                });
            }
        }
        Self { missed, extra }
    }

    fn counts(list: &[ClassifiedDivergence]) -> BTreeMap<DivergenceReason, usize> {
        let mut counts = BTreeMap::new();
        for divergence in list {
            *counts.entry(divergence.reason).or_insert(0) += 1;
        }
        counts
    }
}

impl fmt::Display for ErrorCatalogue {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        const EXAMPLES_SHOWN: usize = 3;
        writeln!(out, "missed ({} total):", self.missed.len())?;
        print_bucket(out, &self.missed, EXAMPLES_SHOWN)?;
        writeln!(out, "extra ({} total):", self.extra.len())?;
        print_bucket(out, &self.extra, EXAMPLES_SHOWN)
    }
}

fn print_bucket(
    out: &mut fmt::Formatter<'_>,
    list: &[ClassifiedDivergence],
    examples_shown: usize,
) -> fmt::Result {
    for (reason, count) in ErrorCatalogue::counts(list) {
        writeln!(out, "  {reason}: {count}")?;
        for divergence in list
            .iter()
            .filter(|one| one.reason == reason)
            .take(examples_shown)
        {
            writeln!(out, "    {} -> {}", divergence.query, divergence.identity)?;
        }
    }
    Ok(())
}

fn duration_ratio(numerator: Duration, denominator: Duration) -> f64 {
    if denominator.is_zero() {
        0.0
    } else {
        numerator.as_secs_f64() / denominator.as_secs_f64()
    }
}

/// Everything this measurement produces.
pub struct Report {
    pub symbols_sampled: usize,
    /// Sampled symbols the server refused because it has no such file. Counted
    /// on neither side, and printed, so a shrinking sample is visible rather
    /// than silent.
    pub outside_the_servers_project: usize,
    pub comparison: Comparison,
    pub precision: f64,
    pub catalogue: ErrorCatalogue,
    /// How long a project-wide pass over the outline query took.
    pub definitions_pass: Duration,
    /// How long a project-wide pass over the references query, plus the
    /// error catalogue's own classification walk, took.
    pub references_pass: Duration,
    pub answering_the_index: Spread,
    pub answering_the_server: Spread,
}

impl fmt::Display for Report {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            out,
            "sampled {} symbols -- precision {:.1}%, recall {:.1}%",
            self.symbols_sampled,
            self.precision * 100.0,
            self.comparison.recall * 100.0
        )?;
        if self.outside_the_servers_project > 0 {
            writeln!(
                out,
                "{} sampled symbols were in files rust-analyzer does not have -- outside the \
                 cargo graph -- and are counted on neither side",
                self.outside_the_servers_project
            )?;
        }
        writeln!(out, "{}", self.comparison)?;
        writeln!(
            out,
            "a pass over definitions took {}; a pass over references took {} ({:.2}x)",
            as_time(self.definitions_pass),
            as_time(self.references_pass),
            duration_ratio(self.references_pass, self.definitions_pass)
        )?;
        writeln!(
            out,
            "answering a query   index: median {:>10} 95th {:>10} slowest {:>10}\n\
             {:20}rust-analyzer: median {:>10} 95th {:>10} slowest {:>10}",
            as_time(self.answering_the_index.median),
            as_time(self.answering_the_index.ninety_fifth),
            as_time(self.answering_the_index.slowest),
            "",
            as_time(self.answering_the_server.median),
            as_time(self.answering_the_server.ninety_fifth),
            as_time(self.answering_the_server.slowest),
        )?;
        write!(out, "{}", self.catalogue)
    }
}

/// Measures the index's references against rust-analyzer's, over `symbol_count`
/// of the project's own defined symbols.
///
/// Builds the local reference index, starts rust-analyzer and waits for it to
/// finish indexing (up to `indexing_timeout`), then asks both sides about
/// each sampled symbol (each `textDocument/references` call bounded by
/// `query_timeout`) and compares the answers.
///
/// The report is returned whether or not it clears the plan's own bars for
/// this step; whether it does is for the caller to decide and to say, after
/// printing it -- a run that could not measure at all is still an `Err`,
/// never a report that quietly reads as a pass.
pub async fn measure(
    root: &Path,
    symbol_count: usize,
    indexing_timeout: Duration,
    query_timeout: Duration,
) -> Result<Report> {
    let rust = rust_language()?;
    let references_query = tree_sitter::Query::new(&rust.grammar, QUERY_TEXT)
        .context("the references query does not compile against the Rust grammar")?;

    let definitions_started = Instant::now();
    let defined = defined_symbols_pass(root, &rust);
    let definitions_pass = definitions_started.elapsed();
    anyhow::ensure!(
        !defined.is_empty(),
        "no definitions were found under {} to sample symbols from",
        root.display()
    );

    let references_started = Instant::now();
    let scan = scan_references(root, &rust, &references_query, defined);
    let references_pass = references_started.elapsed();

    let sampled = sample_symbols(&scan.defined, symbol_count);
    anyhow::ensure!(
        !sampled.is_empty(),
        "there is nothing to compare over zero symbols"
    );

    // Lazily: finding every reference needs whole-graph inference, and
    // priming that ahead does not fit in the machine this runs on -- the
    // server is killed before it answers anything at all.
    let mut server = Server::start(root, Priming::Lazily)
        .await
        .context("starting rust-analyzer")?;
    server
        .wait_until_indexed(indexing_timeout)
        .await
        .context("waiting for rust-analyzer to finish indexing")?;

    let mut answers = Vec::with_capacity(sampled.len());
    let mut index_timings = Vec::with_capacity(sampled.len());
    let mut server_timings = Vec::with_capacity(sampled.len());
    let mut outside_the_servers_project = 0usize;
    for symbol in &sampled {
        let index_started = Instant::now();
        let the_index_found = scan.index.references_to(&symbol.name);
        index_timings.push(index_started.elapsed());

        let server_started = Instant::now();
        let answered = server
            .references(
                &symbol.path,
                symbol.row,
                symbol.column,
                &symbol.name,
                query_timeout,
            )
            .await;
        let the_server_found = match answered {
            Ok(found) => found,
            Err(error) => {
                // A file the server has never heard of is outside the cargo
                // graph, so neither side can be held to it. That is a smaller
                // sample, counted and printed -- not a failed run, and not a
                // reason to keep going after a request that failed for any
                // other reason.
                let refused = error
                    .downcast_ref::<ServerRefused>()
                    .is_some_and(ServerRefused::is_a_file_the_server_does_not_have);
                if refused {
                    outside_the_servers_project += 1;
                    index_timings.pop();
                    continue;
                }
                return Err(error).with_context(|| {
                    format!(
                        "asking rust-analyzer for references to {} at {}:{}:{}",
                        symbol.name, symbol.path, symbol.row, symbol.column
                    )
                });
            }
        };
        server_timings.push(server_started.elapsed());

        answers.push(QueryAnswers {
            query: symbol.name.clone(),
            the_server_found,
            the_index_found,
        });
    }
    anyhow::ensure!(
        !answers.is_empty(),
        "every one of the {} sampled symbols was in a file rust-analyzer does not have; \
         there is nothing to compare",
        sampled.len()
    );

    if let Err(error) = server.shut_down().await {
        log::warn!("rust-analyzer did not shut down cleanly: {error:#}");
    }

    let comparison = compare(&answers);
    let precision = precision_of(&comparison);
    let catalogue = ErrorCatalogue::build(&scan, &comparison);

    Ok(Report {
        symbols_sampled: answers.len(),
        outside_the_servers_project,
        comparison,
        precision,
        catalogue,
        definitions_pass,
        references_pass,
        answering_the_index: spread_of(index_timings),
        answering_the_server: spread_of(server_timings),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(path: &str, name: &str, line: u32) -> Definition {
        Definition {
            path: path.to_string(),
            name: name.to_string(),
            kind: "function_item".to_string(),
            line,
            language: "rust".to_string(),
        }
    }

    fn query() -> tree_sitter::Query {
        let rust = rust_language().expect("Rust is one of the languages the editor ships");
        tree_sitter::Query::new(&rust.grammar, QUERY_TEXT).expect("the references query compiles")
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

    fn scan_of(project: &std::path::Path) -> ProjectScan {
        let rust = rust_language().expect("Rust is one of the languages the editor ships");
        let references_query = query();
        let defined = defined_symbols_pass(project, &rust);
        scan_references(project, &rust, &references_query, defined)
    }

    /// A call, a type use, a field access and a macro invocation are each
    /// found, with the right line.
    #[test]
    fn each_of_the_four_reference_kinds_is_found_with_the_right_line() {
        let project = project_with(&[(
            "one.rs",
            "struct Holder {\n    value: u32,\n}\n\nfn make_holder() -> Holder {\n    Holder { value: 42 }\n}\n\nfn use_it() {\n    let holder: Holder = make_holder();\n    let seen = holder.value;\n    println!(\"{}\", seen);\n}\n",
        )]);
        let scan = scan_of(project.path());

        let calls = scan.index.references_to("make_holder");
        assert_eq!(
            calls.len(),
            1,
            "found {calls:?}; the whole index holds {:?}",
            scan.index
                .by_name
                .iter()
                .map(|(name, found)| (name.as_str(), found.len()))
                .collect::<std::collections::BTreeMap<_, _>>()
        );
        assert_eq!(calls[0].path, "one.rs");
        assert_eq!(calls[0].line, 10, "the call site, not the declaration");

        let mut type_lines: Vec<u32> = scan
            .index
            .references_to("Holder")
            .iter()
            .map(|found| found.line)
            .collect();
        type_lines.sort_unstable();
        assert_eq!(
            type_lines,
            vec![5, 6, 10],
            "the return type, the struct literal's own type, and the let binding's \
             annotation -- never the struct's own declaration on line 1"
        );

        let fields = scan.index.references_to("value");
        assert_eq!(fields.len(), 1, "{fields:?}");
        assert_eq!(
            fields[0].line, 11,
            "the field read, not the field's declaration or its use as a struct \
             literal's own key"
        );

        let macros = scan.index.references_to("println");
        assert_eq!(macros.len(), 1, "{macros:?}");
        assert_eq!(macros[0].line, 12);
    }

    /// The field read above sits on a line of its own on purpose. A macro's
    /// arguments are one opaque token tree to this grammar, so a reference
    /// written only inside a macro call is invisible to every pattern in the
    /// query -- which is why `OnlyInsideAMacroInvocation` is a bucket of the
    /// error catalogue rather than a bug to fix in the query. Asserted here so
    /// the limitation is a measured fact and not a remark in a comment.
    #[test]
    fn a_reference_written_only_inside_a_macro_call_is_invisible_to_the_grammar() {
        let project = project_with(&[(
            "one.rs",
            "struct Holder {\n    value: u32,\n}\n\nfn use_it(holder: Holder) {\n    println!(\"{}\", holder.value);\n}\n",
        )]);
        let scan = scan_of(project.path());

        assert!(
            scan.index.references_to("value").is_empty(),
            "the only read of the field is inside a macro call: {:?}",
            scan.index.references_to("value")
        );
        // The macro's own name is not inside the token tree, so it is found.
        assert_eq!(scan.index.references_to("println").len(), 1);
        // And the type in the parameter, which is outside the macro, is found.
        assert_eq!(scan.index.references_to("Holder").len(), 1);
    }

    /// The near miss that separates this from a regular expression: the
    /// grammar never decomposes a comment's or a string's own text into
    /// identifier nodes at all, so a name written only inside either is
    /// structurally invisible to every pattern in the query.
    #[test]
    fn a_name_inside_a_comment_and_inside_a_string_literal_is_not_found() {
        let project = project_with(&[(
            "one.rs",
            "// make_holder is mentioned here as plain text, not as code\n\
             fn irrelevant() {\n    let text = \"make_holder\";\n}\n",
        )]);
        let scan = scan_of(project.path());
        assert!(
            scan.index.references_to("make_holder").is_empty(),
            "{:?}",
            scan.index.references_to("make_holder")
        );
    }

    /// Two unrelated local variables sharing a name: with no scope
    /// resolution, both calls come back under the one name, indistinguishable
    /// from each other. In the real error catalogue, sampling either as a
    /// symbol (were either ever sampled -- local bindings never are, since
    /// the outline query does not define them) would classify the other
    /// function's call as `NameSharedByALocalBinding`, because `helper` is
    /// recorded as a local binding in both `first` and `second`.
    #[test]
    fn two_local_variables_of_the_same_name_in_different_functions_are_not_told_apart() {
        let project = project_with(&[(
            "one.rs",
            "fn first() {\n    let helper = || 1;\n    helper();\n}\n\n\
             fn second() {\n    let helper = || 2;\n    helper();\n}\n",
        )]);
        let scan = scan_of(project.path());

        let mut lines: Vec<u32> = scan
            .index
            .references_to("helper")
            .iter()
            .map(|found| found.line)
            .collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            vec![3, 8],
            "both calls come back under the one name, from two unrelated locals"
        );
        assert!(scan.local_bindings.contains("helper"));
    }

    /// A method name shared by two types: with no type resolution, a call to
    /// either type's method comes back under the one name. In the real error
    /// catalogue this classifies as `NameSharedByAnotherDefinition`, since
    /// the outline query itself records two `work` definitions.
    #[test]
    fn a_method_name_shared_by_two_types_is_not_told_apart() {
        let project = project_with(&[(
            "one.rs",
            "struct Left;\n\nimpl Left {\n    fn work(&self) -> u32 {\n        1\n    }\n}\n\n\
             struct Right;\n\nimpl Right {\n    fn work(&self) -> u32 {\n        2\n    }\n}\n\n\
             fn use_both(left: &Left, right: &Right) {\n    left.work();\n    right.work();\n}\n",
        )]);
        let scan = scan_of(project.path());

        assert_eq!(
            scan.defined
                .iter()
                .filter(|symbol| symbol.name == "work")
                .count(),
            2,
            "two methods named work, one on each type"
        );
        let mut lines: Vec<u32> = scan
            .index
            .references_to("work")
            .iter()
            .map(|found| found.line)
            .collect();
        lines.sort_unstable();
        assert_eq!(lines, vec![18, 19]);
    }

    #[test]
    fn precision_is_matched_over_everything_the_index_reported() {
        let comparison = compare(&[QueryAnswers {
            query: "work".to_string(),
            the_server_found: vec![definition("a.rs", "work", 1)],
            the_index_found: vec![definition("a.rs", "work", 1), definition("b.rs", "work", 9)],
        }]);
        assert_eq!(precision_of(&comparison), 0.5);
    }

    #[test]
    fn precision_when_everything_the_index_found_was_wrong_is_zero() {
        let comparison = compare(&[QueryAnswers {
            query: "work".to_string(),
            the_server_found: Vec::new(),
            the_index_found: vec![definition("a.rs", "work", 1)],
        }]);
        assert_eq!(precision_of(&comparison), 0.0);
    }

    #[test]
    fn precision_when_the_index_found_nothing_at_all_is_a_vacuous_one() {
        let comparison = compare(&[QueryAnswers {
            query: "work".to_string(),
            the_server_found: vec![definition("a.rs", "work", 1)],
            the_index_found: Vec::new(),
        }]);
        assert_eq!(
            precision_of(&comparison),
            1.0,
            "finding nothing is not the same as finding something wrongly"
        );
    }

    #[test]
    fn precision_of_the_empty_case_is_also_a_vacuous_one() {
        assert_eq!(precision_of(&compare(&[])), 1.0);
    }

    #[test]
    fn a_divergence_whose_name_has_two_definitions_is_classified_as_shared() {
        let scan = ProjectScan {
            defined: vec![
                NamedAt {
                    path: "a.rs".to_string(),
                    name: "work".to_string(),
                    row: 3,
                    column: 7,
                },
                NamedAt {
                    path: "b.rs".to_string(),
                    name: "work".to_string(),
                    row: 11,
                    column: 7,
                },
            ],
            index: ReferenceIndex {
                by_name: HashMap::new(),
            },
            macro_spans: HashMap::new(),
            use_spans: HashMap::new(),
            cfg_gated_spans: HashMap::new(),
            local_bindings: HashSet::new(),
        };
        let comparison = compare(&[QueryAnswers {
            query: "work".to_string(),
            the_server_found: vec![definition("a.rs", "work", 4)],
            the_index_found: vec![
                definition("a.rs", "work", 4),
                definition("b.rs", "work", 12),
            ],
        }]);
        let catalogue = ErrorCatalogue::build(&scan, &comparison);
        assert_eq!(catalogue.extra.len(), 1, "{:?}", catalogue.extra);
        assert_eq!(
            catalogue.extra[0].reason,
            DivergenceReason::NameSharedByAnotherDefinition
        );
    }

    #[test]
    fn a_miss_whose_line_sits_inside_a_macro_invocation_is_classified_as_such() {
        let mut macro_spans = HashMap::new();
        macro_spans.insert("a.rs".to_string(), vec![(4, 4)]);
        let scan = ProjectScan {
            defined: Vec::new(),
            index: ReferenceIndex {
                by_name: HashMap::new(),
            },
            macro_spans,
            use_spans: HashMap::new(),
            cfg_gated_spans: HashMap::new(),
            local_bindings: HashSet::new(),
        };
        let comparison = compare(&[QueryAnswers {
            query: "x".to_string(),
            the_server_found: vec![definition("a.rs", "x", 5)],
            the_index_found: Vec::new(),
        }]);
        let catalogue = ErrorCatalogue::build(&scan, &comparison);
        assert_eq!(catalogue.missed.len(), 1, "{:?}", catalogue.missed);
        assert_eq!(
            catalogue.missed[0].reason,
            DivergenceReason::OnlyInsideAMacroInvocation
        );
    }

    /// The same claim [`crate::definitions`] already establishes for the
    /// outline query: a file caught mid-edit contributes nothing at all,
    /// never a partial answer that would read as though it parsed.
    #[test]
    fn a_file_that_does_not_parse_contributes_nothing_rather_than_partial_results() {
        let project = project_with(&[("broken.rs", "pub struct Half {\npub fn work(\n")]);
        let rust = rust_language().expect("Rust is one of the languages the editor ships");
        let defined = defined_symbols_pass(project.path(), &rust);
        assert!(
            defined.is_empty(),
            "a file that does not parse defines nothing: {defined:?}"
        );

        let scan = scan_references(project.path(), &rust, &query(), defined);
        assert!(scan.index.references_to("Half").is_empty());
        assert!(scan.index.references_to("work").is_empty());
    }
}
