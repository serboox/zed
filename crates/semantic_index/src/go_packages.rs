use std::path::Path;
use std::sync::LazyLock;

use crate::walk;

/// One Go module in the project: the import path it claims, and the directory
/// that path stands for, relative to the project root with forward slashes.
///
/// The root module's directory is the empty string, which is what makes the
/// prefix arithmetic below work without a special case for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub path: String,
    pub directory: String,
}

/// Every import path the project itself answers for, and the directory each
/// one names.
///
/// A Go package is a directory, and an import path is a module path followed
/// by the directory below that module. Both halves are read from the `go.mod`
/// files in the tree: one project can hold several modules -- a client library
/// beside the service that uses it is the ordinary shape -- and a `replace`
/// line pointing at a directory is a third way an import path becomes a
/// directory of this project.
///
/// What is *not* here is every import: `fmt` and `github.com/some/dependency`
/// name no directory of this project, and asking about one answers `None`.
/// That is the useful answer rather than a missing one -- a name reached
/// through a dependency is not a name this project declares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoModules {
    modules: Vec<Module>,
}

impl GoModules {
    /// Reads every `go.mod` under `root`.
    ///
    /// One walk of the project, so it belongs in a background pass and not on
    /// the path of a keystroke.
    pub fn read(root: &Path) -> Self {
        let mut modules = Vec::new();
        for found in walk::files_under(root) {
            if found.file_name().is_none_or(|name| name != "go.mod") {
                continue;
            }
            let Some(directory) = directory_of(root, &found) else {
                continue;
            };
            // The `go` tool builds neither, so a module declared in either is
            // not one this project's own imports can reach -- and a vendored
            // copy of a dependency claims the very paths the real one does.
            if directory
                .split('/')
                .any(|part| part == "vendor" || part == "testdata")
            {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&found) else {
                continue;
            };
            modules.extend(modules_declared_by(&contents, &directory));
        }
        Self::of(modules)
    }

    /// The modules as they are, for a caller that read the `go.mod` files some
    /// other way -- a test, or a project whose files are not on this disk.
    pub fn of(modules: impl IntoIterator<Item = Module>) -> Self {
        let mut modules: Vec<Module> = modules.into_iter().collect();
        // Longest path first, so the first match is the most specific one: a
        // file under `client/` belongs to the module rooted there and not to
        // the one rooted above it, whatever order the walk found them in.
        modules.sort_by(|one, other| {
            other
                .path
                .len()
                .cmp(&one.path.len())
                .then_with(|| one.path.cmp(&other.path))
                .then_with(|| one.directory.cmp(&other.directory))
        });
        modules.dedup();
        // A path two directories both claim resolves to neither. Sorting puts
        // them side by side, which is what makes this one pass; taking the
        // first would be picking by spelling, which is the confident wrong
        // answer this index exists not to give.
        let disputed: Vec<String> = modules
            .windows(2)
            .filter(|pair| pair[0].path == pair[1].path)
            .map(|pair| pair[0].path.clone())
            .collect();
        modules.retain(|module| !disputed.contains(&module.path));
        Self { modules }
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// The directory `import_path` names inside this project, relative to the
    /// root with forward slashes, or nothing where no module of this project
    /// claims it.
    pub fn directory_of(&self, import_path: &str) -> Option<String> {
        let import_path = import_path.trim_matches('/');
        for module in &self.modules {
            let Some(below) = below(&module.path, import_path) else {
                continue;
            };
            return Some(joined(&module.directory, below));
        }
        None
    }
}

/// The part of `import_path` under `module_path`, or nothing where the import
/// is not under that module at all.
///
/// Whole segments only. `pd-trkd-ts1-tools/thing` is not under `pd-trkd-ts1`,
/// however the two read as strings.
fn below<'a>(module_path: &str, import_path: &'a str) -> Option<&'a str> {
    if module_path.is_empty() {
        return None;
    }
    if import_path == module_path {
        return Some("");
    }
    import_path
        .strip_prefix(module_path)
        .and_then(|rest| rest.strip_prefix('/'))
}

fn joined(directory: &str, below: &str) -> String {
    match (directory.is_empty(), below.is_empty()) {
        (true, _) => below.to_string(),
        (false, true) => directory.to_string(),
        (false, false) => format!("{directory}/{below}"),
    }
}

/// `path` as a directory relative to `root`, forward slashes, for the file it
/// names -- so `<root>/client/go.mod` is `client`, and `<root>/go.mod` is the
/// empty string.
fn directory_of(root: &Path, path: &Path) -> Option<String> {
    let inside = path.strip_prefix(root).ok()?;
    let directory = inside.parent()?;
    Some(directory.to_string_lossy().replace('\\', "/"))
}

/// What one `go.mod` says this project answers for: its own module path, and
/// every `replace` that points at a directory rather than at another module.
///
/// Read as text rather than parsed. The two lines this needs -- `module` and
/// the arrow of a `replace` -- have a fixed shape the `go` tool itself
/// documents, and a `go.mod` too broken for this to read is one the build
/// cannot use either.
fn modules_declared_by(contents: &str, directory: &str) -> Vec<Module> {
    let mut found = Vec::new();
    let mut inside_a_replace_block = false;
    for line in contents.lines() {
        let line = match line.split_once("//") {
            Some((before, _)) => before.trim(),
            None => line.trim(),
        };
        if line.is_empty() {
            continue;
        }
        if let Some(path) = line.strip_prefix("module ") {
            let path = path.trim().trim_matches('"');
            if !path.is_empty() {
                found.push(Module {
                    path: path.to_string(),
                    directory: directory.to_string(),
                });
            }
            continue;
        }
        if line == "replace (" {
            inside_a_replace_block = true;
            continue;
        }
        if inside_a_replace_block && line == ")" {
            inside_a_replace_block = false;
            continue;
        }
        let replacement = if inside_a_replace_block {
            Some(line)
        } else {
            line.strip_prefix("replace ").map(str::trim)
        };
        if let Some(replacement) = replacement
            && let Some(module) = replacement_of(replacement, directory)
        {
            found.push(module);
        }
    }
    found
}

/// One `replace` line, where what it points at is a directory of this project.
///
/// `replace a/b => ./c` and `replace a/b v1.2.3 => ../c` both count; a
/// replacement by another published module names no directory here and is
/// left alone.
fn replacement_of(line: &str, directory: &str) -> Option<Module> {
    let (left, right) = line.split_once("=>")?;
    let claimed = left.split_whitespace().next()?;
    let target = right.split_whitespace().next()?;
    if !(target.starts_with("./") || target.starts_with("../") || target.starts_with('/')) {
        return None;
    }
    Some(Module {
        path: claimed.to_string(),
        directory: resolved_against(directory, target)?,
    })
}

/// A directory written in a `go.mod`, resolved against the directory that
/// `go.mod` sits in, as a path relative to the project root.
///
/// Nothing where it climbs out of the project. A replacement pointing at a
/// sibling checkout names a directory this index has never read, and folding
/// the climb away would turn `../shared` into a `shared` of this project that
/// has nothing to do with it.
fn resolved_against(directory: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    for step in target.split('/') {
        match step {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            named => parts.push(named),
        }
    }
    Some(parts.join("/"))
}

/// The Go grammar, built once.
///
/// [`crate::languages::readable`] compiles every outline query the editor
/// ships, which is far more than parsing one file's import block needs, and
/// this runs while a reader holds a modifier key down.
pub fn grammar() -> Option<&'static tree_sitter::Language> {
    static GO: LazyLock<Option<tree_sitter::Language>> =
        LazyLock::new(|| crate::languages::grammar_named("go"));
    GO.as_ref()
}

/// One import line of one Go file: the name it binds in that file, and the
/// path it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoImport {
    pub binds: String,
    pub path: String,
}

/// What one Go file's imports bind.
///
/// The file is parsed rather than scanned, because a quoted string in a line
/// before the first declaration is not always an import: `imports/forward.go`
/// in the Go tools opens with a comment mentioning `"go/format"`, and reading
/// that as an import is how a real symbol was once silenced.
///
/// A dot import and a blank import bind no qualifier, so neither is returned:
/// there is no `name` a reader could write before the dot.
pub fn imports_of(text: &str, grammar: &tree_sitter::Language) -> Vec<GoImport> {
    let text = where_the_imports_can_still_be(text);
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(grammar).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_imports(tree.root_node(), text.as_bytes(), &mut found);
    found
}

/// The head of a Go file, down to the first top-level declaration.
///
/// Go requires every import to come before every declaration, so nothing below
/// the first one can be an import -- and cutting there is what keeps this
/// affordable on the path of a modifier-hover, where it runs per mouse move.
///
/// The cut only starts looking once the `package` clause has gone by. A
/// licence header written as a block comment can hold a line beginning with
/// `func`, and cutting the file above its own package clause would leave
/// nothing to read.
fn where_the_imports_can_still_be(text: &str) -> &str {
    let mut at = 0;
    let mut past_the_package_clause = false;
    for line in text.split_inclusive('\n') {
        if past_the_package_clause
            && (line.starts_with("func ")
                || line.starts_with("type ")
                || line.starts_with("var ")
                || line.starts_with("const "))
        {
            return &text[..at];
        }
        if line.starts_with("package ") {
            past_the_package_clause = true;
        }
        at += line.len();
    }
    text
}

fn collect_imports(node: tree_sitter::Node, contents: &[u8], into: &mut Vec<GoImport>) {
    // An import block the grammar could not read is a block whose paths cannot
    // be trusted: an unterminated string swallows the lines below it, and what
    // comes back looks like a path and names something else. The block is
    // skipped rather than half read.
    if node.kind() == "import_declaration" && node.has_error() {
        return;
    }
    if node.kind() == "import_spec" {
        if let Some(found) = import_spec(node, contents) {
            into.push(found);
        }
        return;
    }
    let mut walking = node.walk();
    for child in node.named_children(&mut walking) {
        collect_imports(child, contents, into);
    }
}

fn import_spec(node: tree_sitter::Node, contents: &[u8]) -> Option<GoImport> {
    let quoted = node.child_by_field_name("path")?;
    let path = quoted.utf8_text(contents).ok()?.trim_matches(['"', '`']);
    if path.is_empty() {
        return None;
    }
    let binds = match node.child_by_field_name("name") {
        Some(name) => {
            let written = name.utf8_text(contents).ok()?;
            // `.` brings the names in unqualified and `_` brings none in at
            // all. Neither leaves a qualifier anyone can write.
            if written == "." || written == "_" {
                return None;
            }
            written.to_string()
        }
        None => package_name_of(path)?.to_string(),
    };
    Some(GoImport {
        binds,
        path: path.to_string(),
    })
}

/// The name a Go file binds for an import it does not rename: the last element
/// of the path, except that a major-version element is not a name -- the
/// package of `github.com/thing/parser/v4` is written `parser`.
fn package_name_of(path: &str) -> Option<&str> {
    let mut parts = path.rsplit('/');
    let last = parts.next()?;
    if is_a_major_version(last) {
        return parts.next().filter(|earlier| !earlier.is_empty());
    }
    Some(last).filter(|last| !last.is_empty())
}

fn is_a_major_version(part: &str) -> bool {
    part.strip_prefix('v').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// Whether any import of this file binds this qualifier at all.
///
/// Not the same question as [`what_a_qualifier_means`], and the difference
/// decides what a caller may fall back to: a qualifier that names a package
/// says the name after it belongs to that package, so if where that package
/// declares it cannot be worked out, nothing else may be offered instead.
/// A qualifier that names no import is a value, which is a different question
/// entirely.
pub fn any_import_binds(imports: &[GoImport], qualifier: &str) -> bool {
    imports.iter().any(|import| import.binds == qualifier)
}

/// The import a qualifier written in this file stands for.
///
/// Nothing where the file imports no such name: a qualifier that is not an
/// import is a value, and what a value's type is, is the one thing no index
/// here knows.
pub fn what_a_qualifier_means<'a>(
    imports: &'a [GoImport],
    qualifier: &str,
) -> Option<&'a GoImport> {
    let mut matching = imports.iter().filter(|import| import.binds == qualifier);
    let only = matching.next()?;
    // Two imports binding one name do not compile, but a file being edited is
    // allowed to be halfway there, and picking one of the two would be a
    // confident wrong answer.
    if matching.next().is_some() {
        return None;
    }
    Some(only)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go() -> tree_sitter::Language {
        grammars::native_grammars()
            .into_iter()
            .find(|(name, _)| *name == "go")
            .map(|(_, grammar)| grammar)
            .expect("the Go grammar is built in")
    }

    #[test]
    fn an_import_path_under_the_root_module_is_a_directory_of_the_project() {
        let modules = GoModules::of([Module {
            path: "pd-trkd-ts1".to_string(),
            directory: String::new(),
        }]);
        assert_eq!(
            modules.directory_of("pd-trkd-ts1/internal/models"),
            Some("internal/models".to_string())
        );
        assert_eq!(modules.directory_of("pd-trkd-ts1"), Some(String::new()));
    }

    #[test]
    fn the_module_rooted_deepest_claims_an_import_both_could_read_as_theirs() {
        let modules = GoModules::of([
            Module {
                path: "example.com/thing".to_string(),
                directory: String::new(),
            },
            Module {
                path: "example.com/thing/client".to_string(),
                directory: "client".to_string(),
            },
        ]);
        assert_eq!(
            modules.directory_of("example.com/thing/client/pkg/models"),
            Some("client/pkg/models".to_string()),
            "the client module roots at client/, so its own package is not client/client/pkg/models"
        );
    }

    #[test]
    fn an_import_outside_every_module_names_no_directory() {
        let modules = GoModules::of([Module {
            path: "pd-trkd-ts1".to_string(),
            directory: String::new(),
        }]);
        assert_eq!(modules.directory_of("fmt"), None);
        assert_eq!(modules.directory_of("github.com/gocql/gocql"), None);
    }

    #[test]
    fn a_module_path_is_matched_by_whole_segments() {
        let modules = GoModules::of([Module {
            path: "pd-trkd".to_string(),
            directory: String::new(),
        }]);
        assert_eq!(
            modules.directory_of("pd-trkd-ts1/internal/models"),
            None,
            "`pd-trkd-ts1` merely starts with `pd-trkd`; it is a different module"
        );
    }

    #[test]
    fn a_replace_pointing_at_a_directory_is_a_module_of_this_project() {
        let declared = modules_declared_by(
            "module pd-trkd-ts1\n\
             \n\
             go 1.24.1\n\
             \n\
             replace (\n\
             \tgithub.com/example/thing/client => ./client\n\
             \tgithub.com/gocql/gocql => github.com/scylladb/gocql v1.12.0\n\
             )\n",
            "",
        );
        assert_eq!(
            declared,
            vec![
                Module {
                    path: "pd-trkd-ts1".to_string(),
                    directory: String::new(),
                },
                Module {
                    path: "github.com/example/thing/client".to_string(),
                    directory: "client".to_string(),
                },
            ],
            "the replacement by another published module names no directory here"
        );
    }

    #[test]
    fn a_replace_on_one_line_and_with_a_version_counts_too() {
        let declared = modules_declared_by(
            "module inner\nreplace example.com/other v1.2.3 => ../other\n",
            "nested/deep",
        );
        assert_eq!(
            declared,
            vec![
                Module {
                    path: "inner".to_string(),
                    directory: "nested/deep".to_string(),
                },
                Module {
                    path: "example.com/other".to_string(),
                    directory: "nested/other".to_string(),
                },
            ]
        );
    }

    #[test]
    fn a_commented_out_replace_is_not_a_module() {
        let declared = modules_declared_by(
            "module thing\n// replace example.com/other => ./other\n",
            "",
        );
        assert_eq!(
            declared,
            vec![Module {
                path: "thing".to_string(),
                directory: String::new(),
            }]
        );
    }

    #[test]
    fn a_replace_pointing_outside_the_project_names_no_directory_of_it() {
        let declared = modules_declared_by(
            "module thing\nreplace example.com/shared => ../shared\n",
            "",
        );
        assert_eq!(
            declared,
            vec![Module {
                path: "thing".to_string(),
                directory: String::new(),
            }],
            "a sibling checkout is not a directory this index has ever read"
        );
        assert_eq!(resolved_against("", "../shared"), None);
        assert_eq!(
            resolved_against("nested/deep", "../../../out"),
            None,
            "one step past the root is past the root, however many follow"
        );
    }

    #[test]
    fn a_path_two_directories_both_claim_resolves_to_neither() {
        let modules = GoModules::of([
            Module {
                path: "example.com/thing".to_string(),
                directory: "one".to_string(),
            },
            Module {
                path: "example.com/thing".to_string(),
                directory: "two".to_string(),
            },
            Module {
                path: "example.com/other".to_string(),
                directory: "three".to_string(),
            },
        ]);
        assert_eq!(
            modules.directory_of("example.com/thing/pkg"),
            None,
            "picking one of the two would be picking by spelling"
        );
        assert_eq!(
            modules.directory_of("example.com/other/pkg"),
            Some("three/pkg".to_string()),
            "the undisputed one is unaffected"
        );
    }

    /// One `replace` and one nested `go.mod` naming the same directory is the
    /// ordinary shape of a client library beside the service that uses it, and
    /// must not read as two claimants.
    #[test]
    fn one_directory_claimed_twice_over_is_still_one_claim() {
        let modules = GoModules::of([
            Module {
                path: "example.com/thing/client".to_string(),
                directory: "client".to_string(),
            },
            Module {
                path: "example.com/thing/client".to_string(),
                directory: "client".to_string(),
            },
        ]);
        assert_eq!(
            modules.directory_of("example.com/thing/client/pkg/models"),
            Some("client/pkg/models".to_string())
        );
    }

    #[test]
    fn a_module_the_go_tool_never_builds_is_not_read() {
        let held = tempfile::tempdir().expect("a directory");
        let root = held.path();
        std::fs::write(root.join("go.mod"), "module thing\n").expect("the root module");
        for shadow in ["vendor/example.com/dep", "internal/testdata/fixture"] {
            std::fs::create_dir_all(root.join(shadow)).expect("a directory");
            std::fs::write(root.join(shadow).join("go.mod"), "module thing/deep\n")
                .expect("a module the go tool ignores");
        }

        let modules = GoModules::read(root);
        assert_eq!(
            modules.directory_of("thing/deep"),
            Some("deep".to_string()),
            "the root module answers for it; neither shadow does"
        );
    }

    /// An unterminated string swallows the lines below it, and what comes back
    /// out of the wreckage reads like a path and names something else.
    #[test]
    fn an_import_block_the_grammar_cannot_read_binds_nothing() {
        let imports = imports_of(
            "package one\n\
             \n\
             import (\n\
             \t\"pd/internal/models\n\
             \t\"pd/internal/other\"\n\
             )\n",
            &go(),
        );
        assert_eq!(imports, Vec::new());
    }

    #[test]
    fn the_go_mod_files_of_a_real_tree_are_read() {
        let held = tempfile::tempdir().expect("a directory");
        let root = held.path();
        std::fs::write(root.join("go.mod"), "module pd-trkd-ts1\n\ngo 1.24.1\n")
            .expect("the root module");
        std::fs::create_dir_all(root.join("client")).expect("a directory");
        std::fs::write(
            root.join("client/go.mod"),
            "module github.com/example/thing/client\n",
        )
        .expect("the client module");

        let modules = GoModules::read(root);
        assert_eq!(
            modules.directory_of("pd-trkd-ts1/internal/models"),
            Some("internal/models".to_string())
        );
        assert_eq!(
            modules.directory_of("github.com/example/thing/client/pkg/models"),
            Some("client/pkg/models".to_string())
        );
    }

    #[test]
    fn every_shape_of_import_binds_the_name_a_reader_writes() {
        let imports = imports_of(
            "package process\n\
             \n\
             import (\n\
             \t\"context\"\n\
             \t\"pd-trkd-ts1/internal/models\"\n\
             \n\
             \tpkgModels \"github.com/example/thing/client/pkg/models\"\n\
             \t_ \"github.com/example/driver\"\n\
             \t. \"github.com/example/helpers\"\n\
             \t\"github.com/example/parser/v4\"\n\
             )\n\
             \n\
             func work() {}\n",
            &go(),
        );
        assert_eq!(
            imports,
            vec![
                GoImport {
                    binds: "context".to_string(),
                    path: "context".to_string()
                },
                GoImport {
                    binds: "models".to_string(),
                    path: "pd-trkd-ts1/internal/models".to_string()
                },
                GoImport {
                    binds: "pkgModels".to_string(),
                    path: "github.com/example/thing/client/pkg/models".to_string()
                },
                GoImport {
                    binds: "parser".to_string(),
                    path: "github.com/example/parser/v4".to_string()
                },
            ]
        );
    }

    #[test]
    fn a_single_import_with_no_block_is_read() {
        let imports = imports_of("package one\n\nimport \"pd/internal/models\"\n", &go());
        assert_eq!(
            imports,
            vec![GoImport {
                binds: "models".to_string(),
                path: "pd/internal/models".to_string()
            }]
        );
    }

    /// The trap this crate already met once: a quoted path in a doc comment is
    /// not an import, and reading it as one silences a real symbol.
    #[test]
    fn a_quoted_path_in_a_comment_is_not_an_import() {
        let imports = imports_of(
            "// Package imports is a pretty-printer (like package \"go/format\").\n\
             package imports\n\
             \n\
             import \"strings\"\n",
            &go(),
        );
        assert_eq!(
            imports,
            vec![GoImport {
                binds: "strings".to_string(),
                path: "strings".to_string()
            }]
        );
    }

    /// A licence header written as a block comment can hold a line that starts
    /// like a declaration. Cutting there would take the package clause and the
    /// imports with it.
    #[test]
    fn a_block_comment_above_the_package_clause_does_not_cut_the_file() {
        let imports = imports_of(
            "/*\nfunc is a word this licence happens to start a line with.\n*/\n\
             package one\n\
             \n\
             import \"pd/internal/models\"\n\
             \n\
             func work() {}\n",
            &go(),
        );
        assert_eq!(
            imports,
            vec![GoImport {
                binds: "models".to_string(),
                path: "pd/internal/models".to_string()
            }]
        );
    }

    #[test]
    fn only_the_head_of_a_file_is_parsed_for_imports() {
        let text = "package one\n\nimport \"a/b\"\n\nfunc work() {\n\tsay(\"c/d\")\n}\n";
        assert_eq!(
            where_the_imports_can_still_be(text),
            "package one\n\nimport \"a/b\"\n\n"
        );
    }

    #[test]
    fn a_qualifier_the_file_does_not_import_means_nothing() {
        let imports = vec![GoImport {
            binds: "models".to_string(),
            path: "pd/internal/models".to_string(),
        }];
        assert_eq!(
            what_a_qualifier_means(&imports, "models").map(|found| found.path.as_str()),
            Some("pd/internal/models")
        );
        assert_eq!(what_a_qualifier_means(&imports, "job"), None);
    }

    #[test]
    fn a_name_two_imports_both_bind_is_refused_rather_than_guessed() {
        let imports = vec![
            GoImport {
                binds: "models".to_string(),
                path: "pd/internal/models".to_string(),
            },
            GoImport {
                binds: "models".to_string(),
                path: "pd/client/models".to_string(),
            },
        ];
        assert_eq!(what_a_qualifier_means(&imports, "models"), None);
    }
}
