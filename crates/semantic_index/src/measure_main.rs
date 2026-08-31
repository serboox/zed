use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use semantic_index::measure::{Numbers, all_within, measure};

/// How far apart three runs of the same measurement may be and still be called
/// repeatable. The plan's first gate.
const ALLOWED_SPREAD: f64 = 0.1;

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
    let mut every_run: Vec<Numbers> = Vec::new();
    for run in 1..=runs {
        let numbers = measure(&root, cores)?;
        println!("\nrun {run} of {runs}\n{numbers}");
        every_run.push(numbers);
    }

    if runs > 1 {
        let passes: Vec<Duration> = every_run.iter().map(|one| one.the_whole_pass).collect();
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
