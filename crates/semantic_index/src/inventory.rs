use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use rayon::prelude::*;
use sha2::{Digest as _, Sha256};
use sqlez::connection::Connection;
use sqlez::statement::Statement;

use crate::languages;
use crate::walk;

/// One file as the inventory knows it. No symbols: this step only answers which
/// files there are and which of them have changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    /// Relative to the project root, with forward slashes whatever the platform,
    /// so a row written on one machine reads the same on another.
    pub path: String,
    pub bytes: u64,
    /// Seconds since the epoch. Recorded because it is cheap and worth having,
    /// never trusted: what decides whether a file changed is its fingerprint.
    pub changed_at: i64,
    /// Hex of the SHA-256 of the contents.
    pub fingerprint: String,
    pub language: Option<String>,
}

/// What one pass over the project did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stocktake {
    /// Files the walk found.
    pub read: usize,
    /// Rows written, whether new or changed in any field at all. The number the
    /// plan's second gate is about: a pass over an untouched project writes none.
    pub written: usize,
    /// Rows whose *contents* changed -- a new file, or one whose fingerprint is
    /// not what it was. The number the plan's third gate is about, and not the
    /// same as `written`: a branch switch puts a new time of last change on files
    /// whose contents it did not touch, so the row has to be written while the
    /// file has not really changed. Confusing the two would have the index
    /// reparse files it has already read.
    pub contents_changed: usize,
    pub unchanged: usize,
    /// Rows dropped for files that are no longer there.
    pub gone: usize,
    pub bytes: u64,
    pub took: Duration,
}

/// The project's file inventory, in a table of its own.
pub struct Inventory {
    connection: Connection,
}

impl Inventory {
    /// Opens the inventory kept at `path`, creating it if there is none.
    pub fn open(path: &Path) -> Result<Self> {
        let uri = path
            .to_str()
            .context("the inventory's own path is not text")?;
        Self::of(Connection::open_file(uri))
    }

    /// An inventory that lives only as long as the process, for tests and for a
    /// measurement that should leave nothing behind.
    pub fn open_in_memory() -> Result<Self> {
        Self::of(Connection::open_memory(None))
    }

    fn of(connection: Connection) -> Result<Self> {
        connection
            .exec(
                "CREATE TABLE IF NOT EXISTS files (
                     path TEXT PRIMARY KEY,
                     bytes INTEGER NOT NULL,
                     changed_at INTEGER NOT NULL,
                     fingerprint TEXT NOT NULL,
                     language TEXT
                 ) STRICT;",
            )
            .context("preparing the inventory")?()
        .context("preparing the inventory")?;
        Ok(Self { connection })
    }

    /// Every file the inventory holds, by path.
    pub fn known(&self) -> Result<HashMap<String, Known>> {
        let mut statement = Statement::prepare(
            &self.connection,
            "SELECT path, bytes, changed_at, fingerprint, language FROM files",
        )?;
        let rows = statement.map(|row| {
            Ok(Known {
                path: row.column_text(0)?.to_string(),
                bytes: row.column_int64(1)? as u64,
                changed_at: row.column_int64(2)?,
                fingerprint: row.column_text(3)?.to_string(),
                language: {
                    let said = row.column_text(4)?.to_string();
                    (!said.is_empty()).then_some(said)
                },
            })
        })?;
        Ok(rows
            .into_iter()
            .map(|known| (known.path.clone(), known))
            .collect())
    }

    /// Reads the project and brings the inventory in line with it.
    ///
    /// Every file is read and fingerprinted, every time. The size and the time of
    /// last change are recorded but never used to decide whether to read: a file
    /// edited twice within the same tick of the clock keeps both, and an
    /// inventory that trusted them would report it unchanged.
    pub fn take_stock(&self, root: &Path, cores: usize) -> Result<Stocktake> {
        let started = Instant::now();
        let claimed = languages::by_suffix();
        let found = walk::files_under(root);

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(cores)
            .build()
            .context("building the pool the pass runs on")?;
        let seen: Vec<Known> = pool.install(|| {
            found
                .par_iter()
                .filter_map(|path| read_one(root, path, &claimed))
                .collect()
        });

        let before = self.known()?;
        let mut stocktake = Stocktake {
            read: seen.len(),
            bytes: seen.iter().map(|known| known.bytes).sum(),
            ..Stocktake::default()
        };

        // One transaction for the whole pass: an inventory half brought in line
        // is worse than one not brought in line at all, because the next pass
        // would believe it.
        self.connection
            .exec("BEGIN IMMEDIATE")
            .context("beginning the pass")?()
        .context("beginning the pass")?;

        let mut write = Statement::prepare(
            &self.connection,
            "INSERT INTO files (path, bytes, changed_at, fingerprint, language)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET
                 bytes = excluded.bytes,
                 changed_at = excluded.changed_at,
                 fingerprint = excluded.fingerprint,
                 language = excluded.language",
        )?;
        for known in &seen {
            // Compared here rather than left to the database: the gate is that an
            // unchanged project causes no write at all, and a statement that
            // happens to write the same values is still a write.
            let known_before = before.get(&known.path);
            if known_before == Some(known) {
                stocktake.unchanged += 1;
                continue;
            }
            if known_before.is_none_or(|before| before.fingerprint != known.fingerprint) {
                stocktake.contents_changed += 1;
            }
            write.reset();
            write.bind_text(1, &known.path)?;
            write.bind_int64(2, known.bytes as i64)?;
            write.bind_int64(3, known.changed_at)?;
            write.bind_text(4, &known.fingerprint)?;
            match &known.language {
                Some(language) => write.bind_text(5, language)?,
                None => write.bind_null(5)?,
            }
            write.exec()?;
            stocktake.written += 1;
        }

        let still_there: std::collections::HashSet<&str> =
            seen.iter().map(|known| known.path.as_str()).collect();
        let mut forget = Statement::prepare(&self.connection, "DELETE FROM files WHERE path = ?")?;
        for path in before.keys() {
            if !still_there.contains(path.as_str()) {
                forget.reset();
                forget.bind_text(1, path)?;
                forget.exec()?;
                stocktake.gone += 1;
            }
        }

        self.connection
            .exec("COMMIT")
            .context("finishing the pass")?()
        .context("finishing the pass")?;

        stocktake.took = started.elapsed();
        Ok(stocktake)
    }

    /// How much room the inventory takes, which is the number the plan compares
    /// against the size of the sources.
    pub fn on_disk(&self, path: &Path) -> Option<u64> {
        std::fs::metadata(path).ok().map(|about| about.len())
    }
}

/// Reads one file. `None` for a file that cannot be read at all -- it is one
/// file fewer in the inventory, not a reason to abandon the pass.
fn read_one(root: &Path, path: &Path, claimed: &HashMap<String, String>) -> Option<Known> {
    let contents = std::fs::read(path).ok()?;
    let about = std::fs::metadata(path).ok()?;
    let changed_at = about
        .modified()
        .ok()
        .and_then(|when| when.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default();
    let name = path.file_name()?.to_str()?;
    Some(Known {
        path: relative_to(root, path)?,
        bytes: contents.len() as u64,
        changed_at,
        fingerprint: fingerprint_of(&contents),
        language: languages::of_file(name, claimed).map(str::to_string),
    })
}

/// The path as the table records it: relative to the root, forward slashes.
fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let inside = path.strip_prefix(root).ok()?;
    Some(inside.to_string_lossy().replace('\\', "/"))
}

/// The hex of the contents' SHA-256.
fn fingerprint_of(contents: &[u8]) -> String {
    let digest = Sha256::digest(contents);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        // Writing into a String cannot fail, and the alternative is an unwrap.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// A time in seconds since the epoch, for tests that need to set one.
pub fn seconds_since_the_epoch(when: SystemTime) -> i64 {
    when.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_project() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("a directory to put a project in");
        let at = root.path();
        std::fs::create_dir_all(at.join("cmd")).expect("a directory in it");
        std::fs::write(at.join("cmd/main.rs"), "pub fn first() {}\n").expect("a file");
        std::fs::write(at.join("notes.txt"), "not a language we parse\n").expect("another file");
        std::fs::write(at.join(".gitignore"), "built/\n").expect("the ignore file");
        std::fs::create_dir_all(at.join("built")).expect("an ignored directory");
        std::fs::write(at.join("built/output.rs"), "pub fn never() {}\n").expect("an ignored file");
        root
    }

    #[test]
    fn a_pass_records_what_the_project_holds_and_nothing_it_ignores() {
        let project = a_project();
        let inventory = Inventory::open_in_memory().expect("an inventory");

        let first = inventory
            .take_stock(project.path(), 2)
            .expect("the first pass");
        assert_eq!(first.read, 3, "two files and the ignore file itself");
        assert_eq!(first.written, 3);
        assert_eq!(first.unchanged, 0);
        assert_eq!(first.gone, 0);

        let known = inventory.known().expect("what it recorded");
        assert_eq!(known.len(), 3);
        assert!(
            !known.contains_key("built/output.rs"),
            "an ignored file is not part of the project"
        );
        let source = known.get("cmd/main.rs").expect("the source file");
        assert_eq!(source.language.as_deref(), Some("rust"));
        assert_eq!(source.bytes, 18);
        assert_eq!(source.fingerprint.len(), 64);
        assert_eq!(
            known
                .get("notes.txt")
                .and_then(|file| file.language.clone()),
            None,
            "a file of no language we parse is still in the inventory, without one"
        );
    }

    /// The plan's second gate: a pass over a project nothing has touched writes
    /// nothing at all.
    #[test]
    fn a_pass_over_an_unchanged_project_writes_nothing() {
        let project = a_project();
        let inventory = Inventory::open_in_memory().expect("an inventory");
        let first = inventory
            .take_stock(project.path(), 2)
            .expect("the first pass");

        for again in 0..3 {
            let repeated = inventory
                .take_stock(project.path(), 2)
                .expect("a repeated pass");
            assert_eq!(
                repeated.written,
                0,
                "pass {} wrote {} rows over an unchanged project",
                again + 2,
                repeated.written
            );
            assert_eq!(repeated.contents_changed, 0);
            assert_eq!(repeated.unchanged, first.read);
            assert_eq!(repeated.gone, 0);
        }
    }

    #[test]
    fn only_what_changed_is_written_and_what_is_gone_is_dropped() {
        let project = a_project();
        let at = project.path();
        let inventory = Inventory::open_in_memory().expect("an inventory");
        inventory.take_stock(at, 2).expect("the first pass");

        std::fs::write(
            at.join("cmd/main.rs"),
            "pub fn first() {}\npub fn second() {}\n",
        )
        .expect("editing one file");
        let after_an_edit = inventory.take_stock(at, 2).expect("a pass after the edit");
        assert_eq!(
            after_an_edit.contents_changed, 1,
            "one file changed, one row written"
        );
        assert_eq!(after_an_edit.unchanged, 2);

        std::fs::remove_file(at.join("notes.txt")).expect("removing one file");
        let after_a_removal = inventory
            .take_stock(at, 2)
            .expect("a pass after the removal");
        assert_eq!(after_a_removal.gone, 1);
        assert_eq!(after_a_removal.written, 0);
        assert!(
            !inventory
                .known()
                .expect("what it holds")
                .contains_key("notes.txt")
        );
    }

    /// A file whose contents change while its size and its recorded time do not.
    /// This is why the fingerprint is taken every time rather than trusted from
    /// the clock: two edits within one tick would otherwise pass unnoticed.
    #[test]
    fn a_change_that_keeps_the_size_and_the_time_is_still_noticed() {
        let project = a_project();
        let at = project.path();
        let inventory = Inventory::open_in_memory().expect("an inventory");
        inventory.take_stock(at, 2).expect("the first pass");

        let file = at.join("cmd/main.rs");
        let before = std::fs::metadata(&file).expect("about the file");
        std::fs::write(&file, "pub fn FIRST() {}\n").expect("the same length, other contents");
        // Put the clock back where it was, which is what a fast editor and a
        // coarse file system between them amount to.
        let when = before.modified().expect("when it changed");
        std::fs::File::options()
            .write(true)
            .open(&file)
            .expect("the file, to set its time")
            .set_modified(when)
            .expect("putting the time back");

        let after = inventory.take_stock(at, 2).expect("a pass after the edit");
        assert_eq!(
            after.written, 1,
            "the contents changed, so the row has to be written whatever the clock says"
        );
    }

    /// The plan's third gate, stated in its own terms: after a branch switch the
    /// number of files whose fingerprint changed is the number git reports.
    #[test]
    fn a_branch_switch_changes_exactly_what_git_says_it_changes() {
        let project = tempfile::tempdir().expect("a directory");
        let at = project.path();
        let git = |arguments: &[&str]| {
            // Through smol rather than std, as the project requires everywhere:
            // a spawn that blocks the calling thread for an unknown time has no
            // business in a test suite either.
            let done = smol::block_on(
                smol::process::Command::new("git")
                    .args(arguments)
                    .current_dir(at)
                    .env("GIT_AUTHOR_NAME", "index")
                    .env("GIT_AUTHOR_EMAIL", "index@example.com")
                    .env("GIT_COMMITTER_NAME", "index")
                    .env("GIT_COMMITTER_EMAIL", "index@example.com")
                    .output(),
            )
            .expect("git runs");
            assert!(
                done.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&done.stderr)
            );
            String::from_utf8_lossy(&done.stdout).to_string()
        };

        git(&["init", "--initial-branch=first"]);
        for at_number in 0..6 {
            std::fs::write(
                at.join(format!("file{at_number}.rs")),
                format!("pub fn first{at_number}() {{}}\n"),
            )
            .expect("a file");
        }
        git(&["add", "."]);
        git(&["commit", "-m", "first"]);

        git(&["checkout", "-b", "second"]);
        // Three of the six differ between the branches.
        for at_number in 0..3 {
            std::fs::write(
                at.join(format!("file{at_number}.rs")),
                format!("pub fn second{at_number}() {{}}\n"),
            )
            .expect("editing a file");
        }
        git(&["add", "."]);
        git(&["commit", "-m", "second"]);

        let inventory = Inventory::open_in_memory().expect("an inventory");
        git(&["checkout", "first"]);
        let on_the_first = inventory.take_stock(at, 2).expect("stock on the first");

        git(&["checkout", "second"]);
        let after_the_switch = inventory.take_stock(at, 2).expect("stock after the switch");
        let held: Vec<String> = inventory
            .known()
            .expect("what it holds")
            .into_keys()
            .collect();

        let git_says = git(&["diff", "--name-only", "first", "second"])
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        assert_eq!(
            git_says, 3,
            "the branches were built to differ in three files"
        );
        assert_eq!(
            after_the_switch.contents_changed, git_says,
            "the contents of exactly the files git reports as different have to change.\n\
             on the first: {on_the_first:?}\nafter the switch: {after_the_switch:?}\nheld: {held:?}"
        );
        // The switch also puts a new time of last change on files whose contents
        // it did not touch, so more rows are written than files really changed.
        // Asserted rather than remarked on: it is the reason the two numbers
        // exist at all.
        assert!(
            after_the_switch.written >= after_the_switch.contents_changed,
            "written {} is fewer than changed {}",
            after_the_switch.written,
            after_the_switch.contents_changed
        );
    }

    #[test]
    fn a_fingerprint_is_of_the_contents_and_of_nothing_else() {
        assert_eq!(fingerprint_of(b"").len(), 64);
        assert_eq!(fingerprint_of(b"one"), fingerprint_of(b"one"));
        assert_ne!(fingerprint_of(b"one"), fingerprint_of(b"two"));
        // The known answer, so a change of algorithm cannot pass unnoticed.
        assert_eq!(
            fingerprint_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn an_inventory_kept_on_disk_comes_back_as_it_was_left() {
        let project = a_project();
        let held = tempfile::tempdir().expect("a directory for the inventory");
        let kept_at = held.path().join("inventory.db");

        let written = {
            let inventory = Inventory::open(&kept_at).expect("an inventory on disk");
            inventory
                .take_stock(project.path(), 2)
                .expect("the first pass");
            inventory.known().expect("what it holds")
        };

        let reopened = Inventory::open(&kept_at).expect("the same inventory again");
        assert_eq!(reopened.known().expect("what it holds"), written);
        // And a pass against the reopened one still writes nothing, which is what
        // makes the inventory worth keeping at all.
        assert_eq!(
            reopened
                .take_stock(project.path(), 2)
                .expect("a pass")
                .written,
            0
        );
        assert!(reopened.on_disk(&kept_at).is_some_and(|size| size > 0));
    }
}
