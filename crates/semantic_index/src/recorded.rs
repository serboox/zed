use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::definitions::Definition;

/// What a language server answered, kept in a file so a later run can be
/// measured without starting the server again.
///
/// The measurement this feeds costs 16.2 GiB of rust-analyzer on this project
/// and is killed by an 18 GiB machine at 600 symbols. A number that expensive
/// cannot be taken again after every change to the index, and one taken rarely
/// is one nobody trusts. Recorded once, the same sample answers instantly and
/// without the server, so an index change can be measured against exactly what
/// the server said before it.
///
/// Two properties are what make it usable rather than merely present. It is
/// keyed by the symbol asked about, so a recording can be **extended** run by
/// run -- which is the only way a 600-symbol sample is ever taken on a machine
/// that dies at 600. And it carries where it came from, so a number read off it
/// always says which server, which corpus and which day it is a number about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recording {
    pub taken_from: Provenance,
    /// Keyed by [`Asked::key`] rather than held in a list, so merging two runs
    /// is a question of which keys are already present and never of which
    /// order the corpus happened to be walked in.
    answers: BTreeMap<String, Answer>,
}

/// Where a recording came from. Every field here is something that changes the
/// answers, so a recording that outlives one of them can be spotted rather than
/// quietly believed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub language: String,
    /// The server binary's own `--version` line, verbatim.
    pub server: String,
    /// The corpus root as it was given, and the commit it stood at.
    pub corpus: String,
    pub corpus_commit: Option<String>,
    /// How many runs have contributed answers to this file.
    pub runs: u32,
}

/// One symbol the server was asked about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Answer {
    pub path: String,
    pub row: u32,
    pub column: u32,
    pub name: String,
    pub seen: Seen,
    /// Empty when `seen` is anything but [`Seen::Yes`], and meaningful only
    /// then: a server that never resolved the name at that position did not
    /// answer "no references", it answered nothing at all.
    pub references: Vec<Reference>,
}

/// What the probe before the question said. Kept rather than collapsed into a
/// boolean because the three ways of not knowing cost the sample differently,
/// and a recording that forgot which one happened would turn a suspect run into
/// a clean-looking one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Seen {
    /// The server resolves the name at that position and answered.
    Yes,
    /// The server knows the file and resolves nothing there -- a declaration it
    /// cannot see, typically behind a `#[cfg]` this build switches off.
    No,
    /// The server refused the file: outside the project it was started on.
    Refused,
    /// The probe itself failed. Neither side can be held to the symbol, and a
    /// run full of these has to read as suspect.
    Failed,
}

/// One reference, as the server gave it. A mirror of [`Definition`] rather than
/// the type itself, so the file format stays this module's to change and does
/// not follow an internal struct that has other reasons to move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub language: String,
}

impl From<&Definition> for Reference {
    fn from(definition: &Definition) -> Self {
        Self {
            path: definition.path.clone(),
            name: definition.name.clone(),
            kind: definition.kind.clone(),
            line: definition.line,
            language: definition.language.clone(),
        }
    }
}

impl From<&Reference> for Definition {
    fn from(reference: &Reference) -> Self {
        Self {
            path: reference.path.clone(),
            name: reference.name.clone(),
            kind: reference.kind.clone(),
            line: reference.line,
            language: reference.language.clone(),
        }
    }
}

/// The symbol a question was about, and the key a recording is filed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub path: String,
    pub row: u32,
    pub column: u32,
    pub name: String,
}

impl Asked {
    /// Position as well as name, because two symbols in one file can share a
    /// name and the whole point of the sample is to keep them apart.
    pub fn key(&self) -> String {
        format!("{}:{}:{}:{}", self.path, self.row, self.column, self.name)
    }
}

impl Recording {
    pub fn new(taken_from: Provenance) -> Self {
        Self {
            taken_from,
            answers: BTreeMap::new(),
        }
    }

    /// Reads a recording, or starts an empty one under this provenance when the
    /// file is not there yet -- which is what makes `--record` the same command
    /// the first time and the fifth.
    pub fn read_or_start(at: &Path, taken_from: Provenance) -> Result<Self> {
        if !at.exists() {
            return Ok(Self::new(taken_from));
        }
        let text = std::fs::read_to_string(at)
            .with_context(|| format!("reading the recording at {}", at.display()))?;
        let mut recording: Self = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a recording this version reads", at.display()))?;
        recording.taken_from.runs = recording.taken_from.runs.saturating_add(1);

        // The provenance a recording carries is the first run's, because that
        // is what the answers already in it are about. What a later run must
        // not do is change it silently: a corpus that moved or a server that
        // was upgraded makes the answers in the file and the answers being
        // added answers about two different things.
        if recording.taken_from.server != taken_from.server {
            log::warn!(
                "this recording was taken from {} and is being extended by {} -- the two are \
                 not answers about the same thing",
                recording.taken_from.server,
                taken_from.server
            );
        }
        match (
            &recording.taken_from.corpus_commit,
            &taken_from.corpus_commit,
        ) {
            (Some(recorded), Some(now)) if recorded != now => log::warn!(
                "this recording was taken at {recorded} and is being extended at {now} -- the \
                 corpus moved under it"
            ),
            // Filled in rather than left unknown: a recording taken before the
            // commit could be read is still a recording of that commit.
            (None, Some(now)) => recording.taken_from.corpus_commit = Some(now.clone()),
            _ => {}
        }
        Ok(recording)
    }

    pub fn read(at: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(at)
            .with_context(|| format!("reading the recording at {}", at.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("{} is not a recording this version reads", at.display()))
    }

    /// Written indented on purpose: this file is read by people deciding
    /// whether a number is trustworthy, and it lands in diffs.
    pub fn write(&self, at: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)
            .context("turning the recording into something writable")?;
        if let Some(directory) = at.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("making {}", directory.display()))?;
        }
        std::fs::write(at, text).with_context(|| format!("writing {}", at.display()))
    }

    pub fn remember(&mut self, answer: Answer) {
        let key = Asked {
            path: answer.path.clone(),
            row: answer.row,
            column: answer.column,
            name: answer.name.clone(),
        }
        .key();
        self.answers.insert(key, answer);
    }

    pub fn answer(&self, asked: &Asked) -> Option<&Answer> {
        self.answers.get(&asked.key())
    }

    pub fn holds(&self, asked: &Asked) -> bool {
        self.answers.contains_key(&asked.key())
    }

    pub fn len(&self) -> usize {
        self.answers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.answers.is_empty()
    }

    /// Every symbol recorded, in the file's own order, which is the sample a
    /// run measuring against it uses. Deliberately not re-sampled from the
    /// corpus: a recording is the definition of its own sample, so the number
    /// it produces does not move with how a directory walk happened to order
    /// itself.
    pub fn asked(&self) -> Vec<Asked> {
        self.answers
            .values()
            .map(|answer| Asked {
                path: answer.path.clone(),
                row: answer.row,
                column: answer.column,
                name: answer.name.clone(),
            })
            .collect()
    }
}

/// The commit the corpus stands at, for the provenance. Best effort: a corpus
/// that is not a git checkout is a corpus without a commit, not a failure.
///
/// Asynchronous because spawning a process blocks the thread for as long as the
/// process takes, and this crate is read by the editor as well as by the stand.
pub async fn commit_of(root: &Path) -> Option<String> {
    // A corpus that arrived by rsync has no `.git` -- the sync deliberately
    // does not bring one -- so whatever copied it leaves the commit beside it.
    // Read first, because on such a copy asking git would answer about
    // whatever repository happens to contain the directory, which is worse
    // than answering nothing.
    if let Ok(left_behind) = std::fs::read_to_string(root.join(".synced-commit")) {
        let commit = left_behind.trim().to_string();
        if !commit.is_empty() {
            return Some(commit);
        }
    }
    let output = smol::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The server's own version line, for the provenance.
pub async fn version_of(binary: &str) -> String {
    let Ok(output) = smol::process::Command::new(binary)
        .arg("--version")
        .output()
        .await
    else {
        return format!("{binary} (version unknown)");
    };
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if line.is_empty() {
        format!("{binary} (version unknown)")
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_provenance() -> Provenance {
        Provenance {
            language: "rust".to_string(),
            server: "rust-analyzer 1.95.0".to_string(),
            corpus: "/workspace/zed".to_string(),
            corpus_commit: Some("abc123".to_string()),
            runs: 1,
        }
    }

    fn an_answer(path: &str, row: u32, name: &str) -> Answer {
        Answer {
            path: path.to_string(),
            row,
            column: 4,
            name: name.to_string(),
            seen: Seen::Yes,
            references: vec![Reference {
                path: "src/other.rs".to_string(),
                name: name.to_string(),
                kind: "identifier".to_string(),
                line: 12,
                language: "rust".to_string(),
            }],
        }
    }

    #[test]
    fn a_recording_survives_the_round_trip() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let at = directory.path().join("rust.json");
        let mut recording = Recording::new(a_provenance());
        recording.remember(an_answer("src/lib.rs", 10, "thing"));
        recording.write(&at).expect("writing");

        let read = Recording::read(&at).expect("reading");
        assert_eq!(read.len(), 1);
        let asked = Asked {
            path: "src/lib.rs".to_string(),
            row: 10,
            column: 4,
            name: "thing".to_string(),
        };
        let answer = read.answer(&asked).expect("the answer is filed under it");
        assert_eq!(answer.references.len(), 1);
        assert_eq!(answer.seen, Seen::Yes);
    }

    // What makes a 600-symbol sample possible on a machine that dies at 600:
    // a second run adds to the first rather than replacing it.
    #[test]
    fn a_second_run_extends_the_first_rather_than_replacing_it() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let at = directory.path().join("rust.json");
        let mut first = Recording::new(a_provenance());
        first.remember(an_answer("src/lib.rs", 10, "thing"));
        first.write(&at).expect("writing");

        let mut second = Recording::read_or_start(&at, a_provenance()).expect("reading");
        assert_eq!(second.taken_from.runs, 2, "the run count carries");
        second.remember(an_answer("src/other.rs", 20, "another"));
        second.write(&at).expect("writing again");

        let read = Recording::read(&at).expect("reading");
        assert_eq!(read.len(), 2, "both runs' answers are in the file");
        assert!(read.holds(&Asked {
            path: "src/lib.rs".to_string(),
            row: 10,
            column: 4,
            name: "thing".to_string(),
        }));
    }

    // Two symbols of the same name in one file are the case the sample exists
    // to keep apart, so the key cannot be the name alone.
    #[test]
    fn two_symbols_of_one_name_are_filed_apart() {
        let mut recording = Recording::new(a_provenance());
        recording.remember(an_answer("src/lib.rs", 10, "thing"));
        recording.remember(an_answer("src/lib.rs", 40, "thing"));
        assert_eq!(recording.len(), 2);
    }

    #[test]
    fn a_missing_file_starts_an_empty_recording_rather_than_failing() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let at = directory.path().join("not-yet.json");
        let recording = Recording::read_or_start(&at, a_provenance()).expect("starting");
        assert!(recording.is_empty());
        assert_eq!(recording.taken_from.runs, 1);
    }
}
