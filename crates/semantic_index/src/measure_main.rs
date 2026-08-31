use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use semantic_index::inventory::Inventory;
use semantic_index::measure::{
    Numbers, Spread, all_within, as_memory, as_time, measure, spread_of,
};
use semantic_index::symbols::{Built, Catalogue, Symbols, build};

/// How far apart three runs of the same measurement may be and still be called
/// repeatable. The plan's first gate.
const ALLOWED_SPREAD: f64 = 0.1;

/// How many parse passes the whole index build is allowed to cost. From the
/// plan: the index's own overhead must not exceed the parsing it is built on by
/// more than itself again.
const PASSES_THE_BUILD_MAY_COST: u32 = 3;

/// The most the process may ever reach, in bytes. Parse trees are the large
/// thing, and they have to be freed as the pass goes rather than held.
const THE_MEMORY_CEILING: u64 = 512 * 1024 * 1024;

/// The share of the sources the symbols may take on disk. The reference point
/// is a classic code search index, which takes eighteen per cent while holding
/// trigrams; symbols alone should cost far less.
const THE_SHARE_OF_THE_SOURCES: f64 = 0.10;

/// How long a search may take. The ceiling for "instant" is a tenth of a
/// second; these are taken with room to spare.
const THE_SEARCH_MEDIAN: Duration = Duration::from_millis(10);
const THE_SEARCH_TAIL: Duration = Duration::from_millis(50);

/// How many searches the latency is measured over, and how many results each
/// asks for -- more than any list shows at once.
const SEARCHES: usize = 100;
const RESULTS_A_SEARCH_ASKS_FOR: usize = 100;

fn main() -> Result<()> {
    let mut root: Option<PathBuf> = None;
    let mut runs = 3usize;
    let mut cores = every_core_but_one();

    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--root" => root = arguments.next().map(PathBuf::from),
            "--runs" => {
                runs = arguments
                    .next()
                    .context("--runs wants a number")?
                    .parse()
                    .context("--runs wants a number")?
            }
            "--cores" => {
                cores = arguments
                    .next()
                    .context("--cores wants a number")?
                    .parse()
                    .context("--cores wants a number")?
            }
            "--help" | "-h" => {
                println!(
                    "Reads a project with every built-in outline query and reports what it cost.\n\
                     \n\
                     --root <path>   the project to read; the working directory by default\n\
                     --runs <n>      how many times, so the spread between them can be seen (3)\n\
                     --cores <n>     threads the pass may use (every core but one)"
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }

    let root = match root {
        Some(root) => root,
        None => std::env::current_dir().context("the working directory")?,
    };
    anyhow::ensure!(runs >= 1, "there is nothing to measure in zero runs");
    anyhow::ensure!(cores >= 1, "the pass needs at least one thread");

    println!("reading {} on {cores} cores, {runs} times", root.display());
    // One pass first whose numbers are thrown away. A cold page cache makes the
    // first read of sixty megabytes a measurement of the disk rather than of the
    // parse, and the runs then fall monotonically as the cache warms -- which
    // reads as an unrepeatable stand when what is unrepeatable is the state of
    // the cache. Every counted run therefore starts from the same warm cache.
    measure(&root, cores).context("the warm-up pass")?;

    let mut every_run: Vec<Numbers> = Vec::new();
    for run in 1..=runs {
        let numbers = measure(&root, cores)?;
        println!("\nrun {run} of {runs}\n{numbers}");
        every_run.push(numbers);
    }

    take_stock_of(&root, cores)?;

    let passes: Vec<Duration> = every_run.iter().map(|one| one.the_whole_pass).collect();
    build_the_index(&root, cores, &passes)?;

    if runs > 1 {
        let repeatable = all_within(&passes, ALLOWED_SPREAD);
        println!(
            "\nthe whole pass across {runs} runs: {} -- {}",
            passes
                .iter()
                .map(|pass| format!("{:.2} s", pass.as_secs_f64()))
                .collect::<Vec<_>>()
                .join(", "),
            match repeatable {
                true => format!("within a {:.0}th, so repeatable", 1. / ALLOWED_SPREAD),
                false => format!(
                    "further than a {:.0}th apart, so not yet repeatable",
                    1. / ALLOWED_SPREAD
                ),
            }
        );
        // The gate is the tool's own answer, not something read off the numbers
        // by hand: a stand whose own repeatability has to be judged by eye is
        // the thing this step exists to replace.
        anyhow::ensure!(repeatable, "the runs are too far apart to build on");
    }

    Ok(())
}

/// Every core but one, which is what the editor's own scanner takes: a number
/// measured on all of them is not a number about the editor.
fn every_core_but_one() -> usize {
    std::thread::available_parallelism()
        .map(|cores| cores.get().saturating_sub(1).max(1))
        .unwrap_or(1)
}

/// The inventory pass, twice: once to fill it and once to show that a project
/// nothing has touched costs no writes at all. Written to a real file rather
/// than to memory, because the size it takes on disk is one of the numbers.
fn take_stock_of(root: &std::path::Path, cores: usize) -> Result<()> {
    let held = tempfile::tempdir().context("somewhere to keep the inventory")?;
    let kept_at = held.path().join("inventory.db");
    let inventory = Inventory::open(&kept_at).context("opening the inventory")?;

    let first = inventory
        .take_stock(root, cores)
        .context("the first pass over the project")?;
    let again = inventory
        .take_stock(root, cores)
        .context("the second pass over the project")?;

    println!(
        "\nthe inventory\n           first pass          {:>10}   {} files, {} rows written\n           second pass         {:>10}   {} rows written, {} unchanged\n           on disk             {:>10}",
        format!("{:.2} s", first.took.as_secs_f64()),
        first.read,
        first.written,
        format!("{:.2} s", again.took.as_secs_f64()),
        again.written,
        again.unchanged,
        inventory
            .on_disk(&kept_at)
            .map(|bytes| format!("{:.1} MB", bytes as f64 / (1024. * 1024.)))
            .unwrap_or_else(|| "-- not reported".to_string()),
    );

    // Both of the plan's gates for this step, checked by the stand rather than
    // read off the numbers by hand.
    anyhow::ensure!(
        again.written == 0,
        "a second pass over an untouched project wrote {} rows",
        again.written
    );
    anyhow::ensure!(
        again.took.as_secs_f64() <= 1.0,
        "a pass over an untouched project took {:.2} s, past the second it is allowed",
        again.took.as_secs_f64()
    );
    Ok(())
}

/// The index build, and then a hundred searches over what it produced. Written
/// to a real file rather than to memory, because the size it takes on disk is
/// one of the plan's five numbers.
///
/// `parse_passes` are the runs already measured above: the plan states the build
/// ceiling relative to them rather than as a time, so that the gate means the
/// same thing on a laptop and on a builder in a cluster.
fn build_the_index(root: &std::path::Path, cores: usize, parse_passes: &[Duration]) -> Result<()> {
    let held = tempfile::tempdir().context("somewhere to keep the symbols")?;
    let kept_at = held.path().join("symbols.db");
    let store = Symbols::open(&kept_at).context("opening the symbol store")?;

    let built = build(root, cores, &store).context("building the index")?;
    store.compact().context("compacting the symbol store")?;
    let on_disk = Symbols::on_disk(&kept_at);

    let catalogue = Catalogue::read_from(&store).context("reading the symbols back")?;
    let searches = searches_over(&catalogue);
    // Warmed first, so what is measured is the search and not the first touch
    // of the memory it reads.
    for query in searches.iter().take(5) {
        catalogue.candidates(query, RESULTS_A_SEARCH_ASKS_FOR);
    }
    let mut answered = Vec::with_capacity(searches.len());
    let mut results = 0usize;
    for query in &searches {
        let started = std::time::Instant::now();
        let found = catalogue.candidates(query, RESULTS_A_SEARCH_ASKS_FOR);
        answered.push(started.elapsed());
        results += found.len();
    }
    let answering = spread_of(answered);

    let allowed_on_disk = (built.bytes as f64 * THE_SHARE_OF_THE_SOURCES) as u64;
    let allowed_to_take = median_of(parse_passes) * PASSES_THE_BUILD_MAY_COST;

    println!(
        "\nthe index\n\
         \x20          the whole build     {:>10}   ceiling {}, {} parse passes\n\
         \x20          reading             {:>10}   writing {}\n\
         \x20          files · symbols     {:>10}   {} symbols\n\
         \x20          on disk             {:>10}   ceiling {}, {} a symbol\n\
         \x20          the most memory     {:>10}   ceiling {}\n\
         \x20          answering a search  {:>10}   95th {}, over {} searches finding {} results",
        as_time(built.took),
        as_time(allowed_to_take),
        PASSES_THE_BUILD_MAY_COST,
        as_time(built.reading),
        as_time(built.writing),
        built.files,
        built.symbols,
        on_disk.map(as_memory).unwrap_or_else(not_reported),
        as_memory(allowed_on_disk),
        on_disk
            .filter(|_| built.symbols > 0)
            .map(|bytes| format!("{} bytes", bytes / built.symbols as u64))
            .unwrap_or_else(not_reported),
        built
            .the_most_memory
            .map(as_memory)
            .unwrap_or_else(not_reported),
        as_memory(THE_MEMORY_CEILING),
        as_time(answering.median),
        as_time(answering.ninety_fifth),
        searches.len(),
        results,
    );

    check_the_gates(&built, on_disk, allowed_on_disk, allowed_to_take, answering)
}

/// The plan's five gates for this step, checked by the stand rather than read
/// off the numbers by hand.
fn check_the_gates(
    built: &Built,
    on_disk: Option<u64>,
    allowed_on_disk: u64,
    allowed_to_take: Duration,
    answering: Spread,
) -> Result<()> {
    anyhow::ensure!(built.symbols > 0, "the build recorded no symbols at all");
    anyhow::ensure!(
        built.took <= allowed_to_take,
        "the build took {}, past the {} it is allowed",
        as_time(built.took),
        as_time(allowed_to_take)
    );
    if let Some(reached) = built.the_most_memory {
        anyhow::ensure!(
            reached <= THE_MEMORY_CEILING,
            "the process reached {}, past the {} it is allowed",
            as_memory(reached),
            as_memory(THE_MEMORY_CEILING)
        );
    }
    // Missing rather than over: a system that does not report the size is not a
    // system that passed, and the stand has to say which it is.
    let on_disk = on_disk.context("the size of the symbol store was not reported")?;
    anyhow::ensure!(
        on_disk <= allowed_on_disk,
        "the symbols take {} on disk, past the {} they are allowed",
        as_memory(on_disk),
        as_memory(allowed_on_disk)
    );
    anyhow::ensure!(
        answering.median < THE_SEARCH_MEDIAN,
        "a search takes {} in the middle, past the {} it is allowed",
        as_time(answering.median),
        as_time(THE_SEARCH_MEDIAN)
    );
    anyhow::ensure!(
        answering.ninety_fifth < THE_SEARCH_TAIL,
        "a search takes {} in the tail, past the {} it is allowed",
        as_time(answering.ninety_fifth),
        as_time(THE_SEARCH_TAIL)
    );
    Ok(())
}

/// A prepared list of searches, drawn from the symbols the project really has.
///
/// The names are taken at an even stride so the list is the same on every run,
/// and the prefixes are of mixed length because a short query matches far more
/// and costs far more: a list of long prefixes would measure the easy half of
/// the work only.
fn searches_over(catalogue: &Catalogue) -> Vec<String> {
    const SHORTEST: usize = 2;
    const LONGEST: usize = 6;

    let names: Vec<&str> = catalogue.names().collect();
    if names.is_empty() {
        return Vec::new();
    }
    let stride = (names.len() / SEARCHES).max(1);
    names
        .iter()
        .step_by(stride)
        .take(SEARCHES)
        .enumerate()
        .filter_map(|(at, name)| {
            let wanted = SHORTEST + at % (LONGEST - SHORTEST + 1);
            let taken: String = name.chars().take(wanted).collect();
            (!taken.is_empty()).then_some(taken)
        })
        .collect()
}

fn median_of(measurements: &[Duration]) -> Duration {
    let mut sorted = measurements.to_vec();
    sorted.sort_unstable();
    sorted
        .get(sorted.len() / 2)
        .copied()
        .unwrap_or(Duration::ZERO)
}

fn not_reported() -> String {
    "-- not reported".to_string()
}
