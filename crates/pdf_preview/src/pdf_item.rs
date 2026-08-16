use std::path::PathBuf;

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Entity, EventEmitter, Task};
use project::{Project, ProjectItem, ProjectPath};

/// A PDF the editor has been asked to open. It holds the path rather than the
/// bytes: the engine reads the file itself, and a document can be far larger
/// than anything worth keeping a second copy of in memory.
pub struct PdfItem {
    pub abs_path: PathBuf,
    pub project_path: ProjectPath,
}

impl EventEmitter<()> for PdfItem {}

/// Whether a file with this extension is a PDF. The extension is all the editor
/// has to go on when it decides what to open a file with: that happens before a
/// byte of it has been read.
pub fn is_pdf_extension(extension: Option<&str>) -> bool {
    extension.is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

/// The extension to judge a path by. A file opened on its own -- from the command
/// line, say -- becomes a worktree of itself, and then the path inside that
/// worktree is empty and carries no extension at all. The worktree's own path is
/// the file in that case, so it answers instead.
fn extension_to_judge_by<'a>(
    path_in_project: Option<&'a str>,
    worktree_path: Option<&'a str>,
) -> Option<&'a str> {
    path_in_project.or(worktree_path)
}

impl ProjectItem for PdfItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        let worktree_extension = project
            .read(cx)
            .worktree_for_id(path.worktree_id, cx)
            .and_then(|worktree| {
                worktree
                    .read(cx)
                    .abs_path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .map(str::to_owned)
            });
        if !is_pdf_extension(extension_to_judge_by(
            path.path.extension(),
            worktree_extension.as_deref(),
        )) {
            return None;
        }
        let project = project.clone();
        let path = path.clone();
        Some(cx.spawn(async move |cx| {
            let Some(abs_path) = cx.update(|cx| project.read(cx).absolute_path(&path, cx)) else {
                return Err(anyhow!("the PDF is not on a worktree of this machine"));
            };
            anyhow::Ok(cx.new(|_| PdfItem {
                abs_path,
                project_path: path,
            }))
        }))
    }

    fn entry_id(&self, _: &App) -> Option<project::ProjectEntryId> {
        None
    }

    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pdf_is_recognised_whatever_the_spelling_of_its_extension() {
        assert!(is_pdf_extension(Some("pdf")));
        assert!(is_pdf_extension(Some("PDF")));
        assert!(is_pdf_extension(Some("PdF")));
    }

    #[test]
    fn a_file_opened_on_its_own_is_judged_by_the_worktree_it_became() {
        // Nothing inside the worktree, because the worktree is the file: this is
        // what opening `zed report.pdf` from a shell looks like.
        assert_eq!(
            extension_to_judge_by(None, Some("pdf")),
            Some("pdf"),
            "a file that is its own worktree has to be judged by that worktree"
        );
        // A file inside a folder answers for itself, whatever the folder is named.
        assert_eq!(
            extension_to_judge_by(Some("pdf"), Some("git")),
            Some("pdf"),
            "the path inside the project comes first"
        );
        assert_eq!(extension_to_judge_by(None, None), None);
    }

    #[test]
    fn nothing_else_is_taken_for_a_pdf() {
        assert!(!is_pdf_extension(Some("md")));
        assert!(!is_pdf_extension(Some("bak")));
        assert!(
            !is_pdf_extension(None),
            "a file with no extension is not one"
        );
    }
}
