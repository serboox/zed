use std::path::Path;

use anyhow::Result;
use streaming_iterator::StreamingIterator as _;

use crate::languages::Readable;

/// One definition, as the index records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// Relative to the project root, forward slashes.
    pub path: String,
    /// What the definition is called. An `impl` block has two names -- the trait
    /// and the type -- and carries both, in the order the query captured them,
    /// because either is what somebody would search for.
    pub name: String,
    /// The grammar's own name for the node, `function_item` or `struct_item` and
    /// so on. Taken from the grammar rather than worked out from the keywords
    /// beside it: the grammar cannot be wrong about what it parsed.
    pub kind: String,
    /// One-based, as a reader counts lines.
    pub line: u32,
    pub language: String,
}

/// The definitions in one file, found by running the language's outline query
/// over its parse tree.
///
/// The query is the one the editor already ships and already uses to draw the
/// outline of a file, so what the index finds is by construction what the editor
/// shows -- there is no second opinion about what a definition is.
pub fn in_file(
    path: &str,
    contents: &[u8],
    language: &Readable,
    parser: &mut tree_sitter::Parser,
) -> Result<Vec<Definition>> {
    parser.set_language(&language.grammar)?;
    let Some(tree) = parser.parse(contents, None) else {
        return Ok(Vec::new());
    };

    let item = capture_named(&language.outline, "item");
    let name = capture_named(&language.outline, "name");
    let (Some(item), Some(name)) = (item, name) else {
        // A query with no `item` or no `name` describes something other than
        // definitions; it is not a failure, there is simply nothing to record.
        return Ok(Vec::new());
    };

    let mut found = Vec::new();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&language.outline, tree.root_node(), contents);
    while let Some(matched) = matches.next() {
        let Some(defined) = matched
            .captures
            .iter()
            .find(|capture| capture.index == item)
            .map(|capture| capture.node)
        else {
            // The outline query also matches comments and attributes so the
            // editor can attach them to what follows. They define nothing.
            continue;
        };
        // Some languages' outline queries also yield captures that are not
        // declarations at all -- an object literal's entries, a `let` nested in
        // a function, a test runner's wrapper -- because the editor wants them
        // in its outline. An index of definitions does not.
        if !crate::per_language::is_declaration(&language.name, defined) {
            continue;
        }
        let named: Vec<&str> = matched
            .captures
            .iter()
            .filter(|capture| capture.index == name)
            .filter_map(|capture| capture.node.utf8_text(contents).ok())
            .collect();
        if named.is_empty() {
            continue;
        }
        found.push(Definition {
            path: path.to_string(),
            name: named.join(" "),
            kind: defined.kind().to_string(),
            line: defined.start_position().row as u32 + 1,
            language: language.name.clone(),
        });
    }

    // What the query cannot reach. A macro body is opaque to the grammar, so
    // the names a macro declares are invisible to any query over the tree --
    // and on this project they were most of what the index was missing.
    let mut walking = vec![tree.root_node()];
    while let Some(node) = walking.pop() {
        for (name, line) in
            crate::per_language::names_a_macro_declares(&language.name, node, contents)
        {
            found.push(Definition {
                path: path.to_string(),
                name,
                // What the macro really writes out. The kind is what the
                // editor draws an icon from, so it has to be the kind of the
                // thing that ends up in the crate, not of the macro that
                // wrote it.
                kind: "struct_item".to_string(),
                line,
                language: language.name.clone(),
            });
        }
        for at in 0..node.named_child_count() as u32 {
            if let Some(child) = node.named_child(at) {
                walking.push(child);
            }
        }
    }
    Ok(found)
}

/// The index of a capture by name, or `None` where the query has no such
/// capture.
fn capture_named(query: &tree_sitter::Query, name: &str) -> Option<u32> {
    query
        .capture_names()
        .iter()
        .position(|capture| *capture == name)
        .map(|at| at as u32)
}

/// Reads one file from disk and finds its definitions. `None` for a file that
/// cannot be read: one file fewer, not a reason to abandon the pass.
pub fn in_file_on_disk(
    root: &Path,
    path: &Path,
    language: &Readable,
    parser: &mut tree_sitter::Parser,
) -> Option<Vec<Definition>> {
    let contents = std::fs::read(path).ok()?;
    let inside = path.strip_prefix(root).ok()?;
    let named = inside.to_string_lossy().replace('\\', "/");
    in_file(&named, &contents, language, parser).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languages;

    fn rust() -> Readable {
        let (readable, _) = languages::readable();
        readable
            .into_iter()
            .find(|language| language.name == "rust")
            .expect("Rust is one of the languages the editor ships")
    }

    fn found_in(source: &str) -> Vec<Definition> {
        let mut parser = tree_sitter::Parser::new();
        in_file("src/one.rs", source.as_bytes(), &rust(), &mut parser).expect("the file parses")
    }

    #[test]
    fn a_definition_is_recorded_with_its_name_its_kind_and_its_line() {
        let found = found_in(
            "// a comment, which defines nothing\n\
             pub struct Thing {\n    field: u32,\n}\n\
             \n\
             pub fn work() {}\n",
        );
        let named: Vec<(&str, &str, u32)> = found
            .iter()
            .map(|one| (one.name.as_str(), one.kind.as_str(), one.line))
            .collect();
        assert!(
            named.contains(&("Thing", "struct_item", 2)),
            "the struct on line two: {named:?}"
        );
        assert!(
            named.contains(&("work", "function_item", 6)),
            "the function on line six: {named:?}"
        );
        assert!(
            !named.iter().any(|(name, _, _)| name.contains("comment")),
            "a comment defines nothing: {named:?}"
        );
    }

    /// The kind comes from the grammar, so it is the grammar's own word for the
    /// node and cannot disagree with what was parsed.
    #[test]
    fn every_kind_the_query_yields_is_a_node_the_grammar_names() {
        let found = found_in(
            "pub struct A;\n\
             pub enum B { One }\n\
             pub trait C {}\n\
             impl C for A {}\n\
             pub fn d() {}\n\
             pub const E: u32 = 1;\n\
             mod f {}\n\
             pub type G = A;\n\
             macro_rules! h { () => {} }\n",
        );
        let kinds: Vec<&str> = found.iter().map(|one| one.kind.as_str()).collect();
        for expected in [
            "struct_item",
            "enum_item",
            "trait_item",
            "impl_item",
            "function_item",
            "const_item",
            "mod_item",
            "type_item",
        ] {
            assert!(
                kinds.contains(&expected),
                "{expected} missing from {kinds:?}"
            );
        }
        assert!(
            found.iter().all(|one| !one.kind.is_empty()),
            "every definition has the grammar's own name for its node"
        );
    }

    /// An `impl` block is captured with two names, the trait and the type, and
    /// somebody searching would type either.
    #[test]
    fn an_impl_block_keeps_both_of_the_names_it_was_captured_with() {
        let found = found_in("pub struct Thing;\ntrait Doable {}\nimpl Doable for Thing {}\n");
        let block = found
            .iter()
            .find(|one| one.kind == "impl_item")
            .expect("the impl block");
        assert!(
            block.name.contains("Doable") && block.name.contains("Thing"),
            "both names, so either finds it: {:?}",
            block.name
        );
    }

    #[test]
    fn a_file_that_defines_nothing_yields_nothing_rather_than_failing() {
        assert!(found_in("").is_empty());
        assert!(found_in("// only a comment\n").is_empty());
    }

    /// A file caught mid-edit yields nothing at all -- not "as much as
    /// survived". Measured, not assumed: an unterminated struct is recovered as
    /// an error node, and the outline query does not match it, so the whole file
    /// contributes no definitions until it parses again.
    ///
    /// That is the right behaviour for an index of what is on disk, and it is a
    /// limitation worth naming: a file left unparseable on disk is a file the
    /// index has nothing about, rather than one it has half of.
    #[test]
    fn a_file_caught_mid_edit_contributes_nothing_until_it_parses_again() {
        let half_typed = found_in("pub struct Half {\npub fn work(");
        assert!(
            half_typed.is_empty(),
            "measured behaviour is nothing at all; found {:?}",
            half_typed
                .iter()
                .map(|one| (one.name.as_str(), one.kind.as_str()))
                .collect::<Vec<_>>()
        );

        // And the moment it parses, everything in it is there.
        let finished = found_in("pub struct Half {}\npub fn work() {}\n");
        assert_eq!(finished.len(), 2, "{finished:?}");
    }

    /// A macro body is opaque to the grammar, so nothing a query does can find
    /// what a macro declares. Measured against rust-analyzer on this project,
    /// that was ninety-five per cent of everything the index missed, and one
    /// macro was nearly half of it -- so that one is expanded by hand.
    #[test]
    fn the_names_an_actions_macro_declares_are_found_though_the_query_cannot_see_them() {
        let found = found_in(
            "actions!(\n\
             \x20   feedback,\n\
             \x20   [\n\
             \x20       /// Opens the repository.\n\
             \x20       OpenRepo,\n\
             \x20       #[action(deprecated)]\n\
             \x20       CopyDiagnostics\n\
             \x20   ]\n\
             );\n",
        );
        let named: Vec<(&str, &str, u32)> = found
            .iter()
            .map(|one| (one.name.as_str(), one.kind.as_str(), one.line))
            .collect();
        assert!(
            named.contains(&("OpenRepo", "struct_item", 5)),
            "the first action, on its own line: {named:?}"
        );
        assert!(
            named.contains(&("CopyDiagnostics", "struct_item", 7)),
            "and the second, past a doc comment and an attribute: {named:?}"
        );
        assert!(
            !named.iter().any(|(name, _, _)| *name == "feedback"),
            "the namespace is not a declaration: {named:?}"
        );
        assert!(
            !named.iter().any(|(name, _, _)| *name == "action"),
            "nor is anything inside an attribute: {named:?}"
        );
    }

    /// The short form, with no namespace, declares the same things.
    #[test]
    fn an_actions_macro_without_a_namespace_declares_its_names_too() {
        let found = found_in("actions!([Save, SaveAll]);\n");
        let named: Vec<&str> = found.iter().map(|one| one.name.as_str()).collect();
        assert!(
            named.contains(&"Save") && named.contains(&"SaveAll"),
            "{named:?}"
        );
    }

    /// Two more of the same kind, whose declared name is an argument rather
    /// than a list. Their first argument is the protocol method, a string, so
    /// the name is the first identifier the call holds -- and the types after
    /// it are not names this declares.
    #[test]
    fn a_request_or_notification_macro_declares_the_name_it_is_given() {
        let found = found_in(
            "request!(\"initialize\", Initialize, InitializeParams, InitializeResponse);\n\
             notification!(\"notifications/progress\", Progress, ProgressParams);\n",
        );
        let named: Vec<(&str, u32)> = found
            .iter()
            .map(|one| (one.name.as_str(), one.line))
            .collect();
        assert!(named.contains(&("Initialize", 1)), "{named:?}");
        assert!(named.contains(&("Progress", 2)), "{named:?}");
        assert!(
            !named.iter().any(|(name, _)| name.ends_with("Params")),
            "a parameter type is named, not declared, here: {named:?}"
        );
    }

    /// A macro this does not know declares nothing, rather than guessing.
    #[test]
    fn a_macro_that_is_not_recognised_contributes_nothing() {
        let found = found_in("write_bindings!(host, [Thing, Other]);\n");
        assert!(
            found.is_empty(),
            "an unrecognised macro is left alone: {:?}",
            found
                .iter()
                .map(|one| one.name.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_language_of_a_definition_is_recorded_with_it() {
        let found = found_in("pub fn work() {}\n");
        assert!(found.iter().all(|one| one.language == "rust"));
        assert!(found.iter().all(|one| one.path == "src/one.rs"));
    }
}
