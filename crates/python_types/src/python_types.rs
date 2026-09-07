mod edited_system;
mod name_semantics;
mod type_completions;
mod type_errors;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gpui::{App, AppContext as _, Entity, Task};
use language::Buffer;
use project::{Hover, HoverBlock, HoverBlockKind, InProcessHover, InProcessHoverContext};
use ruff_db::files::{File, system_path_to_file};
use ruff_db::system::SystemPathBuf;
use ruff_text_size::{Ranged, TextRange, TextSize};
use text::PointUtf16;
use ty_ide::MarkupKind;
use ty_project::{ProjectDatabase, ProjectMetadata, SemanticDb as _};

use crate::edited_system::{EditedSystem, OpenBuffers};

pub use crate::type_errors::{TypeChecker, type_checker};

/// Answers hover, completion, type errors, go-to-definition, find-references
/// and rename for Python out of `ty`'s own inference and resolution, in this
/// process.
///
/// One source object behind all of them, so they share the project databases
/// they answer out of rather than each building its own -- which would cost
/// the whole of one again per source.
pub fn init(cx: &mut App) {
    let types = Arc::new(TypesFromTy::default());
    project::register_in_process_hover(types.clone(), cx);
    project::register_in_process_completions(types.clone(), cx);
    project::register_in_process_semantics(types.clone(), cx);
    type_errors::register(types, cx);
}

#[derive(Default)]
pub(crate) struct TypesFromTy {
    projects: Mutex<HashMap<PathBuf, Arc<PythonProject>>>,
}

pub(crate) struct PythonProject {
    // One lock over the database rather than a handle per query: salsa cancels
    // every outstanding query when an input changes, and every hover changes an
    // input (the text of the buffer being read). Serialising them is what makes
    // the cancellation path unreachable, and hovers are rare enough to afford it.
    database: Mutex<ProjectDatabase>,
    open: Arc<OpenBuffers>,
}

impl TypesFromTy {
    fn project_for(&self, root: &Path) -> Option<Arc<PythonProject>> {
        let mut projects = self.projects.lock().ok()?;
        if let Some(project) = projects.get(root) {
            return Some(project.clone());
        }
        let project = Arc::new(PythonProject::discover(root)?);
        projects.insert(root.to_path_buf(), project.clone());
        Some(project)
    }
}

impl PythonProject {
    pub(crate) fn discover(root: &Path) -> Option<Self> {
        let root = SystemPathBuf::from_path_buf(root.to_path_buf()).ok()?;
        let open = Arc::new(OpenBuffers::default());
        let system = EditedSystem::new(&root, open.clone());
        let metadata = match ProjectMetadata::discover(&root, &system) {
            Ok(metadata) => metadata,
            Err(error) => {
                log::warn!("no Python project at {root}: {error}");
                return None;
            }
        };
        Some(Self {
            database: Mutex::new(ProjectDatabase::use_defaults(metadata, system)),
            open,
        })
    }
}

impl InProcessHover for TypesFromTy {
    fn hover(
        &self,
        context: &InProcessHoverContext,
        buffer: &Entity<Buffer>,
        position: PointUtf16,
        cx: &mut App,
    ) -> Task<Option<Vec<Hover>>> {
        let Some(asked) = what_was_asked(&context.worktree_roots, buffer, position, cx) else {
            return Task::ready(None);
        };
        let Some(project) = self.project_for(&asked.root) else {
            return Task::ready(None);
        };
        let snapshot = buffer.read(cx).snapshot();

        cx.background_spawn(async move {
            let said = answer(&project, &asked)?;
            let at = snapshot.anchor_before(usize::from(said.about.start()))
                ..snapshot.anchor_after(usize::from(said.about.end()));
            Some(vec![Hover {
                contents: vec![HoverBlock {
                    text: said.markdown,
                    kind: HoverBlockKind::Markdown,
                }],
                range: Some(at),
                language: None,
            }])
        })
    }
}

/// Everything an answer needs that can only be read on the foreground thread.
pub(crate) struct Asked {
    pub(crate) root: PathBuf,
    pub(crate) path: SystemPathBuf,
    pub(crate) text: String,
    pub(crate) offset: TextSize,
}

/// What `ty` said, and the span of source it said it about.
struct Said {
    markdown: String,
    about: TextRange,
}

pub(crate) fn what_was_asked(
    worktree_roots: &[PathBuf],
    buffer: &Entity<Buffer>,
    position: PointUtf16,
    cx: &App,
) -> Option<Asked> {
    let buffer = buffer.read(cx);
    let path = buffer.file()?.as_local()?.abs_path(cx);
    if !matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("py" | "pyi")
    ) {
        return None;
    }
    let root = worktree_roots
        .iter()
        .find(|root| path.starts_with(root))
        .cloned()
        .or_else(|| path.parent().map(Path::to_path_buf))?;

    let snapshot = buffer.snapshot();
    let offset = snapshot.point_utf16_to_offset(position);
    Some(Asked {
        root,
        path: SystemPathBuf::from_path_buf(path).ok()?,
        text: snapshot.text(),
        offset: TextSize::try_from(offset).ok()?,
    })
}

/// Asks `ty` what the name under the cursor is, as Markdown.
///
/// Returns `None` rather than propagating a panic: a type-inference query that
/// falls over is a wrong answer to one hover, and must not be the end of the
/// editor the reader is typing into.
fn answer(project: &PythonProject, asked: &Asked) -> Option<Said> {
    let mut database = project.database.lock().ok()?;
    if project.open.record(&asked.path, asked.text.clone()) {
        File::sync_path(&mut *database, &asked.path);
    }
    let database = &*database;

    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let file = system_path_to_file(database, &asked.path).ok()?;
        let found = ty_ide::hover(database, database.program_file(file), asked.offset)?;
        // A hover whose range lands in another file cannot be turned into an
        // anchor in this buffer, and its offsets would silently point at the
        // wrong text if it were tried.
        if found.file_range().file() != file {
            return None;
        }
        let about = found.file_range().range();
        let markdown = found.display(database, MarkupKind::Markdown).to_string();
        (!markdown.trim().is_empty()).then_some(Said { markdown, about })
    }))
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str =
        "def double(value: int) -> int:\n    return value * 2\n\n\nanswer = double(21)\n";

    /// Drives the same path a hover takes, without an editor in front of it.
    fn what_ty_says(source: &str, offset: usize) -> Option<Said> {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("asked.py");
        std::fs::write(&path, source).expect("a file to ask about");
        let project = PythonProject::discover(directory.path())?;
        answer(
            &project,
            &Asked {
                root: directory.path().to_path_buf(),
                path: SystemPathBuf::from_path_buf(path).expect("a UTF-8 path"),
                text: source.to_string(),
                offset: TextSize::try_from(offset).expect("an offset that fits"),
            },
        )
    }

    /// Breaks loudly when the pinned `ty` stops answering the way this crate
    /// reads it -- whether by changing the signatures used above, which stops
    /// this compiling, or by changing what an answer says, which stops these
    /// assertions. A rev bump that survives both is safe to take.
    #[test]
    fn ty_says_what_a_name_is() {
        let at = SOURCE.rfind("answer").expect("the assignment");
        let said = what_ty_says(SOURCE, at).expect("ty knows the type of `answer`");
        assert!(
            said.markdown.contains("int"),
            "`answer` should be an int, ty said: {}",
            said.markdown
        );
        assert_eq!(
            &SOURCE[usize::from(said.about.start())..usize::from(said.about.end())],
            "answer",
            "the answer should be about the name that was asked about"
        );

        let at = SOURCE.rfind("double").expect("the call");
        let said = what_ty_says(SOURCE, at).expect("ty knows what `double` is");
        assert!(
            said.markdown.contains("double") && said.markdown.contains("int"),
            "`double` should show its signature, ty said: {}",
            said.markdown
        );
    }

    /// The type has to follow the text of the buffer rather than what is on
    /// disk, which is what the overlay system exists for.
    #[test]
    fn ty_reads_the_buffer_rather_than_the_file() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("asked.py");
        std::fs::write(&path, "answer = 1\n").expect("a file to ask about");
        let project = PythonProject::discover(directory.path()).expect("a project");
        let said = answer(
            &project,
            &Asked {
                root: directory.path().to_path_buf(),
                path: SystemPathBuf::from_path_buf(path).expect("a UTF-8 path"),
                text: "answer = 'one'\n".to_string(),
                offset: TextSize::new(0),
            },
        )
        .expect("ty knows the type of `answer`");
        assert!(
            said.markdown.contains("one"),
            "the unsaved text says `answer` is the string `one`, ty said: {}",
            said.markdown
        );
    }

    /// A place `ty` has nothing to say about must produce no card at all, so
    /// the index's own answer is what the reader gets instead of an empty one.
    #[test]
    fn silence_where_ty_knows_nothing() {
        assert!(what_ty_says("# just a comment\n", 4).is_none());
    }

    /// Reports what one project's database costs after answering hovers and
    /// completions across several modules that pull the standard library in,
    /// against the 293 MB a `pyright` server holds for the same job. Printed
    /// rather than asserted: the number is for a person to read, and a
    /// threshold here would fail on an unrelated allocator change.
    #[test]
    fn resident_memory_after_reading_a_project() {
        let modules = [
            (
                "shapes.py",
                "import dataclasses\n\n\n@dataclasses.dataclass\nclass Point:\n    x: int\n    y: float\n",
            ),
            (
                "loading.py",
                "import json\nimport pathlib\n\n\ndef read(path: pathlib.Path) -> dict:\n    return json.loads(path.read_text())\n",
            ),
            (
                "waiting.py",
                "import asyncio\nimport typing\n\n\nasync def gather(items: typing.Sequence[int]) -> list[int]:\n    await asyncio.sleep(0)\n    return sorted(items)\n",
            ),
            (
                "main.py",
                "import loading\nimport pathlib\nimport shapes\n\n\nplace = shapes.Point(1, 2.0)\nsettings = loading.read(pathlib.Path(\"settings.json\"))\n",
            ),
        ];
        let directory = tempfile::tempdir().expect("a temporary directory");
        for (name, source) in modules {
            std::fs::write(directory.path().join(name), source).expect("a module");
        }
        let before = resident_bytes();
        let project = PythonProject::discover(directory.path()).expect("a project");
        let asked_about = |name: &str, source: &str, offset: usize| Asked {
            root: directory.path().to_path_buf(),
            path: SystemPathBuf::from_path_buf(directory.path().join(name)).expect("a UTF-8 path"),
            text: source.to_string(),
            offset: TextSize::try_from(offset).expect("an offset that fits"),
        };
        let mut answered = 0;
        for (name, source) in modules {
            for (offset, _) in source.char_indices() {
                if answer(&project, &asked_about(name, source, offset)).is_some() {
                    answered += 1;
                }
            }
        }
        let after_hovers = resident_bytes();

        // The completion side asks the same database for the members of every
        // expression a `.` follows, which is where a menu is actually offered:
        // once with nothing typed after the dot, and once with the whole
        // member name typed, which is the two ends of what a reader does.
        let mut offered = 0;
        for (name, source) in modules {
            for (dot, _) in source.match_indices('.') {
                let typed = source[dot + 1..]
                    .find(|character: char| !(character.is_alphanumeric() || character == '_'))
                    .unwrap_or(source.len() - dot - 1);
                for offset in [dot + 1, dot + 1 + typed] {
                    offered += crate::type_completions::offer(
                        &project,
                        &asked_about(name, source, offset),
                    )
                    .len();
                }
            }
        }
        let after_completions = resident_bytes();

        // The same database is then asked the three questions that resolve a
        // name rather than infer a type, at every offset inside a name -- which
        // is every place a reader could ask one of them from.
        let mut resolved = 0;
        for (name, source) in modules {
            for (offset, character) in source.char_indices() {
                if !(character.is_alphanumeric() || character == '_') {
                    continue;
                }
                let asked = asked_about(name, source, offset);
                for found in [
                    crate::name_semantics::definitions_of(&project, &asked),
                    crate::name_semantics::references_of(&project, &asked),
                    crate::name_semantics::rename_of(&project, &asked, "renamed"),
                ] {
                    resolved += found.map_or(0, |spans| spans.len());
                }
            }
        }
        let after = resident_bytes();
        assert!(answered > 0, "the measurement needs real answers");
        assert!(offered > 0, "the measurement needs real suggestions");
        assert!(resolved > 0, "the measurement needs real resolutions");
        println!(
            "resident memory over {} modules, {answered} hovers, {offered} suggestions and {resolved} resolved spans: {:.1} MB before, {:.1} MB after hovers, {:.1} MB after completions, {:.1} MB after definitions, references and renames, {:.1} MB for the database",
            modules.len(),
            before as f64 / 1e6,
            after_hovers as f64 / 1e6,
            after_completions as f64 / 1e6,
            after as f64 / 1e6,
            after.saturating_sub(before) as f64 / 1e6
        );
    }

    fn resident_bytes() -> u64 {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        let pages: u64 = statm
            .split_whitespace()
            .nth(1)
            .and_then(|pages| pages.parse().ok())
            .unwrap_or_default();
        pages * 4096
    }
}
