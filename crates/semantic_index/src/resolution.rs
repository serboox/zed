use anyhow::Result;

use crate::symbols::Symbols;

/// What the index is prepared to say about a name, with no language server
/// running.
///
/// The rule the whole thing rests on: answer where the name means one thing,
/// and say nothing where it does not. Measured, answering about every name put
/// precision at 0.9% -- 78,926 of 78,930 wrong answers were "the name also
/// names something else" -- while declining the ambiguous ones put it at 97.7%.
/// An index that is right about a third of names and knows which third is worth
/// more than one that answers always and is usually wrong, because what is
/// built on top of it is a rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhatItMeans {
    /// Every place the index calls a reference to this name.
    TheseAre(Vec<Where>),
    /// The index will not say. Which rule stopped it is part of the answer:
    /// what shows it to a reader says why, and what measures it counts by
    /// reason -- which is how the next thing worth building is chosen.
    AskTheServer(WhyNot),
}

/// One place a name is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Where {
    pub path: String,
    /// Zero-based, as tree-sitter and the LSP protocol both count.
    pub row: u32,
    pub column: u32,
}

/// Why the index declined to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhyNot {
    /// The project declares this name more than once, and nothing here can say
    /// which one an occurrence means.
    DeclaredMoreThanOnce,
    /// The name is also bound locally somewhere, and the store does not hold
    /// the rows of that binding -- which is the only thing that could tell an
    /// occurrence inside it from one outside.
    AlsoALocalBinding,
    /// The name is a member of a type somewhere. Almost every occurrence of
    /// such a name is written on a value -- `thing.name()` -- and which type
    /// that value has is the one thing this knows nothing about.
    AMemberOfAType,
    /// Nothing in the store declares this name at all, so there is nothing to
    /// be right or wrong about: an unbuilt index and an unknown name look the
    /// same from here, and both mean the same thing to the caller.
    NothingIsIndexed,
    /// The name is declared more than once and something written under it was
    /// not accounted for -- an occurrence nothing resolved, or a line importing
    /// the name that this cannot attribute to one declaration. Answering with
    /// what *is* accounted for would be worse than not answering: a rename that
    /// finds a fraction of its occurrences breaks the build without saying so.
    NotEverythingIsExplained,
}

/// What the index says about a name declared in one place.
///
/// Reads what the store already holds rather than walking the project: the
/// resolution a question needs was worked out when each file was last read, so
/// asking is a few indexed lookups and not a pass over every file. A pass per
/// question is the appetite this store exists to remove.
pub fn what_a_name_means(store: &Symbols, name: &str) -> Result<WhatItMeans> {
    what_a_symbol_means(store, None, name)
}

/// What the index says about one symbol: the name, and the file that declares
/// the one being asked about.
///
/// The file matters as soon as the name is declared more than once, and it is
/// the shape every caller actually has -- a rename is invoked on a symbol, not
/// on a word. Asked without it, a name declared twice can only be declined,
/// because "which of the two" has no answer.
pub fn what_a_symbol_means(
    store: &Symbols,
    declared_in: Option<&str>,
    name: &str,
) -> Result<WhatItMeans> {
    let declared = store.declarations_named(name)?;
    if declared == 0 {
        return Ok(WhatItMeans::AskTheServer(WhyNot::NothingIsIndexed));
    }
    if store.is_a_member_name(name)? {
        return Ok(WhatItMeans::AskTheServer(WhyNot::AMemberOfAType));
    }
    if declared > 1 {
        let Some(declared_in) = declared_in else {
            return Ok(WhatItMeans::AskTheServer(WhyNot::DeclaredMoreThanOnce));
        };
        return what_one_of_several_means(store, declared_in, name);
    }

    // A name that is also somebody's local is ambiguous only where that local
    // is in scope. Declining it everywhere -- which is what a store of bare
    // names forces -- is what costs the index two names in three, and the rows
    // are held precisely so that this can be a filter instead of a refusal.
    let bound = store.locals_named(name)?;
    let mut places: Vec<Where> = store
        .occurrences_named(name)?
        .into_iter()
        .filter(|(path, occurrence)| {
            !bound.iter().any(|(bound_in, local)| {
                bound_in == path
                    && occurrence.row >= local.from_row
                    && occurrence.row <= local.to_row
            })
        })
        .map(|(path, occurrence)| Where {
            path,
            row: occurrence.row,
            column: occurrence.column,
        })
        .collect();

    // The lines that bring the name in. The references query does not capture
    // an imported name -- there it is a plain identifier, and capturing it
    // would widen the query to every identifier in the file -- so they are
    // added from what each file was recorded as importing. A rename that
    // changed every use and left the `use` line spelling the old name does not
    // compile.
    for (path, import) in store.imports_named(name)? {
        places.push(Where {
            path,
            row: import.row,
            column: import.column,
        });
    }
    places.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.row.cmp(&right.row))
            .then(left.column.cmp(&right.column))
    });
    places.dedup();
    Ok(WhatItMeans::TheseAre(places))
}

/// A name the project declares more than once, asked about one of them.
///
/// Measured on this fork: of 350 such names only 86 have their declarations
/// inside one crate; the other 264 are told apart by which crate and module
/// each is in, which needs no type inference -- and is what each occurrence was
/// resolved to when its file was last read.
fn what_one_of_several_means(
    store: &Symbols,
    declared_in: &str,
    name: &str,
) -> Result<WhatItMeans> {
    let occurrences = store.occurrences_named(name)?;
    // One occurrence nothing resolved is enough to withhold the whole answer.
    // The alternative -- answer with what was resolved -- was measured: 75 of
    // 110 shared names answered with an empty list, and per-symbol recall came
    // to 61%. A rename that finds some of its occurrences is worse than one
    // that admits it cannot.
    if occurrences
        .iter()
        .any(|(_, occurrence)| occurrence.resolves_to.is_none())
    {
        return Ok(WhatItMeans::AskTheServer(WhyNot::NotEverythingIsExplained));
    }
    // A line importing an ambiguous name is not attributed to one declaration
    // here: that needs the path the import stands for to be resolved the way a
    // compiler resolves it. Until that is recorded beside the import, a name
    // imported anywhere is declined rather than renamed halfway -- the `use`
    // line left spelling the old name is a build failure, not a cosmetic miss.
    if !store.imports_named(name)?.is_empty() {
        return Ok(WhatItMeans::AskTheServer(WhyNot::NotEverythingIsExplained));
    }

    let bound = store.locals_named(name)?;
    let mut places: Vec<Where> = occurrences
        .into_iter()
        .filter(|(_, occurrence)| occurrence.resolves_to.as_deref() == Some(declared_in))
        .filter(|(path, occurrence)| {
            !bound.iter().any(|(bound_in, local)| {
                bound_in == path
                    && occurrence.row >= local.from_row
                    && occurrence.row <= local.to_row
            })
        })
        .map(|(path, occurrence)| Where {
            path,
            row: occurrence.row,
            column: occurrence.column,
        })
        .collect();
    places.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.row.cmp(&right.row))
            .then(left.column.cmp(&right.column))
    });
    places.dedup();
    Ok(WhatItMeans::TheseAre(places))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definitions::Definition;
    use crate::symbols::{FileFacts, Local, Occurrence};

    fn definition(path: &str, name: &str, line: u32) -> Definition {
        Definition {
            path: path.to_string(),
            name: name.to_string(),
            kind: "function_item".to_string(),
            line,
            language: "rust".to_string(),
        }
    }

    fn occurrence(name: &str, row: u32) -> Occurrence {
        Occurrence {
            name: name.to_string(),
            row,
            column: 4,
            resolves_to: None,
        }
    }

    fn a_store_with_one_declaration() -> Symbols {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record("src/lib.rs", &[definition("src/lib.rs", "thing", 3)])
            .expect("recording a definition");
        store
            .record_facts(
                "src/other.rs",
                &FileFacts {
                    occurrences: vec![occurrence("thing", 9), occurrence("thing", 14)],
                    ..Default::default()
                },
            )
            .expect("recording occurrences");
        store
    }

    #[test]
    fn a_name_declared_once_is_answered_about() {
        let store = a_store_with_one_declaration();
        let answer = what_a_name_means(&store, "thing").expect("an answer");
        assert_eq!(
            answer,
            WhatItMeans::TheseAre(vec![
                Where {
                    path: "src/other.rs".to_string(),
                    row: 9,
                    column: 4,
                },
                Where {
                    path: "src/other.rs".to_string(),
                    row: 14,
                    column: 4,
                },
            ])
        );
    }

    #[test]
    fn a_name_declared_twice_is_left_to_the_server() {
        let store = a_store_with_one_declaration();
        store
            .record("src/second.rs", &[definition("src/second.rs", "thing", 8)])
            .expect("a second declaration");
        assert_eq!(
            what_a_name_means(&store, "thing").expect("an answer"),
            WhatItMeans::AskTheServer(WhyNot::DeclaredMoreThanOnce)
        );
    }

    // A name that is also somebody's local is ambiguous only where that local
    // is in scope. The occurrence inside those rows is left out; the one
    // outside them is still answered about -- which is the difference between
    // an index that answers about a third of names and one that answers about
    // most of them.
    #[test]
    fn an_occurrence_inside_a_local_is_left_out_and_the_rest_are_answered() {
        let store = a_store_with_one_declaration();
        store
            .record_facts(
                "src/other.rs",
                &FileFacts {
                    occurrences: vec![occurrence("thing", 9), occurrence("thing", 14)],
                    locals: vec![Local {
                        name: "thing".to_string(),
                        from_row: 12,
                        to_row: 20,
                    }],
                    ..Default::default()
                },
            )
            .expect("recording a local beside the occurrences");
        assert_eq!(
            what_a_name_means(&store, "thing").expect("an answer"),
            WhatItMeans::TheseAre(vec![Where {
                path: "src/other.rs".to_string(),
                row: 9,
                column: 4,
            }])
        );
    }

    // A rename has to touch the line that brings the name in, and the
    // references query deliberately does not capture it.
    #[test]
    fn the_line_that_imports_the_name_is_one_of_the_places() {
        let store = a_store_with_one_declaration();
        store
            .record_facts(
                "src/third.rs",
                &FileFacts {
                    imports: vec![crate::symbols::Import {
                        name: "thing".to_string(),
                        means: "crate::lib::thing".to_string(),
                        row: 2,
                        column: 16,
                    }],
                    ..Default::default()
                },
            )
            .expect("recording an import");
        let answer = what_a_name_means(&store, "thing").expect("an answer");
        let WhatItMeans::TheseAre(places) = answer else {
            panic!("a name declared once is answered about");
        };
        assert!(
            places.contains(&Where {
                path: "src/third.rs".to_string(),
                row: 2,
                column: 16,
            }),
            "the import line is missing from {places:?}"
        );
    }

    // The rows are what make that possible, so a local recorded without them
    // would be a silent return to declining everything. Guarded here: a local
    // covering every row of the file leaves nothing to answer about.
    #[test]
    fn a_local_covering_the_whole_file_leaves_nothing_to_answer() {
        let store = a_store_with_one_declaration();
        store
            .record_facts(
                "src/other.rs",
                &FileFacts {
                    occurrences: vec![occurrence("thing", 9), occurrence("thing", 14)],
                    locals: vec![Local {
                        name: "thing".to_string(),
                        from_row: 0,
                        to_row: 1000,
                    }],
                    ..Default::default()
                },
            )
            .expect("recording a local over everything");
        assert_eq!(
            what_a_name_means(&store, "thing").expect("an answer"),
            WhatItMeans::TheseAre(Vec::new())
        );
    }

    // Answering about members measured recall at 6.3%: 119 of 166 shared names
    // had nothing the tree could attribute, and a rename that finds a fraction
    // of its occurrences breaks the build without saying so.
    #[test]
    fn a_member_of_a_type_is_left_to_the_server() {
        let store = a_store_with_one_declaration();
        store
            .record_members("src/lib.rs", &["thing".to_string()])
            .expect("marking");
        assert_eq!(
            what_a_name_means(&store, "thing").expect("an answer"),
            WhatItMeans::AskTheServer(WhyNot::AMemberOfAType)
        );
    }

    // Two declarations of one name, asked about one of them: the occurrences
    // that were resolved to the other are not this symbol's.
    #[test]
    fn one_of_two_declarations_is_answered_about_by_what_each_occurrence_resolved_to() {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record("src/first.rs", &[definition("src/first.rs", "thing", 3)])
            .expect("the first declaration");
        store
            .record("src/second.rs", &[definition("src/second.rs", "thing", 3)])
            .expect("the second declaration");
        store
            .record_facts(
                "src/uses.rs",
                &FileFacts {
                    occurrences: vec![
                        Occurrence {
                            name: "thing".to_string(),
                            row: 5,
                            column: 4,
                            resolves_to: Some("src/first.rs".to_string()),
                        },
                        Occurrence {
                            name: "thing".to_string(),
                            row: 9,
                            column: 4,
                            resolves_to: Some("src/second.rs".to_string()),
                        },
                    ],
                    ..Default::default()
                },
            )
            .expect("recording occurrences");

        assert_eq!(
            what_a_symbol_means(&store, Some("src/first.rs"), "thing").expect("an answer"),
            WhatItMeans::TheseAre(vec![Where {
                path: "src/uses.rs".to_string(),
                row: 5,
                column: 4,
            }])
        );
    }

    // Asked without saying which of the two, there is no answer to give.
    #[test]
    fn a_name_declared_twice_asked_about_as_a_bare_name_is_left_to_the_server() {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record("src/first.rs", &[definition("src/first.rs", "thing", 3)])
            .expect("the first declaration");
        store
            .record("src/second.rs", &[definition("src/second.rs", "thing", 3)])
            .expect("the second declaration");
        assert_eq!(
            what_a_name_means(&store, "thing").expect("an answer"),
            WhatItMeans::AskTheServer(WhyNot::DeclaredMoreThanOnce)
        );
    }

    // One occurrence nothing resolved withholds the whole answer.
    #[test]
    fn an_unexplained_occurrence_withholds_the_answer_for_an_ambiguous_name() {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record("src/first.rs", &[definition("src/first.rs", "thing", 3)])
            .expect("the first declaration");
        store
            .record("src/second.rs", &[definition("src/second.rs", "thing", 3)])
            .expect("the second declaration");
        store
            .record_facts(
                "src/uses.rs",
                &FileFacts {
                    occurrences: vec![
                        Occurrence {
                            name: "thing".to_string(),
                            row: 5,
                            column: 4,
                            resolves_to: Some("src/first.rs".to_string()),
                        },
                        occurrence("thing", 9),
                    ],
                    ..Default::default()
                },
            )
            .expect("recording occurrences");
        assert_eq!(
            what_a_symbol_means(&store, Some("src/first.rs"), "thing").expect("an answer"),
            WhatItMeans::AskTheServer(WhyNot::NotEverythingIsExplained)
        );
    }

    #[test]
    fn a_name_the_store_never_heard_of_is_left_to_the_server() {
        let store = a_store_with_one_declaration();
        assert_eq!(
            what_a_name_means(&store, "nothing_like_this").expect("an answer"),
            WhatItMeans::AskTheServer(WhyNot::NothingIsIndexed)
        );
    }
}
