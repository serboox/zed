use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use semantic_index::against_the_server::{
    QueryAnswers, Server, attribute_lines, compare, macro_body_lines, re_export_lines,
    sample_queries,
};
use std::collections::{HashMap, HashSet};

use semantic_index::definitions::Definition;
use semantic_index::measure::{as_time, spread_of};
use semantic_index::symbols::{Catalogue, Symbols, build};

/// The plan's own sample size for this check.
const QUERY_COUNT: usize = 200;

/// The plan's own gate: the index has to find at least this share of what
/// rust-analyzer finds.
const REQUIRED_RECALL: f64 = 0.95;

/// Asked of the index without a cap at all.
///
/// The first version of this asked for the best five hundred and compared two
/// capped lists, which measured the wrong thing: the index deliberately does not
/// rank -- the editor's own matcher does that -- so a cap on its side is a cut
/// through an unordered set, and its own tie-break keeps the shortest names. The
/// server's longer answers were being thrown away before the comparison, and the
/// result read as the index having missed them. Recall of an index is a question
/// of coverage, not of order.
const RESULTS_PER_QUERY: usize = usize::MAX;

/// This measurement is about Rust, which is what the plan's step is about, and
/// the server answers about Rust alone. The index reads thirteen languages, so
/// without this the two sides are compared over different sets entirely -- a
/// heading in a Markdown file counted against the index as something the server
/// had not found.
fn is_rust(definition: &semantic_index::definitions::Definition) -> bool {
    definition.path.ends_with(".rs")
}

/// Whether the index ever reads this file at all.
///
/// The server indexes generated build output under `target/`; the index walks
/// the project the way the editor's own scanner does, which leaves build output
/// out. Counting a symbol in a generated file as something the index missed
/// measures the difference between two scopes rather than the index's coverage
/// of its own. How many are set aside this way is printed, so the difference is
/// stated rather than hidden.
fn within_reach(read: &HashSet<String>, definition: &Definition) -> bool {
    read.contains(&definition.path)
}

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

    println!(
        "building the index over {} on {cores} cores",
        root.display()
    );
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
    let mut server = Server::start(&root)
        .await
        .context("starting rust-analyzer")?;
    let indexing_started = Instant::now();
    server
        .wait_until_indexed(indexing_timeout)
        .await
        .context("waiting for rust-analyzer to finish indexing")?;
    println!(
        "rust-analyzer finished indexing in {}",
        as_time(indexing_started.elapsed())
    );

    // Every file the index actually read, so the server's answers can be held to
    // the same set of files rather than to a larger one. Taken from the files
    // the index read, not from the files its symbols came from: a file that
    // defines nothing was still read, and setting its symbols aside would flatter
    // the number by shrinking the denominator.
    let read_by_the_index: HashSet<String> = store
        .file_paths()
        .context("reading back which files the index read")?
        .into_iter()
        .collect();
    // Where each file re-exports something, so a name the server lists at the
    // place it is re-exported is not counted against an index that holds it at
    // the place it is defined. The plan's step is about definitions, and
    // `workspace/symbol` answers with more than those.
    let rust = semantic_index::languages::readable()
        .0
        .into_iter()
        .find(|language| language.name == "rust")
        .context("Rust is one of the languages the editor ships")?;
    let mut re_exports: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    for path in &read_by_the_index {
        if !path.ends_with(".rs") {
            continue;
        }
        let Ok(contents) = std::fs::read(root.join(path)) else {
            continue;
        };
        let spans = re_export_lines(&contents, &rust.grammar);
        if !spans.is_empty() {
            re_exports.insert(path.clone(), spans);
        }
    }
    let is_a_re_export = |found: &Definition| {
        re_exports.get(&found.path).is_some_and(|spans| {
            spans
                .iter()
                .any(|(from, to)| found.line >= *from && found.line <= *to)
        })
    };
    // A file is a module, and a server says so; an index of what a file defines
    // has nothing to say about the file itself.
    let is_the_file_itself = |found: &Definition| {
        found.line == 1
            && std::path::Path::new(&found.path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem == found.name)
    };

    let mut set_aside = 0usize;
    let mut re_exported = 0usize;
    let mut modules = 0usize;
    let mut answers = Vec::with_capacity(queries.len());
    let mut index_timings = Vec::with_capacity(queries.len());
    let mut server_timings = Vec::with_capacity(queries.len());
    for query in &queries {
        let index_started = Instant::now();
        let the_index_found: Vec<_> = catalogue
            .candidates(query, RESULTS_PER_QUERY)
            .into_iter()
            .filter(is_rust)
            .collect();
        index_timings.push(index_started.elapsed());

        let server_started = Instant::now();
        let answered = server
            .workspace_symbol(query, QUERY_TIMEOUT)
            .await
            .with_context(|| format!("asking rust-analyzer for `{query}`"))?;
        server_timings.push(server_started.elapsed());
        let asked_about: Vec<Definition> = answered.into_iter().filter(is_rust).collect();
        let before = asked_about.len();
        let within: Vec<Definition> = asked_about
            .into_iter()
            .filter(|found| within_reach(&read_by_the_index, found))
            .collect();
        set_aside += before - within.len();
        let the_server_found: Vec<Definition> = within
            .into_iter()
            .filter(|found| {
                if is_a_re_export(found) {
                    re_exported += 1;
                    return false;
                }
                if is_the_file_itself(found) {
                    modules += 1;
                    return false;
                }
                true
            })
            .collect();

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

    if set_aside > 0 {
        println!(
            "\n{set_aside} of the server's findings were in files the index never reads -- \
             generated build output and the like -- and are counted on neither side."
        );
    }
    if re_exported > 0 || modules > 0 {
        println!(
            "{re_exported} of the server's findings were names re-exported rather than defined, \
             and {modules} were files reported as modules. An index of definitions holds \
             neither, and each is counted on neither side."
        );
    }
    // What is left over after the comparison is aligned has to be named, not
    // waved at: a miss inside a macro body is one no query over a parse tree can
    // ever close, and a miss outside one is a gap in the queries themselves.
    let mut inside_a_macro = 0usize;
    let mut on_an_attribute = 0usize;
    let mut elsewhere = 0usize;
    let mut spans_by_file: HashMap<String, (Vec<(u32, u32)>, Vec<(u32, u32)>)> = HashMap::new();
    let covers = |spans: &[(u32, u32)], line: u32| {
        spans.iter().any(|(from, to)| line >= *from && line <= *to)
    };
    for diverged in &comparison.divergent_queries {
        for missed in &diverged.the_server_found_and_the_index_missed {
            let (macros, attributes) =
                spans_by_file.entry(missed.path.clone()).or_insert_with(|| {
                    match std::fs::read(root.join(&missed.path)) {
                        Ok(contents) => (
                            macro_body_lines(&contents, &rust.grammar),
                            attribute_lines(&contents, &rust.grammar),
                        ),
                        Err(_) => (Vec::new(), Vec::new()),
                    }
                });
            if covers(macros, missed.line) {
                inside_a_macro += 1;
            } else if covers(attributes, missed.line) {
                on_an_attribute += 1;
            } else {
                elsewhere += 1;
            }
        }
    }

    println!("\n{comparison}");
    let missed = inside_a_macro + on_an_attribute + elsewhere;
    if missed > 0 {
        println!(
            "of the {missed} the index missed, {inside_a_macro} are inside a macro body, which \
             the grammar keeps as an opaque token tree; {on_an_attribute} are on an attribute, \
             where a derive expands code and a `cfg` does not; and {elsewhere} are elsewhere"
        );
    }
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
