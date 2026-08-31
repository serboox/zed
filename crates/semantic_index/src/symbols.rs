use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use fuzzy::CharBag;
use rayon::prelude::*;
use sqlez::connection::Connection;
use sqlez::statement::Statement;

use crate::definitions::{self, Definition};
use crate::languages::{self, Readable};
use crate::measure;
use crate::walk;

/// How many files one round of the build pass parses before its symbols are
/// written and dropped.
///
/// The whole project's symbols held at once would be the largest thing the pass
/// ever allocates, and the step has a memory ceiling to meet. Writing in rounds
/// keeps only a round's worth in hand at a time.
const A_ROUND: usize = 256;

/// The project's symbols, in tables of their own.
///
/// Paths, kinds and language names are held once each and referred to by number,
/// and the symbols themselves carry no row identifier. Both are about room: at a
/// quarter of a million symbols, a forty character path written beside every one
/// of them would spend the whole space the plan allows on the paths alone, and a
/// second index over the file column would spend a fifth of it.
pub struct Symbols {
    connection: Connection,
}

/// What one build pass did.
#[derive(Debug, Clone, Default)]
pub struct Built {
    pub files: usize,
    pub symbols: usize,
    /// Reading and parsing every file and running its outline query.
    pub reading: Duration,
    /// Writing what was found, which is the part the store adds.
    pub writing: Duration,
    pub took: Duration,
    pub bytes: u64,
    pub cores: usize,
    /// The highest the process ever reached, from the kernel's own mark.
    pub the_most_memory: Option<u64>,
}

impl Symbols {
    /// Opens the symbols kept at `path`, creating them if there are none.
    pub fn open(path: &Path) -> Result<Self> {
        let uri = path.to_str().context("the store's own path is not text")?;
        Self::of(Connection::open_file(uri))
    }

    /// Symbols that live only as long as the process, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::of(Connection::open_memory(None))
    }

    fn of(connection: Connection) -> Result<Self> {
        // One statement per call: sqlez prepares every statement of a call
        // before running any of them, so an index prepared alongside the table
        // it is on would be prepared before that table exists.
        for statement in [
            "CREATE TABLE IF NOT EXISTS files (
                 id INTEGER PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE
             ) STRICT;",
            "CREATE TABLE IF NOT EXISTS kinds (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL UNIQUE
             ) STRICT;",
            "CREATE TABLE IF NOT EXISTS languages (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL UNIQUE
             ) STRICT;",
            // Keyed by file and by where in that file the symbol was found,
            // and holding no row identifier of its own: the rows of one file
            // then sit together, so replacing what a file defines is a walk
            // over a range rather than a lookup through a second index. That
            // index would have cost a fifth of the room the whole store is
            // allowed. `place` is there because nothing else about a symbol is
            // guaranteed unique -- two definitions can share a file, a line, a
            // name and a kind -- and a key that collides would fail a build
            // rather than record it.
            "CREATE TABLE IF NOT EXISTS symbols (
                 file INTEGER NOT NULL,
                 place INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 kind INTEGER NOT NULL,
                 language INTEGER NOT NULL,
                 line INTEGER NOT NULL,
                 PRIMARY KEY (file, place)
             ) STRICT, WITHOUT ROWID;",
        ] {
            run(&connection, statement).context("preparing the symbol tables")?;
        }
        Ok(Self { connection })
    }

    /// Replaces everything recorded for one file.
    ///
    /// An empty list is not the same as never having read the file: it records
    /// that the file was read and defines nothing, which is what stops a later
    /// pass reading it again for no reason.
    pub fn record(&self, path: &str, found: &[Definition]) -> Result<()> {
        self.in_one_transaction(|store| {
            let mut writing = Writing::on(&store.connection)?;
            writing.replace(path, found)
        })
    }

    /// Replaces everything recorded for each of these files, in one transaction.
    ///
    /// The whole point of taking a list: three hundred files written one at a
    /// time is three hundred transactions, and a branch switch has two seconds
    /// to finish. Files that are gone are named separately from files that were
    /// read, because the two are different things -- one has no symbols, the
    /// other is no longer part of the project.
    /// Returns how many of `gone` the store actually held, so a caller counting
    /// what it dropped does not have to read the whole table to find out.
    pub fn record_all(&self, read: &[(String, Vec<Definition>)], gone: &[String]) -> Result<usize> {
        self.in_one_transaction(|store| {
            let mut writing = Writing::on(&store.connection)?;
            for (path, found) in read {
                writing.replace(path, found)?;
            }
            let mut forgotten = 0;
            for path in gone {
                if writing.forget(path)? {
                    forgotten += 1;
                }
            }
            Ok(forgotten)
        })
    }

    /// Drops everything recorded for one file, and says whether there was any.
    pub fn forget(&self, path: &str) -> Result<bool> {
        self.in_one_transaction(|store| {
            let mut writing = Writing::on(&store.connection)?;
            writing.forget(path)
        })
    }

    /// How many symbols the store holds.
    pub fn count(&self) -> Result<usize> {
        let mut statement = Statement::prepare(&self.connection, "SELECT COUNT(*) FROM symbols")?;
        let counted = statement.maybe(|row| row.column_int64(0))?;
        Ok(counted.unwrap_or_default() as usize)
    }

    /// How many files the store has read, whether they defined anything or not.
    pub fn files(&self) -> Result<usize> {
        let mut statement = Statement::prepare(&self.connection, "SELECT COUNT(*) FROM files")?;
        let counted = statement.maybe(|row| row.column_int64(0))?;
        Ok(counted.unwrap_or_default() as usize)
    }

    /// Everything the store holds, which is what a search is built from.
    pub fn everything(&self) -> Result<Vec<Definition>> {
        let mut statement = Statement::prepare(
            &self.connection,
            "SELECT files.path, symbols.name, kinds.name, symbols.line, languages.name
             FROM symbols
             JOIN files ON files.id = symbols.file
             JOIN kinds ON kinds.id = symbols.kind
             JOIN languages ON languages.id = symbols.language",
        )?;
        statement.map(read_definition)
    }

    /// Everything the store holds for one file, in the order it was recorded.
    pub fn in_file(&self, path: &str) -> Result<Vec<Definition>> {
        let mut statement = Statement::prepare(
            &self.connection,
            "SELECT files.path, symbols.name, kinds.name, symbols.line, languages.name
             FROM symbols
             JOIN files ON files.id = symbols.file
             JOIN kinds ON kinds.id = symbols.kind
             JOIN languages ON languages.id = symbols.language
             WHERE files.path = ?
             ORDER BY symbols.place",
        )?;
        statement.bind_text(1, path)?;
        statement.map(read_definition)
    }

    /// Gives back the room that deleted rows left behind, so the size on disk is
    /// the size of what is held rather than of what has ever been held.
    pub fn compact(&self) -> Result<()> {
        run(&self.connection, "VACUUM").context("compacting")
    }

    /// How much room the store takes.
    pub fn on_disk(path: &Path) -> Option<u64> {
        std::fs::metadata(path).ok().map(|about| about.len())
    }

    /// Runs `work` with the store's tables locked, committing what it did or
    /// undoing all of it. A half-written store is worse than an unwritten one,
    /// because the next pass would believe it.
    fn in_one_transaction<R>(&self, work: impl FnOnce(&Self) -> Result<R>) -> Result<R> {
        run(&self.connection, "BEGIN IMMEDIATE").context("beginning to write")?;
        match work(self) {
            Ok(done) => {
                run(&self.connection, "COMMIT").context("finishing the write")?;
                Ok(done)
            }
            Err(trouble) => {
                // Reported rather than swallowed: a rollback that itself fails
                // leaves the store locked, and whoever comes next has to know
                // why.
                if let Err(and_then) = run(&self.connection, "ROLLBACK") {
                    log::error!("could not undo a failed write to the symbol store: {and_then}");
                }
                Err(trouble)
            }
        }
    }
}

/// Runs one statement that returns nothing.
fn run(connection: &Connection, statement: &str) -> Result<()> {
    let mut prepared = connection.exec(statement)?;
    prepared()
}

fn read_definition(row: &mut Statement) -> Result<Definition> {
    Ok(Definition {
        path: row.column_text(0)?.to_string(),
        name: row.column_text(1)?.to_string(),
        kind: row.column_text(2)?.to_string(),
        line: row.column_int64(3)? as u32,
        language: row.column_text(4)?.to_string(),
    })
}

/// The prepared statements one write needs, so nothing is looked up twice.
struct Writing<'a> {
    files: Dictionary<'a>,
    kinds: Dictionary<'a>,
    languages: Dictionary<'a>,
    clear: Statement<'a>,
    write: Statement<'a>,
}

impl<'a> Writing<'a> {
    fn on(connection: &'a Connection) -> Result<Self> {
        Ok(Self {
            files: Dictionary::on(connection, "files", "path")?,
            kinds: Dictionary::on(connection, "kinds", "name")?,
            languages: Dictionary::on(connection, "languages", "name")?,
            clear: Statement::prepare(connection, "DELETE FROM symbols WHERE file = ?")?,
            write: Statement::prepare(
                connection,
                "INSERT INTO symbols (file, place, name, kind, language, line)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )?,
        })
    }

    /// Replaces one file's symbols with these.
    fn replace(&mut self, path: &str, found: &[Definition]) -> Result<()> {
        let file = self.files.of(path)?;
        self.clear.reset();
        self.clear.bind_int64(1, file)?;
        self.clear.exec()?;
        for (place, one) in found.iter().enumerate() {
            let kind = self.kinds.of(&one.kind)?;
            let language = self.languages.of(&one.language)?;
            self.write.reset();
            self.write.bind_int64(1, file)?;
            self.write.bind_int64(2, place as i64)?;
            self.write.bind_text(3, &one.name)?;
            self.write.bind_int64(4, kind)?;
            self.write.bind_int64(5, language)?;
            self.write.bind_int64(6, one.line as i64)?;
            self.write.exec()?;
        }
        Ok(())
    }

    /// Drops a file and its symbols, and says whether it was there at all.
    fn forget(&mut self, path: &str) -> Result<bool> {
        let Some(file) = self.files.known(path)? else {
            return Ok(false);
        };
        self.clear.reset();
        self.clear.bind_int64(1, file)?;
        self.clear.exec()?;
        self.files.forget(file)?;
        Ok(true)
    }
}

/// One of the tables that hold a name once and hand out a number for it.
struct Dictionary<'a> {
    find: Statement<'a>,
    add: Statement<'a>,
    remove: Statement<'a>,
    numbered: HashMap<String, i64>,
}

impl<'a> Dictionary<'a> {
    fn on(connection: &'a Connection, table: &str, column: &str) -> Result<Self> {
        Ok(Self {
            find: Statement::prepare(
                connection,
                format!("SELECT id FROM {table} WHERE {column} = ?"),
            )?,
            add: Statement::prepare(
                connection,
                format!("INSERT INTO {table} ({column}) VALUES (?)"),
            )?,
            remove: Statement::prepare(connection, format!("DELETE FROM {table} WHERE id = ?"))?,
            numbered: HashMap::new(),
        })
    }

    /// The number this name already has, or `None` where it has none.
    fn known(&mut self, name: &str) -> Result<Option<i64>> {
        if let Some(number) = self.numbered.get(name) {
            return Ok(Some(*number));
        }
        self.find.reset();
        self.find.bind_text(1, name)?;
        let found = self.find.maybe(|row| row.column_int64(0))?;
        if let Some(number) = found {
            self.numbered.insert(name.to_string(), number);
        }
        Ok(found)
    }

    /// The number for this name, giving it one if it has none.
    fn of(&mut self, name: &str) -> Result<i64> {
        if let Some(number) = self.known(name)? {
            return Ok(number);
        }
        self.add.reset();
        self.add.bind_text(1, name)?;
        self.add.exec()?;
        self.find.reset();
        self.find.bind_text(1, name)?;
        self.find
            .maybe(|row| row.column_int64(0))?
            .context("a name just written to a dictionary is not in it")
    }

    fn forget(&mut self, number: i64) -> Result<()> {
        self.remove.reset();
        self.remove.bind_int64(1, number)?;
        self.remove.exec()?;
        self.numbered.retain(|_, held| *held != number);
        Ok(())
    }
}

/// Reads the project and fills the store with its symbols.
///
/// `cores` is how many threads the pass may use, and the whole pass is one
/// transaction: an index built halfway is worse than one not built at all.
pub fn build(root: &Path, cores: usize, into: &Symbols) -> Result<Built> {
    let started = Instant::now();
    let (readable, refused) = languages::readable();
    anyhow::ensure!(
        !readable.is_empty(),
        "no built-in language has an outline query to run"
    );
    for trouble in &refused {
        log::warn!("outline query left out of the build -- {trouble}");
    }

    let claimed = languages::suffixes_of(&readable);
    let readings: Vec<(std::path::PathBuf, usize)> = walk::files_under(root)
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let language = languages::claimant(name, &claimed)?;
            Some((path, language))
        })
        .collect();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cores)
        .build()
        .context("building the pool the pass runs on")?;

    let mut built = Built {
        cores,
        ..Built::default()
    };
    into.in_one_transaction(|store| {
        let mut writing = Writing::on(&store.connection)?;
        for round in readings.chunks(A_ROUND) {
            let reading_started = Instant::now();
            let read: Vec<Read> = pool.install(|| {
                round
                    .par_iter()
                    .filter_map(|(path, language)| read_one(root, path, readable.get(*language)?))
                    .collect()
            });
            built.reading += reading_started.elapsed();

            let writing_started = Instant::now();
            for one in &read {
                writing.replace(&one.path, &one.found)?;
                built.files += 1;
                built.symbols += one.found.len();
                built.bytes += one.bytes;
            }
            built.writing += writing_started.elapsed();
        }
        Ok(())
    })?;

    built.took = started.elapsed();
    built.the_most_memory = measure::the_most_memory_so_far();
    Ok(built)
}

/// One file, read.
struct Read {
    path: String,
    bytes: u64,
    found: Vec<Definition>,
}

/// Reads one file and finds its definitions. `None` for a file that cannot be
/// read or parsed at all: one file fewer, not a reason to abandon the pass.
fn read_one(root: &Path, path: &Path, language: &Readable) -> Option<Read> {
    let bytes = std::fs::metadata(path).ok()?.len();
    let mut parser = tree_sitter::Parser::new();
    let found = definitions::in_file_on_disk(root, path, language, &mut parser)?;
    let named = path.strip_prefix(root).ok()?;
    Some(Read {
        path: named.to_string_lossy().replace('\\', "/"),
        bytes,
        found,
    })
}

/// Every symbol of the project, held in memory to be searched.
///
/// The store answers what the project holds; this answers it quickly. Names are
/// kept as they were written, paths and kinds once each, and every name carries
/// the set of letters it contains so a query that cannot possibly match is
/// rejected without looking at the name at all.
pub struct Catalogue {
    paths: Vec<Box<str>>,
    kinds: Vec<Box<str>>,
    languages: Vec<Box<str>>,
    entries: Vec<Entry>,
}

struct Entry {
    name: Box<str>,
    letters: CharBag,
    path: u32,
    kind: u32,
    language: u32,
    line: u32,
}

impl Catalogue {
    /// Takes a catalogue over symbols already in hand.
    pub fn of(symbols: impl IntoIterator<Item = Definition>) -> Self {
        let mut paths = Interned::default();
        let mut kinds = Interned::default();
        let mut languages = Interned::default();
        let mut entries = Vec::new();
        for one in symbols {
            entries.push(Entry {
                letters: CharBag::from(one.name.as_str()),
                name: one.name.into_boxed_str(),
                path: paths.of(one.path),
                kind: kinds.of(one.kind),
                language: languages.of(one.language),
                line: one.line,
            });
        }
        Self {
            paths: paths.held,
            kinds: kinds.held,
            languages: languages.held,
            entries,
        }
    }

    /// Takes a catalogue over everything a store holds.
    pub fn read_from(store: &Symbols) -> Result<Self> {
        Ok(Self::of(store.everything()?))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The name of every symbol, in the order they are held. What a prepared
    /// list of searches is drawn from, so the searches are of names that really
    /// are in the project.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.name.as_ref())
    }

    /// The symbols whose name contains the query's letters in order, at most
    /// `most` of them.
    ///
    /// This is the half of a search that has to look at every symbol; ranking is
    /// left to whoever shows the results, so that they are ranked by the same
    /// matcher as everything else the editor offers. Where more match than are
    /// asked for, the shortest names win -- a search for `new` should not lose
    /// `new` itself to `renewSubscription` because the latter was recorded
    /// first.
    pub fn candidates(&self, query: &str, most: usize) -> Vec<Definition> {
        if most == 0 {
            return Vec::new();
        }
        let wanted: Vec<char> = query.chars().map(folded).collect();
        if wanted.is_empty() {
            // Nothing to match on, so nothing to prefer: the first of them, in
            // the order the store holds them.
            return self
                .entries
                .iter()
                .take(most)
                .map(|entry| self.definition(entry))
                .collect();
        }
        let letters = CharBag::from(&wanted[..]);

        let mut matched: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|entry| entry.letters.is_superset(letters))
            .filter(|entry| holds_in_order(&entry.name, &wanted))
            .collect();
        util::truncate_to_bottom_n_sorted_by(&mut matched, most, &|one, other| {
            one.name
                .len()
                .cmp(&other.name.len())
                .then_with(|| one.name.cmp(&other.name))
                .then_with(|| one.path.cmp(&other.path))
                .then_with(|| one.line.cmp(&other.line))
        });
        matched
            .into_iter()
            .map(|entry| self.definition(entry))
            .collect()
    }

    fn definition(&self, entry: &Entry) -> Definition {
        let held = |from: &[Box<str>], at: u32| {
            from.get(at as usize)
                .map(|held| held.to_string())
                .unwrap_or_default()
        };
        Definition {
            path: held(&self.paths, entry.path),
            name: entry.name.to_string(),
            kind: held(&self.kinds, entry.kind),
            line: entry.line,
            language: held(&self.languages, entry.language),
        }
    }
}

/// Strings held once and referred to by number.
#[derive(Default)]
struct Interned {
    held: Vec<Box<str>>,
    numbered: HashMap<String, u32>,
}

impl Interned {
    fn of(&mut self, value: String) -> u32 {
        if let Some(at) = self.numbered.get(&value) {
            return *at;
        }
        let at = self.held.len() as u32;
        self.held.push(value.clone().into_boxed_str());
        self.numbered.insert(value, at);
        at
    }
}

/// Whether `name` contains every one of `wanted` in order, ignoring case.
fn holds_in_order(name: &str, wanted: &[char]) -> bool {
    let mut still_wanted = wanted.iter();
    let mut next = still_wanted.next();
    for letter in name.chars().map(folded) {
        match next {
            Some(looking_for) if *looking_for == letter => next = still_wanted.next(),
            Some(_) => {}
            None => return true,
        }
    }
    next.is_none()
}

/// One character, folded for comparison. The same fold the editor's own matcher
/// uses, so a query that finds a symbol here finds it there too.
fn folded(letter: char) -> char {
    letter.to_lowercase().next().unwrap_or(letter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(path: &str, name: &str, kind: &str, line: u32) -> Definition {
        Definition {
            path: path.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line,
            language: "rust".to_string(),
        }
    }

    fn a_project() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("a directory to put a project in");
        let at = root.path();
        std::fs::create_dir_all(at.join("src")).expect("a directory in it");
        std::fs::write(
            at.join("src/one.rs"),
            "pub struct Thing {\n    field: u32,\n}\n\npub fn work() {}\n",
        )
        .expect("the first file");
        std::fs::write(at.join("src/two.rs"), "pub fn other() {}\n").expect("the second file");
        // Read, and defines nothing.
        std::fs::write(at.join("src/three.rs"), "// only a comment\n").expect("the third file");
        // Of no language with an outline query, so it is read by nothing.
        std::fs::write(at.join("notes.txt"), "nothing to parse here\n").expect("the text file");
        std::fs::write(at.join(".gitignore"), "built/\n").expect("the ignore file");
        std::fs::create_dir_all(at.join("built")).expect("an ignored directory");
        std::fs::write(at.join("built/output.rs"), "pub fn never() {}\n").expect("an ignored file");
        root
    }

    #[test]
    fn a_file_s_symbols_come_back_as_they_were_recorded() {
        let store = Symbols::open_in_memory().expect("a store");
        let found = vec![
            symbol("src/one.rs", "Thing", "struct_item", 1),
            symbol("src/one.rs", "work", "function_item", 5),
        ];
        store.record("src/one.rs", &found).expect("recording");

        assert_eq!(store.count().expect("the count"), 2);
        assert_eq!(store.files().expect("the files"), 1);
        assert_eq!(store.in_file("src/one.rs").expect("the file"), found);
        assert_eq!(store.everything().expect("everything").len(), 2);
    }

    #[test]
    fn recording_a_file_again_replaces_what_it_had() {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record(
                "src/one.rs",
                &[
                    symbol("src/one.rs", "Thing", "struct_item", 1),
                    symbol("src/one.rs", "work", "function_item", 5),
                ],
            )
            .expect("the first recording");
        store
            .record(
                "src/one.rs",
                &[symbol("src/one.rs", "Renamed", "struct_item", 1)],
            )
            .expect("the second recording");

        let held = store.in_file("src/one.rs").expect("the file");
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(held[0].name, "Renamed");
        assert_eq!(
            store.count().expect("the count"),
            1,
            "the symbols it no longer defines are gone, not kept beside the new ones"
        );
        assert_eq!(store.files().expect("the files"), 1, "still the one file");
    }

    /// Nothing about a symbol is unique on its own, so the store keys rows by
    /// where in the file they were found. Without that, two definitions sharing
    /// a file, a line, a name and a kind would collide and fail the build
    /// instead of both being recorded.
    #[test]
    fn two_definitions_that_agree_on_everything_are_both_recorded() {
        let store = Symbols::open_in_memory().expect("a store");
        let twice = vec![
            symbol("src/one.rs", "work", "function_item", 1),
            symbol("src/one.rs", "work", "function_item", 1),
        ];
        store.record("src/one.rs", &twice).expect("recording");
        assert_eq!(store.count().expect("the count"), 2);
        assert_eq!(store.in_file("src/one.rs").expect("the file"), twice);
    }

    /// A file read and found to define nothing is not the same as a file never
    /// read: without the difference, a later pass would read it again every time.
    #[test]
    fn a_file_that_defines_nothing_is_still_known_to_have_been_read() {
        let store = Symbols::open_in_memory().expect("a store");
        store.record("src/empty.rs", &[]).expect("recording");
        assert_eq!(store.files().expect("the files"), 1);
        assert_eq!(store.count().expect("the count"), 0);
        assert!(store.in_file("src/empty.rs").expect("the file").is_empty());
    }

    #[test]
    fn forgetting_a_file_takes_its_symbols_with_it() {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record(
                "src/one.rs",
                &[symbol("src/one.rs", "Thing", "struct_item", 1)],
            )
            .expect("recording one");
        store
            .record(
                "src/two.rs",
                &[symbol("src/two.rs", "Other", "struct_item", 1)],
            )
            .expect("recording another");

        assert!(store.forget("src/one.rs").expect("forgetting"));
        assert_eq!(store.files().expect("the files"), 1);
        assert_eq!(store.count().expect("the count"), 1);
        assert_eq!(
            store.everything().expect("everything")[0].name,
            "Other",
            "the other file is untouched"
        );
        assert!(
            !store.forget("src/never.rs").expect("forgetting nothing"),
            "a file the store never had is not a file it forgot"
        );
    }

    /// The reason the store is worth normalising at all: the plan allows about
    /// twenty-four bytes a symbol, and a forty character path recorded per
    /// symbol would spend all of it on the path alone.
    #[test]
    fn a_path_a_kind_and_a_language_are_each_held_once() {
        let store = Symbols::open_in_memory().expect("a store");
        let many: Vec<Definition> = (0..200)
            .map(|at| {
                symbol(
                    "src/one/rather/deeply/nested/module.rs",
                    "work",
                    "function_item",
                    at,
                )
            })
            .collect();
        store
            .record("src/one/rather/deeply/nested/module.rs", &many)
            .expect("recording");

        let counted = |table: &str| {
            let mut statement =
                Statement::prepare(&store.connection, format!("SELECT COUNT(*) FROM {table}"))
                    .expect("a count");
            statement
                .maybe(|row| row.column_int64(0))
                .expect("counting")
                .unwrap_or_default()
        };
        assert_eq!(store.count().expect("the count"), 200);
        assert_eq!(counted("files"), 1);
        assert_eq!(counted("kinds"), 1);
        assert_eq!(counted("languages"), 1);
    }

    /// The store's own claim about itself: a write that fails halfway leaves
    /// nothing behind, because the next pass would believe whatever it found.
    #[test]
    fn a_write_that_fails_halfway_leaves_the_store_as_it_was() {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record(
                "src/one.rs",
                &[symbol("src/one.rs", "Thing", "struct_item", 1)],
            )
            .expect("recording one");

        let broken: Result<()> = store.in_one_transaction(|inner| {
            let mut writing = Writing::on(&inner.connection)?;
            writing.replace(
                "src/two.rs",
                &[symbol("src/two.rs", "Other", "struct_item", 1)],
            )?;
            anyhow::bail!("the pass gave up halfway")
        });
        assert!(broken.is_err());

        assert_eq!(store.count().expect("the count"), 1);
        assert_eq!(store.files().expect("the files"), 1);
        assert!(store.in_file("src/two.rs").expect("the file").is_empty());
        // And the store still writes afterwards, so the rollback released it.
        store
            .record(
                "src/three.rs",
                &[symbol("src/three.rs", "Third", "struct_item", 1)],
            )
            .expect("recording after the failure");
        assert_eq!(store.count().expect("the count"), 2);
    }

    /// A whole pass in one transaction, which is what keeps a branch switch
    /// inside the time it is allowed: what is read is replaced and what is gone
    /// is dropped, together or not at all.
    #[test]
    fn a_list_of_files_is_recorded_and_dropped_in_one_go() {
        let store = Symbols::open_in_memory().expect("a store");
        store
            .record(
                "src/leaving.rs",
                &[symbol("src/leaving.rs", "Old", "struct_item", 1)],
            )
            .expect("something to lose");
        store
            .record(
                "src/staying.rs",
                &[symbol("src/staying.rs", "Kept", "struct_item", 1)],
            )
            .expect("something to keep");

        let forgotten = store
            .record_all(
                &[
                    (
                        "src/one.rs".to_string(),
                        vec![symbol("src/one.rs", "First", "struct_item", 1)],
                    ),
                    ("src/empty.rs".to_string(), Vec::new()),
                ],
                &[
                    "src/leaving.rs".to_string(),
                    "src/never_here.rs".to_string(),
                ],
            )
            .expect("the pass");
        assert_eq!(forgotten, 1, "one of the two named files was actually held");
        assert_eq!(
            store
                .record_all(&[], &["src/never_here.rs".to_string()])
                .expect("a pass that drops nothing"),
            0,
            "a file the store never held is not a file it forgot"
        );

        assert_eq!(store.count().expect("the count"), 2, "First and Kept");
        assert_eq!(
            store.files().expect("the files"),
            3,
            "one, empty and staying; the one that left is gone"
        );
        assert!(store.in_file("src/leaving.rs").expect("gone").is_empty());
        assert!(
            store.in_file("src/empty.rs").expect("read").is_empty(),
            "read and found to define nothing"
        );
        assert_eq!(
            store.in_file("src/staying.rs").expect("kept").len(),
            1,
            "a file the pass never mentioned is untouched"
        );
    }

    #[test]
    fn a_store_kept_on_disk_comes_back_as_it_was_left() {
        let held = tempfile::tempdir().expect("a directory for the store");
        let kept_at = held.path().join("symbols.db");
        let found = vec![
            symbol("src/one.rs", "Thing", "struct_item", 1),
            symbol("src/one.rs", "work", "function_item", 5),
        ];

        {
            let store = Symbols::open(&kept_at).expect("a store on disk");
            store.record("src/one.rs", &found).expect("recording");
            store.compact().expect("compacting");
        }

        let reopened = Symbols::open(&kept_at).expect("the same store again");
        assert_eq!(reopened.in_file("src/one.rs").expect("the file"), found);
        assert!(Symbols::on_disk(&kept_at).is_some_and(|size| size > 0));
    }

    #[test]
    fn the_build_pass_fills_the_store_from_the_project_and_nothing_it_ignores() {
        let project = a_project();
        let store = Symbols::open_in_memory().expect("a store");
        let built = build(project.path(), 2, &store).expect("the build");

        assert_eq!(
            built.files, 3,
            "three Rust files; the text one and the ignored one are not read"
        );
        assert!(built.symbols >= 3, "found {}", built.symbols);
        assert_eq!(built.symbols, store.count().expect("the count"));
        assert_eq!(built.cores, 2);
        assert!(built.took > Duration::ZERO);
        assert!(built.bytes > 0);

        let names: Vec<String> = store
            .everything()
            .expect("everything")
            .into_iter()
            .map(|one| one.name)
            .collect();
        assert!(names.contains(&"Thing".to_string()), "{names:?}");
        assert!(names.contains(&"work".to_string()), "{names:?}");
        assert!(names.contains(&"other".to_string()), "{names:?}");
        assert!(
            !names.contains(&"never".to_string()),
            "an ignored file is not part of the project: {names:?}"
        );

        // The file that defines nothing was still read, so a later pass knows
        // not to read it again.
        assert_eq!(store.files().expect("the files"), 3);
        assert!(store.in_file("src/three.rs").expect("the file").is_empty());
    }

    #[test]
    fn a_second_build_over_the_same_project_holds_the_same_symbols_and_no_more() {
        let project = a_project();
        let store = Symbols::open_in_memory().expect("a store");
        let first = build(project.path(), 2, &store).expect("the first build");
        let again = build(project.path(), 2, &store).expect("the second build");

        assert_eq!(first.symbols, again.symbols);
        assert_eq!(store.count().expect("the count"), first.symbols);
        assert_eq!(store.files().expect("the files"), first.files);
    }

    fn a_catalogue_of(names: &[&str]) -> Catalogue {
        Catalogue::of(
            names
                .iter()
                .enumerate()
                .map(|(at, name)| symbol("src/one.rs", name, "function_item", at as u32 + 1)),
        )
    }

    #[test]
    fn a_search_finds_a_symbol_by_the_letters_of_its_name_in_order() {
        let catalogue = a_catalogue_of(&["take_stock", "the_whole_pass", "read_one"]);
        let found = catalogue.candidates("tst", 10);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "take_stock");

        // The letters have to be in order, not merely present. Both names hold
        // an `s` and an `a`, but in neither does an `a` follow an `s`.
        assert!(catalogue.candidates("sa", 10).is_empty());
    }

    #[test]
    fn a_query_whose_letters_are_not_all_there_finds_nothing() {
        let catalogue = a_catalogue_of(&["take_stock", "read_one"]);
        assert!(catalogue.candidates("zzz", 10).is_empty());
        assert!(catalogue.candidates("take_stockade", 10).is_empty());
    }

    #[test]
    fn case_is_ignored_on_both_sides_of_a_search() {
        let catalogue = a_catalogue_of(&["TakeStock", "readone"]);
        assert_eq!(catalogue.candidates("takestock", 10).len(), 1);
        assert_eq!(catalogue.candidates("TS", 10).len(), 1);
        assert_eq!(catalogue.candidates("READONE", 10).len(), 1);
    }

    /// A search for `new` must not lose `new` itself to a longer name that
    /// happened to be recorded first.
    #[test]
    fn the_cap_keeps_the_shortest_names_and_not_the_first_recorded() {
        let catalogue = a_catalogue_of(&[
            "renew_the_subscription",
            "newly_added_thing",
            "new",
            "newer",
        ]);
        let found = catalogue.candidates("new", 2);
        let names: Vec<&str> = found.iter().map(|one| one.name.as_str()).collect();
        assert_eq!(names, vec!["new", "newer"], "{found:?}");
        // And with room for all of them, all of them come back.
        assert_eq!(catalogue.candidates("new", 10).len(), 4);
    }

    #[test]
    fn a_search_with_no_room_for_an_answer_gives_none() {
        let catalogue = a_catalogue_of(&["work"]);
        assert!(catalogue.candidates("work", 0).is_empty());
    }

    #[test]
    fn an_empty_query_gives_back_what_there_is_up_to_the_cap() {
        let catalogue = a_catalogue_of(&["one", "two", "three"]);
        assert_eq!(catalogue.candidates("", 2).len(), 2);
        assert_eq!(catalogue.candidates("", 10).len(), 3);
        assert_eq!(catalogue.len(), 3);
        assert!(!catalogue.is_empty());
        assert!(Catalogue::of(Vec::new()).is_empty());
    }

    /// Paths are held once and still reported with every symbol that has one,
    /// which is what makes holding them once safe.
    #[test]
    fn every_result_carries_its_own_path_kind_and_language() {
        let catalogue = Catalogue::of(vec![
            symbol("src/one.rs", "work", "function_item", 5),
            symbol("src/two.rs", "work", "const_item", 9),
        ]);
        let mut found = catalogue.candidates("work", 10);
        found.sort_by(|one, other| one.path.cmp(&other.path));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].path, "src/one.rs");
        assert_eq!(found[0].kind, "function_item");
        assert_eq!(found[0].line, 5);
        assert_eq!(found[1].path, "src/two.rs");
        assert_eq!(found[1].kind, "const_item");
        assert_eq!(found[1].line, 9);
        assert!(found.iter().all(|one| one.language == "rust"));
    }

    #[test]
    fn a_name_that_is_not_ascii_is_searched_by_the_same_rules() {
        let catalogue = a_catalogue_of(&["Wörterbuch", "worker"]);
        assert_eq!(catalogue.candidates("wör", 10).len(), 1);
        assert_eq!(catalogue.candidates("WÖRTERBUCH", 10).len(), 1);
        assert_eq!(catalogue.candidates("wor", 10).len(), 1);
    }

    #[test]
    fn a_catalogue_read_from_a_store_holds_what_the_store_held() {
        let project = a_project();
        let store = Symbols::open_in_memory().expect("a store");
        let built = build(project.path(), 2, &store).expect("the build");
        let catalogue = Catalogue::read_from(&store).expect("a catalogue");
        assert_eq!(catalogue.len(), built.symbols);
        assert_eq!(catalogue.candidates("Thing", 10).len(), 1);
    }

    #[test]
    fn the_order_check_is_total_over_the_awkward_queries() {
        assert!(holds_in_order("anything", &[]));
        assert!(!holds_in_order("", &['a']));
        assert!(holds_in_order("", &[]));
        assert!(holds_in_order("aaa", &['a', 'a', 'a']));
        assert!(!holds_in_order("aa", &['a', 'a', 'a']));
    }
}
