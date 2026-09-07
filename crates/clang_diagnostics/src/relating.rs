use std::path::PathBuf;

use gpui::{App, AppContext as _, Entity, Task};
use language::Buffer;
use project::Project;
use text::PointUtf16;

use crate::parsing::{Asked, Place, Related, Relatives, Request, ask_libclang};
use crate::watching::{c_or_cpp, how_it_is_compiled};

/// Which type a question is about.
pub enum Target {
    /// The type at this position in the buffer itself, which is where a reader
    /// asks from.
    Under(PointUtf16),
    /// The type declared at this place in one of the files the buffer's
    /// translation unit reaches -- a base class in a header, most often.
    Declared(Place),
}

/// The type a place holds and its neighbours in one direction, out of the
/// compiler's own front end.
///
/// `None`, and not an empty answer, wherever the front end cannot be asked:
/// a buffer in some other language, a file with no path, a project with no
/// compilation database, a place that holds no type. Each of those leaves
/// whatever asked before this to keep its own answer.
///
/// The buffer decides which translation unit is parsed, and the question is
/// asked inside it. That is what lets a base class declared in a header be
/// asked about at all, and it is also the limit on what the `Derived`
/// direction can find.
pub fn ask_about_types(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    target: Target,
    direction: Related,
    cx: &mut App,
) -> Option<Task<Option<Relatives>>> {
    let asked = what_was_asked(project, buffer, target, cx)?;
    Some(cx.background_spawn(async move { relatives_of_the_type(&asked, direction).await }))
}

/// Everything the question needs that can only be read on the foreground
/// thread.
struct AskedAbout {
    path: PathBuf,
    root: PathBuf,
    text: String,
    target: Place,
}

fn what_was_asked(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    target: Target,
    cx: &App,
) -> Option<AskedAbout> {
    c_or_cpp(buffer, cx)?;
    let read = buffer.read(cx);
    let file = read.file()?;
    let path = file.as_local()?.abs_path(cx);
    let root = project
        .read(cx)
        .worktree_for_id(file.worktree_id(cx), cx)?
        .read(cx)
        .abs_path()
        .to_path_buf();

    let snapshot = read.snapshot();
    let target = match target {
        Target::Under(position) => Place {
            file: path.clone(),
            offset: snapshot.point_utf16_to_offset(position),
        },
        Target::Declared(place) => place,
    };
    Some(AskedAbout {
        path,
        root,
        text: snapshot.text(),
        target,
    })
}

async fn relatives_of_the_type(asked: &AskedAbout, direction: Related) -> Option<Relatives> {
    let arguments = how_it_is_compiled(&asked.path, &asked.root)?;
    ask_libclang(Request {
        file: asked.path.clone(),
        text: asked.text.clone(),
        arguments,
        asked: Asked::WhichTypesRelateTo {
            at: asked.target.clone(),
            direction,
        },
    })
    .await
    .ok()?
    .relatives
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A base class in a header, two classes derived from it in the file that
    /// includes it, and one class with no bases at all. The header is what
    /// makes the answers interesting: a base class is almost never declared in
    /// the file the reader is looking at.
    const HEADER: &str = r#"
#pragma once

class Shape {
  public:
    virtual ~Shape() = default;
    virtual double area() const = 0;
};

struct Tagged {
    int tag = 0;
};
"#;

    const SOURCE: &str = r#"
#include "shapes.h"

class Square : public Shape, public Tagged {
  public:
    double area() const override { return side * side; }
    double side = 1.0;
};

class Circle : public Shape {
  public:
    double area() const override { return 3.14 * radius * radius; }
    double radius = 1.0;
};

class Unrelated {
  public:
    int value = 0;
};

int main() {
    Square square;
    Circle circle;
    Unrelated unrelated;
    return static_cast<int>(square.area() + circle.area()) + unrelated.value;
}
"#;

    struct OnDisk {
        _directory: tempfile::TempDir,
        source: PathBuf,
        header: PathBuf,
    }

    /// The two files on disk, with a compilation database for the source where
    /// one was asked for.
    fn on_disk(database: bool) -> OnDisk {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let source = directory.path().join("shapes.cpp");
        let header = directory.path().join("shapes.h");
        std::fs::write(&source, SOURCE).expect("the source on disk");
        std::fs::write(&header, HEADER).expect("the header on disk");
        if database {
            let entries = serde_json::json!([{
                "directory": directory.path(),
                "file": source,
                "arguments": ["clang++", "-std=c++20", "-c", source],
            }]);
            std::fs::write(
                directory.path().join("compile_commands.json"),
                entries.to_string(),
            )
            .expect("a compilation database");
        }
        OnDisk {
            _directory: directory,
            source,
            header,
        }
    }

    /// Asks the front end about a place, with the source file as the
    /// translation unit. Written out rather than going through
    /// `ask_about_types` so a test needs no window, no project and no buffer:
    /// everything below the foreground read is the same code either way.
    fn what_clang_says(
        held: &OnDisk,
        at: Place,
        direction: Related,
        database: bool,
    ) -> Option<Relatives> {
        let root = held
            .source
            .parent()
            .expect("the directory the files are in")
            .to_path_buf();
        let asked = AskedAbout {
            path: held.source.clone(),
            root,
            text: SOURCE.to_string(),
            target: at,
        };
        assert_eq!(
            how_it_is_compiled(&asked.path, &asked.root).is_some(),
            database,
            "the fixture should have a compilation database only where asked for one",
        );
        futures::executor::block_on(relatives_of_the_type(&asked, direction))
    }

    fn in_the_source(held: &OnDisk, needle: &str) -> Place {
        Place {
            file: held.source.clone(),
            offset: SOURCE.find(needle).expect("a place in the source"),
        }
    }

    fn in_the_header(held: &OnDisk, needle: &str) -> Place {
        Place {
            file: held.header.clone(),
            offset: HEADER.find(needle).expect("a place in the header"),
        }
    }

    fn names(relatives: &Relatives) -> Vec<&str> {
        relatives
            .related
            .iter()
            .map(|one| one.name.as_str())
            .collect()
    }

    /// The claim: a class's own declaration names what it derives from, and
    /// the front end reads it straight off -- including the base declared in
    /// a header the reader is not looking at, and including the fact that one
    /// of them is a `struct`.
    #[test]
    #[ignore = "needs libclang on the machine"]
    fn a_class_names_the_classes_it_derives_from() {
        let held = on_disk(true);
        let said = what_clang_says(
            &held,
            in_the_source(&held, "Square : public"),
            Related::Bases,
            true,
        )
        .expect("clang knows what Square derives from");

        assert_eq!(said.subject.name, "Square");
        assert_eq!(said.subject.kind, "class");
        assert_eq!(
            names(&said),
            vec!["Shape", "Tagged"],
            "both bases, in the order the base clause names them",
        );
        assert_eq!(
            said.related
                .iter()
                .map(|one| one.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["class", "struct"],
            "a struct is not renamed a class",
        );
        for base in &said.related {
            assert_eq!(
                base.at.file, held.header,
                "both bases are declared in the header, not the file being read",
            );
            assert_eq!(
                &HEADER[base.at.offset..base.end],
                base.name,
                "a base's place spans its own name",
            );
        }
    }

    /// The hard direction, and the honest one: the classes derived from a base
    /// are only those this translation unit holds. Both derived classes are in
    /// the file that includes the header, so both are here.
    #[test]
    #[ignore = "needs libclang on the machine"]
    fn a_base_names_the_classes_derived_from_it_in_this_translation_unit() {
        let held = on_disk(true);
        let said = what_clang_says(
            &held,
            in_the_header(&held, "Shape {"),
            Related::Derived,
            true,
        )
        .expect("clang knows what derives from Shape");

        assert_eq!(said.subject.name, "Shape");
        let mut found = names(&said);
        found.sort_unstable();
        assert_eq!(found, vec!["Circle", "Square"]);
        assert!(
            !names(&said).contains(&"Unrelated"),
            "a class that derives from nothing is not derived from Shape",
        );
        for one in &said.related {
            assert_eq!(one.at.file, held.source);
            assert_eq!(&SOURCE[one.at.offset..one.end], one.name);
        }
    }

    /// A class with no bases is a question with an answer, and the answer is
    /// nothing. Kept apart from the tests below, where the front end has no
    /// answer at all: "no supertypes" and "not a question I can take" are
    /// different things and must stay different.
    #[test]
    #[ignore = "needs libclang on the machine"]
    fn a_class_with_no_bases_answers_nothing_rather_than_an_empty_list() {
        let held = on_disk(true);
        let said = what_clang_says(
            &held,
            in_the_source(&held, "Unrelated {"),
            Related::Bases,
            true,
        )
        .expect("clang knows Unrelated is a class");

        assert_eq!(said.subject.name, "Unrelated");
        assert!(
            said.related.is_empty(),
            "Unrelated derives from nothing, so there is nothing to list",
        );
    }

    /// A place that holds no type is not a question about a type, and the
    /// front end says so by saying nothing.
    #[test]
    #[ignore = "needs libclang on the machine"]
    fn a_cursor_that_is_not_a_type_answers_nothing() {
        let held = on_disk(true);
        for needle in ["return static_cast", "int main"] {
            assert!(
                what_clang_says(&held, in_the_source(&held, needle), Related::Bases, true)
                    .is_none(),
                "`{needle}` names no type",
            );
        }
    }

    /// Silence where the project has no compilation database. Without one a
    /// translation unit's include paths are unknown, the header is never
    /// found, and a hierarchy read out of what is left would be missing
    /// exactly the bases that matter.
    #[test]
    fn nothing_at_all_without_a_compilation_database() {
        let held = on_disk(false);
        assert!(
            what_clang_says(
                &held,
                in_the_source(&held, "Square : public"),
                Related::Bases,
                false,
            )
            .is_none()
        );
    }

    /// The gate the whole feature sits behind. The front end is asked about C
    /// and C++ and nothing else, so every other language keeps whatever
    /// answered it before -- which for a type hierarchy is a language server
    /// or nothing.
    #[test]
    fn only_c_and_cpp_are_ever_asked() {
        assert_eq!(crate::watching::front_end_language("C"), Some("C"));
        assert_eq!(crate::watching::front_end_language("C++"), Some("C++"));
        for other in ["Rust", "Go", "Python", "TypeScript", "Objective-C", ""] {
            assert_eq!(
                crate::watching::front_end_language(other),
                None,
                "{other} is not a language this front end is asked about",
            );
        }
    }
}
