use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use semantic_index::against_the_server::{QueryAnswers, Server, compare, sample_queries};
use semantic_index::measure::{as_time, spread_of};
use semantic_index::symbols::{Catalogue, Symbols, build};

/// The plan's own sample size for this check.
const QUERY_COUNT: usize = 200;

/// The plan's own gate: the index has to find at least this share of what
/// rust-analyzer finds.
const REQUIRED_RECALL: f64 = 0.95;

/// Asked of both sides for every query, generous enough that neither side's
/// own result cap is what decides a divergence -- the queries are full symbol
/// names, so neither side should come anywhere near this many matches.
const RESULTS_PER_QUERY: usize = 500;

/// How long to wait for rust-analyzer to finish indexing before giving up.
/// Generous on purpose: a real project can take minutes, and a run that gives
/// up too early would measure a half-built index and call the result a
/// failure that was never real.
const INDEXING_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a single `workspace/symbol` query may take before it is treated as
/// a failure of the run rather than skipped -- a query that hangs is a sign
/// something is systemically wrong, not something to quietly leave out of the
/// sample.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> Result<()> {
    smol::block_on(run())
}

async fn run() -> Result<()> {
    let mut root: Option<PathBuf> = None;
    let mut cores = every_core_but_one();
    let mut query_count = QUERY_COUNT;
    let mut indexing_timeout = INDEXING_TIMEOUT;

    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--root" => root = arguments.next().map(PathBuf::from),
            "--cores" => {
                cores = arguments
                    .next()
                    .context("--cores wants a number")?
                    .parse()
                    .context("--cores wants a number")?
            }
            "--queries" => {
                query_count = arguments
                    .next()
                    .context("--queries wants a number")?
                    .parse()
                    .context("--queries wants a number")?
            }
            "--indexing-timeout" => {
                let seconds: u64 = arguments
                    .next()
                    .context("--indexing-timeout wants a number of seconds")?
                    .parse()
                    .context("--indexing-timeout wants a number of seconds")?;
                indexing_timeout = Duration::from_secs(seconds);
            }
            "--help" | "-h" => {
                println!(
                    "Compares the index's own answers with rust-analyzer's, over a sample of\n\
                     the index's own symbol names, and prints where they diverge.\n\
                     \n\
                     --root <path>              the project to read; the working directory by \
                     default\n\
                     --cores <n>                threads the index build may use (every core but \
                     one)\n\
                     --queries <n>              how many queries to sample ({QUERY_COUNT})\n\
                     --indexing-timeout <secs>  how long to wait for rust-analyzer to finish \
                     indexing ({}s)",
                    INDEXING_TIMEOUT.as_secs()
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
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    anyhow::ensure!(cores >= 1, "the index build needs at least one thread");
    anyhow::ensure!(
        query_count >= 1,
        "there is nothing to compare over zero queries"
    );

    println!("building the index over {} on {cores} cores", root.display());
    let store = Symbols::open_in_memory().context("opening an in-memory symbol store")?;
    let built = build(&root, cores, &store).context("building the index")?;
    let catalogue = Catalogue::read_from(&store).context("reading the symbols back")?;
    println!("{} files, {} symbols", built.files, built.symbols);

    let names: Vec<String> = catalogue.names().map(str::to_string).collect();
    let queries = sample_queries(&names, query_count);
    println!(
        "sampled {} of {} requested queries from the index's {} symbol names, at an even stride \
         through them -- the same sample every run over the same project",
        queries.len(),
        query_count,
        names.len()
    );
    anyhow::ensure!(
        !queries.is_empty(),
        "the index holds no symbol names to sample queries from"
    );

    println!(
        "starting rust-analyzer and waiting for it to finish indexing (up to {}s)...",
        indexing_timeout.as_secs()
    );
    let mut server = Server::start(&root).await.context("starting rust-analyzer")?;
    let indexing_started = Instant::now();
    server
        .wait_until_indexed(indexing_timeout)
        .await
        .context("waiting for rust-analyzer to finish indexing")?;
    println!(
        "rust-analyzer finished indexing in {}",
        as_time(indexing_started.elapsed())
    );

    let mut answers = Vec::with_capacity(queries.len());
    let mut index_timings = Vec::with_capacity(queries.len());
    let mut server_timings = Vec::with_capacity(queries.len());
    for query in &queries {
        let index_started = Instant::now();
        let the_index_found = catalogue.candidates(query, RESULTS_PER_QUERY);
        index_timings.push(index_started.elapsed());

        let server_started = Instant::now();
        let the_server_found = server
            .workspace_symbol(query, QUERY_TIMEOUT)
            .await
            .with_context(|| format!("asking rust-analyzer for `{query}`"))?;
        server_timings.push(server_started.elapsed());

        answers.push(QueryAnswers {
            query: query.clone(),
            the_server_found,
            the_index_found,
        });
    }

    if let Err(error) = server.shut_down().await {
        log::warn!("rust-analyzer did not shut down cleanly: {error:#}");
    }

    let comparison = compare(&answers);
    let answering_the_index = spread_of(index_timings);
    let answering_the_server = spread_of(server_timings);

    println!("\n{comparison}");
    println!(
        "answering a query   index: median {:>10} 95th {:>10}   rust-analyzer: median {:>10} \
         95th {:>10}",
        as_time(answering_the_index.median),
        as_time(answering_the_index.ninety_fifth),
        as_time(answering_the_server.median),
        as_time(answering_the_server.ninety_fifth),
    );

    // The gates are checked only after every number above is printed, so a
    // failing run still shows what it found rather than only that it failed.
    anyhow::ensure!(
        comparison.the_servers_findings > 0,
        "rust-analyzer found nothing at all across {} queries -- this measures nothing, not a \
         passing recall",
        comparison.queries
    );
    anyhow::ensure!(
        comparison.recall >= REQUIRED_RECALL,
        "the index found {:.1}% of what rust-analyzer found, short of the {:.0}% the plan requires",
        comparison.recall * 100.0,
        REQUIRED_RECALL * 100.0
    );
    Ok(())
}

/// Every core but one, which is what the editor's own scanner takes: a build
/// measured on all of them is not a build the editor would ever actually run.
fn every_core_but_one() -> usize {
    std::thread::available_parallelism()
        .map(|cores| cores.get().saturating_sub(1).max(1))
        .unwrap_or(1)
}
