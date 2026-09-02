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
use crate::per_language;
use crate::walk;

/// The references query this measurement runs for `language`, compiled once
/// per call to [`measure`] and reused for the whole project.
fn query_text(language: &str) -> Result<&'static str> {
    per_language::references_query(language).with_context(|| {
        format!(
            "no references query is written for {language}; the ones that are: {}",
            per_language::LANGUAGES_WITH_A_REFERENCES_QUERY.join(", ")
        )
    })
}

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

/// Whether the index is willing to answer about a name at all.
///
/// Grouping by name is only sound when the name means one thing in the whole
/// project. Where it does not, every answer is a guess, and measured on this
/// project the guessing is what destroyed precision: 78 926 of 78 930 wrong
/// answers were "the name also names something else". Declining is not a
/// failure to answer -- it is the difference between an index that is
/// sometimes right and one that knows when it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certainty {
    /// Answer about every name, right or wrong. What the first measurement did.
    Always,
    /// Answer only where the name is declared once in the project and is not
    /// also used as a local binding anywhere.
    OnlyWhenTheNameMeansOneThing,
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
    /// Every name that means something local somewhere in the project: a
    /// `let`, a function or closure parameter, a name a destructuring pattern
    /// unpacks, or a generic parameter. All of them are names whose meaning
    /// depends on the scope they are read in, which is exactly what the index
    /// cannot resolve.
    local_bindings: HashSet<String>,
    /// Files rust-analyzer never reads at all: the `mod` line declaring them
    /// is behind a `#[cfg(...)]`, the file itself opens with one, or they
    /// belong to a cargo target the server does not build because its
    /// `required-features` are off. The grammar reads them regardless.
    files_out_of_the_servers_sight: HashSet<String>,
    /// How many files the grammar could only parse with recovery, and how
    /// many matches were dropped for falling inside a recovered range.
    files_with_recovery: usize,
    dropped_to_recovery: usize,
}

/// The Rust language the rest of the crate already knows how to read, found
/// the same way [`crate::symbols::build`] and [`crate::structural::search`]
/// do.
fn language_named(name: &str) -> Result<Readable> {
    let (readable, _) = languages::readable();
    readable
        .into_iter()
        .find(|language| language.name == name)
        .with_context(|| {
            format!("{name} is not one of the languages the editor ships an outline query for")
        })
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
    language: &str,
    contents: &[u8],
    outline: &tree_sitter::Query,
    tree: &tree_sitter::Tree,
) -> Vec<NamedAt> {
    let Some(name_index) = capture_index(outline, "name") else {
        return Vec::new();
    };
    let item_index = capture_index(outline, "item");
    let mut found = Vec::new();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(outline, tree.root_node(), contents);
    while let Some(matched) = matches.next() {
        // The same filter the index itself applies, and for the same reason:
        // some languages' outline queries yield what the editor wants to show
        // -- an object literal's entries, a `let` nested in a function -- and
        // an index of definitions holds none of it. Without this, the
        // measurement's idea of what is declared was not the index's, and
        // TypeScript's every local `const` counted as a definition: names
        // like `result` looked declared in hundreds of places, so the index
        // declined nearly every symbol it was offered.
        let declared = item_index
            .and_then(|item| {
                matched
                    .captures
                    .iter()
                    .find(|capture| capture.index == item)
                    .map(|capture| capture.node)
            })
            .map(|node| per_language::is_declaration(language, node))
            .unwrap_or(true);
        if !declared {
            continue;
        }
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
fn defined_symbols_pass(root: &Path, language: &str, rust: &Readable) -> Vec<NamedAt> {
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
        // A name recovery invented is not a definition, the same claim the
        // references side makes for a reference.
        let mut recovered = Vec::new();
        recovered_rows(tree.root_node(), &mut recovered);
        found.extend(
            defined_symbols_in(&relative_path, language, &contents, &rust.outline, &tree)
                .into_iter()
                .filter(|symbol| !row_falls_in(&recovered, symbol.row)),
        );
    }
    found
}

/// Every occurrence `query` finds in one already-parsed file: any capture
/// whose name starts with `reference.`, whichever of the query's patterns it
/// came from.
fn occurrences_in(
    path: &str,
    language: &str,
    contents: &[u8],
    query: &tree_sitter::Query,
    tree: &tree_sitter::Tree,
    foreign: &HashSet<String>,
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
            // A name selected through a package from outside the project is
            // that package's, not this project's -- whatever it is called.
            if per_language::selected_from_a_foreign_package(
                language,
                capture.node,
                contents,
                foreign,
            ) {
                continue;
            }
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

/// Every name a crate in this workspace answers to: the workspace's own
/// members and everything they depend on, with `-` written the way code
/// writes it. A project symbol under one of these names cannot be told apart
/// from the crate: `crates/util/src/util.rs` declares `pub mod serde`, and
/// asking about `serde` by text alone answers with every `#[serde(...)]`
/// attribute and every `use serde::...` in the project -- three and a half
/// thousand findings for one sampled name, none of them about that module.
fn names_of_crates(root: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    for manifest_path in member_manifests(root) {
        let Ok(manifest) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let mut inside_dependencies = false;
        let mut inside_a_named_table = false;
        let mut depth = 0i32;
        for line in manifest.lines() {
            let line = line.trim();
            let continuing = depth > 0;
            depth = (depth + bracket_depth_change(line)).max(0);
            if continuing {
                continue;
            }
            if let Some(table) = line.strip_prefix('[') {
                let table = table.trim_end_matches(']');
                inside_dependencies = table.ends_with("dependencies");
                inside_a_named_table = matches!(table, "package" | "lib");
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if inside_a_named_table {
                if let Some(value) = line.strip_prefix("name")
                    && let Some(value) = value.split('=').nth(1)
                {
                    names.insert(value.trim().trim_matches('"').replace('-', "_"));
                }
                continue;
            }
            if inside_dependencies {
                let key = line
                    .split(['=', '.'])
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .trim_matches('"');
                if !key.is_empty() {
                    names.insert(key.replace('-', "_"));
                }
            }
        }
    }
    names
}

/// Every file a file opening with `#![cfg(...)]` covers. The file itself
/// always; and where it is a crate's root -- `src/lib.rs`, `src/main.rs`, or
/// the `src/<crate>.rs` this project's own guidelines ask new crates to use
/// -- the whole crate, since a gate on the root removes every module
/// reachable from it. `crates/gpui_macos` is the real case: one
/// `#![cfg(target_os = "macos")]` and rust-analyzer reads none of its
/// twenty-odd files on any other platform.
fn files_a_gated_file_covers(gated: &str, all_files: &HashSet<String>) -> Vec<String> {
    let mut covered = vec![gated.to_string()];
    let Some((directory, file)) = gated.rsplit_once('/') else {
        return covered;
    };
    let Some(crate_root_directory) = directory.strip_suffix("/src") else {
        return covered;
    };
    let stem = file.strip_suffix(".rs").unwrap_or(file);
    let crate_name = crate_root_directory.rsplit('/').next().unwrap_or_default();
    let is_a_crate_root = stem == "lib" || stem == "main" || stem == crate_name;
    if !is_a_crate_root {
        return covered;
    }
    let inside = format!("{directory}/");
    covered.extend(
        all_files
            .iter()
            .filter(|path| path.starts_with(&inside))
            .cloned(),
    );
    covered
}

/// Every file belonging to a cargo target the server does not build. A
/// `[[test]]`, `[[bin]]`, `[[bench]]` or `[[example]]` with
/// `required-features` is built only when those features are on, and
/// rust-analyzer resolves the workspace with the default set -- so it never
/// reads the target's files, while the grammar reads every file it finds.
/// `crates/collab`'s integration tests and `crates/html_render`'s engine
/// tests are the real cases.
///
/// The manifest is read the same crude way [`excluded_from_the_workspace`]
/// reads the workspace one, which is enough for the shape cargo actually
/// writes: a target's own table, one key per line.
fn files_behind_a_required_feature(root: &Path, all_files: &HashSet<String>) -> Vec<String> {
    let mut covered = Vec::new();
    for manifest_path in member_manifests(root) {
        let Ok(manifest) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Some(directory) = manifest_path.parent().and_then(|parent| {
            parent
                .strip_prefix(root)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        }) else {
            continue;
        };
        for target in targets_needing_a_feature(&manifest) {
            let path = if directory.is_empty() {
                target
            } else {
                format!("{directory}/{target}")
            };
            if all_files.contains(&path) {
                covered.push(path.clone());
            }
            // A target's own modules live beside its root file -- but only
            // where that directory belongs to the target alone. A `[[bin]]`
            // rooted at `src/helper.rs` shares `src` with the crate's whole
            // library, and taking the directory there would set aside the
            // crate, not the target: `crates/sandbox` and `crates/zed` both
            // have such a bin, and the mistake would have quietly flattered
            // precision by the size of two crates.
            if let Some(inside) = a_directory_of_its_own(&path) {
                covered.extend(
                    all_files
                        .iter()
                        .filter(|other| other.starts_with(&inside))
                        .cloned(),
                );
            }
        }
    }
    covered
}

/// The directory holding `path`, where that directory belongs to one cargo
/// target alone -- a subdirectory of `tests`, `benches` or `examples`, the
/// layout cargo gives a target with its own modules. `src` and the roots of
/// those three are shared, and are never returned.
fn a_directory_of_its_own(path: &str) -> Option<String> {
    let (directory, _) = path.rsplit_once('/')?;
    let (above, _) = directory.rsplit_once('/')?;
    let owner = above.rsplit('/').next()?;
    matches!(owner, "tests" | "benches" | "examples").then(|| format!("{directory}/"))
}

/// Whether `line` closes as many brackets as it opens, and how the running
/// depth changes. A dependency written as a multi-line inline table --
/// `image = { features = [\n  "bmp",\n] }` -- puts bare strings on their own
/// lines, and reading those as keys invented crate names like `bmp` and
/// `File`, each of which then silenced a real symbol for the wrong reason.
fn bracket_depth_change(line: &str) -> i32 {
    line.chars()
        .map(|character| match character {
            '{' | '[' => 1,
            '}' | ']' => -1,
            _ => 0,
        })
        .sum()
}

/// The `path` of every target table in `manifest` that also carries
/// `required-features`.
fn targets_needing_a_feature(manifest: &str) -> Vec<String> {
    let mut needing = Vec::new();
    let mut path: Option<String> = None;
    let mut needs_a_feature = false;
    let mut depth = 0i32;
    fn finish(path: &mut Option<String>, needs: &mut bool, into: &mut Vec<String>) {
        if let Some(found) = path.take()
            && *needs
        {
            into.push(found);
        }
        *needs = false;
    }
    for line in manifest.lines() {
        let line = line.trim();
        let continuing = depth > 0;
        depth = (depth + bracket_depth_change(line)).max(0);
        if continuing {
            continue;
        }
        if line.starts_with('[') {
            finish(&mut path, &mut needs_a_feature, &mut needing);
            continue;
        }
        if let Some(value) = line.strip_prefix("path") {
            path = value
                .split('=')
                .nth(1)
                .map(|value| value.trim().trim_matches('"').to_string());
        }
        if line.starts_with("required-features") {
            needs_a_feature = true;
        }
    }
    finish(&mut path, &mut needs_a_feature, &mut needing);
    needing
}

/// The root manifest and one manifest per workspace member -- not every
/// `Cargo.toml` under the tree. The difference matters twice: the root
/// excludes `crates/collab`, whose package name would otherwise decline the
/// unrelated `pub mod collab` in `crates/title_bar`; and nested example
/// packages such as `crates/gpui_web/examples/hello_web` are their own
/// workspaces the server never resolves at all.
fn member_manifests(root: &Path) -> Vec<PathBuf> {
    let root_manifest = root.join("Cargo.toml");
    let mut manifests = vec![root_manifest.clone()];
    let Ok(manifest) = std::fs::read_to_string(&root_manifest) else {
        return manifests;
    };
    for member in quoted_list_named("members", &manifest) {
        if excluded_from_the_workspace(root, &member) {
            continue;
        }
        let at = root.join(&member).join("Cargo.toml");
        if at.is_file() {
            manifests.push(at);
        }
    }
    manifests
}

/// The quoted entries of a `<name> = [ ... ]` array, which cargo writes one
/// per line. Read the same crude way [`excluded_from_the_workspace`] reads
/// the workspace's `exclude`, and for the same reason: one shape, written by
/// one tool.
fn quoted_list_named(name: &str, manifest: &str) -> Vec<String> {
    let opening = format!("{name} = [");
    let Some(after) = manifest.split(&opening).nth(1) else {
        return Vec::new();
    };
    let Some(inside) = after.split(']').next() else {
        return Vec::new();
    };
    inside
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.split('#').next().unwrap_or_default().trim();
            let entry = line.trim_end_matches(',').trim().trim_matches('"');
            (!entry.is_empty()).then(|| entry.to_string())
        })
        .collect()
}

/// The classification-support spans and names one file's own tree yields, on
/// the side, while it is parsed for the references pass anyway.
#[derive(Default)]
struct FileSpans {
    macro_invocations: Vec<(u32, u32)>,
    use_declarations: Vec<(u32, u32)>,
    cfg_gated: Vec<(u32, u32)>,
    /// Row ranges of every `ERROR` and `MISSING` node in the file. Recovery
    /// hands out identifiers from text that is not yet code, so a match
    /// inside one of these is not a reference -- but the rest of the file is
    /// parsed correctly and dropping all of it would lose the largest files
    /// in the project to one construct the grammar cannot read.
    recovered: Vec<(u32, u32)>,
    /// Whether the file carries a root-level `#![cfg(...)]`, which takes the
    /// whole file, and at a crate root the whole crate, out of the server's
    /// sight.
    gated_as_a_whole: bool,
    /// Names of modules this file declares behind a `#[cfg(...)]`. A gate on
    /// the `mod` line takes the module's whole file out of the server's sight,
    /// which a span inside that file cannot express.
    cfg_gated_modules: Vec<String>,
    local_bindings: HashSet<String>,
}

/// Whether `attribute_item`'s own attribute is a bare `#[cfg(...)]` whose
/// predicate does **not** hold where this runs. `cfg_attr` is deliberately
/// not included: unlike `cfg`, it does not remove the item it decorates when
/// its predicate is false.
///
/// Evaluating the predicate is the whole point. `crates/gpui_linux` opens
/// with `#![cfg(any(target_os = "linux", target_os = "freebsd"))]`, and on
/// the Linux machine this measurement runs on that gate is *open*: treating
/// every `#[cfg]` as shut would have set the crate aside from the comparison
/// and quietly flattered the number by the size of it.
fn is_a_shut_cfg_attribute(attribute_item: tree_sitter::Node, contents: &[u8]) -> bool {
    // Both `attribute_item` and `inner_attribute_item` hold the attribute as
    // their only named child, so one reader serves both.
    let Some(attribute) = attribute_item.named_child(0) else {
        return false;
    };
    let Some(name) = attribute.named_child(0) else {
        return false;
    };
    if name.kind() != "identifier" || name.utf8_text(contents) != Ok("cfg") {
        return false;
    }
    let Some(arguments) = attribute.child_by_field_name("arguments") else {
        return false;
    };
    let Ok(text) = arguments.utf8_text(contents) else {
        return false;
    };
    cfg_holds(text.trim_start_matches('(').trim_end_matches(')')) == Some(false)
}

/// Whether a `cfg` predicate holds on the platform this runs on, or `None`
/// where nothing here can tell. Only the keys this project actually gates on
/// are decided: the target, and the two shorthands for its family. A feature,
/// `test`, `debug_assertions` and anything else is `None` -- unknown, and an
/// unknown gate is never called shut, because calling it shut is what
/// excuses a wrong answer.
fn cfg_holds(predicate: &str) -> Option<bool> {
    let predicate = predicate.trim();
    let combinators: [(&str, fn(&str) -> Option<bool>); 3] =
        [("any", any_of), ("all", all_of), ("not", negation)];
    for (combinator, verdict) in combinators {
        if let Some(rest) = predicate.strip_prefix(combinator)
            && let Some(inside) = rest.trim_start().strip_prefix('(')
            && let Some(inside) = inside.strip_suffix(')')
        {
            return verdict(inside);
        }
    }
    if let Some((key, value)) = predicate.split_once('=') {
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        return match key {
            "target_os" => Some(value == std::env::consts::OS),
            "target_family" => Some(value == std::env::consts::FAMILY),
            "target_arch" => Some(value == std::env::consts::ARCH),
            _ => None,
        };
    }
    match predicate {
        "unix" => Some(std::env::consts::FAMILY == "unix"),
        "windows" => Some(std::env::consts::FAMILY == "windows"),
        _ => None,
    }
}

/// The comma-separated predicates of a combinator, split at the top level so
/// that a nested `any(a, b)` stays whole.
fn predicates_of(inside: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (at, character) in inside.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(inside[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
    }
    let last = inside[start..].trim();
    if !last.is_empty() {
        parts.push(last);
    }
    parts
}

fn any_of(inside: &str) -> Option<bool> {
    let mut anything_unknown = false;
    for part in predicates_of(inside) {
        match cfg_holds(part) {
            Some(true) => return Some(true),
            Some(false) => {}
            None => anything_unknown = true,
        }
    }
    (!anything_unknown).then_some(false)
}

fn all_of(inside: &str) -> Option<bool> {
    let mut anything_unknown = false;
    for part in predicates_of(inside) {
        match cfg_holds(part) {
            Some(false) => return Some(false),
            Some(true) => {}
            None => anything_unknown = true,
        }
    }
    (!anything_unknown).then_some(true)
}

fn negation(inside: &str) -> Option<bool> {
    cfg_holds(inside).map(|held| !held)
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

/// Records every name `pattern` binds, unpacking destructuring patterns down
/// to the identifiers they introduce. Two parts of a pattern name something
/// other than a binding and are skipped: the path a pattern matches *against*
/// (`Some(value)` binds `value` and names `Some`) and a match arm's guard
/// expression. Reading either as a binding would decline every symbol sharing
/// a name with a matched-on variant or a guard's variable.
fn record_binding_name(pattern: tree_sitter::Node, contents: &[u8], into: &mut FileSpans) {
    match pattern.kind() {
        "identifier" | "shorthand_field_identifier" => {
            if let Ok(text) = pattern.utf8_text(contents) {
                into.local_bindings.insert(text.to_string());
            }
        }
        // `Point { x: left }` binds `left`, not `x`; `Point { x }` binds `x`.
        "field_pattern" => {
            if let Some(inner) = pattern.child_by_field_name("pattern") {
                record_binding_name(inner, contents, into);
            } else {
                let mut cursor = pattern.walk();
                for child in pattern.named_children(&mut cursor) {
                    if child.kind() == "shorthand_field_identifier" {
                        record_binding_name(child, contents, into);
                    }
                }
            }
        }
        "tuple_struct_pattern"
        | "struct_pattern"
        | "match_pattern"
        | "mut_pattern"
        | "ref_pattern"
        | "reference_pattern"
        | "tuple_pattern"
        | "slice_pattern"
        | "or_pattern"
        | "captured_pattern" => {
            let matched_against = pattern.child_by_field_name("type");
            let guard = pattern.child_by_field_name("condition");
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                if Some(child) != matched_against && Some(child) != guard {
                    record_binding_name(child, contents, into);
                }
            }
        }
        _ => {}
    }
}

/// The row ranges of every `ERROR` and `MISSING` node under `node`, pruning
/// whole subtrees that parsed cleanly -- `has_error` is true only where the
/// node or one of its descendants is one, so a correct file costs one check.
fn recovered_rows(node: tree_sitter::Node, into: &mut Vec<(u32, u32)>) {
    if node.is_error() || node.is_missing() {
        into.push((
            node.start_position().row as u32,
            node.end_position().row as u32,
        ));
        return;
    }
    if !node.has_error() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        recovered_rows(child, into);
    }
}

/// Whether one row falls inside any of `spans`.
fn row_falls_in(spans: &[(u32, u32)], row: u32) -> bool {
    spans
        .iter()
        .any(|(start, end)| *start <= row && row <= *end)
}

/// Every file the module `module`, declared in `declaring_file` behind a
/// `#[cfg(...)]`, covers. Both module layouts are tried, since which one
/// applies depends on whether the declaring file is its directory's root --
/// something only the crate manifest settles, and guessing wrong in the
/// harmless direction (a file that happens to sit at the other candidate
/// path) costs a set-aside finding, not a wrong number. Files under the
/// module's own directory are covered by the same prefix, which is what makes
/// the gate transitive: a module nested inside a gated one is gated too.
fn files_a_gated_module_covers(
    declaring_file: &str,
    module: &str,
    all_files: &HashSet<String>,
) -> Vec<String> {
    let directory = match declaring_file.rfind('/') {
        Some(cut) => &declaring_file[..cut],
        None => "",
    };
    let stem = declaring_file
        .rsplit('/')
        .next()
        .and_then(|name| name.strip_suffix(".rs"))
        .unwrap_or_default();
    let mut roots = Vec::new();
    if directory.is_empty() {
        roots.push(module.to_string());
        roots.push(format!("{stem}/{module}"));
    } else {
        roots.push(format!("{directory}/{module}"));
        roots.push(format!("{directory}/{stem}/{module}"));
    }

    let mut covered = Vec::new();
    for root in roots {
        let as_a_file = format!("{root}.rs");
        if all_files.contains(&as_a_file) {
            covered.push(as_a_file);
        }
        let as_a_directory = format!("{root}/");
        covered.extend(
            all_files
                .iter()
                .filter(|path| path.starts_with(&as_a_directory))
                .cloned(),
        );
    }
    covered
}

/// Walks the whole tree once, collecting everything [`FileSpans`] needs.
fn walk_for_classification(
    language: &str,
    node: tree_sitter::Node,
    contents: &[u8],
    into: &mut FileSpans,
) {
    // What a language other than Rust binds in a scope is described beside
    // the language, not here: the shapes have nothing in common beyond being
    // names whose meaning is their scope's.
    into.local_bindings
        .extend(per_language::names_bound_in_a_scope(
            language, node, contents,
        ));
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
        // A root-level `#![cfg(...)]` gates everything the file contains --
        // `crates/gpui_macos` opens with `#![cfg(target_os = "macos")]`, and
        // on any other platform the server never reads a line of it.
        "inner_attribute_item" => {
            if is_a_shut_cfg_attribute(node, contents)
                && node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "source_file")
            {
                into.gated_as_a_whole = true;
            }
        }
        // A generic parameter is a name whose meaning is its scope's, exactly
        // like a local: `impl<V: Bound> Trait for V` puts `V` in the outline
        // as a type, and answering about it means answering about every `V`
        // in the project.
        "type_parameters" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if matches!(child.kind(), "type_parameter" | "const_parameter")
                    && let Some(name) = child.child_by_field_name("name")
                    && let Ok(text) = name.utf8_text(contents)
                {
                    into.local_bindings.insert(text.to_string());
                }
            }
        }
        "attribute_item" => {
            if is_a_shut_cfg_attribute(node, contents)
                && let Some(gated) = item_gated_by(node)
            {
                into.cfg_gated.push((
                    gated.start_position().row as u32,
                    gated.end_position().row as u32,
                ));
                if gated.kind() == "mod_item"
                    && let Some(name) = gated.child_by_field_name("name")
                    && let Ok(text) = name.utf8_text(contents)
                {
                    into.cfg_gated_modules.push(text.to_string());
                }
            }
        }
        "closure_parameters" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                record_binding_name(child, contents, into);
            }
        }
        // Every grammar node that owns a `pattern` field is a binding site:
        // `let`, a parameter, an `if let`/`while let` condition, a match arm,
        // a `for` loop. Named rather than enumerated, so a grammar that grows
        // another one does not quietly stop being covered.
        _ if node.child_by_field_name("pattern").is_some() => {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                record_binding_name(pattern, contents, into);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_classification(language, child, contents, into);
    }
}

/// A reference position turned into the index's own `Definition` shape --
/// `kind` is left empty: a reference's grammar node kind (`identifier`,
/// `field_identifier`, and so on) is not the vocabulary `Definition::kind`
/// otherwise carries, and is not something the comparison needs.
fn definition_of(named: &NamedAt, language: &str) -> Definition {
    Definition {
        path: named.path.clone(),
        name: named.name.clone(),
        kind: String::new(),
        line: named.row + 1,
        language: language.to_string(),
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
    language: &str,
    rust: &Readable,
    references_query: &tree_sitter::Query,
    defined: Vec<NamedAt>,
) -> ProjectScan {
    let mut raw_occurrences = Vec::new();
    let mut macro_spans: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    let mut use_spans: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    let mut cfg_gated_spans: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    let mut local_bindings: HashSet<String> = HashSet::new();
    let mut all_files: HashSet<String> = HashSet::new();
    let mut gated_modules: Vec<(String, String)> = Vec::new();
    let mut gated_files: Vec<String> = Vec::new();
    let mut files_with_recovery = 0usize;
    let mut dropped_to_recovery = 0usize;
    let own_path = per_language::own_path(language, root);

    for path in rust_files_under(root, rust) {
        let Ok(contents) = std::fs::read(&path) else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative_path = relative.to_string_lossy().replace('\\', "/");
        all_files.insert(relative_path.clone());

        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&rust.grammar).is_err() {
            continue;
        }
        let Some(tree) = parser.parse(&contents, None) else {
            continue;
        };
        let mut spans = FileSpans::default();
        walk_for_classification(language, tree.root_node(), &contents, &mut spans);
        recovered_rows(tree.root_node(), &mut spans.recovered);

        // What recovery produced is not code, and this query's widest pattern
        // is a bare identifier -- so a match inside a recovered range would
        // read as a reference when it is not. Only those ranges are dropped,
        // not the file: the fifteen files this project cannot parse include
        // the largest and most-referenced ones in it (`editor.rs`,
        // `gpui/src/window.rs`, `language.rs`), each over one construct the
        // grammar does not read, and dropping them whole made the index look
        // like it had never heard of them.
        if !spans.recovered.is_empty() {
            files_with_recovery += 1;
        }
        let foreign =
            per_language::foreign_qualifiers(language, tree.root_node(), &contents, &own_path);
        let found = occurrences_in(
            &relative_path,
            language,
            &contents,
            references_query,
            &tree,
            &foreign,
        );
        let before = found.len();
        let kept: Vec<NamedAt> = found
            .into_iter()
            .filter(|occurrence| !row_falls_in(&spans.recovered, occurrence.row))
            .collect();
        dropped_to_recovery += before - kept.len();
        raw_occurrences.extend(kept);

        if spans.gated_as_a_whole {
            gated_files.push(relative_path.clone());
        }
        if !spans.macro_invocations.is_empty() {
            macro_spans.insert(relative_path.clone(), spans.macro_invocations);
        }
        if !spans.use_declarations.is_empty() {
            use_spans.insert(relative_path.clone(), spans.use_declarations);
        }
        if !spans.cfg_gated.is_empty() {
            cfg_gated_spans.insert(relative_path.clone(), spans.cfg_gated);
        }
        for module in spans.cfg_gated_modules {
            gated_modules.push((relative_path.clone(), module));
        }
        local_bindings.extend(spans.local_bindings);
    }

    let mut files_out_of_the_servers_sight: HashSet<String> = HashSet::new();
    for (declaring_file, module) in &gated_modules {
        files_out_of_the_servers_sight.extend(files_a_gated_module_covers(
            declaring_file,
            module,
            &all_files,
        ));
    }
    for gated in &gated_files {
        files_out_of_the_servers_sight.extend(files_a_gated_file_covers(gated, &all_files));
    }
    files_out_of_the_servers_sight.extend(files_behind_a_required_feature(root, &all_files));

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
            .push(definition_of(&occurrence, language));
    }

    ProjectScan {
        defined,
        index: ReferenceIndex { by_name },
        macro_spans,
        use_spans,
        cfg_gated_spans,
        local_bindings,
        files_out_of_the_servers_sight,
        files_with_recovery,
        dropped_to_recovery,
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
    // Only symbols with a plain name. The outline query also yields compound
    // ones -- an `impl` block is captured with both the trait and the type it
    // joins, so its name comes through as `EventEmitter<GitStoreEvent>` or
    // `platform::Os`. Those are not names anybody renames, and asking an index
    // that groups by identifier for references to a whole path is comparing two
    // different things: measured, they alone accounted for most of what looked
    // like a recall failure.
    let symbols: Vec<NamedAt> = symbols
        .iter()
        .filter(|symbol| is_a_plain_name(&symbol.name))
        .cloned()
        .collect();
    let symbols = symbols.as_slice();
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

/// Whether the project's own manifest excludes the crate this path is in, and
/// with it the server's knowledge of the file. Read from `exclude` in the root
/// `Cargo.toml` rather than hard-coded: which crates are left out of the
/// workspace is the project's decision and it changes.
fn excluded_from_the_workspace(root: &Path, path: &str) -> bool {
    use std::sync::OnceLock;
    static EXCLUDED: OnceLock<Vec<String>> = OnceLock::new();
    let excluded = EXCLUDED.get_or_init(|| {
        let Ok(manifest) = std::fs::read_to_string(root.join("Cargo.toml")) else {
            return Vec::new();
        };
        let Some(after) = manifest.split("exclude = [").nth(1) else {
            return Vec::new();
        };
        let Some(inside) = after.split(']').next() else {
            return Vec::new();
        };
        inside
            .split(',')
            .filter_map(|entry| {
                let entry = entry.trim().trim_matches('"');
                (!entry.is_empty()).then(|| entry.to_string())
            })
            .collect()
    });
    excluded
        .iter()
        .any(|left_out| path.starts_with(left_out.as_str()))
}

/// Which crate a project-relative path belongs to, or the path's first
/// component where it is not under `crates/`.
fn crate_of(path: &str) -> &str {
    let mut parts = path.split('/');
    match (parts.next(), parts.next()) {
        (Some("crates"), Some(name)) => name,
        (Some(first), _) => first,
        _ => path,
    }
}

/// Whether a name is one identifier and nothing else -- the only shape an
/// index that groups by name can be asked about, and the only shape a rename
/// applies to.
fn is_a_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|letter| letter.is_alphanumeric() || letter == '_')
        && !name.chars().next().is_some_and(|first| first.is_numeric())
}

/// `matched` over everything the index reported, `matched` plus
/// [`Comparison::the_indexs_extra_findings`]. `1.0` where the index reported
/// nothing at all: there is then no wrong answer among its output, which is
/// the standard convention for precision over an empty result set, the same
/// vacuous-truth shape [`compare`]'s own `recall` already uses for the
/// opposite case.
/// Precision with `set_aside` of the extra findings not counted against the
/// index, because they are not wrong.
///
/// Two kinds qualify, and both are the same fault as the ones this measurement
/// has already had to correct: the two sides are looking at different code.
///
/// A reference behind a `#[cfg(...)]` that is off in this build exists in the
/// file and a rename has to change it; rust-analyzer does not list it because
/// it is not compiling that branch. Holding that against the index measures
/// the server's configuration.
///
/// A reference in a crate the workspace excludes is outside the server's cargo
/// graph, so the server has no opinion about it at all -- while the code is
/// still there and still has to be renamed.
fn precision_over(comparison: &Comparison, set_aside: usize) -> f64 {
    let wrong = comparison
        .the_indexs_extra_findings
        .saturating_sub(set_aside);
    let index_findings = comparison.matched + wrong;
    if index_findings == 0 {
        1.0
    } else {
        comparison.matched as f64 / index_findings as f64
    }
}

/// The plan's own precision gate: below it, references are not fit to base a
/// rename on. Read here as well as by the binary that enforces it, so the
/// share of symbols that clear the gate can be reported beside the figure.
pub const REQUIRED_PRECISION: f64 = 0.90;

/// Precision read one symbol at a time. A pooled figure weights every symbol
/// by how many times its name occurs, which lets a single name decide the
/// whole number -- and a rename is invoked on one symbol, never on a pool.
/// Reported beside the pooled figure, never instead of it.
#[derive(Debug, Clone, Copy)]
pub struct PerSymbol {
    pub mean: f64,
    pub median: f64,
    /// How many symbols answered at or above the gate, and how many were
    /// judged at all -- a symbol the index answered nothing about, and the
    /// server nothing either, is not a clean answer, it is no answer.
    pub at_or_above_the_gate: usize,
    pub judged: usize,
    /// Symbols the index answered nothing about, so there was no precision to
    /// read. Excluded from the figures above -- precision is about the answers
    /// given, and scoring a silence either way would be a claim about recall.
    /// Printed, so the figures are never read without knowing how many
    /// symbols they left out.
    pub answered_nothing: usize,
}

fn precision_per_symbol(
    comparison: &Comparison,
    catalogue: &ErrorCatalogue,
    never_looking: &impl Fn(&ClassifiedDivergence) -> bool,
) -> PerSymbol {
    let mut set_aside_by_query: HashMap<usize, usize> = HashMap::new();
    for extra in &catalogue.extra {
        if never_looking(extra) {
            *set_aside_by_query.entry(extra.at).or_default() += 1;
        }
    }
    let mut precisions = Vec::with_capacity(comparison.per_query.len());
    let mut answered_nothing = 0usize;
    for (at, outcome) in comparison.per_query.iter().enumerate() {
        let set_aside = set_aside_by_query.get(&at).copied().unwrap_or(0);
        let wrong = outcome.extra.saturating_sub(set_aside);
        let findings = outcome.matched + wrong;
        if findings == 0 {
            answered_nothing += 1;
            continue;
        }
        precisions.push(outcome.matched as f64 / findings as f64);
    }
    if precisions.is_empty() {
        return PerSymbol {
            mean: 1.0,
            median: 1.0,
            at_or_above_the_gate: 0,
            judged: 0,
            answered_nothing,
        };
    }
    let judged = precisions.len();
    let mean = precisions.iter().sum::<f64>() / judged as f64;
    let at_or_above_the_gate = precisions
        .iter()
        .filter(|precision| **precision >= REQUIRED_PRECISION)
        .count();
    precisions.sort_by(|left, right| left.total_cmp(right));
    let median = if judged % 2 == 1 {
        precisions[judged / 2]
    } else {
        (precisions[judged / 2 - 1] + precisions[judged / 2]) / 2.0
    };
    PerSymbol {
        mean,
        median,
        at_or_above_the_gate,
        judged,
        answered_nothing,
    }
}

/// Every zero-based column on the reported line where `identity`'s name
/// stands as a whole identifier. The index's own `Definition` carries no
/// column, and asking a language server about a position needs one -- so the
/// name is found in the line again.
///
/// Every occurrence, not the first: the first may be inside a string or a
/// comment, and a finding is only treated as one the server never saw when
/// the server resolves nothing at *any* of them. Guessing one position and
/// trusting the answer would excuse a wrong answer on a coin toss.
///
/// Columns are counted in UTF-16 code units, which is what the protocol
/// means by a character; counting scalars is short by one per astral-plane
/// character, and this project has source lines with those in them.
fn columns_of(root: &Path, identity: &Identity) -> Vec<u32> {
    let Some(row) = identity.line.checked_sub(1) else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(root.join(&identity.path)) else {
        return Vec::new();
    };
    let Some(line) = contents.lines().nth(row as usize) else {
        return Vec::new();
    };
    let name = identity.name.as_str();
    if name.is_empty() {
        return Vec::new();
    }
    let mut columns = Vec::new();
    for (at, _) in line.match_indices(name) {
        let before = line[..at].chars().next_back();
        let after = line[at + name.len()..].chars().next();
        if before.is_some_and(is_name_character) || after.is_some_and(is_name_character) {
            continue;
        }
        if let Ok(column) = u32::try_from(line[..at].encode_utf16().count()) {
            columns.push(column);
        }
    }
    columns
}

fn is_name_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Whether an extra finding sits in code the server was never looking at, as
/// far as reading the source can tell: a `#[cfg]` that is shut here, or a
/// crate the workspace excludes. The caller adds what the server itself
/// says about the file, which is the part that decides.
fn was_the_server_never_looking(root: &Path, language: &str, extra: &ClassifiedDivergence) -> bool {
    matches!(
        extra.reason,
        DivergenceReason::BehindACfgAttribute | DivergenceReason::InAModuleGatedByACfg
    ) || excluded_from_the_workspace(root, &extra.identity.path)
        || per_language::out_of_the_servers_build(language, &extra.identity.path)
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
    /// The reference lives in a file rust-analyzer never reads, because the
    /// `mod` line declaring it is behind a `#[cfg(...)]` -- an optional module
    /// of a crate, switched off by the feature set the server resolves with.
    InAModuleGatedByACfg,
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
                "the name is also bound as a local variable, a parameter or a generic parameter \
                 somewhere in the project"
            }
            DivergenceReason::BehindACfgAttribute => {
                "the reference sits behind a #[cfg(...)] the grammar parses but \
                 rust-analyzer does not activate"
            }
            DivergenceReason::InAModuleGatedByACfg => {
                "the reference is in a module declared behind a #[cfg(...)], so rust-analyzer \
                 never read the file at all"
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
    /// Which of `Comparison::per_query` this came from. Two sampled symbols
    /// can share a name, so the name alone does not identify a query -- and
    /// attributing one query's set-aside findings to another's would subtract
    /// a real wrong answer from the wrong figure.
    pub at: usize,
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
    if scan.files_out_of_the_servers_sight.contains(&identity.path) {
        return DivergenceReason::InAModuleGatedByACfg;
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
                    at: divergence.at,
                    query: divergence.query.clone(),
                    identity: identity.clone(),
                    reason: classify_missed(scan, identity),
                });
            }
            for identity in &divergence.the_index_found_and_the_server_did_not {
                extra.push(ClassifiedDivergence {
                    at: divergence.at,
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
    /// Sampled symbols whose own declaration the server cannot see -- gated by
    /// a `#[cfg]` its build switches off, or in a file it never reads. It
    /// answers nothing about them, so neither side can be held to the
    /// comparison. Counted and printed rather than excused afterwards.
    pub declaration_the_server_cannot_see: usize,
    /// How many probes the server answered with an error rather than a yes or
    /// a no. Each is read as a no -- that is the question the probe asks --
    /// and counted here, because a run where the server failed on many of
    /// them is a run to distrust, not one to report.
    pub probes_the_server_failed: usize,
    /// Sampled symbols the index refused to answer about, because the name
    /// means more than one thing. Printed beside the sample size: a precision
    /// figure over a subset is only worth reading next to how big the subset
    /// is.
    pub declined: usize,
    /// Extra findings not counted against precision, because the server was
    /// never looking at that code: a branch its build has switched off, or a
    /// crate the workspace excludes. Printed, so a precision figure is never
    /// quietly helped along.
    pub outside_the_servers_sight: usize,
    /// Of those, how many for each reason. Which rule costs the coverage is the
    /// thing that decides whether it is worth refining.
    pub declined_shared_declaration: usize,
    pub declined_local_binding: usize,
    /// Sampled symbols declined because a crate or package in the project, or
    /// one it depends on, answers to the same name.
    pub declined_crate_name: usize,
    /// Of the names declared more than once: how many were declared several
    /// times inside a single crate. Kept because it is the number that closed
    /// off import-based narrowing -- see the note below.
    pub declined_within_one_crate: usize,
    pub comparison: Comparison,
    pub precision: f64,
    /// The same precision read one symbol at a time -- see [`PerSymbol`].
    pub per_symbol: PerSymbol,
    pub catalogue: ErrorCatalogue,
    /// How long a project-wide pass over the outline query took.
    pub definitions_pass: Duration,
    /// How long a project-wide pass over the references query, plus the
    /// error catalogue's own classification walk, took.
    pub references_pass: Duration,
    pub answering_the_index: Spread,
    pub answering_the_server: Spread,
    /// How many files the grammar could only parse with recovery, and how
    /// many matches were dropped for falling inside a recovered range.
    /// Printed, because a grammar gap is a coverage hole either way: the
    /// alternative to naming it is losing the file silently.
    pub files_with_recovery: usize,
    pub dropped_to_recovery: usize,
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
        writeln!(
            out,
            "read one symbol at a time: precision is {:.1}% on average and {:.1}% at the median; \
             {} of {} symbols answered at {:.0}% or better",
            self.per_symbol.mean * 100.0,
            self.per_symbol.median * 100.0,
            self.per_symbol.at_or_above_the_gate,
            self.per_symbol.judged,
            REQUIRED_PRECISION * 100.0
        )?;
        if self.per_symbol.answered_nothing > 0 {
            writeln!(
                out,
                "{} more symbols the index answered nothing about, left out of those two \
                 figures: a silence is not a wrong answer, and scoring it either way would be a \
                 claim about recall",
                self.per_symbol.answered_nothing
            )?;
        }
        if self.outside_the_servers_sight > 0 {
            writeln!(
                out,
                "{} of the index's extra findings are not counted against it: the server itself \
                 resolves nothing there -- a branch its build switches off, a file it never \
                 loaded, a package outside the project -- so it was never looking at that code, \
                 while a rename still has to change it",
                self.outside_the_servers_sight
            )?;
        }
        if self.declined > 0 {
            writeln!(
                out,
                "{} sampled symbols were declined: {} because the name is declared more than \
                 once, {} because it is also a local binding or a generic parameter somewhere, \
                 {} because a crate or a package answers to the same name",
                self.declined,
                self.declined_shared_declaration,
                self.declined_local_binding,
                self.declined_crate_name
            )?;
            writeln!(
                out,
                "{} of those were declared several times inside one crate or package, which \
                 nothing but a type could separate",
                self.declined_within_one_crate
            )?;
        }
        if self.declaration_the_server_cannot_see > 0 {
            writeln!(
                out,
                "{} sampled symbols were not asked about at all: the server resolves nothing \
                 at their own declaration, so it knows of no such symbol -- a branch its build \
                 switches off, or a file it never loaded",
                self.declaration_the_server_cannot_see
            )?;
        }
        if self.probes_the_server_failed > 0 {
            writeln!(
                out,
                "{} probes the server answered with an error rather than yes or no; each was \
                 read as a no, which is the question the probe asks -- but a large number here \
                 means the figures above rest on the server failing, not on it disagreeing",
                self.probes_the_server_failed
            )?;
        }
        if self.outside_the_servers_project > 0 {
            writeln!(
                out,
                "{} sampled symbols were in files the language server does not have -- outside the \
                 project it loaded -- and are counted on neither side",
                self.outside_the_servers_project
            )?;
        }
        if self.files_with_recovery > 0 {
            writeln!(
                out,
                "the grammar needed recovery in {} files and {} matches were dropped for falling \
                 inside a recovered range; the rest of each file still counts",
                self.files_with_recovery, self.dropped_to_recovery
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
             {:20}the server:    median {:>10} 95th {:>10} slowest {:>10}",
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
    language: &str,
    symbol_count: usize,
    indexing_timeout: Duration,
    query_timeout: Duration,
    certainty: Certainty,
) -> Result<Report> {
    let rust = language_named(language)?;
    let references_query = tree_sitter::Query::new(&rust.grammar, query_text(language)?)
        .with_context(|| {
            format!("the references query does not compile against the {language} grammar")
        })?;

    let definitions_started = Instant::now();
    let defined = defined_symbols_pass(root, language, &rust);
    let definitions_pass = definitions_started.elapsed();
    anyhow::ensure!(
        !defined.is_empty(),
        "no definitions were found under {} to sample symbols from",
        root.display()
    );

    let references_started = Instant::now();
    let scan = scan_references(root, language, &rust, &references_query, defined);
    let references_pass = references_started.elapsed();

    let sampled = sample_symbols(&scan.defined, symbol_count);
    anyhow::ensure!(
        !sampled.is_empty(),
        "there is nothing to compare over zero symbols"
    );

    // Lazily: finding every reference needs whole-graph inference, and
    // priming that ahead does not fit in the machine this runs on -- the
    // server is killed before it answers anything at all.
    let mut server = Server::start(root, language, Priming::Lazily)
        .await
        .context("starting the language server")?;
    server
        .wait_until_indexed(indexing_timeout)
        .await
        .context("waiting for the language server to finish indexing")?;

    let mut answers = Vec::with_capacity(sampled.len());
    let mut index_timings = Vec::with_capacity(sampled.len());
    let mut server_timings = Vec::with_capacity(sampled.len());
    let mut outside_the_servers_project = 0usize;
    let mut declined = 0usize;
    // Split, because the two rules cost very different amounts of coverage and
    // only one of them is as coarse as it looks: a name declared twice is
    // genuinely ambiguous everywhere, while a name that is also somebody's
    // local variable is ambiguous only where that local is in scope.
    let mut declined_shared_declaration = 0usize;
    let mut declined_local_binding = 0usize;
    let mut declined_crate_name = 0usize;
    let mut declined_within_one_crate = 0usize;
    let mut declaration_the_server_cannot_see = 0usize;
    let mut probes_the_server_failed = 0usize;
    let mut crate_names = names_of_crates(root);
    crate_names.extend(per_language::names_of_packages(language, root, &rust));
    // How many definitions in the project carry each name, so a name that means
    // one thing can be told from one that means several.
    let mut declarations_named: HashMap<&str, usize> = HashMap::new();
    for symbol in &scan.defined {
        *declarations_named.entry(symbol.name.as_str()).or_default() += 1;
    }
    for symbol in &sampled {
        // A symbol the server does not know is a question neither side can be
        // held to: `clear_globals` carries `#[cfg(any(test, feature =
        // "test-support"))]`, so the server knows of no such function and
        // answers nothing, while the index answers with every real call in
        // the project. The server is asked rather than the `#[cfg]` text
        // read: a gate that reads shut in the source can be open here --
        // `crates/gpui_linux` is gated on Linux, and this runs on Linux.
        // Told the file is open first, the way an editor would: a server
        // that answers only about what something has opened -- `tsserver`
        // does -- otherwise reports every file as one it does not have.
        if let Err(error) = server.open(&symbol.path, language, query_timeout).await {
            log::warn!("{error:#}");
        }
        let known = server
            .resolves_at(&symbol.path, symbol.row, symbol.column, query_timeout)
            .await;
        match known {
            Ok(true) => {}
            Ok(false) => {
                declaration_the_server_cannot_see += 1;
                continue;
            }
            Err(error) => {
                let refused = error
                    .downcast_ref::<ServerRefused>()
                    .is_some_and(ServerRefused::is_a_file_the_server_does_not_have);
                if refused {
                    outside_the_servers_project += 1;
                    continue;
                }
                // The probe asks one thing -- does the server resolve this
                // name here -- and an error is that question answered no.
                // Counted and printed rather than swallowed: a run where the
                // server failed on half the probes has to read as suspect,
                // not as clean.
                log::warn!(
                    "the language server could not say whether it resolves {} at {}:{}:{}: \
                     {error:#}",
                    symbol.name,
                    symbol.path,
                    symbol.row,
                    symbol.column
                );
                probes_the_server_failed += 1;
                declaration_the_server_cannot_see += 1;
                continue;
            }
        }
        if certainty == Certainty::OnlyWhenTheNameMeansOneThing {
            let shared = declarations_named
                .get(symbol.name.as_str())
                .copied()
                .unwrap_or(0)
                > 1;
            if shared {
                // Where the several declarations live decides whether anything
                // short of type inference could tell them apart. Declarations
                // spread across crates are narrowed below by what each file
                // imports; several in one crate under one name are, as a rule,
                // the same method on different types, and only a type separates
                // those.
                let crates: HashSet<&str> = scan
                    .defined
                    .iter()
                    .filter(|other| other.name == symbol.name)
                    .map(|other| crate_of(&other.path))
                    .collect();
                declined_shared_declaration += 1;
                if crates.len() <= 1 {
                    declined_within_one_crate += 1;
                }
                declined += 1;
                continue;
            }
            if scan.local_bindings.contains(&symbol.name) {
                declined_local_binding += 1;
                declined += 1;
                continue;
            }
            if crate_names.contains(&symbol.name) {
                declined_crate_name += 1;
                declined += 1;
                continue;
            }
        }
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
                        "asking the language server for references to {} at {}:{}:{}",
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
        "every one of the {} sampled symbols was in a file the language server does not have; \
         there is nothing to compare",
        sampled.len()
    );

    let comparison = compare(&answers);
    let catalogue = ErrorCatalogue::build(&scan, &comparison);

    // Whether the server sees anything at each extra finding's own position
    // is asked of the server, while it is still running. Everything the
    // classification walk worked out from `#[cfg]` text stays -- it is what
    // names the reason in the catalogue -- but what counts against precision
    // is decided here, so no wrong answer can be excused by a gate this
    // measurement only thought was shut.
    let mut seen_by_the_server: HashMap<Identity, bool> = HashMap::new();
    for extra in &catalogue.extra {
        if seen_by_the_server.contains_key(&extra.identity) {
            continue;
        }
        if let Err(error) = server
            .open(&extra.identity.path, language, query_timeout)
            .await
        {
            log::warn!("{error:#}");
        }
        let columns = columns_of(root, &extra.identity);
        let row = extra.identity.line.saturating_sub(1);
        let mut answers = Vec::new();
        for column in &columns {
            answers.push(
                server
                    .resolves_at(&extra.identity.path, row, *column, query_timeout)
                    .await,
            );
        }
        if columns.is_empty() {
            // The name is not on the line the index reported, which should not
            // happen; asking whether the file is loaded at all is the honest
            // fallback, rather than deciding it without asking.
            answers.push(server.has_read(&extra.identity.path, query_timeout).await);
        }
        let mut seen = false;
        for answered in answers {
            match answered {
                Ok(true) => seen = true,
                Ok(false) => {}
                Err(error) => {
                    let refused = error
                        .downcast_ref::<ServerRefused>()
                        .is_some_and(ServerRefused::is_a_file_the_server_does_not_have);
                    if !refused {
                        log::warn!(
                            "the language server could not say what it resolves at {}:{}: \
                             {error:#}",
                            extra.identity.path,
                            extra.identity.line
                        );
                        probes_the_server_failed += 1;
                    }
                }
            }
        }
        seen_by_the_server.insert(extra.identity.clone(), seen);
    }

    if let Err(error) = server.shut_down().await {
        log::warn!("the language server did not shut down cleanly: {error:#}");
    }

    let never_looking = |extra: &ClassifiedDivergence| {
        was_the_server_never_looking(root, language, extra)
            || !seen_by_the_server
                .get(&extra.identity)
                .copied()
                .unwrap_or(true)
    };
    let outside_the_servers_sight = catalogue
        .extra
        .iter()
        .filter(|extra| never_looking(extra))
        .count();
    let precision = precision_over(&comparison, outside_the_servers_sight);
    let per_symbol = precision_per_symbol(&comparison, &catalogue, &never_looking);

    Ok(Report {
        symbols_sampled: answers.len(),
        outside_the_servers_project,
        declaration_the_server_cannot_see,
        probes_the_server_failed,
        outside_the_servers_sight,
        declined,
        declined_shared_declaration,
        declined_local_binding,
        declined_crate_name,
        declined_within_one_crate,
        comparison,
        precision,
        per_symbol,
        catalogue,
        definitions_pass,
        references_pass,
        answering_the_index: spread_of(index_timings),
        answering_the_server: spread_of(server_timings),
        files_with_recovery: scan.files_with_recovery,
        dropped_to_recovery: scan.dropped_to_recovery,
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
        let rust = language_named("rust").expect("Rust is one of the languages the editor ships");
        tree_sitter::Query::new(
            &rust.grammar,
            query_text("rust").expect("Rust has a references query"),
        )
        .expect("the references query compiles")
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
        scan_of_language(project, "rust")
    }

    fn scan_of_language(project: &std::path::Path, name: &str) -> ProjectScan {
        let language =
            language_named(name).expect("the language is one the editor ships an outline for");
        let references_query = tree_sitter::Query::new(
            &language.grammar,
            query_text(name).expect("the language has a references query"),
        )
        .expect("the references query compiles");
        let defined = defined_symbols_pass(project, name, &language);
        scan_references(project, name, &language, &references_query, defined)
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

        let mut field_lines: Vec<u32> = scan
            .index
            .references_to("value")
            .iter()
            .map(|found| found.line)
            .collect();
        field_lines.sort_unstable();
        assert_eq!(
            field_lines,
            vec![6, 11],
            "the struct expression's own key and the field read -- both are places a \
             rename has to change; never the field's declaration on line 2"
        );

        let macros = scan.index.references_to("println");
        assert_eq!(macros.len(), 1, "{macros:?}");
        assert_eq!(macros[0].line, 12);
    }

    /// A macro's arguments are an opaque token tree to the grammar as far as
    /// *structure* goes -- no pattern can say "the field of this expression"
    /// inside one. But the tokens themselves are still nodes, so the widest
    /// pattern in the query, a bare identifier, does reach them. A name read
    /// only inside a macro call is therefore found, which it was not before
    /// that pattern existed.
    ///
    /// What is still lost is the shape: the index knows the name occurs, not
    /// that it occurs as a field of that particular value. That is what the
    /// `OnlyInsideAMacroInvocation` bucket of the error catalogue now measures.
    #[test]
    fn a_name_read_only_inside_a_macro_call_is_found_by_the_widest_pattern() {
        let project = project_with(&[(
            "one.rs",
            "struct Holder {\n    value: u32,\n}\n\nfn use_it(holder: Holder) {\n    println!(\"{}\", holder.value);\n}\n",
        )]);
        let scan = scan_of(project.path());

        let inside_the_macro = scan.index.references_to("value");
        assert_eq!(
            inside_the_macro.len(),
            1,
            "the read inside the macro call is reached as a bare name: {inside_the_macro:?}"
        );
        assert_eq!(inside_the_macro[0].line, 6);
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
            vec![2, 3, 7, 8],
            "two unrelated locals, and both their bindings, come back under the one name"
        );
        // Which is exactly why a name used as a local binding is one the index
        // declines to answer about at all: everything it could say here would
        // be a guess about which `helper` was meant.
        assert!(scan.local_bindings.contains("helper"));
    }

    /// The column a language server is asked about is found by looking the
    /// name up in the line again, and two things about that are easy to get
    /// wrong: a name inside a longer identifier is not the name, and the
    /// protocol counts a character as a UTF-16 code unit, not as a scalar.
    #[test]
    fn every_whole_occurrence_of_a_name_on_its_line_is_offered() {
        let project = project_with(&[(
            "one.rs",
            "let holder = \"\u{1f980}\u{1f525}\"; let unholder = holder + holder_of;\n",
        )]);
        let identity = Identity {
            path: "one.rs".to_string(),
            line: 1,
            name: "holder".to_string(),
        };
        let columns = columns_of(project.path(), &identity);

        // `unholder` and `holder_of` contain the name but are not it; the two
        // real occurrences are the binding and the use. Both crab and fire
        // are astral-plane, so each counts as two code units, which is what
        // puts the second column at 36 rather than 34.
        assert_eq!(columns, vec![4, 36], "columns were {columns:?}");
    }

    /// A field named in a struct expression is a reference to the field, and
    /// the place a rename most obviously has to change. The grammar calls it a
    /// `field_identifier` inside a `field_initializer`, not the plain
    /// `identifier` the widest pattern catches, so it needed its own pattern:
    /// eight of eighteen misses in one run were exactly this.
    #[test]
    fn a_field_named_in_a_struct_expression_is_a_reference_to_it() {
        let project = project_with(&[(
            "one.rs",
            "struct Holder {\n    value: u32,\n    other: u32,\n}\n\n             fn make(other: u32) -> Holder {\n                 Holder {\n        value: 42,\n        other,\n    }\n}\n",
        )]);
        let scan = scan_of(project.path());

        let named = scan.index.references_to("value");
        assert_eq!(
            named.iter().map(|found| found.line).collect::<Vec<_>>(),
            vec![8],
            "the initialiser names the field; found {named:?}"
        );
        // The shorthand form is a plain identifier, which the widest pattern
        // already catches -- the parameter it reads and the field it fills
        // are one word, and neither side can tell them apart anyway.
        let shorthand = scan.index.references_to("other");
        assert!(
            shorthand.iter().any(|found| found.line == 9),
            "found {shorthand:?}"
        );
    }

    /// The four reference kinds in Go: a call, a selected field, a type use
    /// and a plain name. Written as its own test rather than shared with the
    /// Rust one, because what the grammar calls each of them is different and
    /// a shared test would only prove that both languages have identifiers.
    #[test]
    fn each_reference_kind_in_go_is_found_with_the_right_line() {
        let project = project_with(&[(
            "one.go",
            "package holding\n\n\
             type Holder struct {\n\tValue int\n}\n\n\
             func MakeHolder() Holder {\n\treturn Holder{Value: 42}\n}\n\n\
             func UseIt() int {\n\th := MakeHolder()\n\treturn h.Value\n}\n",
        )]);
        let scan = scan_of_language(project.path(), "go");

        let calls = scan.index.references_to("MakeHolder");
        assert_eq!(
            calls.iter().map(|found| found.line).collect::<Vec<_>>(),
            vec![12],
            "the call site, not the declaration on line 7; found {calls:?}"
        );

        let mut type_lines: Vec<u32> = scan
            .index
            .references_to("Holder")
            .iter()
            .map(|found| found.line)
            .collect();
        type_lines.sort_unstable();
        assert_eq!(
            type_lines,
            vec![7, 8],
            "the result type and the composite literal's own type -- never the \
             declaration on line 3"
        );

        let mut field_lines: Vec<u32> = scan
            .index
            .references_to("Value")
            .iter()
            .map(|found| found.line)
            .collect();
        field_lines.sort_unstable();
        assert_eq!(
            field_lines,
            vec![8, 13],
            "the composite literal's key and the selection -- never the field's \
             declaration on line 4"
        );
    }

    /// What Go binds in a scope. Every one of these is a name the index
    /// declines to answer about, for the same reason a Rust local is: its
    /// meaning is the scope it is read in, and the index resolves no scopes.
    #[test]
    fn go_binds_names_with_short_declarations_parameters_and_ranges() {
        let project = photographed_go_project();
        let scan = scan_of_language(project.path(), "go");

        for bound in [
            "shortly",
            "declared",
            "parameter",
            "result",
            "receiver",
            "element",
            "key",
            "value",
            "Element",
            "aliased",
            // `switch switched := thing.(type)` and `case received := <-channel:`
            "switched",
            "received",
            "channel",
            "thing",
        ] {
            assert!(
                scan.local_bindings.contains(bound),
                "{bound} is bound in a scope; bindings were {:?}",
                scan.local_bindings
            );
        }
        // The near miss: the type a parameter is declared with, and the
        // function's own name, are not bindings.
        for named in ["Holder", "Work"] {
            assert!(
                !scan.local_bindings.contains(named),
                "{named} is not a name bound in a scope; bindings were {:?}",
                scan.local_bindings
            );
        }
    }

    fn photographed_go_project() -> tempfile::TempDir {
        project_with(&[(
            "one.go",
            "package holding\n\nimport aliased \"encoding/json\"\n\ntype Holder struct {\n\tValue int\n}\n\nfunc Work[Element any](parameter Holder) (result int) {\n\tshortly := 1\n\tvar declared int\n\tfor key, value := range map[int]int{} {\n\t\t_ = key + value\n\t}\n\tfor _, element := range []int{} {\n\t\t_ = element\n\t}\n\tvar thing any\n\tswitch switched := thing.(type) {\n\tcase int:\n\t\t_ = switched\n\t}\n\tchannel := make(chan int)\n\tselect {\n\tcase received := <-channel:\n\t\t_ = received\n\t}\n\t_ = aliased.Marshal\n\treturn shortly + declared\n}\n\nfunc (receiver Holder) Read() int {\n\treturn receiver.Value\n}\n",
        )])
    }

    /// A Go file named for another target is not part of this build, and the
    /// server never loads it. The file's own name is the constraint -- the go
    /// tool reads `_GOOS.go`, `_GOARCH.go` and `_GOOS_GOARCH.go` that way --
    /// so the reason can be named without opening the file.
    #[test]
    fn a_go_file_named_for_another_target_is_out_of_the_build() {
        let elsewhere = if std::env::consts::OS == "windows" {
            "linux"
        } else {
            "windows"
        };
        for out in [
            format!("internal/mmap_{elsewhere}.go"),
            format!("internal/mmap_{elsewhere}_amd64.go"),
            "internal/testdata/broken.go".to_string(),
        ] {
            assert!(
                per_language::out_of_the_servers_build("go", &out),
                "{out} should be out of the build"
            );
        }
        for inside in [
            "internal/mmap.go",
            "internal/mmap_test.go",
            "internal/parser_helper.go",
        ] {
            assert!(
                !per_language::out_of_the_servers_build("go", inside),
                "{inside} is part of this build"
            );
        }
        // And nothing here applies to a language that has no such rule.
        assert!(!per_language::out_of_the_servers_build(
            "rust",
            "crates/gpui_macos/src/window.rs"
        ));
    }

    /// The names Go packages answer to, which a symbol of the same name
    /// cannot be told from. Read from the imports, because that is where the
    /// identifier a file writes for a package actually appears.
    #[test]
    fn go_package_names_are_read_from_the_imports_and_the_module() {
        let project = project_with(&[
            ("go.mod", "module example.com/holding\n\ngo 1.24\n"),
            (
                "one.go",
                "// Package holding prints things (like package \"go/format\").\npackage holding\n\nimport (\n\t\"fmt\"\n\t\"encoding/json\"\n)\n\nfunc Work() {\n\tfmt.Println(json.Marshal, \"net/http\")\n}\n",
            ),
        ]);
        let go = language_named("go").expect("Go is one of the languages the editor ships");
        let names = per_language::names_of_packages("go", project.path(), &go);

        for named in ["holding", "fmt", "json"] {
            assert!(names.contains(named), "{named} is missing from {names:?}");
        }
        // The full path is not what code writes, so it is not a name here.
        assert!(!names.contains("encoding/json"));
        // The near miss that silenced a real symbol: a quoted path inside a
        // package comment is not an import, and neither is a string in a
        // function body. `x/tools` opens `imports/forward.go` with exactly
        // the first shape, and reading it as an import declined the real
        // `format` symbol for a reason that was not true.
        for invented in ["format", "http"] {
            assert!(
                !names.contains(invented),
                "{invented} is quoted text, not an import; got {names:?}"
            );
        }
    }

    /// A name selected through a package from outside the project is that
    /// package's, whatever it is called here. `x/tools` declares one
    /// `type Slice`, and `*types.Slice` -- `go/types`'s own -- occurs 98
    /// times; answering about the project's `Slice` with all of them took a
    /// pooled precision figure from 99.7 to 85.5 per cent on one symbol.
    ///
    /// The near miss that makes the rule safe: a package *inside* the project
    /// is not foreign, and a name selected through it is a real reference a
    /// rename has to change.
    #[test]
    fn a_name_selected_through_a_foreign_package_is_not_a_reference_here() {
        let project = project_with(&[
            ("go.mod", "module example.com/holding\n\ngo 1.24\n"),
            (
                "one.go",
                "package holding\n\nimport (\n\t\"go/types\"\n\t\"example.com/holding/inner\"\n)\n\ntype Slice struct{}\n\nfunc Work(outside *types.Slice, inside *inner.Slice, own *Slice) {\n\t_, _, _ = outside, inside, own\n}\n",
            ),
            ("inner/inner.go", "package inner\n\ntype Slice struct{}\n"),
        ]);
        let scan = scan_of_language(project.path(), "go");

        let mut lines: Vec<u32> = scan
            .index
            .references_to("Slice")
            .iter()
            .map(|found| found.line)
            .collect();
        lines.sort_unstable();
        // Line 10 holds `inner.Slice` and the bare `*Slice`; `types.Slice` on
        // the same line is `go/types`'s and is not one of them. The two
        // declarations, on line 8 here and line 3 of the inner package, are
        // dropped as declarations.
        assert_eq!(
            lines,
            vec![10, 10],
            "found {:?}",
            scan.index.references_to("Slice")
        );
    }

    /// The reference kinds Python has. There is only one node kind for a
    /// name in this grammar, so the point of the test is not that the
    /// patterns narrow anything down -- they do not -- but that a
    /// declaration is still dropped and a call, an attribute and a keyword
    /// argument are all found.
    #[test]
    fn each_reference_kind_in_python_is_found_with_the_right_line() {
        let project = project_with(&[(
            "one.py",
            "class Holder:\n    def __init__(self, value):\n        self.value = value\n\n    def read(self):\n        return self.value\n\n\ndef make_holder(value):\n    return Holder(value=value)\n\n\ndef use_it():\n    holder = make_holder(1)\n    return holder.read()\n",
        )]);
        let scan = scan_of_language(project.path(), "python");

        let calls = scan.index.references_to("make_holder");
        assert_eq!(
            calls.iter().map(|found| found.line).collect::<Vec<_>>(),
            vec![14],
            "the call site, not the declaration on line 9; found {calls:?}"
        );

        let mut value_lines: Vec<u32> = scan
            .index
            .references_to("value")
            .iter()
            .map(|found| found.line)
            .collect();
        value_lines.sort_unstable();
        // Lines 2 and 9 declare a parameter called `value`, line 3 assigns
        // the attribute and reads the parameter, line 6 reads the attribute,
        // line 10 is the keyword argument's name and the value passed to it.
        // A parameter's own name is among them because the outline query
        // does not call a parameter a declaration -- and it is right not to:
        // renaming the parameter has to change that position too. Every one
        // of these is a place a rename of one of the two names has to look,
        // which is exactly why the index declines to answer about a name
        // that means two things, as this one does.
        assert_eq!(value_lines, vec![2, 3, 3, 6, 9, 10, 10], "{value_lines:?}");

        let attributes = scan.index.references_to("read");
        assert_eq!(
            attributes
                .iter()
                .map(|found| found.line)
                .collect::<Vec<_>>(),
            vec![15],
            "the attribute read, not the method's declaration on line 5"
        );
    }

    /// What Python binds in a scope. The list is long because the grammar
    /// spells a parameter five ways and a binding six, and every form missed
    /// here shows up directly as a wrong answer: with one node kind for all
    /// names, the declining rules carry the whole weight in this language.
    #[test]
    fn python_binds_names_with_assignments_parameters_and_aliases() {
        let project = project_with(&[(
            "one.py",
            "import numpy as aliased\nfrom typing import Any as Anything\n\n\ndef work(parameter, defaulted=1, annotated: int = 2, *splatted, **keyworded):\n    assigned = 1\n    first, second = (2, 3)\n    for looped in range(3):\n        _ = looped\n    comprehended = [inner for inner in range(3)]\n    with open(\"x\") as handle:\n        _ = handle\n    try:\n        pass\n    except ValueError as failure:\n        _ = failure\n    if (walrus := parameter) is not None:\n        _ = walrus\n    lambdaed = lambda shadowed: shadowed\n    return (assigned, first, second, comprehended, lambdaed, aliased, Anything,\n            defaulted, annotated, splatted, keyworded)\n",
        )]);
        let scan = scan_of_language(project.path(), "python");

        for bound in [
            "parameter",
            "defaulted",
            "annotated",
            "splatted",
            "keyworded",
            "assigned",
            "first",
            "second",
            "looped",
            "comprehended",
            "inner",
            "handle",
            "failure",
            "walrus",
            "lambdaed",
            "shadowed",
            "aliased",
            "Anything",
        ] {
            assert!(
                scan.local_bindings.contains(bound),
                "{bound} is bound in a scope; bindings were {:?}",
                scan.local_bindings
            );
        }
        // The near miss: what a binding is bound *to* is not itself one.
        for named in ["work", "range", "open", "ValueError"] {
            assert!(
                !scan.local_bindings.contains(named),
                "{named} is not a name bound in a scope; bindings were {:?}",
                scan.local_bindings
            );
        }
    }

    /// A gate is only called shut where that can be shown. The target keys
    /// this project gates on are decided; a feature, `test` and anything else
    /// is unknown, and an unknown gate is never called shut -- calling one
    /// shut is what excuses a wrong answer.
    #[test]
    fn a_cfg_predicate_is_only_called_shut_where_that_can_be_shown() {
        let here = std::env::consts::OS;
        let nowhere = "nothing_is_built_for_this";
        assert_eq!(cfg_holds(&format!("target_os = \"{here}\"")), Some(true));
        assert_eq!(
            cfg_holds(&format!("target_os = \"{nowhere}\"")),
            Some(false)
        );
        assert_eq!(
            cfg_holds(&format!(
                "any(target_os = \"{nowhere}\", target_os = \"{here}\")"
            )),
            Some(true)
        );
        assert_eq!(
            cfg_holds(&format!(
                "all(target_os = \"{here}\", target_os = \"{nowhere}\")"
            )),
            Some(false)
        );
        assert_eq!(
            cfg_holds(&format!("not(target_os = \"{here}\")")),
            Some(false)
        );
        // A nested combinator keeps its own commas.
        assert_eq!(
            cfg_holds(&format!(
                "all(not(target_os = \"{nowhere}\"), any(target_os = \"{here}\"))"
            )),
            Some(true)
        );
        for unknown in [
            "feature = \"test-support\"",
            "test",
            "debug_assertions",
            "any(feature = \"a\", feature = \"b\")",
        ] {
            assert_eq!(cfg_holds(unknown), None, "{unknown} should read as unknown");
        }
        // An unknown branch leaves `any` unknown, but a true one settles it
        // regardless -- and a false branch settles `all` the same way.
        assert_eq!(
            cfg_holds(&format!("any(feature = \"a\", target_os = \"{here}\")")),
            Some(true)
        );
        assert_eq!(
            cfg_holds(&format!("all(feature = \"a\", target_os = \"{nowhere}\")")),
            Some(false)
        );
    }

    /// A struct pattern's field shorthand introduces a local under the
    /// field's own name, which is why a field name used that way anywhere in
    /// the project is one the index declines to answer about. Before this was
    /// unpacked, the local went unrecorded and every use of the local counted
    /// against precision as a reference to the field.
    #[test]
    fn a_destructuring_pattern_binds_every_name_it_unpacks() {
        let project = project_with(&[(
            "one.rs",
            "enum Track {\n    Playing { publication: u32 },\n    None,\n}\n\n             fn look(track: &Track) -> u32 {\n                 if let Track::Playing { publication } = track {\n                     return *publication;\n    }\n    0\n}\n\n             fn pair() {\n    let (left, right) = (1, 2);\n    let _ = left + right;\n}\n",
        )]);
        let scan = scan_of(project.path());

        assert!(
            scan.local_bindings.contains("publication"),
            "a field shorthand binds a local under the field's name; bindings were {:?}",
            scan.local_bindings
        );
        assert!(scan.local_bindings.contains("left"));
        assert!(scan.local_bindings.contains("right"));
        // The near miss that makes unpacking safe: a pattern's path is matched
        // against, not bound. Reading `Track` or `Playing` as a binding would
        // decline every symbol sharing a name with a matched-on variant.
        assert!(
            !scan.local_bindings.contains("Track"),
            "the type a pattern matches against is not a binding"
        );
        assert!(!scan.local_bindings.contains("Playing"));
    }

    /// A `#[cfg(...)]` on the `mod` line takes the module's whole file out of
    /// the server's sight -- rust-analyzer never reads it, while the grammar
    /// reads every file it finds. Nothing inside that file can be held
    /// against the index, and no span inside the file can say so: the gate is
    /// written one level up, in the file that declares the module.
    #[test]
    fn a_cfg_on_a_mod_line_takes_the_whole_module_file_out_of_the_servers_sight() {
        let project = project_with(&[
            (
                "lib.rs",
                "#[cfg(target_os = \"nothing_is_built_for_this\")]\npub mod optional;\npub mod always;\n",
            ),
            ("optional/inner.rs", "pub fn deep() -> u32 {\n    1\n}\n"),
            (
                "optional.rs",
                "pub mod inner;\n\npub fn only_on_that_target() -> u32 {\n    2\n}\n",
            ),
            ("always.rs", "pub fn plain() -> u32 {\n    3\n}\n"),
        ]);
        let scan = scan_of(project.path());

        assert!(
            scan.files_out_of_the_servers_sight.contains("optional.rs"),
            "out of sight: {:?}",
            scan.files_out_of_the_servers_sight
        );
        // The gate is transitive through the module's own directory: a module
        // nested inside a switched-off one is switched off too.
        assert!(
            scan.files_out_of_the_servers_sight
                .contains("optional/inner.rs"),
            "out of sight: {:?}",
            scan.files_out_of_the_servers_sight
        );
        assert!(
            !scan.files_out_of_the_servers_sight.contains("always.rs"),
            "an ungated module stays in the server's sight"
        );

        let identity = Identity {
            path: "optional.rs".to_string(),
            line: 3,
            name: "only_on_that_target".to_string(),
        };
        assert_eq!(
            classify_extra(&scan, "only_on_that_target", &identity),
            DivergenceReason::InAModuleGatedByACfg
        );
    }

    /// The grammar cannot read `Box<dyn \'static + Send + ...>` -- a lifetime
    /// bound written first in a trait object -- and this project writes it
    /// that way in fifteen files, among them the largest and most-referenced
    /// ones it has. Dropping such a file whole cost every reference in it;
    /// only the recovered range is dropped now.
    #[test]
    fn a_file_the_grammar_cannot_parse_keeps_everything_outside_the_error() {
        let project = project_with(&[(
            "one.rs",
            "type Queued = Box<dyn \'static + Send + FnOnce()>;\n\n             fn make_holder() -> u32 {\n    7\n}\n\n             fn use_it() -> u32 {\n    make_holder()\n}\n",
        )]);
        let scan = scan_of(project.path());

        assert_eq!(
            scan.files_with_recovery, 1,
            "the fixture is only meaningful while the grammar still cannot read it"
        );
        let calls = scan.index.references_to("make_holder");
        assert_eq!(
            calls.len(),
            1,
            "the call below the unparsable line is still a reference; found {calls:?}"
        );
        assert_eq!(calls[0].line, 8);
    }

    /// `crates/gpui_macos` opens with `#![cfg(target_os = "macos")]`: on any
    /// other platform rust-analyzer reads none of its files, while the
    /// grammar reads all of them. A gate on a crate's root covers the crate.
    #[test]
    fn a_crate_root_behind_an_inner_cfg_takes_the_whole_crate_out_of_sight() {
        let open_here = format!(
            "#![cfg(any(target_os = \"{}\", target_os = \"nothing_is_built_for_this\"))]\npub fn here() -> u32 {{\n    3\n}}\n",
            std::env::consts::OS
        );
        let project = project_with(&[
            (
                "crates/only_there/src/only_there.rs",
                "#![cfg(target_os = \"nothing_is_built_for_this\")]\n\npub mod window;\n",
            ),
            (
                "crates/only_there/src/window.rs",
                "pub fn draw() -> u32 {\n    1\n}\n",
            ),
            (
                "crates/everywhere/src/everywhere.rs",
                "pub fn draw_too() -> u32 {\n    2\n}\n",
            ),
            ("crates/right_here/src/right_here.rs", open_here.as_str()),
        ]);
        let scan = scan_of(project.path());

        for covered in [
            "crates/only_there/src/only_there.rs",
            "crates/only_there/src/window.rs",
        ] {
            assert!(
                scan.files_out_of_the_servers_sight.contains(covered),
                "{covered} is not covered; out of sight: {:?}",
                scan.files_out_of_the_servers_sight
            );
        }
        assert!(
            !scan
                .files_out_of_the_servers_sight
                .contains("crates/everywhere/src/everywhere.rs"),
            "an ungated crate stays in the server's sight"
        );
        // The near miss that would have cost a whole crate: `crates/gpui_linux`
        // opens with `#![cfg(any(target_os = "linux", target_os = "freebsd"))]`,
        // and on the machine this measurement runs on that gate is open.
        // Reading every `#[cfg]` as shut would have set the crate aside from
        // the comparison and flattered the number by the size of it.
        assert!(
            !scan
                .files_out_of_the_servers_sight
                .contains("crates/right_here/src/right_here.rs"),
            "a gate that holds here is open; out of sight: {:?}",
            scan.files_out_of_the_servers_sight
        );
    }

    /// A cargo target with `required-features` is built only when they are
    /// on, and rust-analyzer resolves the workspace with the default set --
    /// so it never reads the target\'s files. `crates/collab`\'s integration
    /// tests are the real case.
    #[test]
    fn a_target_behind_required_features_is_out_of_the_servers_sight() {
        let project = project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/served\",\n]\n",
            ),
            (
                "crates/served/Cargo.toml",
                "[package]\nname = \"served\"\n\n[lib]\npath = \"src/served.rs\"\n\n[[test]]\nname = \"integration\"\nrequired-features = [\"test-support\"]\npath = \"tests/integration/integration.rs\"\n\n[[test]]\nname = \"plain\"\npath = \"tests/plain.rs\"\n\n[[bin]]\nname = \"helper\"\npath = \"src/helper.rs\"\nrequired-features = [\"test-support\"]\n",
            ),
            ("crates/served/src/served.rs", "pub fn work() {}\n"),
            ("crates/served/src/helper.rs", "pub fn helping() {}\n"),
            (
                "crates/served/tests/integration/integration.rs",
                "mod helpers;\n",
            ),
            (
                "crates/served/tests/integration/helpers.rs",
                "pub fn help() {}\n",
            ),
            ("crates/served/tests/plain.rs", "pub fn plain() {}\n"),
        ]);
        let scan = scan_of(project.path());

        for covered in [
            "crates/served/tests/integration/integration.rs",
            "crates/served/tests/integration/helpers.rs",
        ] {
            assert!(
                scan.files_out_of_the_servers_sight.contains(covered),
                "{covered} is not covered; out of sight: {:?}",
                scan.files_out_of_the_servers_sight
            );
        }
        // The near miss that cost the most: a `[[bin]]` rooted at
        // `src/helper.rs` shares `src` with the crate's library, so its
        // directory is not its own and must not be taken with it.
        assert!(
            !scan
                .files_out_of_the_servers_sight
                .contains("crates/served/src/served.rs"),
            "a bin rooted in src must not set aside the crate; out of sight: {:?}",
            scan.files_out_of_the_servers_sight
        );
        assert!(
            scan.files_out_of_the_servers_sight
                .contains("crates/served/src/helper.rs"),
            "the gated bin's own file is still out of sight"
        );
        // The near miss: a target with no `required-features` is built with
        // the default set, so the server does read it.
        assert!(
            !scan
                .files_out_of_the_servers_sight
                .contains("crates/served/tests/plain.rs"),
            "out of sight: {:?}",
            scan.files_out_of_the_servers_sight
        );
    }

    /// A dependency written as a multi-line inline table puts bare strings
    /// on their own lines. Read as keys they invented crate names -- `bmp`,
    /// `File` -- and each invented name then silenced a real symbol for a
    /// reason that was not true.
    #[test]
    fn a_multi_line_dependency_table_does_not_invent_crate_names() {
        let project = project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/inner\",\n]\n\n[package]\nname = \"outer\"\n\n[dependencies]\nanyhow = \"1\"\nimage = { version = \"0.25\", default-features = false, features = [\n    \"bmp\",\n    \"gif\",\n] }\nserde = \"1\"\n",
            ),
            (
                "crates/inner/Cargo.toml",
                "[package]\nname = \"inner\"\n\n[dependencies]\nlog = \"0.4\"\n",
            ),
            (
                "crates/outside/Cargo.toml",
                "[package]\nname = \"outside\"\n",
            ),
        ]);
        let names = names_of_crates(project.path());

        for real in ["outer", "anyhow", "image", "serde", "inner", "log"] {
            assert!(names.contains(real), "{real} is missing from {names:?}");
        }
        for invented in ["bmp", "gif"] {
            assert!(
                !names.contains(invented),
                "{invented} is a feature name, not a crate; got {names:?}"
            );
        }
        // A package the workspace does not list is its own workspace as far as
        // the server is concerned, and its name is not a name in this project.
        assert!(
            !names.contains("outside"),
            "a package outside the workspace is not one of its names; got {names:?}"
        );
    }

    /// `impl<V: Bound> Trait for V` puts `V` in the outline as though it were
    /// a type, and every `V` in the project answers to it. One such symbol in
    /// a sample of 181 produced 299 of 343 wrong findings -- 87 per cent of
    /// them -- until a generic parameter was read for what it is: a name
    /// whose meaning is its scope\'s, exactly like a local.
    #[test]
    fn a_generic_parameter_is_a_name_the_index_declines_to_answer_about() {
        let project = project_with(&[(
            "one.rs",
            "pub type Holder<K, V> = std::collections::HashMap<K, V>;\n\n             pub fn take<Element>(seen: Element) -> Element {\n    seen\n}\n\n             pub const N: usize = 1;\n",
        )]);
        let scan = scan_of(project.path());

        for parameter in ["K", "V", "Element"] {
            assert!(
                scan.local_bindings.contains(parameter),
                "{parameter} is a generic parameter; bindings were {:?}",
                scan.local_bindings
            );
        }
        assert!(
            !scan.local_bindings.contains("Holder"),
            "the name a generic parameter list belongs to is not itself one"
        );
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
        assert_eq!(precision_over(&comparison, 0), 0.5);
    }

    #[test]
    fn precision_when_everything_the_index_found_was_wrong_is_zero() {
        let comparison = compare(&[QueryAnswers {
            query: "work".to_string(),
            the_server_found: Vec::new(),
            the_index_found: vec![definition("a.rs", "work", 1)],
        }]);
        assert_eq!(precision_over(&comparison, 0), 0.0);
    }

    #[test]
    fn precision_when_the_index_found_nothing_at_all_is_a_vacuous_one() {
        let comparison = compare(&[QueryAnswers {
            query: "work".to_string(),
            the_server_found: vec![definition("a.rs", "work", 1)],
            the_index_found: Vec::new(),
        }]);
        assert_eq!(
            precision_over(&comparison, 0),
            1.0,
            "finding nothing is not the same as finding something wrongly"
        );
    }

    /// Findings the server was never looking at are not wrong answers, and
    /// leaving them in would measure the server's build configuration rather
    /// than the index. The count is printed with the number for that reason.
    #[test]
    fn what_the_server_could_not_see_is_not_held_against_the_index() {
        let comparison = compare(&[QueryAnswers {
            query: "work".to_string(),
            the_server_found: vec![definition("a.rs", "work", 4)],
            the_index_found: vec![
                definition("a.rs", "work", 4),
                definition("a.rs", "work", 9),
                definition("a.rs", "work", 14),
            ],
        }]);
        assert_eq!(
            precision_over(&comparison, 0),
            1.0 / 3.0,
            "counted against the index, two of its three answers are wrong"
        );
        assert_eq!(
            precision_over(&comparison, 2),
            1.0,
            "set aside as code the server never compiled, none of them are"
        );
        assert_eq!(
            precision_over(&comparison, 9),
            1.0,
            "setting aside more than there were does not go below zero wrong"
        );
    }

    #[test]
    fn precision_of_the_empty_case_is_also_a_vacuous_one() {
        assert_eq!(precision_over(&compare(&[]), 0), 1.0);
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
            files_out_of_the_servers_sight: HashSet::new(),
            files_with_recovery: 0,
            dropped_to_recovery: 0,
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
            files_out_of_the_servers_sight: HashSet::new(),
            files_with_recovery: 0,
            dropped_to_recovery: 0,
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
        let rust = language_named("rust").expect("Rust is one of the languages the editor ships");
        let defined = defined_symbols_pass(project.path(), "rust", &rust);
        assert!(
            defined.is_empty(),
            "a file that does not parse defines nothing: {defined:?}"
        );

        let scan = scan_references(project.path(), "rust", &rust, &query(), defined);
        assert!(scan.index.references_to("Half").is_empty());
        assert!(scan.index.references_to("work").is_empty());
    }
}
