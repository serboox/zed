use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use rayon::prelude::*;
use streaming_iterator::StreamingIterator as _;

use crate::languages::{self, Readable};
use crate::walk;

/// The five numbers the whole plan is measured against. Nothing here is a target
/// -- every later step states its own target *relative* to these, so they are
/// taken first and written down.
#[derive(Debug, Clone, Default)]
pub struct Numbers {
    /// How long one file takes to parse.
    pub parsing_a_file: Spread,
    /// The same, per kilobyte of source, which is what makes files of different
    /// sizes comparable and shows up the pathological ones.
    pub parsing_a_kilobyte: Spread,
    /// Parsing every file and running its outline query over it, on every core
    /// but one. This is the number the plan calls "the time to build the index".
    pub the_whole_pass: Duration,
    /// The highest the process ever reached during the pass. `None` where the
    /// system does not report it, rather than a number that is not the answer.
    pub the_most_memory: Option<u64>,
    /// How many symbols the outline queries yielded, and how much room they take
    /// once written down. The size is `None` until there is a table to write
    /// them to, which is the next step of the plan, not this one.
    pub symbols: usize,
    pub symbols_on_disk: Option<u64>,
    /// How long a symbol search takes. `None` for the same reason: there is
    /// nothing to search yet.
    pub answering_a_search: Option<Spread>,
    /// What was actually read, so a number can never be quoted without the size
    /// of the thing it was measured on.
    pub files: usize,
    pub bytes: u64,
    pub cores: usize,
}

/// A measurement's middle and its tail. The tail is the half of the pair that
/// matters: a median hides the file that takes a hundred times as long.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spread {
    pub median: Duration,
    pub ninety_fifth: Duration,
    /// The slowest sample. Carried because a run under a time limit has to be
    /// able to say how close it came to it: a percentile hides exactly the one
    /// measurement that decides whether the limit was the thing being measured.
    pub slowest: Duration,
}

/// The middle and the ninety-fifth percentile of a set of measurements.
///
/// The percentile is taken by rank on the sorted samples -- the nearest-rank
/// method -- because that is the one that cannot invent a value that was never
/// measured, which matters when the tail is what is being looked at.
pub fn spread_of(mut samples: Vec<Duration>) -> Spread {
    if samples.is_empty() {
        return Spread::default();
    }
    samples.sort_unstable();
    Spread {
        median: at_rank(&samples, 0.5),
        ninety_fifth: at_rank(&samples, 0.95),
        slowest: at_rank(&samples, 1.0),
    }
}

fn at_rank(sorted: &[Duration], fraction: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    // Ceiling of fraction * n, counted from one, which is the rank the sample at
    // or above that fraction sits at.
    let rank = (fraction * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// Whether every one of these measurements is within `fraction` of the largest,
/// which is how the plan's first gate is stated: three runs, no more than a
/// tenth apart.
pub fn all_within(measurements: &[Duration], fraction: f64) -> bool {
    let Some(largest) = measurements.iter().max().copied() else {
        return true;
    };
    let Some(smallest) = measurements.iter().min().copied() else {
        return true;
    };
    if largest.is_zero() {
        return true;
    }
    let apart = largest.as_secs_f64() - smallest.as_secs_f64();
    apart / largest.as_secs_f64() <= fraction
}

/// A file the stand will read, with the language it belongs to.
struct Reading {
    path: PathBuf,
    language: usize,
}

/// Everything under `root` that one of `languages` claims.
fn readings_under(root: &Path, languages: &[Readable]) -> Vec<Reading> {
    let claimed = languages::suffixes_of(languages);
    walk::files_under(root)
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let language = languages::claimant(name, &claimed)?;
            Some(Reading { path, language })
        })
        .collect()
}

/// What one file cost.
struct Cost {
    parsing: Duration,
    bytes: u64,
    symbols: usize,
}

/// Reads the project once and reports the numbers.
///
/// `cores` is how many threads the pass is allowed; the plan says every core but
/// one, which is what the editor's own scanner takes, so the number measured
/// here is the number the editor would see.
pub fn measure(root: &Path, cores: usize) -> Result<Numbers> {
    let (languages, refused) = languages::readable();
    anyhow::ensure!(
        !languages.is_empty(),
        "no built-in language has an outline query to run"
    );
    for trouble in &refused {
        log::warn!("outline query left out of the measurement -- {trouble}");
    }

    let readings = readings_under(root, &languages);
    anyhow::ensure!(
        !readings.is_empty(),
        "nothing under {} belongs to a language with an outline query",
        root.display()
    );

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cores)
        .build()
        .context("building the pool the pass runs on")?;

    let started = Instant::now();
    let costs: Vec<Cost> = pool.install(|| {
        readings
            .par_iter()
            .filter_map(|reading| read_one(reading, &languages))
            .collect()
    });
    let the_whole_pass = started.elapsed();

    let bytes: u64 = costs.iter().map(|cost| cost.bytes).sum();
    let symbols: usize = costs.iter().map(|cost| cost.symbols).sum();
    let parsing_a_file = spread_of(costs.iter().map(|cost| cost.parsing).collect());
    let parsing_a_kilobyte = spread_of(
        costs
            .iter()
            .filter(|cost| cost.bytes > 0)
            .map(|cost| {
                let kilobytes = cost.bytes as f64 / 1024.;
                Duration::from_secs_f64(cost.parsing.as_secs_f64() / kilobytes)
            })
            .collect(),
    );

    Ok(Numbers {
        parsing_a_file,
        parsing_a_kilobyte,
        the_whole_pass,
        the_most_memory: the_most_memory_so_far(),
        symbols,
        symbols_on_disk: None,
        answering_a_search: None,
        files: costs.len(),
        bytes,
        cores,
    })
}

/// Parses one file and runs its outline query over it. `None` for a file that
/// cannot be read or parsed at all -- it is one file fewer in the measurement,
/// not a reason to abandon the run.
fn read_one(reading: &Reading, languages: &[Readable]) -> Option<Cost> {
    let language = languages.get(reading.language)?;
    let source = std::fs::read(&reading.path).ok()?;
    let bytes = source.len() as u64;

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language.grammar).ok()?;
    let started = Instant::now();
    let tree = parser.parse(&source, None)?;
    let parsing = started.elapsed();

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut symbols = 0;
    let mut matches = cursor.matches(&language.outline, tree.root_node(), source.as_slice());
    while matches.next().is_some() {
        symbols += 1;
    }

    Some(Cost {
        parsing,
        bytes,
        symbols,
    })
}

/// The highest the process has reached, in bytes. Read from the kernel's own
/// high-water mark rather than sampled, because a sample taken after the pass
/// has already missed the peak.
pub fn the_most_memory_so_far() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("VmHWM:") {
                let kilobytes: u64 = value
                    .split_whitespace()
                    .next()
                    .and_then(|number| number.parse().ok())?;
                return Some(kilobytes * 1024);
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// A size, for printing. Public so the stand prints every number the same way.
pub fn as_memory(bytes: u64) -> String {
    const MEGABYTE: f64 = 1024. * 1024.;
    format!("{:.1} MB", bytes as f64 / MEGABYTE)
}

/// A length of time, for printing, in whichever unit reads best.
pub fn as_time(how_long: Duration) -> String {
    let seconds = how_long.as_secs_f64();
    if seconds >= 1. {
        return format!("{seconds:.2} s");
    }
    let milliseconds = seconds * 1_000.;
    if milliseconds >= 1. {
        return format!("{milliseconds:.2} ms");
    }
    format!("{:.1} us", milliseconds * 1_000.)
}

impl fmt::Display for Numbers {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            out,
            "read {} files, {} of source, on {} cores",
            self.files,
            as_memory(self.bytes),
            self.cores
        )?;
        writeln!(
            out,
            "  parsing a file      median {:>10}   95th {:>10}",
            as_time(self.parsing_a_file.median),
            as_time(self.parsing_a_file.ninety_fifth)
        )?;
        writeln!(
            out,
            "  parsing a kilobyte  median {:>10}   95th {:>10}",
            as_time(self.parsing_a_kilobyte.median),
            as_time(self.parsing_a_kilobyte.ninety_fifth)
        )?;
        writeln!(
            out,
            "  the whole pass      {:>17}",
            as_time(self.the_whole_pass)
        )?;
        writeln!(
            out,
            "  the most memory     {:>17}",
            self.the_most_memory
                .map(as_memory)
                .unwrap_or_else(|| "-- not reported here".to_string())
        )?;
        writeln!(
            out,
            "  symbols             {:>17}   on disk {}",
            self.symbols,
            self.symbols_on_disk
                .map(as_memory)
                .unwrap_or_else(|| "-- see the index below".to_string())
        )?;
        write!(
            out,
            "  answering a search  {:>17}",
            self.answering_a_search
                .map(|spread| as_time(spread.median))
                .unwrap_or_else(|| "-- see the index below".to_string())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn milliseconds(count: u64) -> Duration {
        Duration::from_millis(count)
    }

    #[test]
    fn the_spread_is_taken_by_rank_and_never_invents_a_sample() {
        let samples: Vec<Duration> = (1..=100).map(milliseconds).collect();
        let spread = spread_of(samples.clone());
        assert_eq!(spread.median, milliseconds(50));
        assert_eq!(spread.ninety_fifth, milliseconds(95));

        // Every reported value is one that was actually measured, which is the
        // whole point of taking the percentile by rank: an interpolated tail
        // would report a time no file took.
        assert!(samples.contains(&spread.median));
        assert!(samples.contains(&spread.ninety_fifth));
    }

    #[test]
    fn the_spread_of_almost_nothing_still_answers() {
        assert_eq!(spread_of(Vec::new()), Spread::default());
        let one = spread_of(vec![milliseconds(7)]);
        assert_eq!(one.median, milliseconds(7));
        assert_eq!(one.ninety_fifth, milliseconds(7));
    }

    /// The tail is the half that matters: a median alone hides the one file that
    /// takes a hundred times as long as the rest.
    #[test]
    fn one_slow_file_shows_up_in_the_tail_and_not_in_the_middle() {
        let mut samples: Vec<Duration> = (1..=99).map(milliseconds).collect();
        samples.push(milliseconds(10_000));
        let spread = spread_of(samples);
        assert_eq!(spread.median, milliseconds(50));
        assert_eq!(spread.ninety_fifth, milliseconds(95));

        let mut many_slow: Vec<Duration> = (1..=90).map(milliseconds).collect();
        many_slow.extend((0..10).map(|_| milliseconds(10_000)));
        assert_eq!(spread_of(many_slow).ninety_fifth, milliseconds(10_000));
    }

    #[test]
    fn the_first_gate_is_a_measurement_and_not_a_judgement() {
        assert!(all_within(
            &[milliseconds(100), milliseconds(105), milliseconds(109)],
            0.1
        ));
        assert!(!all_within(
            &[milliseconds(100), milliseconds(105), milliseconds(120)],
            0.1
        ));
        // Nothing measured, and one measurement, are both within any spread.
        assert!(all_within(&[], 0.1));
        assert!(all_within(&[milliseconds(7)], 0.0));
    }

    /// The stand end to end, on a project small enough to know the answer for.
    #[gpui::test]
    async fn the_stand_reads_a_project_and_counts_what_is_in_it(_cx: &mut gpui::TestAppContext) {
        let root = tempfile::tempdir().expect("a directory to put a project in");
        let at = root.path();
        std::fs::write(
            at.join("one.rs"),
            "pub fn first() {}\npub fn second() {}\npub struct Third;\n",
        )
        .expect("the first file");
        std::fs::write(at.join("two.rs"), "pub fn fourth() {}\n").expect("the second file");
        // Ignored, and so outside every number: a measurement that counts build
        // output is not a measurement of the project.
        std::fs::write(at.join(".gitignore"), "ignored/\n").expect("the ignore file");
        std::fs::create_dir_all(at.join("ignored")).expect("the ignored directory");
        std::fs::write(at.join("ignored/three.rs"), "pub fn never() {}\n")
            .expect("the ignored file");
        // Of no language with an outline query, so it is read by nothing.
        std::fs::write(at.join("notes.txt"), "nothing to parse here\n").expect("the text file");

        let numbers = measure(at, 2).expect("the stand reads the project");

        assert_eq!(
            numbers.files, 2,
            "two Rust files are in the project; the ignored one and the text one are not"
        );
        assert!(
            numbers.symbols >= 4,
            "four definitions were written; the outline query found {}",
            numbers.symbols
        );
        assert!(numbers.the_whole_pass > Duration::ZERO);
        assert!(numbers.parsing_a_file.median > Duration::ZERO);
        assert_eq!(numbers.cores, 2);
        assert_eq!(numbers.bytes, {
            let one = std::fs::metadata(at.join("one.rs"))
                .expect("the first file")
                .len();
            let two = std::fs::metadata(at.join("two.rs"))
                .expect("the second file")
                .len();
            one + two
        });
        // The two numbers that need somewhere to write symbols down say so
        // rather than reporting a zero that would read as an answer.
        assert!(numbers.symbols_on_disk.is_none());
        assert!(numbers.answering_a_search.is_none());
    }
}
