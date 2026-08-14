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

impl ProjectItem for PdfItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        if !is_pdf_extension(path.path.extension()) {
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
    fn nothing_else_is_taken_for_a_pdf() {
        assert!(!is_pdf_extension(Some("md")));
        assert!(!is_pdf_extension(Some("bak")));
        assert!(
            !is_pdf_extension(None),
            "a file with no extension is not one"
        );
    }
}
