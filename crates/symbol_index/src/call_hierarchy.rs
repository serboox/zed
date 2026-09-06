use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use collections::HashSet;
use gpui::{App, AppContext as _, Entity, Task, WeakEntity};
use language::{Buffer, Location, PointUtf16};
use project::Project;
use semantic_index::references::references_and_what_encloses_them;
use semantic_index::resolution::{WhatItMeans, Where};
use text::ToOffset as _;
use util::ResultExt as _;

use crate::index_semantics::{MOST_PLACES_WORTH_OPENING, name_on_line, point_of, word_at};
use crate::{Definition, SymbolIndex};

/// One end of a call the index worked out with no language server asked.
///
/// The two halves of a call hierarchy are within reach of what the index
/// already holds: who calls a declaration is its references, grouped by the
/// declaration each one is written inside, and what a declaration calls is the
/// names written as calls inside its own range, each resolved the way going to
/// a definition resolves one. Neither needs a server, and today a reader with
/// none running gets no hierarchy at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Called {
    /// The declaration's own name, or the file's own path where the call is
    /// written at file scope and there is no declaration to name.
    pub name: String,
    /// The grammar's own word for what the declaration is, or
    /// [`Called::FILE_SCOPE`].
    pub kind: String,
    pub path: PathBuf,
    /// Zero-based: the row the name to point a reader at is written on.
    pub row: u32,
}

impl Called {
    /// What `kind` says for a call written inside no declaration at all -- a
    /// call at file scope, a name in a file-level macro. The index will not
    /// name the declaration above such a call as its caller, because a
    /// declaration's starting line says nothing about where it ends, and a
    /// reader shown the wrong function cannot tell that it is wrong.
    pub const FILE_SCOPE: &str = "file scope";

    pub fn at_file_scope(&self) -> bool {
        self.kind == Self::FILE_SCOPE
    }
}

/// The declaration the cursor is on, as the root of a hierarchy the index
/// answers.
///
/// The same gate going to a definition goes through: one declaration of the
/// name in the whole project, or nothing. A hierarchy rooted at the wrong one
/// of two declarations is a tree of confident wrong answers.
pub fn declaration_under(
    index: &Entity<SymbolIndex>,
    buffer: &Entity<Buffer>,
    position: PointUtf16,
    cx: &App,
) -> Option<Called> {
    let snapshot = buffer.read(cx).snapshot();
    let offset = position.to_offset(&snapshot);
    let (_, name) = word_at(&snapshot, offset)?;
    let (path, declared) = index.read(cx).where_declared(&name)?;

    // The store holds what each file said when it was last read from disk, so
    // an unsaved edit above the declaration has moved the row this would point
    // at. Refused only where the declaration is in the very buffer being
    // edited: an edit here cannot move a row in another file.
    let open_path = buffer
        .read(cx)
        .file()
        .map(|file| file.path().to_string().replace('\\', "/"));
    if buffer.read(cx).is_dirty() && open_path.as_deref() == Some(declared.path.as_str()) {
        return None;
    }
    Some(called_from(path, &declared))
}

/// Who calls `name`: every place the index holds for it, each attributed to the
/// declaration the grammar of its own file says it is written inside.
///
/// `None` where the index will not say -- an ambiguous name, a member of a
/// type, an index that has not been built, or more places than a reader would
/// read -- and the caller is then in the position it was in before this
/// existed. An empty answer is different, and means the index found nothing
/// that calls it.
pub fn incoming(
    index: &Entity<SymbolIndex>,
    name: &str,
    cx: &mut App,
) -> Option<Task<Vec<Called>>> {
    let (root, places) = {
        let index = index.read(cx);
        let WhatItMeans::TheseAre(places) = index.what_a_name_means(name)? else {
            return None;
        };
        if places.len() > MOST_PLACES_WORTH_OPENING {
            return None;
        }
        (index.root().to_path_buf(), places)
    };
    let name = name.to_string();
    Some(cx.background_spawn(async move { callers_of(&root, &name, &places) }))
}

/// What the declaration `name` calls: the names written as calls inside its own
/// range, each resolved through the index the same way going to a definition
/// resolves one.
///
/// A callee the index declines is simply absent, rather than guessed at.
pub fn outgoing(
    index: &Entity<SymbolIndex>,
    name: &str,
    cx: &mut App,
) -> Option<Task<Vec<Called>>> {
    let (path, declared) = index.read(cx).where_declared(name)?;
    let index = index.downgrade();
    Some(cx.spawn(async move |cx| {
        let called = cx
            .background_spawn(async move { names_called_inside(&path, &declared) })
            .await;
        index
            .read_with(cx, |index, _| {
                called
                    .iter()
                    .filter_map(|name| {
                        let (path, declared) = index.where_declared(name)?;
                        Some(called_from(path, &declared))
                    })
                    .collect::<Vec<Called>>()
            })
            .log_err()
            .unwrap_or_default()
    }))
}

/// Opens the file the index named and points at the name on the row it
/// recorded, falling back to the start of that row where the name is no longer
/// written there.
pub async fn locate(
    project: WeakEntity<Project>,
    called: &Called,
    cx: &mut gpui::AsyncApp,
) -> Option<Location> {
    let opened = project
        .update(cx, |project, cx| {
            project.open_local_buffer(&called.path, cx)
        })
        .log_err()?
        .await
        .log_err()?;
    let range = opened.read_with(cx, |opened, _| {
        let snapshot = opened.snapshot();
        let at = name_on_line(&snapshot, called.row, &called.name)
            .or_else(|| point_of(&snapshot, called.row, 0).map(|start| start..start))?;
        Some(snapshot.anchor_before(at.start)..snapshot.anchor_after(at.end))
    })?;
    Some(Location {
        buffer: opened,
        range,
    })
}

fn called_from(path: PathBuf, declared: &Definition) -> Called {
    Called {
        name: declared.name.clone(),
        kind: declared.kind.clone(),
        path,
        // `Definition::line` is one-based, as a reader counts lines.
        row: declared.line.saturating_sub(1),
    }
}

/// Reads and parses each file the index named a place in, and reports the
/// declaration each place is written inside.
///
/// One parse per referring file rather than per place: a name written thirty
/// times in one file is one file to read.
fn callers_of(root: &Path, name: &str, places: &[Where]) -> Vec<Called> {
    let mut by_file: BTreeMap<&str, Vec<&Where>> = BTreeMap::new();
    for place in places {
        by_file.entry(place.path.as_str()).or_default().push(place);
    }

    let mut seen: HashSet<(PathBuf, u32)> = HashSet::default();
    let mut callers = Vec::new();
    for (path, places) in by_file {
        let full = root.join(path);
        let Ok(contents) = std::fs::read(&full) else {
            continue;
        };
        let Ok(Some(attributed)) = references_and_what_encloses_them(path, &contents) else {
            continue;
        };
        for place in places {
            // The store holds what the file said when it was last read. A place
            // the parse no longer agrees about is a row that has moved, and one
            // place lost is not a reason to abandon the rest.
            let Some(occurrence) = attributed.iter().find(|one| {
                one.at.row == place.row && one.at.column == place.column && one.at.name == name
            }) else {
                continue;
            };
            let caller = match &occurrence.inside {
                Some(declaration) => Called {
                    name: declaration.name.clone(),
                    kind: declaration.kind.clone(),
                    path: full.clone(),
                    row: declaration.name_row,
                },
                None => Called {
                    name: path.to_string(),
                    kind: Called::FILE_SCOPE.to_string(),
                    path: full.clone(),
                    row: occurrence.at.row,
                },
            };
            if seen.insert((caller.path.clone(), caller.row)) {
                callers.push(caller);
            }
        }
    }
    callers
}

/// The distinct names written as calls inside one declaration's own range.
///
/// Matched by the row the index recorded rather than by the name alone: one
/// file can declare a method of the same name on two types, and only the row
/// says which of them the reader asked about.
fn names_called_inside(path: &Path, declared: &Definition) -> Vec<String> {
    let Ok(contents) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(Some(attributed)) = references_and_what_encloses_them(&declared.path, &contents) else {
        return Vec::new();
    };
    let row = declared.line.saturating_sub(1);
    let mut names: Vec<String> = Vec::new();
    for one in attributed {
        // A name written without a bracket after it is a mention rather than a
        // call: a function handed over as a value, a type in a signature, a
        // constant read. Listing those under "outgoing calls" would fill the
        // tree with everything the body names.
        if !one.called {
            continue;
        }
        let Some(inside) = &one.inside else {
            continue;
        };
        if inside.row != row {
            continue;
        }
        if !names.contains(&one.at.name) {
            names.push(one.at.name.clone());
        }
    }
    names
}
