use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;

use crate::languages::Readable;
use crate::walk;

/// Where one file sits in a project's module tree: which crate it belongs to
/// and the module path its own top level is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    /// The crate's name as code writes it, with `-` turned into `_`.
    pub crate_name: String,
    /// The module path from the crate root, `crate` included:
    /// `crate::window::prompts`. A crate root is just `crate`.
    pub module_path: String,
}

/// Which module every file in a project belongs to.
///
/// This is the half of a name resolver that costs nothing to build and
/// answers most of the question. Measured on this fork: of 350 sampled names
/// the index declines because they are declared more than once, only 86 have
/// their declarations inside one crate. The other 264 are in different
/// crates, and telling those apart needs only to know which crate and module
/// each declaration is in -- which is this -- rather than any type
/// inference.
#[derive(Debug, Default)]
pub struct ModuleTree {
    placed: HashMap<String, Placed>,
    /// Files a crate root reaches, per crate. A file no root reaches is in no
    /// crate, and a name declared in it is reachable from nowhere.
    reached: HashMap<String, Vec<String>>,
    /// What each module declares, by the name it declares it under. Keyed by
    /// crate and module path, because the same module path exists in every
    /// crate -- `crate` does.
    declares: HashMap<(String, String), HashMap<String, Vec<Declared>>>,
    /// What each file brings into scope under which local name: the name code
    /// writes, and the path it stands for. Per file and not per module,
    /// because that is the scope a `use` has.
    imports: HashMap<String, HashMap<String, String>>,
    /// The paths each file brings in wholesale with `use path::*`. Kept apart
    /// from `imports` because a glob names nothing, so answering from one is
    /// a wider claim than answering from a named import, and the caller has
    /// to ask for it -- see [`ModuleTree::what_a_name_means_through_globs`].
    globs: HashMap<String, Vec<String>>,
}

/// One declaration, where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub path: String,
    pub row: u32,
    pub column: u32,
}

impl ModuleTree {
    /// Where a file sits, or nothing where no crate root reaches it -- a
    /// fixture, a scratch file, a module whose `mod` line is behind a
    /// `#[cfg]` this build switches off.
    pub fn placed(&self, path: &str) -> Option<&Placed> {
        self.placed.get(path)
    }

    /// Every file the named crate reaches.
    pub fn files_of(&self, crate_name: &str) -> &[String] {
        self.reached
            .get(crate_name)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn crates(&self) -> impl Iterator<Item = &str> {
        self.reached.keys().map(String::as_str)
    }

    /// The declaration a bare `name`, written in `file`, refers to -- or
    /// nothing where that cannot be told from names alone.
    ///
    /// Rust looks a bare name up in the scope of the module it is written in,
    /// which is that module's own declarations plus whatever the file's `use`
    /// declarations brought in. It does *not* look in the parent module, which
    /// is why `super::` exists. So this looks in exactly those two places, in
    /// that order.
    ///
    /// `None` where the name is not one of them: a method (which needs the
    /// type of what it is called on), a name from the prelude, one a macro
    /// wrote, or one brought in by a glob import. Saying nothing there is the
    /// point -- a resolver that guesses is worse than the text matching it
    /// replaces.
    pub fn what_a_name_means(&self, file: &str, name: &str) -> Option<&Declared> {
        let placed = self.placed(file)?;
        if let Some(declared) = self
            .declares
            .get(&(placed.crate_name.clone(), placed.module_path.clone()))
            .and_then(|declared| declared.get(name))
        {
            // A declaration in this very module is the end of the answer.
            // An item shadows an import of the same name, and importing a
            // name the module already declares does not compile at all -- so
            // several declarations under one name means this cannot say
            // which, not that the imports should be tried next.
            return the_only_one(declared);
        }
        let path = self.imports.get(file)?.get(name)?;
        self.what_a_path_means(file, path)
    }

    /// The declaration a path written in `file` refers to. The path's own
    /// first segment says where to start looking, and the rest walks down.
    pub fn what_a_path_means(&self, file: &str, path: &str) -> Option<&Declared> {
        let placed = self.placed(file)?;
        let mut segments = path.split("::").filter(|segment| !segment.is_empty());
        let first = segments.next()?;
        let rest: Vec<&str> = segments.collect();
        let (last, between) = rest.split_last()?;

        let (crate_name, mut module_path) = match first {
            // `crate::a::B` -- this crate's root.
            "crate" => (placed.crate_name.clone(), "crate".to_string()),
            // `self::a::B` -- this module.
            "self" => (placed.crate_name.clone(), placed.module_path.clone()),
            // `super::a::B` -- the module above this one.
            "super" => {
                let above = placed.module_path.rsplit_once("::").map(|(up, _)| up)?;
                (placed.crate_name.clone(), above.to_string())
            }
            // Another crate in this project.
            other if self.reached.contains_key(other) => (other.to_string(), "crate".to_string()),
            // A module of this one, `window::prompts::Handle`.
            other => (
                placed.crate_name.clone(),
                format!("{}::{other}", placed.module_path),
            ),
        };
        for segment in between {
            module_path = format!("{module_path}::{segment}");
        }
        self.declares
            .get(&(crate_name, module_path))
            .and_then(|declared| declared.get(*last))
            .and_then(|declared| the_only_one(declared))
    }

    /// The declaration a bare `name` refers to, letting a glob import be what
    /// brought it in.
    ///
    /// A glob over a module *of this project* is not a guess: the tree knows
    /// what that module declares, so `use window::*` followed by `Handle`
    /// names exactly `window::Handle` if `window` declares one. A glob over a
    /// dependency is still unreadable, and a name only such a glob could have
    /// brought in still gets no answer.
    ///
    /// [`Self::what_a_name_means`] is asked first and wins, because that is
    /// Rust's own order: the module's own declarations and its named imports
    /// both beat a glob. Only a name exactly one glob offers is answered --
    /// two globs offering it is an ambiguity the compiler rejects outright,
    /// and this declines it rather than picking.
    pub fn what_a_name_means_through_globs(&self, file: &str, name: &str) -> Option<&Declared> {
        if let Some(direct) = self.what_a_name_means(file, name) {
            return Some(direct);
        }
        let mut through: Option<&Declared> = None;
        for path in self.globs.get(file)? {
            let Some(found) = self.what_a_path_means(file, &format!("{path}::{name}")) else {
                continue;
            };
            if through.is_some_and(|already| already != found) {
                return None;
            }
            through = Some(found);
        }
        through
    }
}

/// The one declaration in a list, or nothing where there are several.
///
/// Two declarations of one name in one module are the namespaces Rust keeps
/// apart -- a `struct Thing` and a `fn thing` do not collide, and neither do
/// a type and a trait of the same name. Telling those apart needs to know
/// whether the name was written in a type position or a value one, which is
/// more than this knows. So it declines, which is what the index already does
/// with such a name.
fn the_only_one(declared: &[Declared]) -> Option<&Declared> {
    match declared {
        [only] => Some(only),
        _ => None,
    }
}

/// Walks a project's `mod` declarations from every crate root, and says which
/// module each file is.
///
/// Rust's module tree is not the directory tree: a file is part of a crate
/// only if some `mod` declaration reaches it, and where it lives is decided
/// by the chain of declarations that got there rather than by its path. So
/// this follows the declarations, and a file nothing declares is left out --
/// which is the honest answer, and the same one the compiler gives.
pub fn read(root: &Path, rust: &Readable) -> Result<ModuleTree> {
    read_with(root, rust, std::iter::empty())
}

/// The same, told what the project declares, so the tree can say what a name
/// means rather than only where a file is.
///
/// The declarations are passed in rather than found here: whoever calls this
/// has already run the outline query over every file, and there is no second
/// answer to what a project declares.
pub fn read_with<'a>(
    root: &Path,
    rust: &Readable,
    declared: impl Iterator<Item = (&'a str, &'a str, u32, u32)>,
) -> Result<ModuleTree> {
    let mut tree = ModuleTree::default();
    let all_files = rust_files_under(root, rust);
    for (crate_name, root_file) in crate_roots(root) {
        if !all_files.contains(&root_file) {
            continue;
        }
        let mut reached = Vec::new();
        // A crate root's children are looked for beside it -- `src/holding.rs`
        // declares `mod window;` and the file is `src/window.rs`, not
        // `src/holding/window.rs`. So the base is the root's own directory.
        let base = root_file
            .rsplit_once('/')
            .map(|(directory, _)| directory.to_string())
            .unwrap_or_default();
        walk_from(
            root,
            rust,
            &all_files,
            &crate_name,
            "crate",
            &base,
            &root_file,
            &mut tree,
            &mut reached,
        );
        tree.reached.insert(crate_name, reached);
    }
    // Only now, when every file knows which module it is: a declaration
    // belongs to the module its file is, and a file in no module declares
    // nothing anybody can reach.
    for (path, name, row, column) in declared {
        let Some(placed) = tree.placed.get(path) else {
            continue;
        };
        tree.declares
            .entry((placed.crate_name.clone(), placed.module_path.clone()))
            .or_default()
            .entry(name.to_string())
            .or_default()
            .push(Declared {
                path: path.to_string(),
                row,
                column,
            });
    }
    Ok(tree)
}

#[allow(clippy::too_many_arguments)]
fn walk_from(
    root: &Path,
    rust: &Readable,
    all_files: &std::collections::HashSet<String>,
    crate_name: &str,
    module_path: &str,
    // `base` is the directory a child module of this one is looked for in.
    // Carried rather than derived from the file's name, because Rust decides
    // it from the *module* path: `mod inline { mod nested; }` in a crate root
    // puts `nested` at `src/inline/nested.rs`, whatever the root is called.
    base: &str,
    file: &str,
    tree: &mut ModuleTree,
    reached: &mut Vec<String>,
) {
    // A file two `mod` declarations both name is a mistake in the project,
    // not something to walk twice.
    if tree.placed.contains_key(file) {
        return;
    }
    tree.placed.insert(
        file.to_string(),
        Placed {
            crate_name: crate_name.to_string(),
            module_path: module_path.to_string(),
        },
    );
    reached.push(file.to_string());

    // A file that cannot be read leaves the tree without its `mod`
    // declarations and its imports, so names in it and below it go
    // unanswered. That loses answers rather than inventing them, which is
    // why the walk goes on -- but it must not do so silently, or a hole in
    // the tree reads as a decision.
    let contents = match std::fs::read(root.join(file)) {
        Ok(contents) => contents,
        Err(error) => {
            log::warn!("the module tree cannot read {file}, so its imports are unknown: {error}");
            return;
        }
    };
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&rust.grammar) {
        log::warn!("the module tree cannot use the Rust grammar: {error}");
        return;
    }
    let Some(tree_of_file) = parser.parse(&contents, None) else {
        log::warn!("the module tree cannot parse {file}, so its imports are unknown");
        return;
    };
    let brought_in = imports_in(tree_of_file.root_node(), &contents, "");
    if !brought_in.is_empty() {
        let (named, globs) = brought_in.settled();
        if !named.is_empty() {
            tree.imports.insert(file.to_string(), named);
        }
        if !globs.is_empty() {
            tree.globs.insert(file.to_string(), globs);
        }
    }

    for declared in modules_declared_in(tree_of_file.root_node(), &contents) {
        walk_declared(
            root,
            rust,
            all_files,
            crate_name,
            module_path,
            base,
            &declared,
            tree,
            reached,
        );
    }
}

/// What one file's `use` declarations bring into scope.
///
/// Every shape the grammar gives a `use` is read, because they are not
/// interchangeable: a list brings several names under one prefix, an `as`
/// clause renames one, and a nested list nests prefixes. A glob names
/// nothing, so it is kept separately and resolved only when asked for.
#[derive(Debug, Default)]
struct Brought {
    /// Local name to the path it stands for.
    named: HashMap<String, String>,
    /// The paths of `use path::*`, in the order written.
    globs: Vec<String>,
    /// Local names two `use` declarations in this file spell differently.
    /// Dropped rather than answered -- see [`Brought::name`].
    ambiguous: HashSet<String>,
}

impl Brought {
    /// Records that `local` stands for `path`.
    ///
    /// A name two `use` declarations in one file spell differently is not
    /// resolved to whichever came last. This tree keeps imports per file, so
    /// a `use` inside `mod inner { ... }` lands in the same map as the file's
    /// own top-level one, and the two can genuinely disagree:
    ///
    /// ```ignore
    /// use second::Shared;
    /// mod inner { use first::Shared; }
    /// ```
    ///
    /// Rust gives each of those its own scope. Keeping one would attribute an
    /// occurrence in the other scope to a declaration the compiler would not
    /// pick, which is a wrong answer and not merely a wide one.
    fn name(&mut self, local: String, path: String) {
        match self.named.get(&local) {
            Some(already) if *already != path => {
                self.ambiguous.insert(local);
            }
            _ => {
                self.named.insert(local, path);
            }
        }
    }

    fn absorb(&mut self, other: Brought) {
        for (local, path) in other.named {
            self.name(local, path);
        }
        self.globs.extend(other.globs);
        self.ambiguous.extend(other.ambiguous);
    }

    /// What this file really brings in: the named imports with every
    /// ambiguous one removed, and the globs.
    fn settled(mut self) -> (HashMap<String, String>, Vec<String>) {
        for name in &self.ambiguous {
            self.named.remove(name);
        }
        (self.named, self.globs)
    }

    fn is_empty(&self) -> bool {
        self.named.is_empty() && self.globs.is_empty()
    }
}

fn imports_in(node: tree_sitter::Node, contents: &[u8], prefix: &str) -> Brought {
    let mut brought = Brought::default();
    let mut walking = node.walk();
    for child in node.named_children(&mut walking) {
        match child.kind() {
            "use_declaration" => {
                if let Some(argument) = child.child_by_field_name("argument") {
                    brought.absorb(one_use(argument, contents, prefix));
                }
            }
            // A `use` inside a `mod name { ... }` belongs to that
            // module's scope and not this file's top level. This tree keeps
            // imports per file, so both land in one map -- useful, because
            // most files have no collision, and safe only because a name the
            // two scopes spell differently is dropped by `Brought::name`
            // rather than resolved to one of them.
            "mod_item" | "declaration_list" => {
                brought.absorb(imports_in(child, contents, prefix));
            }
            _ => {}
        }
    }
    brought
}

fn one_use(node: tree_sitter::Node, contents: &[u8], prefix: &str) -> Brought {
    let mut brought = Brought::default();
    let joined = |head: &str, tail: &str| match head.is_empty() {
        true => tail.to_string(),
        false => format!("{head}::{tail}"),
    };
    match node.kind() {
        // `use thing;` -- the local name is the path itself.
        "identifier" | "self" | "crate" | "super" => {
            if let Ok(text) = node.utf8_text(contents) {
                brought.name(text.to_string(), joined(prefix, text));
            }
        }
        // `use a::b::Thing;`
        "scoped_identifier" => {
            if let Ok(whole) = node.utf8_text(contents)
                && let Some(last) = whole.rsplit("::").next()
            {
                brought.name(last.to_string(), joined(prefix, whole));
            }
        }
        // `use a::{B, C};`
        "use_list" => {
            let mut walking = node.walk();
            for child in node.named_children(&mut walking) {
                brought.absorb(one_use(child, contents, prefix));
            }
        }
        // `use a::{b::C, d::E};` -- the prefix grows and the list nests.
        "scoped_use_list" => {
            let path = node
                .child_by_field_name("path")
                .and_then(|path| path.utf8_text(contents).ok())
                .unwrap_or_default();
            let deeper = joined(prefix, path);
            if let Some(list) = node.child_by_field_name("list") {
                brought.absorb(one_use(list, contents, &deeper));
            }
        }
        // `use a::Thing as Other;` -- the local name is the alias.
        "use_as_clause" => {
            let path = node
                .child_by_field_name("path")
                .and_then(|path| path.utf8_text(contents).ok())
                .unwrap_or_default();
            if let Some(alias) = node
                .child_by_field_name("alias")
                .and_then(|alias| alias.utf8_text(contents).ok())
            {
                brought.name(alias.to_string(), joined(prefix, path));
            }
        }
        // `use thing::*`. The grammar gives the module's path as the node's
        // only child and no field to reach it by, so it is read positionally.
        // A bare `use *;` has no child at all and brings in nothing.
        "use_wildcard" => {
            let path = node
                .named_child(0)
                .and_then(|path| path.utf8_text(contents).ok())
                .unwrap_or_default();
            if !path.is_empty() {
                brought.globs.push(joined(prefix, path));
            }
        }
        _ => {}
    }
    brought
}

#[allow(clippy::too_many_arguments)]
fn walk_declared(
    root: &Path,
    rust: &Readable,
    all_files: &std::collections::HashSet<String>,
    crate_name: &str,
    module_path: &str,
    base: &str,
    declared: &DeclaredModule,
    tree: &mut ModuleTree,
    reached: &mut Vec<String>,
) {
    let inside = format!("{module_path}::{}", declared.name);
    let below = join(base, &declared.name);
    match &declared.body {
        // `mod name { ... }` -- the module is in this same file, and anything
        // it declares in turn is looked for one directory further down.
        Some(nested) => {
            for one in nested {
                walk_declared(
                    root, rust, all_files, crate_name, &inside, &below, one, tree, reached,
                );
            }
        }
        // `mod name;` -- the module is another file, and its own children are
        // looked for in the directory named after it.
        None => {
            if let Some(at) = a_module_file(base, &declared.name, all_files) {
                walk_from(
                    root, rust, all_files, crate_name, &inside, &below, &at, tree, reached,
                );
            }
        }
    }
}

fn join(base: &str, name: &str) -> String {
    match base.is_empty() {
        true => name.to_string(),
        false => format!("{base}/{name}"),
    }
}

/// One `mod` declaration: what it is called, and whether its body is written
/// here or is another file.
struct DeclaredModule {
    name: String,
    /// What its body declares, where the body is written here. `None` for
    /// `mod name;`, whose body is another file.
    body: Option<Vec<DeclaredModule>>,
}

fn modules_declared_in(node: tree_sitter::Node, contents: &[u8]) -> Vec<DeclaredModule> {
    let mut declared = Vec::new();
    let mut walking = node.walk();
    for child in node.named_children(&mut walking) {
        if child.kind() != "mod_item" {
            // A `mod` can be nested in anything the grammar allows -- a
            // `#[cfg]`-gated block, a macro's expansion is not walked here.
            declared.extend(modules_declared_in(child, contents));
            continue;
        }
        let Some(name) = child
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(contents).ok())
        else {
            continue;
        };
        declared.push(DeclaredModule {
            name: name.to_string(),
            body: child
                .child_by_field_name("body")
                .map(|body| modules_declared_in(body, contents)),
        });
    }
    declared
}

/// The file `mod name;` names, looked for in the directory the module tree
/// says its siblings live in.
///
/// Rust allows two spellings for the same module -- `name.rs` beside its
/// siblings, or `name/mod.rs` -- so both are tried. Where both exist the
/// project has a real ambiguity the compiler would also complain about.
fn a_module_file(
    base: &str,
    name: &str,
    all_files: &std::collections::HashSet<String>,
) -> Option<String> {
    [
        join(base, &format!("{name}.rs")),
        join(base, &format!("{name}/mod.rs")),
    ]
    .into_iter()
    .find(|candidate| all_files.contains(candidate))
}

/// Every crate in the project and the file its root is, read from the
/// manifests the workspace lists as members.
///
/// The manifest is read the same crude way the rest of this crate reads one:
/// a target's own table, one key per line. What it has to get right is the
/// `[lib] path`, because this project's own guidelines ask new crates to name
/// that file after the crate rather than call it `lib.rs`.
fn crate_roots(root: &Path) -> Vec<(String, String)> {
    let mut roots = Vec::new();
    for manifest_path in crate::references::member_manifests(root) {
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
        let Some(name) = package_name(&manifest) else {
            continue;
        };
        let at = |path: &str| match directory.is_empty() {
            true => path.to_string(),
            false => format!("{directory}/{path}"),
        };
        let root_file = library_path(&manifest)
            .map(|path| at(&path))
            .unwrap_or_else(|| at("src/lib.rs"));
        roots.push((name, root_file));
    }
    roots
}

fn package_name(manifest: &str) -> Option<String> {
    let mut inside_the_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside_the_package = line.starts_with("[package]");
            continue;
        }
        if inside_the_package
            && let Some(value) = line.strip_prefix("name")
            && let Some(value) = value.split('=').nth(1)
        {
            return Some(value.trim().trim_matches('"').replace('-', "_"));
        }
    }
    None
}

fn library_path(manifest: &str) -> Option<String> {
    let mut inside_the_library = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside_the_library = line.starts_with("[lib]");
            continue;
        }
        if inside_the_library
            && let Some(value) = line.strip_prefix("path")
            && let Some(value) = value.split('=').nth(1)
        {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn rust_files_under(root: &Path, rust: &Readable) -> std::collections::HashSet<String> {
    let claimed = crate::languages::by_suffix();
    walk::files_under(root)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| crate::languages::of_file(name, &claimed))
                .is_some_and(|owner| owner == rust.name)
        })
        .filter_map(|path| {
            path.strip_prefix(root)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust() -> Readable {
        let (readable, _) = crate::languages::readable();
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

    /// The module tree follows `mod` declarations, not directories. Both
    /// layouts Rust allows are found, a `mod` with its body in the same file
    /// nests without a file of its own, and the crate root is `crate`.
    #[test]
    fn every_file_a_crate_root_reaches_knows_its_own_module_path() {
        let project = project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/holding\",\n]\n",
            ),
            (
                "crates/holding/Cargo.toml",
                "[package]\nname = \"holding\"\n\n[lib]\npath = \"src/holding.rs\"\n",
            ),
            (
                "crates/holding/src/holding.rs",
                "pub mod window;\npub mod deep;\n\npub mod inline {\n    pub mod nested;\n}\n",
            ),
            ("crates/holding/src/window.rs", "pub fn draw() {}\n"),
            ("crates/holding/src/deep/mod.rs", "pub fn under() {}\n"),
            ("crates/holding/src/inline/nested.rs", "pub fn far() {}\n"),
            ("crates/holding/src/unreached.rs", "pub fn nobody() {}\n"),
        ]);
        let tree = read(project.path(), &rust()).expect("the tree reads");

        for (file, expected) in [
            ("crates/holding/src/holding.rs", "crate"),
            ("crates/holding/src/window.rs", "crate::window"),
            ("crates/holding/src/deep/mod.rs", "crate::deep"),
            (
                "crates/holding/src/inline/nested.rs",
                "crate::inline::nested",
            ),
        ] {
            let placed = tree
                .placed(file)
                .unwrap_or_else(|| panic!("{file} is not placed"));
            assert_eq!(placed.crate_name, "holding", "{file}");
            assert_eq!(placed.module_path, expected, "{file}");
        }

        // The near miss, and the reason this follows declarations rather than
        // directories: a file beside the others that no `mod` names is in no
        // crate at all, and a name declared in it is reachable from nowhere.
        assert!(
            tree.placed("crates/holding/src/unreached.rs").is_none(),
            "a file nothing declares is in no module"
        );
    }

    /// A crate whose root is named after itself rather than `lib.rs` -- which
    /// is what this project's own guidelines ask for, and what every crate in
    /// it does.
    #[test]
    fn a_crate_root_named_after_the_crate_is_found_from_the_manifest() {
        let project = project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/named\",\n    \"crates/plain\",\n]\n",
            ),
            (
                "crates/named/Cargo.toml",
                "[package]\nname = \"named-with-dashes\"\n\n[lib]\npath = \"src/named.rs\"\n",
            ),
            ("crates/named/src/named.rs", "pub fn work() {}\n"),
            ("crates/plain/Cargo.toml", "[package]\nname = \"plain\"\n"),
            ("crates/plain/src/lib.rs", "pub fn work() {}\n"),
        ]);
        let tree = read(project.path(), &rust()).expect("the tree reads");

        // A dash in the package name is an underscore in code, and the module
        // tree is what code reads.
        assert_eq!(
            tree.placed("crates/named/src/named.rs")
                .map(|placed| placed.crate_name.as_str()),
            Some("named_with_dashes")
        );
        // And a crate that says nothing about its root has the default one.
        assert_eq!(
            tree.placed("crates/plain/src/lib.rs")
                .map(|placed| placed.module_path.as_str()),
            Some("crate")
        );
    }

    /// A project of two crates, the second importing from the first in every
    /// shape a `use` comes in.
    fn two_crates() -> tempfile::TempDir {
        project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/first\",\n    \"crates/second\",\n]\n",
            ),
            ("crates/first/Cargo.toml", "[package]\nname = \"first\"\n"),
            (
                "crates/first/src/lib.rs",
                "pub mod inner;\npub struct Shared;\npub fn work() {}\n",
            ),
            (
                "crates/first/src/inner.rs",
                "pub struct Deep;\npub struct Other;\n",
            ),
            ("crates/second/Cargo.toml", "[package]\nname = \"second\"\n"),
            (
                "crates/second/src/lib.rs",
                "pub mod nearby;\n\n                 use first::Shared;\n                 use first::inner::{Deep, Other as Renamed};\n                 use first::{work, inner::Deep as AlsoDeep};\n                 use crate::nearby::Neighbour;\n                 pub struct Shared;\n",
            ),
            ("crates/second/src/nearby.rs", "pub struct Neighbour;\n"),
        ])
    }

    fn tree_of(project: &tempfile::TempDir) -> ModuleTree {
        let rust = rust();
        let mut parser = tree_sitter::Parser::new();
        let mut declared = Vec::new();
        for path in super::rust_files_under(project.path(), &rust) {
            let Ok(contents) = std::fs::read(project.path().join(&path)) else {
                continue;
            };
            let found = crate::definitions::in_file(&path, &contents, &rust, &mut parser)
                .expect("the outline query runs");
            for one in found {
                declared.push((path.clone(), one.name, one.line.saturating_sub(1), 0u32));
            }
        }
        let borrowed: Vec<(&str, &str, u32, u32)> = declared
            .iter()
            .map(|(path, name, row, column)| (path.as_str(), name.as_str(), *row, *column))
            .collect();
        read_with(project.path(), &rust, borrowed.into_iter()).expect("the tree reads")
    }

    /// A name written in a file resolves to the one declaration it means, and
    /// to that one only -- the crate it was imported from decides, not the
    /// text of the name.
    ///
    /// `Shared` is the case that matters: both crates declare one, the
    /// importing file declares its own, and the index declines such a name
    /// today for exactly that reason. Its own module's declaration wins,
    /// which is what Rust does.
    #[test]
    fn a_name_resolves_to_the_declaration_its_own_module_and_imports_name() {
        let project = two_crates();
        let tree = tree_of(&project);
        let here = "crates/second/src/lib.rs";

        let shared = tree
            .what_a_name_means(here, "Shared")
            .expect("`Shared` resolves");
        assert_eq!(
            shared.path, "crates/second/src/lib.rs",
            "a module's own declaration wins over what it imported"
        );

        // Imported names resolve into the other crate: a plain import, a
        // list, and a list nested under a prefix.
        for (name, expected) in [
            ("Deep", "crates/first/src/inner.rs"),
            ("Renamed", "crates/first/src/inner.rs"),
            ("AlsoDeep", "crates/first/src/inner.rs"),
            ("work", "crates/first/src/lib.rs"),
            ("Neighbour", "crates/second/src/nearby.rs"),
        ] {
            let found = tree
                .what_a_name_means(here, name)
                .unwrap_or_else(|| panic!("`{name}` does not resolve"));
            assert_eq!(found.path, expected, "`{name}`");
        }

        // The near miss: `Other` is the name in the *other* crate, and this
        // file imported it under a different one. Nothing here is called
        // `Other`, and answering with the renamed declaration would rename
        // the wrong word.
        assert!(
            tree.what_a_name_means(here, "Other").is_none(),
            "a name that was renamed on import is not in scope under its old one"
        );
    }

    /// A path resolves by its first segment: this crate, this module, the one
    /// above, or another crate entirely.
    #[test]
    fn a_path_resolves_from_whatever_its_first_segment_names() {
        let project = two_crates();
        let tree = tree_of(&project);
        let here = "crates/second/src/lib.rs";

        for (path, expected) in [
            ("first::Shared", "crates/first/src/lib.rs"),
            ("first::inner::Deep", "crates/first/src/inner.rs"),
            ("crate::nearby::Neighbour", "crates/second/src/nearby.rs"),
            ("self::nearby::Neighbour", "crates/second/src/nearby.rs"),
            ("nearby::Neighbour", "crates/second/src/nearby.rs"),
        ] {
            let found = tree
                .what_a_path_means(here, path)
                .unwrap_or_else(|| panic!("`{path}` does not resolve"));
            assert_eq!(found.path, expected, "`{path}`");
        }

        // `super::` from a module one level down reaches the crate root.
        let below = "crates/second/src/nearby.rs";
        assert_eq!(
            tree.what_a_path_means(below, "super::Shared")
                .map(|found| found.path.as_str()),
            Some("crates/second/src/lib.rs")
        );

        // And a path naming something nobody declared resolves to nothing,
        // rather than to whatever is nearest.
        assert!(tree.what_a_path_means(here, "first::Absent").is_none());
        assert!(tree.what_a_path_means(here, "nowhere::Thing").is_none());
    }

    /// A glob import is deliberately not read. `use thing::*` brings in names
    /// only the other module knows, and mapping one of them to a guess would
    /// be worse than the text matching this replaces.
    #[test]
    fn a_glob_import_brings_in_nothing_rather_than_a_guess() {
        let project = project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/only\",\n]\n",
            ),
            ("crates/only/Cargo.toml", "[package]\nname = \"only\"\n"),
            (
                "crates/only/src/lib.rs",
                "pub mod inner;\nuse crate::inner::*;\n",
            ),
            ("crates/only/src/inner.rs", "pub struct Hidden;\n"),
        ]);
        let tree = tree_of(&project);
        assert!(
            tree.what_a_name_means("crates/only/src/lib.rs", "Hidden")
                .is_none(),
            "a name a glob brought in is not one this can resolve"
        );
        // But the path it came from still resolves, which is what a reader
        // writing it out would get.
        assert_eq!(
            tree.what_a_path_means("crates/only/src/lib.rs", "crate::inner::Hidden")
                .map(|found| found.path.as_str()),
            Some("crates/only/src/inner.rs")
        );
        // And the caller that asks for globs to be read gets the same
        // declaration, from the same file, without any widening of the other
        // method.
        assert_eq!(
            tree.what_a_name_means_through_globs("crates/only/src/lib.rs", "Hidden")
                .map(|found| found.path.as_str()),
            Some("crates/only/src/inner.rs")
        );
    }

    /// A glob over a module of this project is not a guess -- the tree knows
    /// what that module declares. Everything that could make it one is
    /// checked here: what beats it, what makes it ambiguous, and what it
    /// cannot see.
    #[test]
    fn a_glob_is_read_only_where_reading_it_is_not_a_guess() {
        let project = project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/only\",\n]\n",
            ),
            ("crates/only/Cargo.toml", "[package]\nname = \"only\"\n"),
            (
                "crates/only/src/lib.rs",
                "pub mod left;\npub mod right;\npub mod named;\n\
                 use crate::left::*;\n\
                 use crate::right::*;\n\
                 use crate::named::Picked;\n\
                 use nowhere::*;\n\
                 pub struct Own;\n",
            ),
            (
                "crates/only/src/left.rs",
                "pub struct OnlyLeft;\npub struct Both;\npub struct Own;\npub struct Picked;\n",
            ),
            (
                "crates/only/src/right.rs",
                "pub struct OnlyRight;\npub struct Both;\n",
            ),
            ("crates/only/src/named.rs", "pub struct Picked;\n"),
        ]);
        let tree = tree_of(&project);
        let here = "crates/only/src/lib.rs";
        let through = |name: &str| {
            tree.what_a_name_means_through_globs(here, name)
                .map(|found| found.path.as_str())
        };

        assert_eq!(
            through("OnlyLeft"),
            Some("crates/only/src/left.rs"),
            "a name exactly one glob offers is the one it means"
        );
        assert_eq!(through("OnlyRight"), Some("crates/only/src/right.rs"));
        assert_eq!(
            through("Both"),
            None,
            "two globs offering one name is what the compiler calls ambiguous"
        );
        assert_eq!(
            through("Own"),
            Some(here),
            "the module's own declaration beats a glob, as it does in Rust"
        );
        assert_eq!(
            through("Picked"),
            Some("crates/only/src/named.rs"),
            "a named import beats a glob, as it does in Rust"
        );
        assert_eq!(
            through("Serialize"),
            None,
            "a glob over a crate outside this project stays unreadable"
        );
    }

    /// The number this exists for. Two crates each declaring `work`: the
    /// index declines such a name today because text cannot tell the two
    /// apart, and the module tree tells them apart without any knowledge of
    /// types at all.
    /// Cargo lets a package be named with dashes; code always writes it with
    /// underscores. A tree that kept the manifest's spelling would answer
    /// nothing for every `use` of such a crate.
    #[test]
    fn a_dashed_package_is_known_under_the_name_code_writes() {
        let project = project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/two-words\",\n]\n",
            ),
            (
                "crates/two-words/Cargo.toml",
                "[package]\nname = \"two-words\"\n",
            ),
            ("crates/two-words/src/lib.rs", "pub struct Thing;\n"),
        ]);
        let tree = tree_of(&project);
        let placed = tree
            .placed("crates/two-words/src/lib.rs")
            .expect("the crate root is placed");
        assert_eq!(placed.crate_name, "two_words");
        assert_eq!(
            placed.module_path, "crate",
            "a crate root is the crate itself, not a module inside it"
        );
    }

    /// Rust keeps types and values in separate namespaces, so one module can
    /// declare `Thing` twice without a collision. Choosing between them needs
    /// to know which namespace the name was written in, which this layer does
    /// not, so it declines -- and declining has to be proven, because
    /// answering either one would be a wrong rename half the time.
    #[test]
    fn a_name_a_module_declares_twice_is_declined_rather_than_guessed() {
        let project = project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/both\",\n]\n",
            ),
            ("crates/both/Cargo.toml", "[package]\nname = \"both\"\n"),
            (
                "crates/both/src/lib.rs",
                // Deliberately not valid Rust: importing a name the module
                // already declares does not compile. The resolver reads text
                // off disk, including a file in the middle of an edit, so it
                // has to be right about this shape rather than assume it away
                // -- and answering from the import here would name a
                // declaration the compiler would never pick.
                "pub mod elsewhere;\nuse crate::elsewhere::Thing;\npub struct Thing;\npub fn Thing() {}\npub struct Alone;\n",
            ),
            ("crates/both/src/elsewhere.rs", "pub struct Thing;\n"),
        ]);
        let tree = tree_of(&project);
        let here = "crates/both/src/lib.rs";
        assert!(
            tree.what_a_name_means(here, "Thing").is_none(),
            "a name this module declares twice is declined, and never answered \
             from an import instead"
        );
        assert!(
            tree.what_a_name_means(here, "Alone").is_some(),
            "the declined name must not take its neighbours down with it"
        );
    }

    /// This tree keeps imports per file, so a `use` inside
    /// `mod inner { ... }` lands in the same map as the file's own. Where the
    /// two spell one name differently, Rust gives each its own scope and this
    /// cannot tell which scope an occurrence is in -- so the name is dropped.
    /// Answering would attribute half the occurrences to a declaration the
    /// compiler would not pick.
    #[test]
    fn a_name_two_scopes_in_one_file_spell_differently_is_dropped() {
        let project = project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/first\",\n    \"crates/second\",\n    \"crates/user\",\n]\n",
            ),
            ("crates/first/Cargo.toml", "[package]\nname = \"first\"\n"),
            ("crates/first/src/lib.rs", "pub struct Shared;\n"),
            ("crates/second/Cargo.toml", "[package]\nname = \"second\"\n"),
            (
                "crates/second/src/lib.rs",
                "pub struct Shared;\npub struct Undisputed;\n",
            ),
            ("crates/user/Cargo.toml", "[package]\nname = \"user\"\n"),
            (
                "crates/user/src/lib.rs",
                "use second::Shared;\nuse second::Undisputed;\nmod inner {\n    use first::Shared;\n    pub fn inside(_: Shared) {}\n}\npub fn outside(_: Shared, _: Undisputed) {}\n",
            ),
        ]);
        let tree = tree_of(&project);
        let here = "crates/user/src/lib.rs";
        assert!(
            tree.what_a_name_means(here, "Shared").is_none(),
            "one file importing two different Shared has to drop the name, not \
             answer with whichever `use` came last"
        );
        assert_eq!(
            tree.what_a_name_means(here, "Undisputed")
                .map(|found| found.path.as_str()),
            Some("crates/second/src/lib.rs"),
            "dropping the disputed name must not take its neighbours with it"
        );
    }

    #[test]
    fn two_crates_declaring_one_name_are_told_apart_by_where_they_are() {
        let project = project_with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\n    \"crates/first\",\n    \"crates/second\",\n]\n",
            ),
            ("crates/first/Cargo.toml", "[package]\nname = \"first\"\n"),
            ("crates/first/src/lib.rs", "pub fn work() {}\n"),
            ("crates/second/Cargo.toml", "[package]\nname = \"second\"\n"),
            ("crates/second/src/lib.rs", "pub fn work() {}\n"),
        ]);
        let tree = read(project.path(), &rust()).expect("the tree reads");

        let first = tree
            .placed("crates/first/src/lib.rs")
            .expect("the first crate's root");
        let second = tree
            .placed("crates/second/src/lib.rs")
            .expect("the second crate's root");
        assert_ne!(
            (first.crate_name.as_str(), first.module_path.as_str()),
            (second.crate_name.as_str(), second.module_path.as_str())
        );
        assert_eq!(tree.files_of("first").len(), 1);
        assert_eq!(tree.files_of("second").len(), 1);
    }
}
