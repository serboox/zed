use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use semantic_index::measure::as_time;
use semantic_index::references::{Certainty, Report, measure};

/// The plan's own sample size for this check.
const SYMBOL_COUNT: usize = 100;

/// The plan's own gates. Below either of them, references are not fit to base a
/// rename on, and the step that would have done so is cancelled rather than
/// built on a guess.
const REQUIRED_PRECISION: f64 = 0.90;
const REQUIRED_RECALL: f64 = 0.85;

/// How many passes over definitions a pass over references may cost.
const DEFINITION_PASSES_ALLOWED: u32 = 3;

/// How long to wait for the language server to finish indexing. Generous on
/// purpose: a run that gave up early would measure a half-built server and
/// report a failure that was never real.
const INDEXING_TIMEOUT: Duration = Duration::from_secs(600);

/// How long one request may take before it counts as a failure of the run. A
/// request that hangs is a sign something is systemically wrong, not something
/// to quietly leave out of the sample -- and dropping the slow ones would bias
/// the sample towards symbols with few references, which is exactly the
/// direction that flatters the result. So the limit is generous, and the
/// slowest answer is printed beside it: a run that came close to the limit
/// says so instead of looking comfortable.
const QUERY_TIMEOUT: Duration = Duration::from_secs(180);

fn main() -> Result<()> {
    smol::block_on(run())
}

async fn run() -> Result<()> {
    let mut root: Option<PathBuf> = None;
    let mut symbol_count = SYMBOL_COUNT;
    let mut indexing_timeout = INDEXING_TIMEOUT;
    let mut query_timeout = QUERY_TIMEOUT;
    // Declining an ambiguous name is the default, because answering one is a
    // guess. `--answer-everything` restores the old behaviour, so the two can
    // be compared on the same sample.
    let mut certainty = Certainty::OnlyWhenTheNameMeansOneThing;

    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--root" => root = arguments.next().map(PathBuf::from),
            "--symbols" => {
                symbol_count = arguments
                    .next()
                    .context("--symbols wants a number")?
                    .parse()
                    .context("--symbols wants a number")?
            }
            "--query-timeout" => {
                let seconds: u64 = arguments
                    .next()
                    .context("--query-timeout wants a number of seconds")?
                    .parse()
                    .context("--query-timeout wants a number of seconds")?;
                query_timeout = Duration::from_secs(seconds);
            }
            "--answer-everything" => certainty = Certainty::Always,
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
                    "Measures how well the index's references agree with a language server's.\n\
                     \n\
                     --root <path>               the project to read; the working directory by default\n\
                     --symbols <n>               how many symbols to compare over ({SYMBOL_COUNT})\n\
                     --indexing-timeout <secs>   how long to wait for the server to finish indexing\n\
                     --query-timeout <secs>      how long one request may take ({})\n\
                     --answer-everything         answer about ambiguous names too, as the first\n\
                     \x20                           measurement did",
                    QUERY_TIMEOUT.as_secs()
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
    anyhow::ensure!(
        symbol_count >= 1,
        "there is nothing to compare over zero symbols"
    );

    println!(
        "comparing references under {} over {symbol_count} symbols",
        root.display()
    );
    println!(
        "rust-analyzer's query cache is capped at {} entries (RA_LRU_CAP); its own default is \
         128, which does not fit in this machine's memory",
        semantic_index::against_the_server::lru_capacity()
    );
    let report = measure(
        &root,
        symbol_count,
        indexing_timeout,
        query_timeout,
        certainty,
    )
    .await?;
    // Printed before anything is judged: the divergences are the point of this
    // run, and a gate that failed before they were shown would hide them.
    println!("\n{report}");

    check_the_gates(&report)
}

/// The plan's three gates for this step, checked by the stand rather than read
/// off the numbers by hand.
fn check_the_gates(report: &Report) -> Result<()> {
    let allowed_to_take = report.definitions_pass * DEFINITION_PASSES_ALLOWED;
    anyhow::ensure!(
        report.references_pass <= allowed_to_take,
        "a pass over references took {}, past the {} it is allowed -- {} passes over definitions",
        as_time(report.references_pass),
        as_time(allowed_to_take),
        DEFINITION_PASSES_ALLOWED
    );
    anyhow::ensure!(
        report.precision >= REQUIRED_PRECISION,
        "precision is {:.1}%, under the {:.0}% references need to be worth renaming from",
        report.precision * 100.0,
        REQUIRED_PRECISION * 100.0
    );
    anyhow::ensure!(
        report.comparison.recall >= REQUIRED_RECALL,
        "recall is {:.1}%, under the {:.0}% references need to be worth renaming from",
        report.comparison.recall * 100.0,
        REQUIRED_RECALL * 100.0
    );
    Ok(())
}
