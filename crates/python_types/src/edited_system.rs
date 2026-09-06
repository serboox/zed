use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ruff_db::file_revision::FileRevision;
use ruff_db::system::walk_directory::WalkDirectoryBuilder;
use ruff_db::system::{
    CommandExecutor, DirectoryEntry, FileType, Metadata, OsSystem, Result, System, SystemPath,
    SystemPathBuf, SystemVirtualPath, WhichResult, WritableSystem,
};
use ruff_notebook::{Notebook, NotebookError};

#[derive(Debug, Default)]
struct Edits {
    text: HashMap<SystemPathBuf, String>,
    revision: HashMap<SystemPathBuf, u64>,
}

/// The text of the buffers open in the editor, keyed by absolute path.
#[derive(Debug, Default)]
pub struct OpenBuffers {
    edits: Mutex<Edits>,
    next_revision: AtomicU64,
}

impl OpenBuffers {
    /// Records what a buffer says now. Returns false when the text is the one
    /// already recorded, so the caller can skip telling salsa about a change
    /// that did not happen.
    pub fn record(&self, path: &SystemPath, text: String) -> bool {
        let Ok(mut edits) = self.edits.lock() else {
            return false;
        };
        if edits.text.get(path).is_some_and(|already| *already == text) {
            return false;
        }
        edits.text.insert(path.to_path_buf(), text);
        let revision = self.next_revision.fetch_add(1, Ordering::Relaxed) + 1;
        edits.revision.insert(path.to_path_buf(), revision);
        true
    }

    fn text(&self, path: &SystemPath) -> Option<String> {
        self.edits.lock().ok()?.text.get(path).cloned()
    }

    fn revision(&self, path: &SystemPath) -> Option<u64> {
        self.edits.lock().ok()?.revision.get(path).copied()
    }
}

/// A [`System`] that answers with what the open buffers say and falls back to
/// the disk for everything else.
///
/// Without it every answer would describe the last saved version of the file,
/// which for a buffer being typed into is the wrong text at the wrong offsets.
#[derive(Debug, Clone)]
pub struct EditedSystem {
    disk: OsSystem,
    open: Arc<OpenBuffers>,
}

impl EditedSystem {
    pub fn new(current_directory: &SystemPath, open: Arc<OpenBuffers>) -> Self {
        Self {
            disk: OsSystem::new(current_directory),
            open,
        }
    }
}

impl System for EditedSystem {
    fn path_metadata(&self, path: &SystemPath) -> Result<Metadata> {
        let Some(revision) = self.open.revision(path) else {
            return self.disk.path_metadata(path);
        };
        // The permissions come from disk where there are any; a buffer that has
        // never been saved has no file behind it and is still a readable file
        // as far as the type checker is concerned.
        let permissions = self
            .disk
            .path_metadata(path)
            .ok()
            .and_then(|metadata| metadata.permissions());
        Ok(Metadata::new(
            FileRevision::from(revision),
            permissions,
            FileType::File,
        ))
    }

    fn canonicalize_path(&self, path: &SystemPath) -> Result<SystemPathBuf> {
        self.disk.canonicalize_path(path)
    }

    fn is_same_file(&self, first: &SystemPath, second: &SystemPath) -> Result<bool> {
        self.disk.is_same_file(first, second)
    }

    fn which(&self, binary_name: &str) -> WhichResult {
        self.disk.which(binary_name)
    }

    fn command_executor(&self) -> Option<&dyn CommandExecutor> {
        self.disk.command_executor()
    }

    fn read_to_string(&self, path: &SystemPath) -> Result<String> {
        match self.open.text(path) {
            Some(text) => Ok(text),
            None => self.disk.read_to_string(path),
        }
    }

    fn read_to_notebook(&self, path: &SystemPath) -> std::result::Result<Notebook, NotebookError> {
        match self.open.text(path) {
            Some(text) => Notebook::from_source_code(&text),
            None => self.disk.read_to_notebook(path),
        }
    }

    fn read_virtual_path_to_string(&self, path: &SystemVirtualPath) -> Result<String> {
        self.disk.read_virtual_path_to_string(path)
    }

    fn read_virtual_path_to_notebook(
        &self,
        path: &SystemVirtualPath,
    ) -> std::result::Result<Notebook, NotebookError> {
        self.disk.read_virtual_path_to_notebook(path)
    }

    fn current_directory(&self) -> &SystemPath {
        self.disk.current_directory()
    }

    fn user_config_directory(&self) -> Option<SystemPathBuf> {
        self.disk.user_config_directory()
    }

    fn cache_dir(&self) -> Option<SystemPathBuf> {
        self.disk.cache_dir()
    }

    fn read_directory<'a>(
        &'a self,
        path: &SystemPath,
    ) -> Result<Box<dyn Iterator<Item = Result<DirectoryEntry>> + 'a>> {
        self.disk.read_directory(path)
    }

    fn walk_directory(&self, path: &SystemPath) -> WalkDirectoryBuilder {
        self.disk.walk_directory(path)
    }

    fn env_var(&self, name: &str) -> std::result::Result<String, std::env::VarError> {
        self.disk.env_var(name)
    }

    /// Deliberately not writable: an editor asking what a name's type is must
    /// never give the type checker a way to put something on disk.
    fn as_writable(&self) -> Option<&dyn WritableSystem> {
        None
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn dyn_clone(&self) -> Box<dyn System> {
        Box::new(self.clone())
    }
}
