use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, AppContext as _, Entity, Task};
use language::Buffer;
use project::{Hover, HoverBlock, HoverBlockKind, InProcessHover, InProcessHoverContext};
use text::PointUtf16;

use crate::parsing::{Asked, Described, Request, ask_libclang};
use crate::watching::{c_or_cpp, how_it_is_compiled};

/// Answers hover for C and C++ out of the compiler's own front end, in this
/// process.
pub fn init(cx: &mut App) {
    project::register_in_process_hover(Arc::new(TypesFromClang), cx);
}

struct TypesFromClang;

/// Everything a hover needs that can only be read on the foreground thread.
struct AskedAbout {
    path: PathBuf,
    root: PathBuf,
    text: String,
    offset: usize,
    language: &'static str,
}

impl InProcessHover for TypesFromClang {
    fn hover(
        &self,
        context: &InProcessHoverContext,
        buffer: &Entity<Buffer>,
        position: PointUtf16,
        cx: &mut App,
    ) -> Task<Option<Vec<Hover>>> {
        let Some(asked) = what_was_asked(context, buffer, position, cx) else {
            return Task::ready(None);
        };
        let snapshot = buffer.read(cx).snapshot();

        cx.background_spawn(async move {
            let said = what_is_at(&asked).await?;
            let length = snapshot.len();
            let at = snapshot.anchor_before(said.start.min(length))
                ..snapshot.anchor_after(said.end.min(length));
            Some(vec![Hover {
                contents: card_for(&said, asked.language),
                range: Some(at),
                language: None,
            }])
        })
    }
}

fn what_was_asked(
    context: &InProcessHoverContext,
    buffer: &Entity<Buffer>,
    position: PointUtf16,
    cx: &App,
) -> Option<AskedAbout> {
    let language = c_or_cpp(buffer, cx)?;
    let read = buffer.read(cx);
    let path = read.file()?.as_local()?.abs_path(cx);
    // The roots arrive longest first, so the first that matches is the one that
    // holds the file rather than one that merely contains its worktree.
    let root = context
        .worktree_roots
        .iter()
        .find(|root| path.starts_with(root))
        .cloned()
        .or_else(|| path.parent().map(Path::to_path_buf))?;

    let snapshot = read.snapshot();
    Some(AskedAbout {
        path,
        root,
        offset: snapshot.point_utf16_to_offset(position),
        text: snapshot.text(),
        language,
    })
}

/// What the front end says is at one place in a file.
///
/// Nothing at all where the project has no compilation database. Without one a
/// translation unit's include paths and defines are unknown, and a parse
/// without them understands almost nothing of the file: the card it produced
/// would be a confident wrong answer where silence leaves the reader the name
/// index's own.
async fn what_is_at(asked: &AskedAbout) -> Option<Described> {
    let arguments = how_it_is_compiled(&asked.path, &asked.root)?;
    ask_libclang(Request {
        file: asked.path.clone(),
        text: asked.text.clone(),
        arguments,
        asked: Asked::WhatIsAt(asked.offset),
    })
    .await
    .ok()?
    .described
}

/// The blocks the card is made of: the name with its type in front of it, and
/// the brief of the comment above the declaration where there is one.
fn card_for(said: &Described, language: &str) -> Vec<HoverBlock> {
    let mut blocks = vec![HoverBlock {
        text: said.declaration.clone(),
        kind: HoverBlockKind::Code {
            language: language.to_string(),
        },
    }];
    if let Some(comment) = &said.comment {
        blocks.push(HoverBlock {
            text: comment.clone(),
            kind: HoverBlockKind::Markdown,
        });
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A template, and a use of it through one instantiation. What `keys`
    /// returns is written `std::vector<Key>` and is a `std::vector<std::string>`
    /// here, and that difference is the whole point of asking a compiler.
    const SOURCE: &str = r#"
#include <map>
#include <memory>
#include <string>
#include <vector>

template <typename Key, typename Value>
class Registry {
  public:
    /// Every key held, in order.
    std::vector<Key> keys() const {
        std::vector<Key> found;
        for (const auto &entry : entries_) {
            found.push_back(entry.first);
        }
        return found;
    }

  private:
    std::map<Key, Value> entries_;
};

int main() {
    Registry<std::string, std::unique_ptr<int>> registry;
    auto names = registry.keys();
    return static_cast<int>(names.size());
}
"#;

    struct Project {
        _directory: tempfile::TempDir,
        asked: AskedAbout,
    }

    /// A project on disk holding the source, with a compilation database for it
    /// where one was asked for.
    fn project_with(source: &str, offset: usize, database: bool) -> Project {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("asked.cpp");
        std::fs::write(&path, source).expect("a file to ask about");
        if database {
            let entries = serde_json::json!([{
                "directory": directory.path(),
                "file": path,
                "arguments": ["clang++", "-std=c++20", "-c", path],
            }]);
            std::fs::write(
                directory.path().join("compile_commands.json"),
                entries.to_string(),
            )
            .expect("a compilation database");
        }
        Project {
            asked: AskedAbout {
                path,
                root: directory.path().to_path_buf(),
                text: source.to_string(),
                offset,
                language: "C++",
            },
            _directory: directory,
        }
    }

    fn what_clang_says(source: &str, offset: usize, database: bool) -> Option<Described> {
        let project = project_with(source, offset, database);
        futures::executor::block_on(what_is_at(&project.asked))
    }

    /// The claim the whole crate is for: the type shown is the one this
    /// instantiation has, not the one the template was written with. A name
    /// index reading the declaration can only ever say `std::vector<Key>`.
    #[test]
    #[ignore = "needs libclang on the machine"]
    fn hovering_a_call_shows_the_instantiated_type() {
        let at = SOURCE.rfind("keys").expect("the call");
        let said = what_clang_says(SOURCE, at, true).expect("clang knows what `keys` returns");

        let type_of = said.type_of.as_deref().unwrap_or_default();
        assert!(
            type_of.contains("vector") && type_of.contains("string"),
            "`keys` returns a vector of strings here, clang said: {type_of}"
        );
        assert!(
            !type_of.contains("Key"),
            "the template parameter should be substituted, clang said: {type_of}"
        );
        assert!(
            said.declaration.contains("keys"),
            "the card should name what was hovered, clang said: {}",
            said.declaration
        );
        assert_eq!(
            &SOURCE[said.start..said.end],
            "keys",
            "the card should be about the name that was hovered"
        );
        assert_eq!(
            said.comment.as_deref(),
            Some("Every key held, in order."),
            "the comment above the declaration belongs on the card"
        );
    }

    /// The instantiation is visible on the variable too, template arguments and
    /// all, which is what the declaration as written cannot show.
    #[test]
    #[ignore = "needs libclang on the machine"]
    fn hovering_a_variable_shows_its_template_arguments() {
        let at = SOURCE
            .rfind("registry.keys")
            .expect("the use of the registry");
        let said = what_clang_says(SOURCE, at, true).expect("clang knows what `registry` is");

        let type_of = said.type_of.as_deref().unwrap_or_default();
        assert!(
            type_of.starts_with("Registry<") && type_of.contains("unique_ptr<int>"),
            "`registry` is a Registry of strings to unique pointers, clang said: {type_of}"
        );
    }

    /// The control for the test above: inside the template there is no
    /// instantiation, and `std::vector<Key>` is the whole truth about what
    /// `found` is. It is the same source, the same query and the same
    /// assertion the test above makes the opposite way round -- which is what
    /// shows that test is reading the substitution and not just any type.
    #[test]
    #[ignore = "needs libclang on the machine"]
    fn inside_the_template_the_parameter_is_still_a_parameter() {
        let at = SOURCE
            .find("found;")
            .expect("the local inside the template");
        let said = what_clang_says(SOURCE, at, true).expect("clang knows what `found` is");

        let type_of = said.type_of.as_deref().unwrap_or_default();
        assert!(
            type_of.contains("vector") && type_of.contains("Key"),
            "`found` is a vector of the template parameter, clang said: {type_of}"
        );
    }

    /// Silence where the project has no compilation database. A parse without
    /// one finds none of the headers the file includes, and everything it would
    /// then say about the reader's own code is wrong.
    #[test]
    fn nothing_at_all_without_a_compilation_database() {
        let at = SOURCE.rfind("keys").expect("the call");
        assert!(what_clang_says(SOURCE, at, false).is_none());
    }

    /// The card names the language its code block is written in, so the block
    /// is highlighted rather than shown as plain text.
    #[test]
    fn the_card_says_which_language_it_is_showing() {
        let said = Described {
            declaration: "std::vector<std::string> keys()".to_string(),
            type_of: Some("std::vector<std::string>".to_string()),
            comment: Some("Every key held, in order.".to_string()),
            start: 0,
            end: 4,
        };
        let card = card_for(&said, "C++");
        assert_eq!(card.len(), 2);
        assert_eq!(
            card[0].kind,
            HoverBlockKind::Code {
                language: "C++".to_string()
            }
        );
        assert_eq!(card[0].text, "std::vector<std::string> keys()");
        assert_eq!(card[1].kind, HoverBlockKind::Markdown);

        let card = card_for(
            &Described {
                comment: None,
                ..said
            },
            "C",
        );
        assert_eq!(card.len(), 1, "no comment means no second block");
    }
}
