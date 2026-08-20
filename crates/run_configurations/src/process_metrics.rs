use std::collections::HashMap;
use std::fs;
use std::time::{Duration, Instant};

/// How much of a machine a run is using, as far as this platform will say.
///
/// A number nobody can measure is not reported as zero: `None` with a reason is
/// the truth, and zero would be a lie that reads as "it is using nothing".
#[derive(Clone, Debug, PartialEq)]
pub struct Metrics {
    pub pid: u32,
    /// Every process in the tree, the root included.
    pub processes: usize,
    /// Percentage of one core, summed over the tree. None until two samples have
    /// been taken, since a rate needs two readings.
    pub cpu: Option<f32>,
    /// Resident memory of the tree, in bytes.
    pub memory: Option<u64>,
    /// Why the network is not being reported. Reading a process's own traffic
    /// needs rights this editor does not ask for.
    pub network: Result<u64, &'static str>,
    /// Why the video memory is not being reported.
    pub video_memory: Result<u64, &'static str>,
}

impl Metrics {
    /// Nothing measured yet, which is what there is to say before the first
    /// reading.
    pub fn nothing_yet(pid: u32) -> Self {
        Self {
            pid,
            processes: 0,
            cpu: None,
            memory: None,
            network: Err("needs rights this editor does not ask for"),
            video_memory: Err("nothing is using it"),
        }
    }
}

/// What the machine says about one process, before any of it is turned into a
/// rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub pid: u32,
    pub parent: u32,
    /// Ticks of processor time this process has had, user and system together.
    pub ticks: u64,
    pub memory: u64,
}

/// The ticks a second holds on Linux. `USER_HZ` has been 100 on every Linux this
/// editor runs on; a wrong guess here would only skew the percentage, never the
/// memory.
const TICKS_A_SECOND: f32 = 100.;

/// Reads `/proc/<pid>/stat` and `/proc/<pid>/statm` into a sample.
///
/// The name of a process may hold spaces and brackets -- `(a b) c` is a real
/// name -- so the fields after it are found from the *last* `)`, never by
/// splitting the whole line.
pub fn sample_of(stat: &str, statm: &str) -> Option<Sample> {
    let closes = stat.rfind(')')?;
    let pid: u32 = stat[..stat.find(' ')?].trim().parse().ok()?;
    let after_name: Vec<&str> = stat[closes + 1..].split_whitespace().collect();
    // After the name come: state, ppid, pgrp, ... utime is the 12th, stime the
    // 13th, counting the state as the first.
    let parent: u32 = after_name.get(1)?.parse().ok()?;
    let utime: u64 = after_name.get(11)?.parse().ok()?;
    let stime: u64 = after_name.get(12)?.parse().ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(Sample {
        pid,
        parent,
        ticks: utime + stime,
        memory: pages * page_size(),
    })
}

fn page_size() -> u64 {
    4096
}

/// The processes of a tree, the root first, out of samples of every process on
/// the machine.
pub fn tree_of(root: u32, everything: &[Sample]) -> Vec<Sample> {
    let mut children: HashMap<u32, Vec<Sample>> = HashMap::new();
    for sample in everything {
        children.entry(sample.parent).or_default().push(*sample);
    }
    let Some(root_sample) = everything.iter().find(|sample| sample.pid == root).copied() else {
        return Vec::new();
    };
    let mut tree = vec![root_sample];
    let mut at = 0;
    while at < tree.len() {
        let pid = tree[at].pid;
        if let Some(theirs) = children.get(&pid) {
            for child in theirs {
                // A tree, not a cycle: /proc can hand back a parent that is also a
                // descendant while processes come and go.
                if !tree.iter().any(|kept| kept.pid == child.pid) {
                    tree.push(*child);
                }
            }
        }
        at += 1;
    }
    tree
}

/// What was read last time, so a rate can be worked out from the difference.
#[derive(Clone, Debug, Default)]
pub struct Watcher {
    last: Option<(Instant, u64)>,
}

impl Watcher {
    /// The metrics of `root`'s whole tree. `everything` is every process the
    /// machine will talk about; `now` is when it was read.
    pub fn metrics_of(&mut self, root: u32, everything: &[Sample], now: Instant) -> Metrics {
        let tree = tree_of(root, everything);
        let ticks: u64 = tree.iter().map(|sample| sample.ticks).sum();
        let memory: u64 = tree.iter().map(|sample| sample.memory).sum();
        let cpu = match self.last {
            Some((then, before)) if now > then && ticks >= before => {
                let seconds = now.duration_since(then).as_secs_f32();
                match seconds > 0. {
                    true => Some((ticks - before) as f32 / TICKS_A_SECOND / seconds * 100.),
                    false => None,
                }
            }
            _ => None,
        };
        self.last = Some((now, ticks));
        Metrics {
            pid: root,
            processes: tree.len(),
            cpu,
            memory: match tree.is_empty() {
                true => None,
                false => Some(memory),
            },
            network: Err("needs rights this editor does not ask for"),
            video_memory: Err("nothing is using it"),
        }
    }

    /// How long to wait before reading again. Once a second is what a reader can
    /// follow; more often only spends processor time watching processor time.
    pub const HOW_OFTEN: Duration = Duration::from_secs(1);
}

/// Every process the machine will talk about, read from `/proc`.
///
/// Anything that disappears while being read is skipped: processes come and go,
/// and that is not an error worth reporting.
pub fn everything_running() -> Vec<Sample> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut samples = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.chars().all(|character| character.is_ascii_digit()) {
            continue;
        }
        let stat = fs::read_to_string(entry.path().join("stat"));
        let statm = fs::read_to_string(entry.path().join("statm"));
        if let (Ok(stat), Ok(statm)) = (stat, statm)
            && let Some(sample) = sample_of(&stat, &statm)
        {
            samples.push(sample);
        }
    }
    samples
}

/// `bytes` as a reader reads it.
pub fn as_memory(bytes: u64) -> String {
    const KIB: f32 = 1024.;
    let bytes = bytes as f32;
    match bytes {
        bytes if bytes < KIB * KIB => format!("{:.0} KB", bytes / KIB),
        bytes if bytes < KIB * KIB * KIB => format!("{:.0} MB", bytes / KIB / KIB),
        bytes => format!("{:.1} GB", bytes / KIB / KIB / KIB),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process's name may hold spaces and brackets, so the fields after it are
    /// found from the last `)`. Splitting the line would read the name as a field
    /// and everything after it would be off by one.
    #[test]
    fn a_name_with_spaces_in_it_does_not_shift_the_fields() {
        let stat = "4242 (my program (2)) S 99 4242 4242 0 -1 4194304 100 0 0 0 \
                    250 130 0 0 20 0 5 0 12345 0 0";
        let statm = "1000 512 100 10 0 200 0";
        let sample = sample_of(stat, statm).expect("the line reads");
        assert_eq!(sample.pid, 4242);
        assert_eq!(sample.parent, 99);
        assert_eq!(sample.ticks, 380, "user and system time together");
        assert_eq!(sample.memory, 512 * 4096);
    }

    fn a_sample(pid: u32, parent: u32, ticks: u64, memory: u64) -> Sample {
        Sample {
            pid,
            parent,
            ticks,
            memory,
        }
    }

    /// A run is a tree: the shell, what it started, and what that started in turn.
    /// Measuring only the root would report a build as using nothing at all.
    #[test]
    fn the_whole_tree_is_measured_not_only_its_root() {
        let everything = vec![
            a_sample(1, 0, 0, 0),
            a_sample(10, 1, 5, 1_000),
            a_sample(11, 10, 7, 2_000),
            a_sample(12, 11, 9, 4_000),
            a_sample(20, 1, 99, 99_000),
        ];
        let tree = tree_of(10, &everything);
        assert_eq!(
            tree.iter().map(|sample| sample.pid).collect::<Vec<_>>(),
            vec![10, 11, 12],
            "the root and everything under it, and nothing else"
        );

        let mut watcher = Watcher::default();
        let started = Instant::now();
        let first = watcher.metrics_of(10, &everything, started);
        assert_eq!(first.processes, 3);
        assert_eq!(first.memory, Some(7_000));
        assert_eq!(
            first.cpu, None,
            "a rate needs two readings, and one is not two"
        );

        // A second later, one more second of processor time across the tree.
        let busier: Vec<Sample> = everything
            .iter()
            .map(|sample| match sample.pid {
                11 => a_sample(11, 10, 7 + 100, 2_000),
                other => a_sample(other, sample.parent, sample.ticks, sample.memory),
            })
            .collect();
        let second = watcher.metrics_of(10, &busier, started + Duration::from_secs(1));
        assert_eq!(
            second.cpu,
            Some(100.),
            "a hundred ticks in a second is one core's worth"
        );
    }

    /// A number nobody measured is not zero.
    #[test]
    fn what_cannot_be_measured_says_why() {
        let mut watcher = Watcher::default();
        let metrics = watcher.metrics_of(7, &[], Instant::now());
        assert_eq!(metrics.memory, None, "an empty tree is not zero bytes");
        assert!(metrics.network.is_err());
        assert!(metrics.video_memory.is_err());
        assert!(
            metrics
                .network
                .unwrap_err()
                .contains("rights this editor does not ask for")
        );
    }

    /// The reading is done against a real machine here, not a fixture: this editor
    /// is a process tree of its own, so it can measure itself.
    #[test]
    fn this_very_process_can_be_measured() {
        let everything = everything_running();
        if everything.is_empty() {
            // A machine with no /proc has nothing to say, which is its own answer.
            return;
        }
        let mut watcher = Watcher::default();
        let metrics = watcher.metrics_of(std::process::id(), &everything, Instant::now());
        assert_eq!(metrics.pid, std::process::id());
        assert!(metrics.processes >= 1, "at least the test runner itself");
        assert!(
            metrics.memory.unwrap_or(0) > 1024 * 1024,
            "a running test uses more than a megabyte: {:?}",
            metrics.memory
        );
    }

    #[test]
    fn memory_reads_the_way_a_reader_reads_it() {
        assert_eq!(as_memory(2048), "2 KB");
        assert_eq!(as_memory(84 * 1024 * 1024), "84 MB");
        assert_eq!(
            as_memory(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024),
            "3.5 GB"
        );
    }
}
