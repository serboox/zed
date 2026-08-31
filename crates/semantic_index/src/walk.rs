use std::path::{Path, PathBuf};

/// Directories no index has any business reading, whatever the ignore files say.
///
/// The same list the editor excludes from its own scan, and for the same reason:
/// a repository's own directory changes on every commit, every checkout and every
/// fetch, so an index that read it would reparse the project each time -- which
/// is the worst thing an index can do. The rest are caches and editor droppings
/// that hold nothing anyone searches for.
const NEVER_READ: [&str; 11] = [
    ".git",
    ".svn",
    ".hg",
    ".jj",
    ".sl",
    ".repo",
    "CVS",
    ".DS_Store",
    "Thumbs.db",
    ".classpath",
    ".settings",
];

/// Every file under `root`, as the editor's own scanner would see it.
///
/// What the ignore files exclude is excluded here too, because a number that
/// counts `target/` is not a number about the project. Ignore files are read
/// whether or not the project is a git checkout: a plain directory has ignore
/// files that mean the same thing. Hidden files are kept -- a project's `.env`
/// is one of its files -- and only [`NEVER_READ`] is dropped outright.
pub fn files_under(root: &Path) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .require_git(false)
        .filter_entry(|found| {
            found
                .file_name()
                .to_str()
                .is_none_or(|name| !NEVER_READ.contains(&name))
        })
        .build()
        .flatten()
        .filter(|found| found.file_type().is_some_and(|kind| kind.is_file()))
        .map(|found| found.into_path())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found_under(at: &Path) -> Vec<String> {
        files_under(at)
            .into_iter()
            .filter_map(|path| {
                path.strip_prefix(at)
                    .ok()
                    .map(|inside| inside.to_string_lossy().replace('\\', "/"))
            })
            .collect()
    }

    #[test]
    fn what_the_ignore_files_exclude_is_excluded_without_a_repository() {
        let root = tempfile::tempdir().expect("a directory");
        let at = root.path();
        std::fs::write(at.join("kept.rs"), "").expect("a file");
        std::fs::write(at.join(".gitignore"), "built/\n").expect("the ignore file");
        std::fs::create_dir_all(at.join("built")).expect("a directory");
        std::fs::write(at.join("built/gone.rs"), "").expect("an ignored file");

        let found = found_under(at);
        assert!(found.contains(&"kept.rs".to_string()));
        assert!(found.contains(&".gitignore".to_string()));
        assert!(
            !found.iter().any(|path| path.contains("gone.rs")),
            "there is no git repository here, and the ignore file still means what it says: {found:?}"
        );
    }

    /// A repository's own directory changes on every commit and every checkout.
    /// An index that read it would decide the project had changed each time and
    /// reparse all of it, which is exactly the fault the plan's own numbers are
    /// meant to catch.
    #[test]
    fn a_repositorys_own_directory_is_never_read() {
        let root = tempfile::tempdir().expect("a directory");
        let at = root.path();
        std::fs::create_dir_all(at.join(".git/objects/ab")).expect("a repository");
        std::fs::write(at.join(".git/HEAD"), "ref: refs/heads/main\n").expect("its head");
        std::fs::write(at.join(".git/objects/ab/cdef"), "an object").expect("an object");
        std::fs::create_dir_all(at.join(".jj")).expect("another kind of repository");
        std::fs::write(at.join(".jj/state"), "").expect("its state");
        std::fs::write(at.join("main.rs"), "pub fn main() {}\n").expect("a source file");
        // A project's own hidden files are still its files.
        std::fs::write(at.join(".env"), "TOKEN=1\n").expect("the environment");

        let found = found_under(at);
        assert!(found.contains(&"main.rs".to_string()));
        assert!(
            found.contains(&".env".to_string()),
            "a hidden file of the project is one of its files: {found:?}"
        );
        assert!(
            !found.iter().any(|path| path.starts_with(".git/")),
            "the repository's own directory was read: {found:?}"
        );
        assert!(
            !found.iter().any(|path| path.starts_with(".jj/")),
            "another kind of repository was read: {found:?}"
        );
    }
}
