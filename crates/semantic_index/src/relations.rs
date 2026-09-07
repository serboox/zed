use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

use anyhow::Result;

use crate::definitions::Definition;
use crate::languages;
use crate::per_language;
use crate::resolution::{self, WhatItMeans, WhyNot};
use crate::symbols::{Placement, Symbols};

/// How long either of the lists below may get.
///
/// The same bound, and the same reason, as `MOST_PLACES_WORTH_OPENING` in
/// `symbol_index`: a file with more than a thousand of anything is generated,
/// and a list nobody can read is not a better answer than one that says it
/// stopped. What matters is that a list cut short says so -- a reader takes a
/// short list for a complete one, and on this surface that mistake reads as
/// "nothing else depends on this".
pub const MOST_WORTH_LISTING: usize = 1000;

/// What the index can say about one file's place in the project, read from the
/// tables it already holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relations {
    pub path: String,
    /// The language the index recorded the file under, or the one its name
    /// claims where the file declares nothing.
    pub language: Option<String>,
    /// Where the file sits in the language's own arrangement of code, when a
    /// module tree reached it.
    pub placement: Option<Placement>,
    pub dependents: Dependents,
    /// The file's own top-level declarations, each with what became of it.
    pub exports: Vec<Export>,
    /// Set when the file declares more than a list holds.
    pub exports_cut_short: bool,
}

/// Which files bring in a name this one declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dependents {
    TheseFiles {
        files: Vec<Dependent>,
        cut_short: bool,
    },
    /// The question cannot be answered from what is stored, which is a
    /// different answer from "no file depends on this one".
    CannotTell(NoDependents),
}

/// One file that brings in something this one declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependent {
    pub path: String,
    /// The names it brings in, as this file declares them.
    pub names: Vec<String>,
    /// The first row it brings one in on, zero-based as every row here is.
    pub row: u32,
}

/// Why the dependency question has no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoDependents {
    /// The index has never read this file, so it holds no declarations of it to
    /// look for in anybody's imports.
    NothingIsIndexed,
    /// Nothing here knows what language the file is, so nothing can say whether
    /// its imports would have been read.
    LanguageIsUnknown,
    /// No hand-written walk reads what a file of this language brings into
    /// scope, so the imports table holds nothing for it -- and an empty list
    /// drawn from an empty table reads as "nothing depends on this", which is a
    /// claim nothing here supports.
    ImportsAreNotRead { language: String },
}

/// One of the file's own top-level declarations, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub declaration: Definition,
    pub usage: Usage,
}

/// What the index found written under one declaration, elsewhere in the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Usage {
    /// Places outside the declaring file that the index calls references to it.
    Used {
        elsewhere: usize,
    },
    /// No place outside the declaring file, and every gate below was passed --
    /// so this is the index's own claim and not the absence of one.
    UsedNowhere,
    CannotTell(WhyNotUsage),
}

/// Why a declaration's use cannot be reported.
///
/// Separate from [`WhyNot`] rather than folded into it: two of these are about
/// what the *negative* claim needs, not about whether the name resolves at all,
/// and a caller showing a reader why has to be able to say which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhyNotUsage {
    /// The index declines the name itself, for one of its own reasons.
    TheName(WhyNot),
    /// The name is also bound locally somewhere. Every occurrence inside such a
    /// binding was dropped rather than counted, so an empty count may be a use
    /// the index threw away.
    AlsoBoundLocally,
    /// A use written only on an import line would be invisible for this
    /// language, so nothing may be called unused in it.
    ImportsAreNotRead { language: String },
}

/// What the index can say about `path`, and nothing more than that.
///
/// Reads the tables the last pass left behind -- the declarations of the file,
/// what every other file brings into scope, and where each name is written --
/// and walks no file. A second pass over the project per question is the
/// appetite the store exists to remove, so a question this cannot answer from
/// those tables is declined instead.
pub fn relations_of(store: &Symbols, path: &str) -> Result<Relations> {
    let declared = store.in_file(path)?;
    let language = language_of(path, &declared);
    let placement = store.placement_of(path)?;

    let members: HashSet<String> = store.members_in(path)?.into_iter().collect();
    let mut top_level: Vec<Definition> = declared
        .into_iter()
        .filter(|found| !members.contains(&found.name))
        .collect();
    let exports_cut_short = top_level.len() > MOST_WORTH_LISTING;
    top_level.truncate(MOST_WORTH_LISTING);

    let mut exports = Vec::with_capacity(top_level.len());
    for declaration in top_level {
        let usage = what_became_of(store, path, &declaration.name, language.as_deref())?;
        exports.push(Export { declaration, usage });
    }

    let dependents = who_brings_in(store, path, language.as_deref(), &exports)?;
    Ok(Relations {
        path: path.to_string(),
        language,
        placement,
        dependents,
        exports,
        exports_cut_short,
    })
}

/// What the index found written under one of the file's declarations.
///
/// The positive claim and the negative one are not held to the same standard on
/// purpose. A count above zero rests on occurrences the index resolved, and is
/// worth reporting wherever it has any. "Used nowhere" is the claim a reader
/// acts on by deleting code, so it has to pass every gate the store can offer:
/// the name is declared once in the whole project, it is not a member of a type,
/// it is nowhere a local binding, and this language's imports are actually read.
fn what_became_of(
    store: &Symbols,
    path: &str,
    name: &str,
    language: Option<&str>,
) -> Result<Usage> {
    let answer = resolution::what_a_symbol_means(store, Some(path), name)?;
    let places = match answer {
        WhatItMeans::TheseAre(places) => places,
        WhatItMeans::AskTheServer(why) => return Ok(Usage::CannotTell(WhyNotUsage::TheName(why))),
    };
    let elsewhere = places.iter().filter(|place| place.path != path).count();
    if elsewhere > 0 {
        return Ok(Usage::Used { elsewhere });
    }
    if store.declarations_named(name)? > 1 {
        return Ok(Usage::CannotTell(WhyNotUsage::TheName(
            WhyNot::DeclaredMoreThanOnce,
        )));
    }
    if !store.locals_named(name)?.is_empty() {
        return Ok(Usage::CannotTell(WhyNotUsage::AlsoBoundLocally));
    }
    match language {
        Some(language) if per_language::imports_are_read(language) => Ok(Usage::UsedNowhere),
        Some(language) => Ok(Usage::CannotTell(WhyNotUsage::ImportsAreNotRead {
            language: language.to_string(),
        })),
        None => Ok(Usage::CannotTell(WhyNotUsage::TheName(
            WhyNot::NothingIsIndexed,
        ))),
    }
}

/// Which files bring in a name this one declares.
///
/// A name is attributed to this file only where the project declares it once:
/// an import of a name two files declare could be naming either, and a reader
/// told the wrong file depends on this one has been told something false rather
/// than something incomplete. The path an import stands for has to end in the
/// name as well, which is what keeps `use thing as other` from counting `other`
/// as a name declared here.
fn who_brings_in(
    store: &Symbols,
    path: &str,
    language: Option<&str>,
    exports: &[Export],
) -> Result<Dependents> {
    if !store.knows(path)? {
        return Ok(Dependents::CannotTell(NoDependents::NothingIsIndexed));
    }
    let Some(language) = language else {
        return Ok(Dependents::CannotTell(NoDependents::LanguageIsUnknown));
    };
    if !per_language::imports_are_read(language) {
        return Ok(Dependents::CannotTell(NoDependents::ImportsAreNotRead {
            language: language.to_string(),
        }));
    }

    let mut brought_in: BTreeMap<String, (Vec<String>, u32)> = BTreeMap::new();
    for export in exports {
        let name = export.declaration.name.as_str();
        if store.declarations_named(name)? != 1 {
            continue;
        }
        for (importer, import) in store.imports_named(name)? {
            if importer == path {
                continue;
            }
            if import.means.rsplit("::").next() != Some(name) {
                continue;
            }
            let entry = brought_in
                .entry(importer)
                .or_insert_with(|| (Vec::new(), import.row));
            entry.0.push(name.to_string());
            entry.1 = entry.1.min(import.row);
        }
    }

    let cut_short = brought_in.len() > MOST_WORTH_LISTING;
    let files = brought_in
        .into_iter()
        .take(MOST_WORTH_LISTING)
        .map(|(path, (mut names, row))| {
            names.sort();
            names.dedup();
            Dependent { path, names, row }
        })
        .collect();
    Ok(Dependents::TheseFiles { files, cut_short })
}

/// The language the index recorded the file under, or -- for a file that
/// declares nothing, so carries no recorded language -- the one its name claims.
///
/// The recorded language comes first because it is what the pass actually parsed
/// the file with, and the name is only a claim about that.
fn language_of(path: &str, declared: &[Definition]) -> Option<String> {
    if let Some(first) = declared.first() {
        return Some(first.language.clone());
    }
    static CLAIMED: OnceLock<std::collections::HashMap<String, String>> = OnceLock::new();
    let claimed = CLAIMED.get_or_init(languages::by_suffix);
    let name = path.rsplit('/').next().unwrap_or(path);
    languages::of_file(name, claimed).map(str::to_string)
}

/// What every answer here rests on, in the words a reader needs before acting
/// on one.
///
/// Shown rather than implied. The index matches imports and names as the
/// grammar reports them and infers no types, so a use reached through a
/// re-export, a dynamic import or a macro is one it never saw -- and a reader
/// who takes "used nowhere" for proof deletes working code.
pub fn what_this_rests_on() -> String {
    format!(
        "Read from the index's own tables, and from no compiler. Imports and names are matched as \
         the grammar reports them: a use reached through a re-export, a path built at run time or \
         a name a macro wrote was never seen here. Imports are read for {}; for every other \
         language this says \"cannot tell\" rather than \"nothing\". Read \"used nowhere\" as \
         \"the index found nothing\", never as proof that nothing uses it.",
        a_list_of(per_language::IMPORTS_ARE_READ_FOR)
    )
}

/// The names joined the way a sentence joins them, so the note above reads as
/// prose whether one language reads imports or five do.
fn a_list_of(names: &[&str]) -> String {
    match names {
        [] => "no language".to_string(),
        [only] => (*only).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::{FileFacts, Import, Local, Occurrence};

    fn definition(path: &str, name: &str, line: u32) -> Definition {
        Definition {
            path: path.to_string(),
            name: name.to_string(),
            kind: "function_item".to_string(),
            line,
            language: "rust".to_string(),
        }
    }

    fn import(name: &str, means: &str, row: u32) -> Import {
        Import {
            name: name.to_string(),
            means: means.to_string(),
            row,
            column: 8,
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

    /// Three files with no language server anywhere near them: one declares
    /// three names, two others bring one of them in and write it three times
    /// between them, and two of the three names are written nowhere at all.
    fn a_small_project() -> Symbols {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record(
                "src/stock.rs",
                &[
                    definition("src/stock.rs", "take_stock", 3),
                    definition("src/stock.rs", "count_shelves", 9),
                    definition("src/stock.rs", "unswept", 15),
                ],
            )
            .expect("the declaring file");
        store
            .record_facts(
                "src/one.rs",
                &FileFacts {
                    imports: vec![import("take_stock", "crate::stock::take_stock", 2)],
                    ..Default::default()
                },
            )
            .expect("the first importer");
        store
            .record_facts(
                "src/two.rs",
                &FileFacts {
                    occurrences: vec![occurrence("take_stock", 5)],
                    imports: vec![import("take_stock", "crate::stock::take_stock", 3)],
                    ..Default::default()
                },
            )
            .expect("the second importer");
        store
    }

    fn usage_of(relations: &Relations, name: &str) -> Usage {
        relations
            .exports
            .iter()
            .find(|export| export.declaration.name == name)
            .map(|export| export.usage.clone())
            .unwrap_or_else(|| panic!("{name} is not among this file's exports"))
    }

    #[test]
    fn a_file_two_others_import_is_depended_upon_by_exactly_those_two() {
        let store = a_small_project();
        let relations = relations_of(&store, "src/stock.rs").expect("an answer");
        let Dependents::TheseFiles { files, cut_short } = relations.dependents.clone() else {
            panic!(
                "expected a list of dependents, found {:?}",
                relations.dependents
            );
        };
        assert!(!cut_short, "two files fit in any list");
        let named: Vec<(&str, &[String])> = files
            .iter()
            .map(|found| (found.path.as_str(), found.names.as_slice()))
            .collect();
        assert_eq!(
            named,
            vec![
                ("src/one.rs", ["take_stock".to_string()].as_slice()),
                ("src/two.rs", ["take_stock".to_string()].as_slice()),
            ]
        );
        assert_eq!(files[0].row, 2, "the row the import is written on");
        assert_eq!(files[1].row, 3);
    }

    #[test]
    fn an_export_used_three_times_reports_three() {
        let store = a_small_project();
        let relations = relations_of(&store, "src/stock.rs").expect("an answer");
        // Two import lines and one occurrence. The line that brings a name in is
        // a place the name is written, and a rename that missed it would not
        // compile -- so it counts as a use like any other.
        assert_eq!(
            usage_of(&relations, "take_stock"),
            Usage::Used { elsewhere: 3 }
        );
    }

    #[test]
    fn a_declaration_nothing_mentions_is_reported_as_used_nowhere() {
        let store = a_small_project();
        let relations = relations_of(&store, "src/stock.rs").expect("an answer");
        assert_eq!(usage_of(&relations, "unswept"), Usage::UsedNowhere);
        assert_eq!(usage_of(&relations, "count_shelves"), Usage::UsedNowhere);
    }

    /// The gate the whole surface turns on: where any part of the confidence
    /// test fails, the answer is "cannot tell" and never either of the two real
    /// answers. A reader who takes an unsupported "used nowhere" on faith
    /// deletes working code.
    #[test]
    fn a_name_that_is_also_a_member_of_a_type_cannot_be_told_about() {
        let store = a_small_project();
        store
            .record_facts(
                "src/shelf.rs",
                &FileFacts {
                    members: vec!["unswept".to_string()],
                    ..Default::default()
                },
            )
            .expect("a type with a member of that name");
        let relations = relations_of(&store, "src/stock.rs").expect("an answer");
        assert_eq!(
            usage_of(&relations, "unswept"),
            Usage::CannotTell(WhyNotUsage::TheName(WhyNot::AMemberOfAType))
        );
    }

    #[test]
    fn a_name_declared_twice_cannot_be_called_unused() {
        let store = a_small_project();
        store
            .record("src/spare.rs", &[definition("src/spare.rs", "unswept", 4)])
            .expect("a second declaration of the same name");
        let relations = relations_of(&store, "src/stock.rs").expect("an answer");
        assert_eq!(
            usage_of(&relations, "unswept"),
            Usage::CannotTell(WhyNotUsage::TheName(WhyNot::DeclaredMoreThanOnce))
        );
    }

    /// A name bound locally somewhere has every occurrence inside that binding
    /// dropped rather than counted, so a count of zero may be a use the index
    /// threw away -- which is not the same as no use at all.
    #[test]
    fn a_name_also_bound_locally_cannot_be_called_unused() {
        let store = a_small_project();
        store
            .record_facts(
                "src/three.rs",
                &FileFacts {
                    occurrences: vec![occurrence("unswept", 6)],
                    locals: vec![Local {
                        name: "unswept".to_string(),
                        from_row: 4,
                        to_row: 20,
                    }],
                    ..Default::default()
                },
            )
            .expect("a local of that name over those rows");
        let relations = relations_of(&store, "src/stock.rs").expect("an answer");
        assert_eq!(
            usage_of(&relations, "unswept"),
            Usage::CannotTell(WhyNotUsage::AlsoBoundLocally)
        );
    }

    /// The imports table is filled by a hand-written walk that exists for one
    /// language. For every other language the table is empty, and an empty
    /// table must read as "cannot tell" rather than as an empty list -- an empty
    /// list here says "nothing in the project depends on this file".
    #[test]
    fn a_language_whose_imports_are_not_read_cannot_answer_the_dependency_question() {
        let store = Symbols::open_in_memory().expect("a store");
        let mut declared = definition("src/stock.py", "take_stock", 3);
        declared.language = "python".to_string();
        store
            .record("src/stock.py", &[declared])
            .expect("a Python file");
        let relations = relations_of(&store, "src/stock.py").expect("an answer");
        assert_eq!(
            relations.dependents,
            Dependents::CannotTell(NoDependents::ImportsAreNotRead {
                language: "python".to_string()
            }),
            "an empty list would claim nothing depends on it"
        );
        assert_eq!(
            usage_of(&relations, "take_stock"),
            Usage::CannotTell(WhyNotUsage::ImportsAreNotRead {
                language: "python".to_string()
            }),
            "nor may anything in it be called unused"
        );
    }

    #[test]
    fn a_file_the_index_has_never_read_says_so_rather_than_answering() {
        let store = a_small_project();
        let relations = relations_of(&store, "src/never_seen.rs").expect("an answer");
        assert_eq!(
            relations.dependents,
            Dependents::CannotTell(NoDependents::NothingIsIndexed)
        );
        assert!(relations.exports.is_empty());
    }

    /// A list cut short has to say so. A reader takes a short list for a
    /// complete one, and here that mistake reads as "this is everything the
    /// file exports".
    #[test]
    fn a_list_cut_short_says_it_was_cut_short() {
        let store = Symbols::open_in_memory().expect("a store");
        let many: Vec<Definition> = (0..MOST_WORTH_LISTING + 1)
            .map(|at| definition("src/generated.rs", &format!("name_{at}"), at as u32 + 1))
            .collect();
        store
            .record("src/generated.rs", &many)
            .expect("a generated file");
        let relations = relations_of(&store, "src/generated.rs").expect("an answer");
        assert!(relations.exports_cut_short, "the list was cut short");
        assert_eq!(relations.exports.len(), MOST_WORTH_LISTING);
    }

    #[test]
    fn where_the_file_sits_is_reported_where_a_module_tree_reached_it() {
        let store = a_small_project();
        store
            .record_placement(
                "src/stock.rs",
                Some(&Placement {
                    unit: "shopkeeping".to_string(),
                    path: "crate::stock".to_string(),
                }),
            )
            .expect("recording where the file sits");
        let relations = relations_of(&store, "src/stock.rs").expect("an answer");
        assert_eq!(
            relations.placement,
            Some(Placement {
                unit: "shopkeeping".to_string(),
                path: "crate::stock".to_string()
            })
        );
    }

    /// `use thing as other` records both names against the one path. Counting
    /// the alias as a name declared here would name a file a dependent of
    /// whichever file happens to declare something called `other`.
    #[test]
    fn an_alias_is_not_taken_for_a_name_this_file_declares() {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record(
                "src/stock.rs",
                &[definition("src/stock.rs", "take_stock", 3)],
            )
            .expect("the declaring file");
        store
            .record_facts(
                "src/one.rs",
                &FileFacts {
                    imports: vec![import("take_stock", "crate::stock::count_shelves", 2)],
                    ..Default::default()
                },
            )
            .expect("an import whose path names something else");
        let relations = relations_of(&store, "src/stock.rs").expect("an answer");
        assert_eq!(
            relations.dependents,
            Dependents::TheseFiles {
                files: Vec::new(),
                cut_short: false
            },
            "the path it stands for does not end in the name"
        );
    }

    /// A method of a type is not something the file exports, and listing every
    /// one of them as a "cannot tell" would bury the handful of answers the
    /// surface exists for.
    #[test]
    fn a_member_of_a_type_is_not_one_of_the_files_exports() {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record(
                "src/stock.rs",
                &[
                    definition("src/stock.rs", "Shelf", 1),
                    definition("src/stock.rs", "sweep", 6),
                ],
            )
            .expect("a type and a method on it");
        store
            .record_facts(
                "src/stock.rs",
                &FileFacts {
                    members: vec!["sweep".to_string()],
                    ..Default::default()
                },
            )
            .expect("marking the method");
        let relations = relations_of(&store, "src/stock.rs").expect("an answer");
        let names: Vec<&str> = relations
            .exports
            .iter()
            .map(|export| export.declaration.name.as_str())
            .collect();
        assert_eq!(names, vec!["Shelf"]);
    }

    #[test]
    fn the_basis_names_the_languages_whose_imports_are_read() {
        let said = what_this_rests_on();
        for language in per_language::IMPORTS_ARE_READ_FOR {
            assert!(said.contains(language), "{said}");
        }
        assert!(said.contains("cannot tell"), "{said}");
        assert!(said.contains("never as proof"), "{said}");
    }

    #[test]
    fn a_list_of_names_reads_as_prose() {
        assert_eq!(a_list_of(&[]), "no language");
        assert_eq!(a_list_of(&["rust"]), "rust");
        assert_eq!(a_list_of(&["rust", "go"]), "rust and go");
        assert_eq!(a_list_of(&["rust", "go", "python"]), "rust, go and python");
    }
}
