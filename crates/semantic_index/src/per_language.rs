use std::collections::HashSet;
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
/// The names a macro invocation declares that the outline query cannot see.
///
/// A macro body is an opaque token tree to the grammar, so a query over the
/// parse tree finds nothing inside it -- and a project's own macros are where
/// a great many of its findable names live. Measured on this repository:
/// ninety-five per cent of the definitions the index missed against
/// rust-analyzer were inside a macro, and nearly half of those were one macro.
///
/// This is deliberately not a `macro_rules!` interpreter. It knows the shape of
/// the handful of macros that actually account for the gap, and knows nothing
/// else: a macro it does not recognise contributes nothing, exactly as before.
/// The alternative -- expanding macros in general -- needs the compiler for
/// procedural ones, which is the dependency this index exists to do without.
pub fn names_a_macro_declares(
    language: &str,
    node: tree_sitter::Node,
    contents: &[u8],
) -> Vec<(String, u32)> {
    if language != "rust" || node.kind() != "macro_invocation" {
        return Vec::new();
    }
    let Some(called) = node
        .child_by_field_name("macro")
        .and_then(|name| name.utf8_text(contents).ok())
    else {
        return Vec::new();
    };
    // Only the macro's own name is a named field; its token tree is just the
    // last child, so it is found by kind rather than by asking for a field
    // that does not exist.
    let mut walking = node.walk();
    let Some(tokens) = node
        .children(&mut walking)
        .find(|child| child.kind() == "token_tree")
    else {
        return Vec::new();
    };
    match called {
        // `actions!(namespace, [Name, Name])` and `actions!([Name, Name])`
        // both declare one unit struct per name in the bracketed list.
        // Attributes and doc comments sit between them and are nested a level
        // deeper in the token tree, so the names are exactly the identifiers
        // directly inside it.
        "actions" => {
            let Some(listed) = bracketed_list(tokens) else {
                return Vec::new();
            };
            identifiers_directly_inside(listed, contents)
        }
        // `request!("method", Name, Params, Response)` and
        // `notification!("method", Name, Params)` each declare one unit struct,
        // named by their second argument. The first is a string, so the name is
        // simply the first identifier the invocation holds.
        "request" | "notification" => identifiers_directly_inside(tokens, contents)
            .into_iter()
            .take(1)
            .collect(),
        _ => Vec::new(),
    }
}

/// Every identifier that is a direct child of `tokens`, with its one-based
/// line. Anything nested deeper -- an attribute's own contents, a type's
/// parameters -- is not one of them.
fn identifiers_directly_inside(tokens: tree_sitter::Node, contents: &[u8]) -> Vec<(String, u32)> {
    let mut named = Vec::new();
    let mut walking = tokens.walk();
    for child in tokens.children(&mut walking) {
        if child.kind() != "identifier" {
            continue;
        }
        if let Ok(name) = child.utf8_text(contents) {
            named.push((name.to_string(), child.start_position().row as u32 + 1));
        }
    }
    named
}

/// The `[ ... ]`-delimited token tree inside `tokens`, which is where the list
/// of names lives in both shapes of the invocation.
fn bracketed_list(tokens: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let opens_with_a_bracket = |node: tree_sitter::Node| {
        node.child(0)
            .map(|first| first.kind() == "[")
            .unwrap_or(false)
    };
    // `actions![...]` delimits the invocation itself with brackets, so the list
    // is the token tree already in hand rather than one nested inside it.
    if opens_with_a_bracket(tokens) {
        return Some(tokens);
    }
    let mut walking = tokens.walk();
    tokens
        .children(&mut walking)
        .find(|child| child.kind() == "token_tree" && opens_with_a_bracket(*child))
}

/// The references query for a language, or `None` where this fork has not
/// written one. A language with an `outline.scm` and no references query is
/// covered for definitions and not for references, which is the honest state
/// of most of them.
pub fn references_query(language: &str) -> Option<&'static str> {
    match language {
        "rust" => Some(include_str!("rust_references.scm")),
        "go" => Some(include_str!("go_references.scm")),
        _ => None,
    }
}

/// Every language a references query exists for, in the order they were
/// written, so a caller can say what is covered rather than guess.
pub const LANGUAGES_WITH_A_REFERENCES_QUERY: &[&str] = &["rust", "go"];

/// The language server to measure a language against, and the environment it
/// needs. `None` where no server is wired up.
pub struct Spoken {
    /// The executable to look for on `PATH`.
    pub binary: &'static str,
    /// What to tell a person who does not have it.
    pub how_to_install: &'static str,
    /// Whether the server takes rust-analyzer's `cachePriming` and `lru`
    /// options. Nothing else understands them, and sending them to a server
    /// that does not is asking it to ignore part of the request silently.
    pub takes_rust_analyzer_options: bool,
}

pub fn language_server(language: &str) -> Option<Spoken> {
    match language {
        "rust" => Some(Spoken {
            binary: "rust-analyzer",
            how_to_install: "rustup component add rust-analyzer",
            takes_rust_analyzer_options: true,
        }),
        "go" => Some(Spoken {
            binary: "gopls",
            how_to_install: "go install golang.org/x/tools/gopls@latest",
            takes_rust_analyzer_options: false,
        }),
        _ => None,
    }
}

/// The names a package in the project answers to, which a symbol of the same
/// name cannot be told apart from by text. For Rust that is the workspace's
/// crates and their dependencies; for Go it is the last element of every
/// import path the project writes, plus its own module's.
///
/// The disease this closes was measured on Rust: `crates/util` declares
/// `pub mod serde`, and asking about `serde` by text answered with every
/// `#[serde(...)]` attribute in the project -- 3552 findings for one name.
/// Go has it worse, because the qualifier of every call into the standard
/// library is a plain identifier: `fmt`, `log`, `time`, `errors`, `context`.
pub fn names_of_packages(language: &str, root: &Path, readable: &Readable) -> HashSet<String> {
    match language {
        "go" => go_package_names(root, readable),
        _ => HashSet::new(),
    }
}

/// Every import path under `root`, reduced to the identifier Go code writes
/// for it, plus the module's own last element from `go.mod`.
///
/// Read from the parse tree rather than from the text: a quoted string in a
/// line before the first declaration is not always an import.
/// `imports/forward.go` opens with `// Package imports implements a Go
/// pretty-printer (like package "go/format")`, and reading that as an import
/// silenced the real `format` symbol in `internal/lsp/cmd/format.go` for a
/// reason that was not true.
fn go_package_names(root: &Path, go: &Readable) -> HashSet<String> {
    let mut names = HashSet::new();
    if let Ok(module) = std::fs::read_to_string(root.join("go.mod"))
        && let Some(path) = module
            .lines()
            .find_map(|line| line.trim().strip_prefix("module "))
        && let Some(last) = path.trim().rsplit('/').next()
    {
        names.insert(last.to_string());
    }
    for path in walk::files_under(root) {
        if path.extension().is_none_or(|suffix| suffix != "go") {
            continue;
        }
        let Ok(contents) = std::fs::read(&path) else {
            continue;
        };
        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&go.grammar).is_err() {
            continue;
        }
        let Some(tree) = parser.parse(&contents, None) else {
            continue;
        };
        collect_import_names(tree.root_node(), &contents, &mut names);
    }
    names
}

/// The last path element of every `import_spec` under `node`. An import
/// declaration is a top-level item, so a subtree with none is not walked.
fn collect_import_names(node: tree_sitter::Node, contents: &[u8], into: &mut HashSet<String>) {
    if node.kind() == "import_spec" {
        if let Some(quoted) = node.child_by_field_name("path")
            && let Ok(text) = quoted.utf8_text(contents)
            && let Some(last) = text.trim_matches(['"', '`']).rsplit('/').next()
            && !last.is_empty()
        {
            into.insert(last.to_string());
        }
        return;
    }
    // Nothing inside a function or a type can be an import, and those are
    // most of a file.
    if matches!(
        node.kind(),
        "function_declaration" | "method_declaration" | "type_declaration"
    ) {
        return;
    }
    let mut walking = node.walk();
    for child in node.named_children(&mut walking) {
        collect_import_names(child, contents, into);
    }
}

/// The module path a project's own packages live under, or an empty string
/// where the language has no such thing. For Go it is `go.mod`'s `module`
/// line, which is what tells an import inside the project apart from one
/// outside it.
pub fn own_path(language: &str, root: &Path) -> String {
    match language {
        "go" => std::fs::read_to_string(root.join("go.mod"))
            .ok()
            .and_then(|module| {
                module
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("module "))
                    .map(|path| path.trim().to_string())
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// The local names, in one file, of packages imported from outside the
/// project. A name selected through one of these belongs to that package, not
/// to a same-named symbol in this project -- `*types.Slice` is `go/types`'s
/// `Slice`, and answering about the project's own `Slice` with all 98 of them
/// is how one sampled name took a pooled precision figure from 99.7 to 85.5
/// per cent.
///
/// A package inside the project is deliberately not listed: `ssa.Slice` in
/// this module *is* a reference to this module's `Slice`, and a rename has to
/// change it.
pub fn foreign_qualifiers(
    language: &str,
    root_node: tree_sitter::Node,
    contents: &[u8],
    own_path: &str,
) -> HashSet<String> {
    if language != "go" {
        return HashSet::new();
    }
    let mut foreign = HashSet::new();
    collect_foreign_imports(root_node, contents, own_path, &mut foreign);
    foreign
}

fn collect_foreign_imports(
    node: tree_sitter::Node,
    contents: &[u8],
    own_path: &str,
    into: &mut HashSet<String>,
) {
    if node.kind() == "import_spec" {
        let Some(quoted) = node
            .child_by_field_name("path")
            .and_then(|path| path.utf8_text(contents).ok())
        else {
            return;
        };
        let path = quoted.trim_matches(['"', '`']);
        // An import of the project's own module is not foreign, and neither
        // is a relative one.
        if !own_path.is_empty() && (path == own_path || path.starts_with(&format!("{own_path}/"))) {
            return;
        }
        if path.starts_with('.') {
            return;
        }
        // An alias renames the package locally; without one the local name is
        // the last element of the path. A dot import puts every name in this
        // file's own scope, so nothing about it is a qualifier.
        let local = match node
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(contents).ok())
        {
            Some(".") | Some("_") => return,
            Some(alias) => alias.to_string(),
            None => match path.rsplit('/').next() {
                Some(last) if !last.is_empty() => last.to_string(),
                _ => return,
            },
        };
        into.insert(local);
        return;
    }
    if matches!(
        node.kind(),
        "function_declaration" | "method_declaration" | "type_declaration"
    ) {
        return;
    }
    let mut walking = node.walk();
    for child in node.named_children(&mut walking) {
        collect_foreign_imports(child, contents, own_path, into);
    }
}

/// Whether `named` is selected through a package from outside the project, so
/// that it names that package's symbol rather than one of this project's.
pub fn selected_from_a_foreign_package(
    language: &str,
    named: tree_sitter::Node,
    contents: &[u8],
    foreign: &HashSet<String>,
) -> bool {
    if language != "go" || foreign.is_empty() {
        return false;
    }
    let Some(parent) = named.parent() else {
        return false;
    };
    let qualifier = match parent.kind() {
        // `types.Slice` in a type position.
        "qualified_type" => parent.child_by_field_name("package"),
        // `types.NewSlice(...)` and `fmt.Println` in an expression position.
        // Only where the name is the selected field: the operand of a
        // selection is not selected through anything.
        "selector_expression" if parent.child_by_field_name("field") == Some(named) => {
            parent.child_by_field_name("operand")
        }
        _ => return false,
    };
    qualifier
        .filter(|node| matches!(node.kind(), "identifier" | "package_identifier"))
        .and_then(|node| node.utf8_text(contents).ok())
        .is_some_and(|text| foreign.contains(text))
}

/// Whether the server never reads this file at all, as far as reading the
/// project can tell. What decides the measurement is the server's own answer;
/// this only names the reason.
///
/// For Go the reason is usually the file's name: the go tool takes
/// `_GOOS.go`, `_GOARCH.go` and `_GOOS_GOARCH.go` as build constraints, so
/// `mmap_windows.go` is not part of a Linux build and `gopls` never loads it.
/// A `testdata` directory is skipped by the tool for the same reason -- it
/// holds inputs, not code.
pub fn out_of_the_servers_build(language: &str, path: &str) -> bool {
    match language {
        "go" => go_out_of_the_build(path),
        _ => false,
    }
}

fn go_out_of_the_build(path: &str) -> bool {
    if path.split('/').any(|segment| segment == "testdata") {
        return true;
    }
    let Some(stem) = path
        .rsplit('/')
        .next()
        .and_then(|name| name.strip_suffix(".go"))
    else {
        return false;
    };
    // Only the last one or two `_`-separated words can be a constraint, and
    // only when they name a target the go tool knows.
    let words: Vec<&str> = stem.split('_').collect();
    let host_os = go_name_for_os(std::env::consts::OS);
    let host_arch = go_name_for_arch(std::env::consts::ARCH);
    let names_a_target =
        |word: &str| GO_OPERATING_SYSTEMS.contains(&word) || GO_ARCHITECTURES.contains(&word);
    let matches_the_host = |word: &str| word == host_os || word == host_arch;
    match words.as_slice() {
        [.., second_last, last] if names_a_target(second_last) && names_a_target(last) => {
            !(matches_the_host(second_last) && matches_the_host(last))
        }
        [.., last] if names_a_target(last) => !matches_the_host(last),
        _ => false,
    }
}

/// The `GOOS` value for a Rust target-os name, where the two differ. Only the
/// ones this measurement can actually run on need to agree.
fn go_name_for_os(rust_name: &str) -> &str {
    match rust_name {
        "macos" => "darwin",
        other => other,
    }
}

/// The `GOARCH` value for a Rust target-arch name.
fn go_name_for_arch(rust_name: &str) -> &str {
    match rust_name {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64",
        other => other,
    }
}

const GO_OPERATING_SYSTEMS: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "dragonfly",
    "freebsd",
    "hurd",
    "illumos",
    "ios",
    "js",
    "linux",
    "nacl",
    "netbsd",
    "openbsd",
    "plan9",
    "solaris",
    "wasip1",
    "windows",
    "zos",
];

const GO_ARCHITECTURES: &[&str] = &[
    "386",
    "amd64",
    "amd64p32",
    "arm",
    "arm64",
    "arm64be",
    "armbe",
    "loong64",
    "mips",
    "mips64",
    "mips64le",
    "mips64p32",
    "mips64p32le",
    "mipsle",
    "ppc",
    "ppc64",
    "ppc64le",
    "riscv",
    "riscv64",
    "s390",
    "s390x",
    "sparc",
    "sparc64",
    "wasm",
];

/// Whether `node` declares names whose meaning is the scope they are read in
/// -- a local, a parameter, a generic parameter, an import's alias. Such a
/// name cannot be answered about by text alone, so the index declines to.
/// Returns the names it declares, or nothing where `node` declares none.
///
/// Named per language because the shapes are per language and nothing else
/// about them generalises: Rust unpacks destructuring patterns, Go declares
/// with `:=` and with `var`, and neither reads the other's tree.
pub fn names_bound_in_a_scope(
    language: &str,
    node: tree_sitter::Node,
    contents: &[u8],
) -> Vec<String> {
    match language {
        "go" => go_names_bound_in_a_scope(node, contents),
        // Rust's own unpacking lives beside the measurement that needs it,
        // because it recurses through pattern nodes rather than reading one.
        _ => Vec::new(),
    }
}

fn go_names_bound_in_a_scope(node: tree_sitter::Node, contents: &[u8]) -> Vec<String> {
    let named_field = |field: &str| -> Vec<String> {
        node.child_by_field_name(field)
            .map(|named| identifiers_under(named, contents))
            .unwrap_or_default()
    };
    match node.kind() {
        // `x, err := f()` and `var x, y int` and `const limit = 1`.
        "short_var_declaration" => named_field("left"),
        "var_spec" | "const_spec" => named_field("name"),
        // A parameter, a result name, and a method's receiver, which the
        // grammar writes as a parameter as well.
        "parameter_declaration" | "variadic_parameter_declaration" => named_field("name"),
        // `func F[T any]()` and `type S[T any] struct{}`.
        "type_parameter_declaration" => named_field("name"),
        // `for key, value := range m` and `for i := range n`.
        "range_clause" => named_field("left"),
        // `switch value := thing.(type)` binds `value` once per case arm,
        // and the grammar hands it over as the switch's own `alias`.
        "type_switch_statement" => named_field("alias"),
        // `case received := <-channel:` in a `select`.
        "receive_statement" => named_field("left"),
        // `import fancy "path/to/pkg"` gives the package a local name.
        "import_spec" => named_field("name"),
        // A labelled statement's name lives in its own namespace, and a
        // `goto` to it is not a reference to anything a rename touches.
        "label_name" | "labeled_statement" => {
            vec![]
        }
        _ => Vec::new(),
    }
}

/// Every identifier at or under `node`, which for a name list is exactly the
/// names it declares.
fn identifiers_under(node: tree_sitter::Node, contents: &[u8]) -> Vec<String> {
    if matches!(
        node.kind(),
        "identifier" | "field_identifier" | "package_identifier"
    ) {
        return node
            .utf8_text(contents)
            .ok()
            .map(|text| vec![text.to_string()])
            .unwrap_or_default();
    }
    let mut found = Vec::new();
    let mut walking = node.walk();
    for child in node.named_children(&mut walking) {
        found.extend(identifiers_under(child, contents));
    }
    found
}

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
