use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use rayon::prelude::*;

use crate::definitions::{self, Definition};
use crate::inventory::Inventory;
use crate::languages::{self, Readable};
use crate::measure;
use crate::symbols::{self, Symbols};

/// What one refresh did.
///
/// `files_parsed` is the number the plan's M3 gates are stated in terms of --
/// it does not depend on the machine at all, and it is what catches an index
/// that quietly reparses more than it needed to.
#[derive(Debug, Clone, Default)]
pub struct Refreshed {
    pub files_parsed: usize,
    pub files_forgotten: usize,
    pub symbols_before: usize,
    pub symbols_after: usize,
    pub took: Duration,
}

/// Brings `symbols` in line with the disk under `root`, reparsing only the
/// files whose contents changed since `inventory` last looked, and dropping
/// the symbols of files that are gone.
///
/// `inventory.take_stock` names the files whose contents changed and the files
/// that are gone, as part of the pass it already makes. Because it fingerprints
/// every file fresh every time, that list is exactly "the contents changed" and
/// never "the modification time changed" -- a `git checkout` puts a new
/// modification time on a file it did not otherwise touch, and that file must
/// not be reparsed.
///
/// Every file this reads is read fresh from disk, by path -- see the doc on
/// [`refresh_one_file`] for why that is a structural guarantee here and not
/// only a documented intention. This is the whole-project counterpart to a
/// save: the function to run after something outside the editor's own
/// knowledge could have changed many files at once, such as a branch switch.
pub fn refresh(
    root: &Path,
    cores: usize,
    inventory: &Inventory,
    symbols: &Symbols,
) -> Result<Refreshed> {
    let started = Instant::now();
    let symbols_before = symbols.count()?;
    let cores = cores.max(1);

    // The inventory names what changed and what is gone as part of the pass it
    // already makes, so there is nothing to work out here a second time.
    let stocktake = inventory.take_stock(root, cores)?;

    let (readable, refused) = languages::readable();
    for trouble in &refused {
        log::warn!("outline query left out of the refresh -- {trouble}");
    }
    let claimed = languages::suffixes_of(&readable);

    // Only files of a language the index actually reads are worth reparsing;
    // a changed file of any other kind never had symbols to begin with.
    let to_parse: Vec<(String, PathBuf, usize)> = stocktake
        .changed
        .iter()
        .cloned()
        .filter_map(|relative| {
            let name = Path::new(&relative).file_name()?.to_str()?;
            let language = languages::claimant(name, &claimed)?;
            let full = root.join(&relative);
            Some((relative, full, language))
        })
        .collect();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cores)
        .build()
        .context("building the pool the refresh runs on")?;
    let read: Vec<(String, Vec<Definition>)> = pool.install(|| {
        to_parse
            .par_iter()
            .filter_map(|(relative, full, language)| {
                let mut parser = tree_sitter::Parser::new();
                let found = definitions::in_file_on_disk(
                    root,
                    full,
                    readable.get(*language)?,
                    &mut parser,
                )?;
                Some((relative.clone(), found))
            })
            .collect()
    });

    // One transaction for the whole refresh. Three hundred files written one at a
    // time is three hundred transactions, and a branch switch has two seconds.
    // An empty `found` still replaces whatever was recorded before -- a file
    // caught mid-edit parses to nothing; see the module doc for why that is the
    // chosen behaviour rather than an oversight.
    let files_forgotten = symbols.record_all(&read, &stocktake.dropped)?;
    let files_parsed = read.len();

    Ok(Refreshed {
        files_parsed,
        files_forgotten,
        symbols_before,
        symbols_after: symbols.count()?,
        took: started.elapsed(),
    })
}

/// Reparses one file that has just been saved and records what it defines
/// now, replacing whatever was recorded for it before.
///
/// Takes `path`, never the buffer's own text: there is no parameter through
/// which this function could be handed anything but what is on disk, which is
/// what makes the unsaved-buffer rule a fact about this signature rather than
/// a promise about how it happens to be called. A buffer with edits still
/// pending has not been saved, so it is not a file this function has any way
/// to see; only the last saved version on disk is ever read, by
/// [`definitions::in_file_on_disk`], which itself only ever opens `path`.
///
/// Costs one parse and one small write, never a walk of the project -- this
/// is the function the plan's fifty-millisecond save gate is measured
/// against, and a directory walk alone would put that budget at risk on a
/// large tree. It deliberately does not touch an [`Inventory`]: a save is an
/// event the editor already knows happened to a specific, named file, so
/// there is nothing here to detect, only something to redo. The consequence
/// is that the inventory's own record of this file is left stale until a
/// later whole-project [`refresh`] passes over it, at which point it is
/// reparsed once more -- one avoidable parse, traded for a save that never
/// has to look at any file but its own.
pub fn refresh_one_file(root: &Path, path: &Path, symbols: &Symbols) -> Result<Refreshed> {
    let started = Instant::now();
    let symbols_before = symbols.count()?;
    let unrefreshed = || Refreshed {
        files_parsed: 0,
        files_forgotten: 0,
        symbols_before,
        symbols_after: symbols_before,
        took: started.elapsed(),
    };

    let Some(relative) = relative_path(root, path) else {
        return Ok(unrefreshed());
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(unrefreshed());
    };

    let (readable, refused) = languages::readable();
    for trouble in &refused {
        log::warn!("outline query left out of the refresh -- {trouble}");
    }
    let claimed = languages::suffixes_of(&readable);
    let Some(language) = readable_language(name, &readable, &claimed) else {
        return Ok(unrefreshed());
    };

    let mut parser = tree_sitter::Parser::new();
    let Some(found) = definitions::in_file_on_disk(root, path, language, &mut parser) else {
        // Could not even be read: nothing was parsed, so nothing already
        // recorded for it is disturbed.
        return Ok(unrefreshed());
    };
    // `found` can be empty -- see the doc on `refresh` for why an empty
    // result still replaces what was recorded.
    symbols.record(&relative, &found)?;

    Ok(Refreshed {
        files_parsed: 1,
        files_forgotten: 0,
        symbols_before,
        symbols_after: symbols.count()?,
        took: started.elapsed(),
    })
}

/// The language `name` belongs to, among the languages the index actually
/// reads -- an outline query exists for it -- by the longest-suffix-wins rule
/// the rest of the crate uses everywhere else.
fn readable_language<'a>(
    name: &str,
    readable: &'a [Readable],
    claimed: &HashMap<&str, usize>,
) -> Option<&'a Readable> {
    let at = languages::claimant(name, claimed)?;
    readable.get(at)
}

/// `path`, relative to `root`, with forward slashes -- the shape every table
/// in this crate keys its rows by.
fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let inside = path.strip_prefix(root).ok()?;
    Some(inside.to_string_lossy().replace('\\', "/"))
}

/// The plan's ceiling for saving a file this large.
const SAVE_CEILING: Duration = Duration::from_millis(50);
/// The size of file the save gate is stated against.
const SAVE_FILE_LINES: usize = 2000;
/// The plan's ceiling for a branch switch that changes this many files.
const BRANCH_SWITCH_CEILING: Duration = Duration::from_secs(2);
/// The number of changed files the branch-switch gate is stated against.
const BRANCH_SWITCH_FILE_COUNT: usize = 300;

/// What the plan's two M3 scenarios cost, measured against their own
/// ceilings.
#[derive(Debug, Clone, Default)]
pub struct RefreshNumbers {
    pub saving_a_large_file: Duration,
    pub switching_a_branch: Duration,
    pub files_changed_by_the_switch: usize,
    pub files_parsed_by_the_switch: usize,
}

impl fmt::Display for RefreshNumbers {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            out,
            "saving a {SAVE_FILE_LINES}-line file    {:>10}   ceiling {:>10}   {}",
            measure::as_time(self.saving_a_large_file),
            measure::as_time(SAVE_CEILING),
            if self.saving_a_large_file < SAVE_CEILING {
                "under"
            } else {
                "OVER"
            }
        )?;
        writeln!(
            out,
            "switching a branch ({BRANCH_SWITCH_FILE_COUNT} files)   {:>10}   ceiling {:>10}   {}",
            measure::as_time(self.switching_a_branch),
            measure::as_time(BRANCH_SWITCH_CEILING),
            if self.switching_a_branch < BRANCH_SWITCH_CEILING {
                "under"
            } else {
                "OVER"
            }
        )?;
        write!(
            out,
            "  files parsed on the switch: {} of {} changed",
            self.files_parsed_by_the_switch, self.files_changed_by_the_switch
        )
    }
}

/// Builds `root` once, then times the plan's two M3 scenarios against it and
/// prints the result.
///
/// `root` is working storage for the scenarios themselves, not merely a
/// project to read: saving a file and switching a branch both really happen
/// here, including running a real git repository for the branch-switch
/// scenario. `root` should be a directory built for measuring, never a
/// project whose current contents matter.
pub fn measure(root: &Path, cores: usize) -> Result<RefreshNumbers> {
    let inventory = Inventory::open_in_memory()?;
    let symbols = Symbols::open_in_memory()?;
    inventory.take_stock(root, cores)?;
    symbols::build(root, cores, &symbols)?;

    let saving_a_large_file = {
        let path = root.join("m3_measure_save.rs");
        let mut source = String::new();
        for at in 0..SAVE_FILE_LINES {
            source.push_str(&format!("pub fn generated_{at}() {{}}\n"));
        }
        std::fs::write(&path, &source)
            .context("writing the file the save scenario is measured on")?;
        // Recorded once, unmeasured, so the save that follows is an edit to a
        // file already in the index rather than the first sight of it.
        refresh_one_file(root, &path, &symbols)?;

        source.push_str("pub fn generated_last() {}\n");
        std::fs::write(&path, &source).context("saving the file a second time")?;
        refresh_one_file(root, &path, &symbols)?.took
    };

    let (files_changed_by_the_switch, switching_a_branch, files_parsed_by_the_switch) = {
        run_git(root, &["init", "--initial-branch=first"])
            .context("starting the branch-switch fixture")?;
        for at in 0..BRANCH_SWITCH_FILE_COUNT {
            std::fs::write(
                root.join(format!("m3_measure_branch_{at:04}.rs")),
                format!("pub fn f{at}() {{}}\n"),
            )
            .context("writing a branch-switch fixture file")?;
        }
        run_git(root, &["add", "."])?;
        run_git(root, &["commit", "-m", "first"])?;

        run_git(root, &["checkout", "-b", "second"])?;
        for at in 0..BRANCH_SWITCH_FILE_COUNT {
            std::fs::write(
                root.join(format!("m3_measure_branch_{at:04}.rs")),
                format!("pub fn g{at}() {{}}\n"),
            )
            .context("editing a branch-switch fixture file")?;
        }
        run_git(root, &["add", "."])?;
        run_git(root, &["commit", "-m", "second"])?;

        // The inventory and the symbols are brought up to date while still on
        // the branch that will be switched away from, so the switch back is
        // the change being measured, not the fixture's own construction.
        inventory.take_stock(root, cores)?;
        symbols::build(root, cores, &symbols)?;

        run_git(root, &["checkout", "first"])?;
        let git_says = run_git(root, &["diff", "--name-only", "second", "first"])?
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();

        let started = Instant::now();
        let refreshed = refresh(root, cores, &inventory, &symbols)?;
        (git_says, started.elapsed(), refreshed.files_parsed)
    };

    let numbers = RefreshNumbers {
        saving_a_large_file,
        switching_a_branch,
        files_changed_by_the_switch,
        files_parsed_by_the_switch,
    };
    println!("{numbers}");
    Ok(numbers)
}

/// Runs one git command against a repository at `at` and returns its
/// standard output, failing loudly with the command and its stderr if it did
/// not succeed. Through `smol::process::Command`, never `std::process::Command`,
/// which this workspace's lints refuse: a spawn that blocks the calling
/// thread for an unknown time has no business running synchronously here
/// either, only the wait for it is kept inside one call rather than left to
/// the operating system's own scheduling.
fn run_git(at: &Path, arguments: &[&str]) -> Result<String> {
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
    .context("running git")?;
    anyhow::ensure!(
        done.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&done.stderr)
    );
    Ok(String::from_utf8_lossy(&done.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project already fully built: the inventory has taken stock of it and
    /// the symbol store holds everything its outline queries found. What
    /// every test below starts from, so each one is only about what happens
    /// *after* that point.
    fn baseline(root: &Path, cores: usize) -> Result<(Inventory, Symbols)> {
        let inventory = Inventory::open_in_memory()?;
        inventory.take_stock(root, cores)?;
        let symbols = Symbols::open_in_memory()?;
        symbols::build(root, cores, &symbols)?;
        Ok((inventory, symbols))
    }

    #[test]
    fn saving_one_file_parses_only_that_file_and_leaves_the_rest_byte_for_byte() {
        let project = tempfile::tempdir().expect("a directory");
        let at = project.path();
        std::fs::write(at.join("one.rs"), "pub fn first() {}\n").expect("a file");
        std::fs::write(at.join("two.rs"), "pub fn second() {}\n").expect("another file");

        let symbols = Symbols::open_in_memory().expect("a store");
        symbols::build(at, 2, &symbols).expect("the initial build");
        let two_before = symbols.in_file("two.rs").expect("two.rs's symbols before");
        assert!(!two_before.is_empty());

        std::fs::write(at.join("one.rs"), "pub fn first() {}\npub fn also() {}\n")
            .expect("the save");
        let refreshed =
            refresh_one_file(at, &at.join("one.rs"), &symbols).expect("the one-file refresh");

        assert_eq!(refreshed.files_parsed, 1);
        assert_eq!(refreshed.files_forgotten, 0);
        let one_after = symbols.in_file("one.rs").expect("one.rs's symbols after");
        assert_eq!(one_after.len(), 2, "{one_after:?}");

        let two_after = symbols.in_file("two.rs").expect("two.rs's symbols after");
        assert_eq!(
            two_after, two_before,
            "an unrelated file's symbols must be untouched, byte for byte"
        );
    }

    #[test]
    fn a_deleted_file_has_its_symbols_forgotten_and_nothing_else_touched() {
        let project = tempfile::tempdir().expect("a directory");
        let at = project.path();
        std::fs::write(at.join("one.rs"), "pub fn first() {}\n").expect("a file");
        std::fs::write(at.join("two.rs"), "pub fn second() {}\n").expect("another file");

        let (inventory, symbols) = baseline(at, 2).expect("the baseline");
        let two_before = symbols.in_file("two.rs").expect("two.rs's symbols before");

        std::fs::remove_file(at.join("one.rs")).expect("deleting the file");
        let refreshed = refresh(at, 2, &inventory, &symbols).expect("the refresh");

        assert_eq!(
            refreshed.files_parsed, 0,
            "nothing to reparse, only something to forget"
        );
        assert_eq!(refreshed.files_forgotten, 1);
        assert!(symbols.in_file("one.rs").expect("one.rs after").is_empty());
        assert_eq!(symbols.files().expect("the file count"), 1);
        assert_eq!(symbols.in_file("two.rs").expect("two.rs after"), two_before);
    }

    #[test]
    fn a_file_added_is_parsed_and_recorded() {
        let project = tempfile::tempdir().expect("a directory");
        let at = project.path();
        std::fs::write(at.join("one.rs"), "pub fn first() {}\n").expect("a file");
        let (inventory, symbols) = baseline(at, 2).expect("the baseline");

        std::fs::write(at.join("two.rs"), "pub fn second() {}\n").expect("a new file");
        let refreshed = refresh(at, 2, &inventory, &symbols).expect("the refresh");

        assert_eq!(refreshed.files_parsed, 1);
        assert_eq!(refreshed.files_forgotten, 0);
        let two = symbols.in_file("two.rs").expect("the new file's symbols");
        assert_eq!(two.len(), 1, "{two:?}");
        assert_eq!(two[0].name, "second");
    }

    /// The class of bug an incremental index is most at risk of: a pass over
    /// nothing new quietly reparsing everything anyway.
    #[test]
    fn a_refresh_over_an_untouched_project_parses_nothing() {
        let project = tempfile::tempdir().expect("a directory");
        let at = project.path();
        std::fs::write(at.join("one.rs"), "pub fn first() {}\n").expect("a file");
        let (inventory, symbols) = baseline(at, 2).expect("the baseline");

        let refreshed = refresh(at, 2, &inventory, &symbols).expect("a repeated refresh");
        assert_eq!(refreshed.files_parsed, 0, "nothing on disk changed");
        assert_eq!(refreshed.files_forgotten, 0);
        assert_eq!(refreshed.symbols_before, refreshed.symbols_after);
    }

    /// The plan's own terms for the branch-switch gate: exactly the files
    /// `git diff --name-only` reports are reparsed, and -- the point of the
    /// test -- a file whose modification time the switch touches without
    /// touching its contents is not one of them. `git checkout` puts a new
    /// modification time on every file it writes, changed or not, which is
    /// exactly what makes the untouched files here a real instance of the
    /// case this test exists to catch, not a contrived one.
    #[test]
    fn a_branch_switch_parses_exactly_the_files_that_really_changed() {
        let project = tempfile::tempdir().expect("a directory");
        let at = project.path();

        run_git(at, &["init", "--initial-branch=first"]).expect("git init");
        for at_number in 0..6 {
            std::fs::write(
                at.join(format!("file{at_number}.rs")),
                format!("pub fn first{at_number}() {{}}\n"),
            )
            .expect("a file");
        }
        run_git(at, &["add", "."]).expect("git add");
        run_git(at, &["commit", "-m", "first"]).expect("the first commit");

        run_git(at, &["checkout", "-b", "second"]).expect("a new branch");
        // Three of the six files really change on the second branch; the
        // other three keep their exact contents.
        for at_number in 0..3 {
            std::fs::write(
                at.join(format!("file{at_number}.rs")),
                format!("pub fn second{at_number}() {{}}\n"),
            )
            .expect("editing a file");
        }
        run_git(at, &["add", "."]).expect("git add");
        run_git(at, &["commit", "-m", "second"]).expect("the second commit");

        // Built while on the second branch, so switching back to the first is
        // the change being measured.
        let (inventory, symbols) = baseline(at, 2).expect("the baseline");
        let file3_before = symbols.in_file("file3.rs").expect("file3's symbols before");
        assert_eq!(file3_before.len(), 1, "{file3_before:?}");

        run_git(at, &["checkout", "first"]).expect("switching back");
        let git_says = run_git(at, &["diff", "--name-only", "second", "first"])
            .expect("git's own count")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        assert_eq!(
            git_says, 3,
            "the branches were built to differ in three files"
        );

        let refreshed = refresh(at, 2, &inventory, &symbols).expect("the refresh after the switch");
        assert_eq!(
            refreshed.files_parsed, git_says,
            "exactly the files git reports as different are reparsed, not the whole tree"
        );
        assert_eq!(refreshed.files_parsed, 3);

        // A weaker check than the count above -- reparsing unchanged content
        // would give the same answer back -- but still worth having: the
        // symbol a file the switch did not touch had before the switch is
        // exactly what it has after.
        let file3_after = symbols.in_file("file3.rs").expect("file3's symbols after");
        assert_eq!(file3_after, file3_before);
    }

    /// The fingerprint is what decides, never the size or the clock: two
    /// edits that keep both would otherwise pass unnoticed. Borrows the
    /// technique `inventory.rs` uses for the same claim about `Stocktake`.
    #[test]
    fn a_change_that_keeps_the_size_and_the_time_is_still_reparsed() {
        let project = tempfile::tempdir().expect("a directory");
        let at = project.path();
        let file = at.join("one.rs");
        std::fs::write(&file, "pub fn work() {}\n").expect("the first version");
        let (inventory, symbols) = baseline(at, 2).expect("the baseline");
        assert_eq!(symbols.in_file("one.rs").expect("before")[0].name, "work");

        let before_meta = std::fs::metadata(&file).expect("the file's metadata");
        std::fs::write(&file, "pub fn FIRST() {}\n").expect("the same length, other contents");
        let when = before_meta.modified().expect("when it changed");
        std::fs::File::options()
            .write(true)
            .open(&file)
            .expect("the file, to set its time")
            .set_modified(when)
            .expect("putting the time back");

        let refreshed = refresh(at, 2, &inventory, &symbols).expect("the refresh");
        assert_eq!(
            refreshed.files_parsed, 1,
            "the contents changed, so it has to be reparsed whatever the clock says"
        );
        assert_eq!(symbols.in_file("one.rs").expect("after")[0].name, "FIRST");
    }

    /// The chosen behaviour for a file left mid-edit on disk: a refresh
    /// replaces its symbols with whatever parsing finds right now, which can
    /// be nothing, rather than keeping the last good set.
    ///
    /// Why wipe rather than keep: an incremental refresh has to agree with
    /// what a full rebuild would say about the same disk contents, or the two
    /// ways of arriving at "the index" would disagree with each other for no
    /// reason a person could see. `definitions.rs` already makes this same
    /// call for the same file, for the same reason -- see
    /// `a_file_caught_mid_edit_contributes_nothing_until_it_parses_again` --
    /// and a refresh that kept the old symbols here would make that file the
    /// one case where an incremental update and a fresh build disagree.
    #[test]
    fn a_file_left_mid_edit_has_its_symbols_wiped_not_kept() {
        let project = tempfile::tempdir().expect("a directory");
        let at = project.path();
        let file = at.join("half.rs");
        std::fs::write(&file, "pub fn work() {}\n").expect("the well-formed version");

        let symbols = Symbols::open_in_memory().expect("a store");
        symbols::build(at, 2, &symbols).expect("the initial build");
        assert_eq!(symbols.in_file("half.rs").expect("before")[0].name, "work");

        std::fs::write(&file, "pub struct Half {\npub fn work(").expect("caught mid-edit");
        let refreshed = refresh_one_file(at, &file, &symbols).expect("the refresh");

        assert_eq!(
            refreshed.files_parsed, 1,
            "it was read and parsed -- it simply found nothing"
        );
        assert!(
            symbols.in_file("half.rs").expect("after").is_empty(),
            "the chosen behaviour: an unparseable file's symbols are wiped, not kept"
        );
    }
}
