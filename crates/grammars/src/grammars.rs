use std::borrow::Cow;

use anyhow::Context as _;
use language_core::{LanguageConfig, LanguageQueries, QueryFile, QueryFileContents};

// Dev builds read the checkout's query files at runtime instead of embedding
// them; see the `assets` crate for the rationale.
util::fs_embed! {
    struct GrammarDir,
    crate_relative = "src/",
    root_relative = "crates/grammars/src",
    exclude = ["*.rs"],
}

/// Register all built-in native tree-sitter grammars with the provided registration function.
///
/// Each grammar is registered as a `(&str, tree_sitter_language::LanguageFn)` pair.
/// This must be called before loading language configs/queries.
#[cfg(feature = "load-grammars")]
pub fn native_grammars() -> Vec<(&'static str, tree_sitter::Language)> {
    vec![
        ("bash", tree_sitter_bash::LANGUAGE.into()),
        ("c", tree_sitter_c::LANGUAGE.into()),
        ("cobol", arborium_cobol::language().into()),
        ("cpp", tree_sitter_cpp::LANGUAGE.into()),
        ("css", tree_sitter_css::LANGUAGE.into()),
        ("diff", tree_sitter_diff::LANGUAGE.into()),
        ("go", tree_sitter_go::LANGUAGE.into()),
        ("gomod", tree_sitter_go_mod::LANGUAGE.into()),
        ("gotmpl", gotmpl()),
        ("gowork", tree_sitter_gowork::LANGUAGE.into()),
        ("java", tree_sitter_java::LANGUAGE.into()),
        ("csharp", tree_sitter_c_sharp::LANGUAGE.into()),
        ("php", tree_sitter_php::LANGUAGE_PHP.into()),
        ("ruby", tree_sitter_ruby::LANGUAGE.into()),
        ("swift", tree_sitter_swift::LANGUAGE.into()),
        ("r", tree_sitter_r::LANGUAGE.into()),
        ("perl", tree_sitter_perl::LANGUAGE.into()),
        ("fortran", tree_sitter_fortran::LANGUAGE.into()),
        ("pascal", tree_sitter_pascal::LANGUAGE.into()),
        ("asm", tree_sitter_asm::LANGUAGE.into()),
        ("vb6", tree_sitter_vb6::language()),
        ("jsdoc", tree_sitter_jsdoc::LANGUAGE.into()),
        ("json", tree_sitter_json::LANGUAGE.into()),
        ("jsonc", tree_sitter_json::LANGUAGE.into()),
        ("markdown", tree_sitter_md::LANGUAGE.into()),
        ("markdown-inline", tree_sitter_md::INLINE_LANGUAGE.into()),
        ("proto", tree_sitter_proto::LANGUAGE.into()),
        ("python", tree_sitter_python::LANGUAGE.into()),
        ("regex", tree_sitter_regex::LANGUAGE.into()),
        ("rust", tree_sitter_rust::LANGUAGE.into()),
        ("sql", tree_sitter_sequel::LANGUAGE.into()),
        ("xml", tree_sitter_xml::LANGUAGE_XML.into()),
        ("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()),
        (
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ),
        ("yaml", tree_sitter_yaml::LANGUAGE.into()),
        ("gitcommit", tree_sitter_gitcommit::LANGUAGE.into()),
    ]
}

/// The Go template grammar, compiled from the parser vendored under
/// `vendor/tree-sitter-gotmpl`.
///
/// Declared here rather than taken from the crate published beside that
/// grammar: that crate pins `tree-sitter` 0.19, whose `Language` is a
/// different type from the one this workspace uses.
#[cfg(feature = "load-grammars")]
fn gotmpl() -> tree_sitter::Language {
    unsafe extern "C" {
        fn tree_sitter_gotmpl() -> *const ();
    }
    // Safe because the symbol is the one the vendored parser exports, and it
    // is what a tree-sitter grammar's entry point always is.
    unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_gotmpl) }.into()
}

/// Every language whose files are embedded here, by the directory name its
/// queries and its config live under.
///
/// Needed by anything that wants to work through all of them rather than through
/// the grammars: a language may borrow another's grammar -- JavaScript is parsed
/// by the TSX one -- so the grammars are not the list of languages.
pub fn embedded_languages() -> Vec<String> {
    let mut names: Vec<String> = GrammarDir::iter()
        .filter_map(|path| {
            let (name, rest) = path.as_ref().split_once('/')?;
            (rest == "config.toml").then(|| name.to_string())
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Load and parse the `config.toml` for a given language name.
pub fn load_config(name: &str) -> LanguageConfig {
    let config_toml = String::from_utf8(
        GrammarDir::get(&format!("{}/config.toml", name))
            .unwrap_or_else(|| panic!("missing config for language {:?}", name))
            .data
            .to_vec(),
    )
    .unwrap();

    let config = LanguageConfig::from_toml(&config_toml)
        .with_context(|| format!("failed to load config.toml for language {name:?}"))
        .unwrap();

    config
}

/// Load and parse the `config.toml` for a given language name, stripping fields
/// that require grammar support when grammars are not loaded.
pub fn load_config_for_feature(name: &str, grammars_loaded: bool) -> LanguageConfig {
    let config = load_config(name);

    if grammars_loaded {
        config
    } else {
        LanguageConfig {
            name: config.name,
            matcher: config.matcher,
            jsx_tag_auto_close: config.jsx_tag_auto_close,
            ..Default::default()
        }
    }
}

/// Get a raw embedded file by path (relative to `src/`).
///
/// Returns the file data as bytes, or `None` if the file does not exist.
pub fn get_file(path: &str) -> Option<rust_embed::EmbeddedFile> {
    GrammarDir::get(path)
}

/// Load all Tree-sitter query files for a given language name.
pub fn load_queries(name: &str) -> LanguageQueries {
    LanguageQueries::from_files(GrammarDir::iter().filter_map(|path| {
        let file_name = path.strip_prefix(name)?.strip_prefix('/')?;
        let query_file = file_name.parse::<QueryFile>().ok()?;
        let contents = match GrammarDir::get(path.as_ref())?.data {
            Cow::Borrowed(bytes) => Cow::Borrowed(std::str::from_utf8(bytes).ok()?),
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8(bytes).ok()?),
        };
        Some(QueryFileContents::new(query_file, contents))
    }))
}
