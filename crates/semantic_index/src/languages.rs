use std::collections::HashMap;

/// One language the index can read: the grammar that parses it, the query that
/// finds its definitions, and the file names it claims.
pub struct Readable {
    pub name: String,
    pub grammar: tree_sitter::Language,
    pub outline: tree_sitter::Query,
    pub suffixes: Vec<String>,
}

/// Which language claims each file suffix the editor ships a grammar for.
///
/// Every embedded language, not only those with an outline query: the inventory
/// records what a file *is* before anything looks inside it, and a language
/// without a query today may have one tomorrow.
pub fn by_suffix() -> HashMap<String, String> {
    let mut claimed = HashMap::new();
    for name in grammars::embedded_languages() {
        for suffix in grammars::load_config(&name).matcher.path_suffixes {
            // First claim wins, and the languages come back in a stable order,
            // so which one that is does not change between runs.
            claimed.entry(suffix).or_insert_with(|| name.clone());
        }
    }
    claimed
}

/// The language a file name belongs to, by the longest suffix that fits -- so
/// `.d.ts` wins over `.ts` where two languages claim both.
pub fn of_file<'a>(name: &str, by_suffix: &'a HashMap<String, String>) -> Option<&'a str> {
    by_suffix
        .iter()
        .filter(|(suffix, _)| name.ends_with(suffix.as_str()))
        .max_by_key(|(suffix, _)| suffix.len())
        .map(|(_, language)| language.as_str())
}

/// Every language the editor ships that already has an outline query, which is
/// the set the index plan starts from.
///
/// Worked out from the languages rather than from the grammars: a language may
/// borrow another's grammar -- JavaScript is parsed by the TSX one -- so walking
/// the grammars would silently leave such a language out of every number.
///
/// A query that does not compile is left out and named, rather than bringing the
/// whole thing down: what is wanted is a measurement of what there is.
pub fn readable() -> (Vec<Readable>, Vec<String>) {
    let grammars: HashMap<String, tree_sitter::Language> = grammars::native_grammars()
        .into_iter()
        .map(|(name, grammar)| (name.to_string(), grammar))
        .collect();

    let mut readable = Vec::new();
    let mut refused = Vec::new();
    for name in grammars::embedded_languages() {
        let Some(outline) = grammars::load_queries(&name).outline else {
            continue;
        };
        let config = grammars::load_config(&name);
        // A config that names no grammar is parsed by the one named after it.
        let parsed_by = config
            .grammar
            .as_ref()
            .map(|grammar| grammar.to_string())
            .unwrap_or_else(|| name.clone());
        let Some(grammar) = grammars.get(&parsed_by) else {
            refused.push(format!(
                "{name}: its grammar {parsed_by:?} is not one of the built-in ones"
            ));
            continue;
        };
        match tree_sitter::Query::new(grammar, outline.as_ref()) {
            Ok(outline) => readable.push(Readable {
                name,
                grammar: grammar.clone(),
                outline,
                suffixes: config.matcher.path_suffixes,
            }),
            Err(trouble) => refused.push(format!("{name}: {trouble}")),
        }
    }
    (readable, refused)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_outline_query_compiles_against_its_own_grammar() {
        let (readable, refused) = readable();
        assert!(
            refused.is_empty(),
            "an outline query the editor ships does not compile: {refused:?}"
        );
        // Thirteen directories ship an outline query, and every one of them has
        // to be readable -- including JavaScript, which is parsed by the TSX
        // grammar and would be dropped by anything walking the grammars.
        assert_eq!(
            readable.len(),
            13,
            "readable: {:?}",
            readable
                .iter()
                .map(|language| language.name.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            readable
                .iter()
                .any(|language| language.name == "javascript"),
            "JavaScript borrows the TSX grammar and still has to be measured"
        );
        assert!(
            readable.iter().any(|language| language.name == "rust"),
            "Rust is the language the plan starts from"
        );
    }

    #[test]
    fn a_file_belongs_to_the_language_that_claims_the_longest_suffix() {
        let claimed = by_suffix();
        assert_eq!(of_file("main.rs", &claimed), Some("rust"));
        assert_eq!(of_file("go.mod", &claimed), Some("gomod"));
        assert_eq!(of_file("notes.txt", &claimed), None);

        // `go.mod` is claimed whole by one language while `go` is claimed by
        // another: the longest suffix has to win or every module file would be
        // recorded as Go source.
        let mut two = HashMap::new();
        two.insert("go".to_string(), "go".to_string());
        two.insert("go.mod".to_string(), "gomod".to_string());
        assert_eq!(of_file("go.mod", &two), Some("gomod"));
        assert_eq!(of_file("main.go", &two), Some("go"));
    }

    #[test]
    fn the_suffix_map_is_the_same_every_time_it_is_built() {
        // Two languages may claim one suffix; which of them gets it must not
        // depend on the order a hash map happened to hand them over, or a file's
        // recorded language would change between runs for no reason.
        let once = by_suffix();
        for _ in 0..5 {
            assert_eq!(by_suffix(), once);
        }
    }
}
