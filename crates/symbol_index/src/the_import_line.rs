use std::collections::HashSet;
use std::path::Path;

use semantic_index::modules;

use crate::Definition;

/// A line to insert into a buffer, and where it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Insertion {
    /// A byte offset in the buffer, always at the start of a line.
    pub(crate) at: usize,
    /// What goes there, each line ending in a newline of its own.
    pub(crate) text: String,
}

/// The import line a name needs to resolve where it is being written, or
/// nothing where that cannot be worked out with confidence.
///
/// Nothing is both the common answer and the safe one. A name whose import
/// this cannot derive is left as the bare name the index has always offered,
/// which is a file the reader knows how to finish. A wrong import is a file
/// that fails to compile for a second reason the reader now has to tell apart
/// from the first.
///
/// `buffer_text` is the buffer as it stands *after* the name was inserted,
/// because that is when this is asked: a name the reader has just written into
/// a `use` line, or into a declaration of their own, is already in scope by the
/// time this looks.
pub(crate) fn needed_for(
    root: &Path,
    declared: &Definition,
    written_in: &str,
    buffer_text: &str,
) -> Option<Insertion> {
    if declared.path == written_in {
        return None;
    }
    let declaring_line = line_of(root, &declared.path, declared.line)?;
    // The index records where a declaration was when the file was last read.
    // A line that no longer holds the name means the file has moved on, and
    // every judgement below is about a line that is no longer the declaration.
    if !declaring_line.contains(&declared.name) {
        return None;
    }
    // A declaration that is not at the left margin is inside something else --
    // a method of a type, a function of a class -- and no import brings it in.
    if declaring_line.starts_with([' ', '\t']) {
        return None;
    }
    match (suffix_of(&declared.path), suffix_of(written_in)) {
        (Some("rs"), Some("rs")) => rust(root, declared, written_in, buffer_text, &declaring_line),
        (Some("py"), Some("py")) => python(root, declared, written_in, buffer_text),
        _ => None,
    }
}

fn rust(
    root: &Path,
    declared: &Definition,
    written_in: &str,
    buffer_text: &str,
    declaring_line: &str,
) -> Option<Insertion> {
    let grammar = grammar("rust")?;
    let name = declared.name.as_str();
    let there = placed(root, &declared.path)?;
    let here = placed(root, written_in)?;

    let path = if there.crate_name == here.crate_name {
        if there.module == here.module {
            return None;
        }
        if !visible_within_its_crate(declaring_line) {
            return None;
        }
        let mut segments = vec!["crate".to_string()];
        segments.extend(there.module.iter().cloned());
        segments.push(name.to_string());
        segments.join("::")
    } else {
        // Adding a dependency is not something accepting a completion may do,
        // so a crate this one does not already depend on is left alone.
        if !visible_outside_its_crate(declaring_line)
            || !depends_on(&here.manifest, &there.crate_name)
        {
            return None;
        }
        // Only what the crate root itself offers. The module a name is
        // declared in is private more often than not in a workspace this
        // shape, and a path through a private module does not compile -- so
        // the derived path is only trusted where the root re-exports the name.
        if !there.module.is_empty() {
            let lib = std::fs::read(root.join(&there.lib_root)).ok()?;
            let offered = modules::re_exports_of(&lib, &grammar)?;
            if !offered.contains_key(name) {
                return None;
            }
        }
        format!("{}::{name}", there.crate_name)
    };

    let (brought_in, globs) = modules::imports_of(buffer_text.as_bytes(), &grammar)?;
    if brought_in.contains_key(name) {
        return None;
    }
    let (module_path, _) = path.rsplit_once("::")?;
    if globs.iter().any(|glob| glob.as_str() == module_path) {
        return None;
    }
    if declared_at_the_top_of(buffer_text, &grammar, "rust").contains(name) {
        return None;
    }

    let existing = existing_rust_uses(buffer_text, &grammar);
    Some(placed_among(
        buffer_text,
        &existing,
        &path,
        format!("use {path};\n"),
        first_line_of_rust_code(buffer_text),
    ))
}

fn python(
    root: &Path,
    declared: &Definition,
    written_in: &str,
    buffer_text: &str,
) -> Option<Insertion> {
    let grammar = grammar("python")?;
    let name = declared.name.as_str();
    let there = python_module(root, &declared.path)?;
    let here = python_module(root, written_in)?;
    if there == here {
        return None;
    }

    let brought_in = python_imports(buffer_text, &grammar)?;
    if brought_in.names.contains(name) {
        return None;
    }
    if brought_in.wildcards.iter().any(|module| module == &there) {
        return None;
    }
    if declared_at_the_top_of(buffer_text, &grammar, "python").contains(name) {
        return None;
    }

    Some(placed_among(
        buffer_text,
        &brought_in.existing,
        &there,
        format!("from {there} import {name}\n"),
        first_line_of_python_code(buffer_text, &grammar),
    ))
}

/// Where one Rust file sits: which crate declares it and which module its own
/// top level is, worked out from where the file is rather than from the `mod`
/// declarations that reach it.
struct Placed {
    crate_name: String,
    /// The module path below the crate root, root itself being empty.
    module: Vec<String>,
    manifest: String,
    /// The crate's library root, relative to the project root.
    lib_root: String,
}

/// The names a target other than the library claims, whose files are not
/// modules of it however their paths read.
const NOT_MODULES_OF_THE_LIBRARY: [&str; 5] = ["bin", "tests", "benches", "examples", "main"];

fn placed(root: &Path, path: &str) -> Option<Placed> {
    let (directory, manifest, crate_name) = crate_of(root, path)?;
    let lib_root = joined(
        &directory,
        &modules::library_path(&manifest).unwrap_or_else(|| "src/lib.rs".to_string()),
    );
    // The manifest may name a library root the crate does not have, and a
    // crate with no library at all has no module tree for a `use` to name.
    if !root.join(&lib_root).is_file() {
        return None;
    }
    let module = match path == lib_root {
        true => Vec::new(),
        false => module_below(&lib_root, path)?,
    };
    Some(Placed {
        crate_name,
        module,
        manifest,
        lib_root,
    })
}

fn module_below(lib_root: &str, path: &str) -> Option<Vec<String>> {
    let base = lib_root
        .rsplit_once('/')
        .map(|(directory, _)| directory)
        .unwrap_or("");
    let below = match base.is_empty() {
        true => path,
        false => path.strip_prefix(base)?.strip_prefix('/')?,
    };
    let mut segments: Vec<String> = below.split('/').map(str::to_string).collect();
    let file = segments.pop()?;
    let stem = file.strip_suffix(".rs")?;
    if stem != "mod" {
        segments.push(stem.to_string());
    }
    if segments.is_empty()
        || segments
            .iter()
            .any(|segment| NOT_MODULES_OF_THE_LIBRARY.contains(&segment.as_str()))
        || !segments.iter().all(|segment| is_a_name(segment))
    {
        return None;
    }
    Some(segments)
}

/// The crate a file belongs to: the nearest manifest above it that names a
/// package, its directory, and its text.
fn crate_of(root: &Path, path: &str) -> Option<(String, String, String)> {
    let mut directory = path
        .rsplit_once('/')
        .map(|(head, _)| head.to_string())
        .unwrap_or_default();
    loop {
        if let Ok(manifest) = std::fs::read_to_string(root.join(&directory).join("Cargo.toml"))
            && let Some(name) = modules::package_name(&manifest)
        {
            return Some((directory, manifest, name));
        }
        match directory.rsplit_once('/') {
            Some((above, _)) => directory = above.to_string(),
            None if directory.is_empty() => return None,
            None => directory = String::new(),
        }
    }
}

/// Whether a manifest already lists a crate among what it depends on.
///
/// `[dependencies]` and nothing else. A development dependency is in scope for
/// a test target rather than for the library, and this only ever writes into a
/// file of the library; a target-conditional table is in scope for some builds
/// and not others, which is not something an import may assume.
fn depends_on(manifest: &str, crate_name: &str) -> bool {
    let mut inside_the_dependencies = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside_the_dependencies = line.starts_with("[dependencies]");
            continue;
        }
        if !inside_the_dependencies {
            continue;
        }
        let named = line
            .split(['=', '.'])
            .next()
            .unwrap_or_default()
            .trim()
            .trim_matches('"');
        if !named.is_empty() && named.replace('-', "_") == crate_name {
            return true;
        }
    }
    false
}

fn visible_within_its_crate(declaring_line: &str) -> bool {
    visible_outside_its_crate(declaring_line)
        || declaring_line.starts_with("pub(crate)")
        || declaring_line.starts_with("pub(in crate")
}

fn visible_outside_its_crate(declaring_line: &str) -> bool {
    declaring_line.starts_with("pub ")
}

/// The dotted module a Python file is, honouring the `__init__.py` files that
/// say where the package starts.
fn python_module(root: &Path, path: &str) -> Option<String> {
    let file = path.rsplit('/').next()?;
    let stem = file.strip_suffix(".py")?;
    let mut segments = Vec::new();
    let mut directory = path
        .rsplit_once('/')
        .map(|(head, _)| head.to_string())
        .unwrap_or_default();
    while !directory.is_empty() && root.join(&directory).join("__init__.py").is_file() {
        let (above, last) = match directory.rsplit_once('/') {
            Some((above, last)) => (above.to_string(), last.to_string()),
            None => (String::new(), directory.clone()),
        };
        segments.push(last);
        directory = above;
    }
    segments.reverse();
    if stem != "__init__" {
        segments.push(stem.to_string());
    }
    if segments.is_empty() || !segments.iter().all(|segment| is_a_name(segment)) {
        return None;
    }
    Some(segments.join("."))
}

/// One import already written in a buffer: where it is, and the module it
/// names, which is what decides where a new one sorts among them.
struct Existing {
    start: usize,
    end: usize,
    key: String,
}

/// The module `from __future__ import ...` names, which has to stay the first
/// import in a file whatever else is inserted.
const ALWAYS_THE_FIRST_IMPORT: &str = "__future__";

fn existing_rust_uses(text: &str, grammar: &tree_sitter::Language) -> Vec<Existing> {
    let Some(tree) = parsed(text, grammar) else {
        return Vec::new();
    };
    let root = tree.root_node();
    let mut walking = root.walk();
    let mut found = Vec::new();
    for child in root.named_children(&mut walking) {
        if child.kind() != "use_declaration" {
            continue;
        }
        let key = child
            .child_by_field_name("argument")
            .and_then(|argument| argument.utf8_text(text.as_bytes()).ok())
            .unwrap_or_default()
            .to_string();
        found.push(Existing {
            start: child.start_byte(),
            end: child.end_byte(),
            key,
        });
    }
    found
}

#[derive(Default)]
struct PythonImports {
    /// Every local name the file's imports bind.
    names: HashSet<String>,
    /// The modules its `import *` lines cover.
    wildcards: Vec<String>,
    existing: Vec<Existing>,
}

fn python_imports(text: &str, grammar: &tree_sitter::Language) -> Option<PythonImports> {
    let tree = parsed(text, grammar)?;
    let root = tree.root_node();
    let source = text.as_bytes();
    let mut found = PythonImports::default();
    let mut walking = root.walk();
    for child in root.named_children(&mut walking) {
        let module = match child.kind() {
            "import_from_statement" => {
                let module = child
                    .child_by_field_name("module_name")
                    .and_then(|node| node.utf8_text(source).ok())
                    .unwrap_or_default()
                    .to_string();
                let mut naming = child.walk();
                for named in child.children_by_field_name("name", &mut naming) {
                    if let Some(bound) = local_python_name(named, source) {
                        found.names.insert(bound);
                    }
                }
                let mut inner = child.walk();
                if child
                    .named_children(&mut inner)
                    .any(|node| node.kind() == "wildcard_import")
                {
                    found.wildcards.push(module.clone());
                }
                module
            }
            "import_statement" => {
                let mut naming = child.walk();
                let mut first = String::new();
                for named in child.children_by_field_name("name", &mut naming) {
                    if let Some(bound) = local_python_name(named, source) {
                        // `import a.b` binds `a`, not `a.b`.
                        let head = bound
                            .split('.')
                            .next()
                            .unwrap_or(bound.as_str())
                            .to_string();
                        found.names.insert(head);
                        if first.is_empty() {
                            first = bound;
                        }
                    }
                }
                first
            }
            _ => continue,
        };
        found.existing.push(Existing {
            start: child.start_byte(),
            end: child.end_byte(),
            key: module,
        });
    }
    Some(found)
}

fn local_python_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "aliased_import" => node
            .child_by_field_name("alias")
            .and_then(|alias| alias.utf8_text(source).ok())
            .map(str::to_string),
        _ => node.utf8_text(source).ok().map(str::to_string),
    }
}

/// The names a file declares at its own top level, which are in scope in it
/// without any import.
fn declared_at_the_top_of(
    text: &str,
    grammar: &tree_sitter::Language,
    language: &str,
) -> HashSet<String> {
    let mut declared = HashSet::new();
    let Some(tree) = parsed(text, grammar) else {
        return declared;
    };
    let root = tree.root_node();
    let source = text.as_bytes();
    let mut walking = root.walk();
    for child in root.named_children(&mut walking) {
        // A Python definition carrying decorators is wrapped in a node of its
        // own, with the definition itself inside it.
        let child = match language == "python" && child.kind() == "decorated_definition" {
            true => match child.child_by_field_name("definition") {
                Some(inside) => inside,
                None => child,
            },
            false => child,
        };
        if let Some(named) = child
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
        {
            declared.insert(named.to_string());
        }
        if language == "python" && child.kind() == "expression_statement" {
            let mut inner = child.walk();
            for statement in child.named_children(&mut inner) {
                if statement.kind() != "assignment" {
                    continue;
                }
                if let Some(left) = statement
                    .child_by_field_name("left")
                    .filter(|left| left.kind() == "identifier")
                    .and_then(|left| left.utf8_text(source).ok())
                {
                    declared.insert(left.to_string());
                }
            }
        }
    }
    declared
}

/// Where a new import goes among the ones a file already has.
///
/// Next to them, and in their order where they have one. A file whose imports
/// are in order stays in order; a file whose imports are in some order of its
/// own is appended to rather than rearranged, because reordering somebody
/// else's imports is a change they did not ask for.
fn placed_among(
    text: &str,
    existing: &[Existing],
    key: &str,
    line: String,
    where_the_code_starts: usize,
) -> Insertion {
    let sortable: Vec<&Existing> = existing
        .iter()
        .filter(|one| one.key != ALWAYS_THE_FIRST_IMPORT)
        .collect();
    let Some(last) = existing.last() else {
        // No imports to sit beside, so the line goes above the first thing the
        // file does, with a blank line between it and that.
        let at = where_the_code_starts;
        let text = match line_at(text, at).trim().is_empty() {
            true => line,
            false => format!("{line}\n"),
        };
        return Insertion { at, text };
    };
    let in_order = sortable.windows(2).all(|pair| pair[0].key <= pair[1].key);
    if in_order && let Some(after) = sortable.iter().find(|one| one.key.as_str() > key) {
        return Insertion {
            at: line_start_of(text, after.start),
            text: line,
        };
    }
    Insertion {
        at: line_end_after(text, last.end),
        text: line,
    }
}

/// The first line of a Rust file that is neither blank, a comment, nor an
/// attribute on the file itself.
fn first_line_of_rust_code(text: &str) -> usize {
    let mut at = 0;
    while at < text.len() {
        let line = line_at(text, at);
        let trimmed = line.trim_start();
        if !(trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("#!")) {
            return at;
        }
        at = line_end_after(text, at);
    }
    text.len()
}

/// The same for a Python file, where a leading string is the module's own
/// documentation and belongs above any import.
fn first_line_of_python_code(text: &str, grammar: &tree_sitter::Language) -> usize {
    let mut at = 0;
    if let Some(tree) = parsed(text, grammar) {
        let root = tree.root_node();
        let mut walking = root.walk();
        if let Some(first) = root.named_children(&mut walking).next()
            && first.kind() == "expression_statement"
        {
            let mut inner = first.walk();
            if first
                .named_children(&mut inner)
                .any(|node| node.kind() == "string")
            {
                at = line_end_after(text, first.end_byte());
            }
        }
    }
    while at < text.len() {
        let line = line_at(text, at);
        let trimmed = line.trim_start();
        if !(trimmed.is_empty() || trimmed.starts_with('#')) {
            return at;
        }
        at = line_end_after(text, at);
    }
    at
}

fn parsed(text: &str, grammar: &tree_sitter::Language) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(grammar).ok()?;
    parser.parse(text, None)
}

fn grammar(name: &str) -> Option<tree_sitter::Language> {
    grammars::native_grammars()
        .into_iter()
        .find(|(known, _)| *known == name)
        .map(|(_, grammar)| grammar)
}

/// The line `line` of a file, counting from one the way the index records it.
fn line_of(root: &Path, path: &str, line: u32) -> Option<String> {
    let contents = std::fs::read_to_string(root.join(path)).ok()?;
    contents
        .lines()
        .nth(line.checked_sub(1)? as usize)
        .map(str::to_string)
}

fn suffix_of(path: &str) -> Option<&str> {
    path.rsplit('/')
        .next()?
        .rsplit_once('.')
        .map(|(_, suffix)| suffix)
}

fn joined(directory: &str, path: &str) -> String {
    match directory.is_empty() {
        true => path.to_string(),
        false => format!("{directory}/{path}"),
    }
}

fn is_a_name(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.starts_with(|first: char| first.is_ascii_digit())
        && segment
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
}

fn line_start_of(text: &str, at: usize) -> usize {
    text.get(..at)
        .and_then(|before| before.rfind('\n'))
        .map(|newline| newline + 1)
        .unwrap_or(0)
}

fn line_end_after(text: &str, at: usize) -> usize {
    match text.get(at..).and_then(|rest| rest.find('\n')) {
        Some(newline) => at + newline + 1,
        None => text.len(),
    }
}

fn line_at(text: &str, at: usize) -> &str {
    let rest = text.get(at..).unwrap_or_default();
    match rest.find('\n') {
        Some(newline) => &rest[..newline],
        None => rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let held = tempfile::tempdir().expect("a directory to put a project in");
        for (path, contents) in files {
            let at = held.path().join(path);
            if let Some(directory) = at.parent() {
                std::fs::create_dir_all(directory).expect("a directory for a project file");
            }
            std::fs::write(&at, contents).expect("a project file on disk");
        }
        held
    }

    fn declared(name: &str, path: &str, line: u32, language: &str) -> Definition {
        Definition {
            path: path.to_string(),
            name: name.to_string(),
            kind: "function_item".to_string(),
            line,
            language: language.to_string(),
        }
    }

    /// A crate whose library root is named after the crate rather than called
    /// `lib.rs`, which is what this workspace's own guidelines ask for and what
    /// the mapping has to be right about.
    fn a_workspace_of_one_crate(files: &[(&str, &str)]) -> tempfile::TempDir {
        let mut all = vec![
            (
                "crates/holding/Cargo.toml",
                "[package]\nname = \"holding\"\n\n[lib]\npath = \"src/holding.rs\"\n",
            ),
            ("crates/holding/src/holding.rs", "pub mod window;\n"),
        ];
        all.extend_from_slice(files);
        a_project(&all)
    }

    #[test]
    fn a_named_library_root_gives_the_module_below_it_the_right_path() {
        let held = a_workspace_of_one_crate(&[
            ("crates/holding/src/window.rs", "pub fn open_it() {}\n"),
            (
                "crates/holding/src/other.rs",
                "fn writes() {\n    open_it\n}\n",
            ),
        ]);
        let insertion = needed_for(
            held.path(),
            &declared("open_it", "crates/holding/src/window.rs", 1, "rust"),
            "crates/holding/src/other.rs",
            "fn writes() {\n    open_it\n}\n",
        )
        .expect("the import is derivable");
        assert_eq!(insertion.text, "use crate::window::open_it;\n\n");
        assert_eq!(insertion.at, 0);
    }

    #[test]
    fn a_declaration_that_is_not_public_is_left_alone() {
        let held = a_workspace_of_one_crate(&[
            ("crates/holding/src/window.rs", "fn open_it() {}\n"),
            ("crates/holding/src/other.rs", "fn writes() {}\n"),
        ]);
        assert_eq!(
            needed_for(
                held.path(),
                &declared("open_it", "crates/holding/src/window.rs", 1, "rust"),
                "crates/holding/src/other.rs",
                "fn writes() {}\n",
            ),
            None
        );
    }

    #[test]
    fn a_method_of_a_type_is_not_a_name_an_import_brings_in() {
        let held = a_workspace_of_one_crate(&[
            (
                "crates/holding/src/window.rs",
                "pub struct Held;\nimpl Held {\n    pub fn open_it() {}\n}\n",
            ),
            ("crates/holding/src/other.rs", "fn writes() {}\n"),
        ]);
        assert_eq!(
            needed_for(
                held.path(),
                &declared("open_it", "crates/holding/src/window.rs", 3, "rust"),
                "crates/holding/src/other.rs",
                "fn writes() {}\n",
            ),
            None
        );
    }

    #[test]
    fn a_new_use_sorts_in_among_the_ones_already_there() {
        let held = a_workspace_of_one_crate(&[
            ("crates/holding/src/window.rs", "pub fn open_it() {}\n"),
            ("crates/holding/src/other.rs", "fn writes() {}\n"),
        ]);
        let written = "use crate::alpha::One;\nuse crate::zulu::Two;\n\nfn writes() {}\n";
        let insertion = needed_for(
            held.path(),
            &declared("open_it", "crates/holding/src/window.rs", 1, "rust"),
            "crates/holding/src/other.rs",
            written,
        )
        .expect("the import is derivable");
        let mut after = written.to_string();
        after.insert_str(insertion.at, &insertion.text);
        assert_eq!(
            after,
            "use crate::alpha::One;\nuse crate::window::open_it;\nuse crate::zulu::Two;\n\nfn writes() {}\n"
        );
    }

    #[test]
    fn imports_in_an_order_of_their_own_are_appended_to_rather_than_rearranged() {
        let held = a_workspace_of_one_crate(&[
            ("crates/holding/src/window.rs", "pub fn open_it() {}\n"),
            ("crates/holding/src/other.rs", "fn writes() {}\n"),
        ]);
        let written = "use crate::zulu::Two;\nuse crate::alpha::One;\n\nfn writes() {}\n";
        let insertion = needed_for(
            held.path(),
            &declared("open_it", "crates/holding/src/window.rs", 1, "rust"),
            "crates/holding/src/other.rs",
            written,
        )
        .expect("the import is derivable");
        let mut after = written.to_string();
        after.insert_str(insertion.at, &insertion.text);
        assert_eq!(
            after,
            "use crate::zulu::Two;\nuse crate::alpha::One;\nuse crate::window::open_it;\n\nfn writes() {}\n"
        );
    }

    #[test]
    fn a_glob_over_the_module_is_already_the_import() {
        let held = a_workspace_of_one_crate(&[
            ("crates/holding/src/window.rs", "pub fn open_it() {}\n"),
            ("crates/holding/src/other.rs", "fn writes() {}\n"),
        ]);
        assert_eq!(
            needed_for(
                held.path(),
                &declared("open_it", "crates/holding/src/window.rs", 1, "rust"),
                "crates/holding/src/other.rs",
                "use crate::window::*;\n\nfn writes() {}\n",
            ),
            None
        );
    }

    #[test]
    fn a_crate_the_manifest_does_not_depend_on_is_refused() {
        let held = a_project(&[
            (
                "crates/one/Cargo.toml",
                "[package]\nname = \"one\"\n\n[lib]\npath = \"src/one.rs\"\n",
            ),
            ("crates/one/src/one.rs", "pub fn open_it() {}\n"),
            (
                "crates/two/Cargo.toml",
                "[package]\nname = \"two\"\n\n[lib]\npath = \"src/two.rs\"\n\n[dependencies]\n",
            ),
            ("crates/two/src/two.rs", "fn writes() {}\n"),
        ]);
        assert_eq!(
            needed_for(
                held.path(),
                &declared("open_it", "crates/one/src/one.rs", 1, "rust"),
                "crates/two/src/two.rs",
                "fn writes() {}\n",
            ),
            None
        );
    }

    #[test]
    fn a_crate_the_manifest_does_depend_on_is_named_by_its_root() {
        let held = a_project(&[
            (
                "crates/one/Cargo.toml",
                "[package]\nname = \"one\"\n\n[lib]\npath = \"src/one.rs\"\n",
            ),
            ("crates/one/src/one.rs", "pub fn open_it() {}\n"),
            (
                "crates/two/Cargo.toml",
                "[package]\nname = \"two\"\n\n[lib]\npath = \"src/two.rs\"\n\n[dependencies]\none.workspace = true\n",
            ),
            ("crates/two/src/two.rs", "fn writes() {}\n"),
        ]);
        let insertion = needed_for(
            held.path(),
            &declared("open_it", "crates/one/src/one.rs", 1, "rust"),
            "crates/two/src/two.rs",
            "fn writes() {}\n",
        )
        .expect("the import is derivable");
        assert_eq!(insertion.text, "use one::open_it;\n\n");
    }

    #[test]
    fn a_name_another_crate_does_not_re_export_is_refused() {
        let held = a_project(&[
            (
                "crates/one/Cargo.toml",
                "[package]\nname = \"one\"\n\n[lib]\npath = \"src/one.rs\"\n",
            ),
            ("crates/one/src/one.rs", "mod window;\n"),
            ("crates/one/src/window.rs", "pub fn open_it() {}\n"),
            (
                "crates/two/Cargo.toml",
                "[package]\nname = \"two\"\n\n[lib]\npath = \"src/two.rs\"\n\n[dependencies]\none.workspace = true\n",
            ),
            ("crates/two/src/two.rs", "fn writes() {}\n"),
        ]);
        assert_eq!(
            needed_for(
                held.path(),
                &declared("open_it", "crates/one/src/window.rs", 1, "rust"),
                "crates/two/src/two.rs",
                "fn writes() {}\n",
            ),
            None
        );
    }

    #[test]
    fn a_name_another_crate_does_re_export_is_named_by_the_root() {
        let held = a_project(&[
            (
                "crates/one/Cargo.toml",
                "[package]\nname = \"one\"\n\n[lib]\npath = \"src/one.rs\"\n",
            ),
            (
                "crates/one/src/one.rs",
                "mod window;\npub use window::open_it;\n",
            ),
            ("crates/one/src/window.rs", "pub fn open_it() {}\n"),
            (
                "crates/two/Cargo.toml",
                "[package]\nname = \"two\"\n\n[lib]\npath = \"src/two.rs\"\n\n[dependencies]\none.workspace = true\n",
            ),
            ("crates/two/src/two.rs", "fn writes() {}\n"),
        ]);
        let insertion = needed_for(
            held.path(),
            &declared("open_it", "crates/one/src/window.rs", 1, "rust"),
            "crates/two/src/two.rs",
            "fn writes() {}\n",
        )
        .expect("the import is derivable");
        assert_eq!(insertion.text, "use one::open_it;\n\n");
    }

    #[test]
    fn a_python_package_is_named_by_the_init_files_above_it() {
        let held = a_project(&[
            ("pkg/__init__.py", ""),
            ("pkg/inner/__init__.py", ""),
            ("pkg/inner/shapes.py", "def area():\n    return 1\n"),
            ("main.py", "print(1)\n"),
        ]);
        let insertion = needed_for(
            held.path(),
            &declared("area", "pkg/inner/shapes.py", 1, "python"),
            "main.py",
            "print(1)\n",
        )
        .expect("the import is derivable");
        assert_eq!(insertion.text, "from pkg.inner.shapes import area\n\n");
        assert_eq!(insertion.at, 0);
    }

    #[test]
    fn a_directory_with_no_init_file_is_where_the_package_starts() {
        let held = a_project(&[
            ("src/pkg/__init__.py", ""),
            ("src/pkg/shapes.py", "def area():\n    return 1\n"),
            ("src/main.py", "print(1)\n"),
        ]);
        let insertion = needed_for(
            held.path(),
            &declared("area", "src/pkg/shapes.py", 1, "python"),
            "src/main.py",
            "print(1)\n",
        )
        .expect("the import is derivable");
        assert_eq!(insertion.text, "from pkg.shapes import area\n\n");
    }

    #[test]
    fn a_python_import_goes_below_the_module_docstring_and_beside_the_others() {
        let held = a_project(&[
            ("pkg/__init__.py", ""),
            ("pkg/shapes.py", "def area():\n    return 1\n"),
            ("main.py", ""),
        ]);
        let written = "\"\"\"What this does.\"\"\"\n\nfrom __future__ import annotations\n\nimport os\nimport zoneinfo\n\nprint(1)\n";
        let insertion = needed_for(
            held.path(),
            &declared("area", "pkg/shapes.py", 1, "python"),
            "main.py",
            written,
        )
        .expect("the import is derivable");
        let mut after = written.to_string();
        after.insert_str(insertion.at, &insertion.text);
        assert_eq!(
            after,
            "\"\"\"What this does.\"\"\"\n\nfrom __future__ import annotations\n\nimport os\nfrom pkg.shapes import area\nimport zoneinfo\n\nprint(1)\n"
        );
    }

    #[test]
    fn a_python_name_already_imported_is_not_imported_twice() {
        let held = a_project(&[
            ("pkg/__init__.py", ""),
            ("pkg/shapes.py", "def area():\n    return 1\n"),
            ("main.py", ""),
        ]);
        assert_eq!(
            needed_for(
                held.path(),
                &declared("area", "pkg/shapes.py", 1, "python"),
                "main.py",
                "from pkg.shapes import area\n\nprint(area())\n",
            ),
            None
        );
    }

    #[test]
    fn a_language_outside_the_two_is_left_as_it_was() {
        let held = a_project(&[
            ("one.go", "func OpenIt() {}\n"),
            ("two.go", "func writes() {}\n"),
        ]);
        assert_eq!(
            needed_for(
                held.path(),
                &declared("OpenIt", "one.go", 1, "go"),
                "two.go",
                "func writes() {}\n",
            ),
            None
        );
    }

    #[test]
    fn a_stale_line_number_is_refused_rather_than_read_as_a_declaration() {
        let held = a_workspace_of_one_crate(&[
            (
                "crates/holding/src/window.rs",
                "pub fn something_else() {}\n",
            ),
            ("crates/holding/src/other.rs", "fn writes() {}\n"),
        ]);
        assert_eq!(
            needed_for(
                held.path(),
                &declared("open_it", "crates/holding/src/window.rs", 1, "rust"),
                "crates/holding/src/other.rs",
                "fn writes() {}\n",
            ),
            None
        );
    }
}
