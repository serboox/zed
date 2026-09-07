use std::sync::Arc;

use gpui::{App, Task};
use language::{CodeLabel, LanguageServerId, SymbolKind};
use lsp::LanguageServerName;
use project::{
    InProcessSymbolAnswer, InProcessSymbolContext, InProcessWorkspaceSymbols, ProjectPath, Symbol,
    lsp_store::SymbolLocation,
};
use semantic_index::definitions::Definition;
use text::{PointUtf16, Unclipped};
use util::rel_path::RelPath;

use crate::index_semantics::MOST_PLACES_WORTH_OPENING;

/// Answers "Go to Symbol in Project" out of the index, so that a reader with
/// no language server running still finds what the project declares.
pub fn init(cx: &mut App) {
    project::register_in_process_workspace_symbols(Arc::new(ProjectSymbols), cx);
}

struct ProjectSymbols;

/// The name these symbols are filed under, which is what
/// `Project::open_buffer_for_symbol` matches to open one without looking for
/// a server. Written for a reader, because it is what a symbol reports as its
/// origin wherever the editor shows one.
const NAME: &str = "project index";

/// The id these symbols are filed under. There is no server behind it, in the
/// way `cargo_diagnostics` has none: the editor keys a symbol by server, so a
/// source that is not a server still needs an id. Chosen far above any a
/// running server would be assigned, and one apart from every other such id.
const INDEX_SERVER_ID: LanguageServerId = LanguageServerId(usize::MAX - 1014);

impl InProcessWorkspaceSymbols for ProjectSymbols {
    fn language_server_name(&self) -> LanguageServerName {
        LanguageServerName(NAME.into())
    }

    fn symbols(
        &self,
        context: &InProcessSymbolContext,
        query: &str,
        cx: &mut App,
    ) -> Task<InProcessSymbolAnswer> {
        let nothing = || {
            Task::ready(InProcessSymbolAnswer {
                symbols: Vec::new(),
                cut_short: false,
            })
        };
        // A rust-analyzer path query -- `dir::name` -- names its last segment,
        // the same reading `project_symbols` already gives it.
        let asked = query
            .rsplit_once("::")
            .map_or(query, |(_, suffix)| suffix)
            .trim();
        if asked.is_empty() {
            return nothing();
        }
        let Some(index) = crate::of_project(&context.project, cx) else {
            return nothing();
        };
        let index = index.read(cx);
        let Some(worktree_id) = index.worktree_id() else {
            return nothing();
        };

        // One more than is listed, so that "there were more" can be told from
        // "that was all of them" rather than guessed at from the count.
        let mut found = index.candidates(asked, MOST_PLACES_WORTH_OPENING + 1);
        let cut_short = found.len() > MOST_PLACES_WORTH_OPENING;
        found.truncate(MOST_PLACES_WORTH_OPENING);

        let symbols = found
            .iter()
            .filter_map(|definition| symbol_for(definition, worktree_id))
            .collect();
        Task::ready(InProcessSymbolAnswer { symbols, cut_short })
    }
}

/// One index row in the shape the pickers already read a language server's
/// symbols in, so that nothing downstream needs to know which of the two it
/// came from.
fn symbol_for(definition: &Definition, worktree_id: project::WorktreeId) -> Option<Symbol> {
    let path = RelPath::from_unix_str(&definition.path).ok()?;
    // The index counts lines the way a reader does; a buffer position counts
    // from zero.
    let row = definition.line.saturating_sub(1);
    let at = Unclipped(PointUtf16::new(row, 0));
    Some(Symbol {
        language_server_name: LanguageServerName(NAME.into()),
        source_worktree_id: worktree_id,
        source_language_server_id: INDEX_SERVER_ID,
        path: SymbolLocation::InProject(ProjectPath {
            worktree_id,
            path: path.into(),
        }),
        label: CodeLabel::plain(definition.name.clone(), None),
        name: definition.name.clone(),
        kind: symbol_kind_of(&definition.kind),
        range: at..at,
        container_name: None,
    })
}

/// What the editor's own kinds call the node the grammar parsed.
///
/// The index carries the grammar's name for a node -- `function_item`,
/// `struct_item` -- because the grammar cannot be wrong about what it parsed,
/// but every surface that draws a symbol draws [`SymbolKind`]. A node kind
/// this does not know maps to `Object`, the kind with no shape of its own,
/// rather than to a guess: an `impl` block really is that, and so is anything
/// a grammar names in a way nobody here has seen.
fn symbol_kind_of(node_kind: &str) -> SymbolKind {
    match node_kind {
        "function_item"
        | "function_declaration"
        | "function_definition"
        | "function_signature_item"
        | "generator_function_declaration"
        | "macro_definition"
        | "preproc_def"
        | "preproc_function_def"
        | "subroutine_subprogram" => SymbolKind::Function,
        "method_declaration" | "method_definition" | "method_signature" | "method_spec"
        | "method" | "singleton_method" => SymbolKind::Method,
        "constructor_declaration" | "construct_signature" => SymbolKind::Constructor,
        "struct_item" | "struct_specifier" | "struct_declaration" | "union_item"
        | "union_specifier" | "record_declaration" => SymbolKind::Struct,
        "class_declaration" | "class_definition" | "class_specifier" => SymbolKind::Class,
        "enum_item" | "enum_declaration" | "enum_specifier" => SymbolKind::Enum,
        "enum_variant" | "enumerator" => SymbolKind::EnumMember,
        "trait_item" | "interface_declaration" | "interface_type" => SymbolKind::Interface,
        "mod_item"
        | "module"
        | "module_declaration"
        | "namespace_definition"
        | "namespace_declaration"
        | "package_clause" => SymbolKind::Module,
        "const_item" | "const_declaration" | "constant_declaration" => SymbolKind::Constant,
        "static_item"
        | "variable_declaration"
        | "variable_declarator"
        | "let_declaration"
        | "var_declaration" => SymbolKind::Variable,
        "type_item"
        | "type_alias_declaration"
        | "type_definition"
        | "type_declaration"
        | "associated_type" => SymbolKind::TypeParameter,
        "field_declaration"
        | "field_definition"
        | "property_declaration"
        | "property_signature" => SymbolKind::Field,
        _ => SymbolKind::Object,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(name: &str, kind: &str, path: &str, line: u32) -> Definition {
        Definition {
            path: path.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line,
            language: "rust".to_string(),
        }
    }

    #[test]
    fn kinds_the_grammar_names_map_onto_the_editor_s_own() {
        assert_eq!(symbol_kind_of("function_item"), SymbolKind::Function);
        assert_eq!(symbol_kind_of("struct_item"), SymbolKind::Struct);
        assert_eq!(symbol_kind_of("enum_variant"), SymbolKind::EnumMember);
        assert_eq!(symbol_kind_of("trait_item"), SymbolKind::Interface);
        assert_eq!(symbol_kind_of("method_declaration"), SymbolKind::Method);
    }

    #[test]
    fn a_kind_with_no_sensible_mapping_is_neutral() {
        assert_eq!(symbol_kind_of("impl_item"), SymbolKind::Object);
        assert_eq!(
            symbol_kind_of("something_no_one_has_seen"),
            SymbolKind::Object
        );
    }

    #[test]
    fn a_symbol_lands_on_the_line_the_index_holds() {
        let worktree_id = project::WorktreeId::from_usize(1);
        let symbol = symbol_for(
            &declared("take_stock", "function_item", "src/one.rs", 5),
            worktree_id,
        )
        .expect("a unix relative path is a relative path");
        assert_eq!(symbol.name, "take_stock");
        assert_eq!(symbol.range.start.0.row, 4);
        assert_eq!(symbol.kind, SymbolKind::Function);
        match &symbol.path {
            SymbolLocation::InProject(path) => {
                assert_eq!(path.path.as_unix_str(), "src/one.rs");
                assert_eq!(path.worktree_id, worktree_id);
            }
            SymbolLocation::OutsideProject { .. } => panic!("the index only reads the project"),
        }
    }

    #[test]
    fn the_first_line_of_a_file_does_not_wrap_around() {
        let symbol = symbol_for(
            &declared("first", "function_item", "one.rs", 1),
            project::WorktreeId::from_usize(1),
        )
        .expect("a unix relative path is a relative path");
        assert_eq!(symbol.range.start.0.row, 0);
    }
}
