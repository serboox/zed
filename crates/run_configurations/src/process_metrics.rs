use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::Arc;
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
    pub memory: u64,
    /// Why the network is not being reported. Reading a process's own traffic
    /// needs rights this editor does not ask for.
    pub network: Result<u64, &'static str>,
    /// Why the video memory is not being reported.
    pub video_memory: Result<u64, &'static str>,
    /// Threads of the whole tree, summed over its processes.
    pub threads: u64,
    /// How long the root process has been alive, when the machine says how long
    /// it has itself been up. Without that there is nothing to measure from.
    pub uptime: Option<Duration>,
    /// Every process of the tree and what it is doing, the root first and
    /// every parent before its children.
    pub tree: Vec<ProcessReading>,
}

/// One process of a run, for a reading that lists them rather than summing them.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessReading {
    pub pid: u32,
    /// The process that started it. The root of a run names a parent outside the
    /// run, which is how the tree knows where to stop.
    pub parent: u32,
    pub name: Arc<str>,
    /// Resident memory of this process alone, in bytes.
    pub memory: u64,
    /// Percentage of one core this process alone is using. None until two
    /// readings have been taken of it, since a rate needs two.
    pub cpu: Option<f32>,
    /// Threads this process is running, itself counted.
    pub threads: u64,
    /// What the machine says it is doing: `R`, `S`, `D`, `Z`, `T`, `I`.
    pub state: char,
    /// How long it has been alive, when the machine says how long it has itself
    /// been up.
    pub uptime: Option<Duration>,
}

/// What the machine says about one process, before any of it is turned into a
/// rate.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub pid: u32,
    pub parent: u32,
    /// What the machine calls the program, as `/proc` spells it.
    pub name: Arc<str>,
    /// Ticks of processor time this process has had, user and system together.
    pub ticks: u64,
    pub memory: u64,
    /// Threads it is running, itself counted.
    pub threads: u64,
    /// What the machine says it is doing: `R`, `S`, `D`, `Z`, `T`, `I`.
    pub state: char,
    /// When the machine started it, in ticks since it itself booted. Two
    /// processes under the same number are told apart by this and nothing else:
    /// a fresh process may well have more processor time behind it than the one
    /// that had the number before.
    pub started: u64,
}

/// The ticks a second holds, as the machine itself says. Guessing it skews every
/// percentage; the fallback is the value Linux has used for decades, for a machine
/// that will not answer.
fn ticks_a_second() -> f32 {
    #[cfg(unix)]
    {
        let answer = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if answer > 0 {
            return answer as f32;
        }
    }
    100.
}

/// Reads `/proc/<pid>/stat` and `/proc/<pid>/statm` into a sample.
///
/// The name of a process may hold spaces and brackets -- `(a b) c` is a real
/// name -- so the fields after it are found from the *last* `)`, never by
/// splitting the whole line.
pub fn sample_of(stat: &str, statm: &str) -> Option<Sample> {
    let opens = stat.find('(')?;
    let closes = stat.rfind(')')?;
    let name: Arc<str> = stat.get(opens + 1..closes)?.into();
    let pid: u32 = stat[..stat.find(' ')?].trim().parse().ok()?;
    let after_name: Vec<&str> = stat[closes + 1..].split_whitespace().collect();
    // After the name come: state, ppid, pgrp, ... utime is the 12th, stime the
    // 13th, counting the state as the first.
    let state: char = after_name.first()?.chars().next()?;
    let parent: u32 = after_name.get(1)?.parse().ok()?;
    let utime: u64 = after_name.get(11)?.parse().ok()?;
    let stime: u64 = after_name.get(12)?.parse().ok()?;
    let threads: u64 = after_name.get(17)?.parse().ok()?;
    let started: u64 = after_name.get(19)?.parse().ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(Sample {
        pid,
        parent,
        name,
        ticks: utime.saturating_add(stime),
        memory: pages.saturating_mul(page_size()),
        threads,
        state,
        started,
    })
}

/// What a page of memory holds, as the machine says. `statm` counts pages, so a
/// wrong size here would report the wrong amount of memory.
fn page_size() -> u64 {
    #[cfg(unix)]
    {
        let answer = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if answer > 0 {
            return answer as u64;
        }
    }
    4096
}

/// The processes of a tree, the root first, out of samples of every process on
/// the machine.
pub fn tree_of(root: u32, everything: &[Sample]) -> Vec<Sample> {
    let mut children: HashMap<u32, Vec<Sample>> = HashMap::new();
    for sample in everything {
        children
            .entry(sample.parent)
            .or_default()
            .push(sample.clone());
    }
    let Some(root_sample) = everything.iter().find(|sample| sample.pid == root).cloned() else {
        return Vec::new();
    };
    let mut tree = vec![root_sample];
    // A tree, not a cycle: /proc can name a parent that is also a descendant while
    // processes come and go, and a set says in one look whether a pid is already in.
    let mut taken: HashSet<u32> = HashSet::from([root]);
    let mut at = 0;
    while at < tree.len() {
        let pid = tree[at].pid;
        if let Some(theirs) = children.get(&pid) {
            for child in theirs {
                if taken.insert(child.pid) {
                    tree.push(child.clone());
                }
            }
        }
        at += 1;
    }
    tree
}

/// Which process a sample is of, for as long as the machine runs.
///
/// Pids come back, and a new process under an old number may have more processor
/// time behind it than the one that had the number before -- so the number alone
/// does not say whether this is the same process, and measuring against the wrong
/// one reports a percentage out of nowhere. The moment the machine started it
/// settles the question.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Who {
    pid: u32,
    started: u64,
}

/// What was read last time, so a rate can be worked out from the difference.
#[derive(Clone, Debug, PartialEq)]
struct Baseline {
    when: Instant,
    root: Who,
    /// Each process of the tree and the time it had then. A tree is measured
    /// process by process rather than by its total, because the total falls when
    /// a child ends and rises when a new one starts -- neither of which is work
    /// the machine did in that second.
    ticks: HashMap<Who, u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Watcher {
    last: Option<Baseline>,
}

impl Watcher {
    /// The metrics of `root`'s whole tree. `everything` is every process the
    /// machine will talk about; `now` is when it was read.
    ///
    /// Nothing comes back when the machine has no such process: a run that has
    /// finished leaves its terminal behind, so the pid outlives the run, and a row
    /// of dashes under that pid would read as a running thing that uses nothing.
    pub fn metrics_of(
        &mut self,
        root: u32,
        everything: &[Sample],
        now: Instant,
        machine_uptime: Option<Duration>,
    ) -> Option<Metrics> {
        let tree = tree_of(root, everything);
        let Some(root_sample) = tree.first().cloned() else {
            self.forget();
            return None;
        };
        let memory: u64 = tree
            .iter()
            .fold(0u64, |total, sample| total.saturating_add(sample.memory));
        let who_of = |sample: &Sample| Who {
            pid: sample.pid,
            started: sample.started,
        };
        let ticks: HashMap<Who, u64> = tree
            .iter()
            .map(|sample| (who_of(sample), sample.ticks))
            .collect();
        let root_now = who_of(&root_sample);
        // Since when each process of the tree can be measured against, and over
        // how long. A tree read for the first time has neither, and a rate needs
        // both.
        let since = match &self.last {
            Some(last) if last.root == root_now && now > last.when => {
                let seconds = now.duration_since(last.when).as_secs_f32();
                match seconds > 0. {
                    true => Some((last, seconds)),
                    false => None,
                }
            }
            _ => None,
        };
        let rate_of = |who: &Who, now_ticks: u64| {
            since.map(|(last, seconds)| {
                let before = last.ticks.get(who).copied().unwrap_or(0);
                now_ticks.saturating_sub(before) as f32 / ticks_a_second() / seconds * 100.
            })
        };
        let alive_since = |sample: &Sample| {
            let up = machine_uptime?;
            let started = seconds(sample.started as f32 / ticks_a_second())?;
            // A process the machine says started after the machine itself did is
            // a reading that does not hold together -- two files read a moment
            // apart, or a machine up long enough for the arithmetic to lose the
            // difference. Nothing comes back, rather than an age of zero that
            // would read as "it started just now", every second.
            up.checked_sub(started)
        };
        let holdings: Vec<ProcessReading> = tree
            .iter()
            .map(|sample| ProcessReading {
                pid: sample.pid,
                parent: sample.parent,
                name: sample.name.clone(),
                memory: sample.memory,
                cpu: rate_of(&who_of(sample), sample.ticks),
                threads: sample.threads,
                state: sample.state,
                uptime: alive_since(sample),
            })
            .collect();
        // What the whole tree did since the last reading. A process that has
        // appeared since did all of its work in that time, since a process starts
        // with none; one that has ended took its last moments with it, and they
        // are not counted.
        let cpu = since.map(|_| {
            holdings
                .iter()
                .fold(0., |total, one| total + one.cpu.unwrap_or(0.))
        });
        let threads = tree
            .iter()
            .fold(0u64, |total, sample| total.saturating_add(sample.threads));
        self.last = Some(Baseline {
            when: now,
            root: root_now,
            ticks,
        });
        Some(Metrics {
            pid: root,
            processes: tree.len(),
            cpu,
            memory,
            network: Err("needs rights this editor does not ask for"),
            video_memory: Err("nothing is using it"),
            threads,
            uptime: alive_since(&root_sample),
            tree: holdings,
        })
    }

    /// Drops what was read last time, so the next reading starts its own
    /// reckoning. A run that has ended must not lend its processor time to
    /// whatever takes its number next.
    pub fn forget(&mut self) {
        self.last = None;
    }

    /// How long to wait before reading again. Once a second is what a reader can
    /// follow; more often only spends processor time watching processor time.
    pub const HOW_OFTEN: Duration = Duration::from_secs(1);
}

/// How long the machine has been up, which is what the moment a process started
/// is counted from. Nothing comes back when the machine will not say, rather
/// than a zero that would read as "it started just now".
pub fn machine_uptime() -> Option<Duration> {
    let said = fs::read_to_string("/proc/uptime").ok()?;
    seconds(said.split_whitespace().next()?.parse().ok()?)
}

/// `count` seconds as a duration, or nothing when it is not a number of seconds
/// anything could have lasted. `Duration::from_secs_f32` panics on those rather
/// than refusing them, and a machine is free to write anything into `/proc`.
fn seconds(count: f32) -> Option<Duration> {
    match count.is_finite() && (0. ..u32::MAX as f32).contains(&count) {
        true => Some(Duration::from_secs_f32(count)),
        false => None,
    }
}

/// Every process the machine will talk about, read from `/proc`.
///
/// Anything that disappears while being read is skipped: processes come and go,
/// and that is not an error worth reporting. Nothing at all comes back when the
/// machine did not answer -- an answer holding no processes is one of those, since
/// this editor is always among them -- so a reading that did not happen is never
/// mistaken for a run that has ended.
pub fn everything_running() -> Option<Vec<Sample>> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return None;
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
    match samples.is_empty() {
        true => None,
        false => Some(samples),
    }
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
        assert_eq!(&*sample.name, "my program (2)");
        assert_eq!(sample.parent, 99);
        assert_eq!(sample.ticks, 380, "user and system time together");
        assert_eq!(sample.memory, 512 * 4096);
        assert_eq!(
            sample.threads, 5,
            "the thread count, not the priority beside it"
        );
        assert_eq!(sample.state, 'S');
        assert_eq!(sample.started, 12345, "the moment the machine started it");
    }

    fn a_sample(pid: u32, parent: u32, ticks: u64, memory: u64) -> Sample {
        started_at(pid, parent, ticks, memory, 1_000)
    }

    fn started_at(pid: u32, parent: u32, ticks: u64, memory: u64, started: u64) -> Sample {
        Sample {
            pid,
            parent,
            name: "a program".into(),
            ticks,
            memory,
            threads: 1,
            state: 'S',
            started,
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
        let first = watcher
            .metrics_of(10, &everything, started, None)
            .expect("the tree is running");
        assert_eq!(first.processes, 3);
        assert_eq!(first.memory, 7_000);
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
        let second = watcher
            .metrics_of(10, &busier, started + Duration::from_secs(1), None)
            .expect("the tree is still running");
        // Written against what the machine says a tick is, rather than against a
        // guess: the reading is only as portable as that number.
        let expected = 100. / ticks_a_second() * 100.;
        assert_eq!(
            second.cpu,
            Some(expected),
            "a second's worth of ticks in a second is one core's worth"
        );
    }

    /// Pids come back. A new process under an old number must not be measured
    /// against the processor time of the one that had it before.
    #[test]
    fn a_pid_that_came_back_is_not_measured_against_the_old_one() {
        let mut watcher = Watcher::default();
        let started = Instant::now();
        let busy = vec![started_at(500, 1, 10_000, 1_000, 10)];
        watcher.metrics_of(500, &busy, started, None);

        // The same number, a moment later, on a process the machine started later:
        // a different process altogether, with far less time behind it.
        let fresh = vec![started_at(500, 1, 5, 1_000, 900)];
        let after = watcher
            .metrics_of(500, &fresh, started + Duration::from_secs(1), None)
            .expect("something is running under that number");
        assert_eq!(
            after.cpu, None,
            "a different process under the same number is not the one measured before"
        );

        // And the same again with *more* time behind it than the one before, which
        // the ticks alone would read as a second of furious work.
        let busier = vec![started_at(500, 1, 90_000, 1_000, 7_777)];
        let other_one = watcher
            .metrics_of(500, &busier, started + Duration::from_millis(1_500), None)
            .expect("something is running under that number");
        assert_eq!(
            other_one.cpu, None,
            "only the moment it was started tells the two apart"
        );

        // And a different pid starts its own reckoning.
        let other = vec![a_sample(600, 1, 10, 1_000)];
        let first_of_another = watcher
            .metrics_of(600, &other, started + Duration::from_secs(2), None)
            .expect("that one is running");
        assert_eq!(
            first_of_another.cpu, None,
            "a rate for a process needs two readings of that process"
        );
    }

    /// A number nobody measured is not zero.
    #[test]
    fn what_cannot_be_measured_says_why() {
        let mut watcher = Watcher::default();
        let metrics = watcher
            .metrics_of(7, &[a_sample(7, 1, 3, 2_000)], Instant::now(), None)
            .expect("it is running");
        assert!(metrics.network.is_err());
        assert!(metrics.video_memory.is_err());
        assert!(
            metrics
                .network
                .unwrap_err()
                .contains("rights this editor does not ask for")
        );
    }

    /// A run that has finished leaves its terminal, and its pid, behind. Reading
    /// that pid must come back with nothing rather than with a tree of no
    /// processes using no memory, which reads as a running thing.
    #[test]
    fn a_process_that_is_gone_is_nothing_rather_than_zero() {
        let mut watcher = Watcher::default();
        let started = Instant::now();
        let running = vec![a_sample(300, 1, 10, 4_000)];
        assert!(watcher.metrics_of(300, &running, started, None).is_some());
        assert_eq!(
            watcher.metrics_of(300, &[], started + Duration::from_secs(1), None),
            None,
            "the process is gone, so there is nothing to say about it"
        );

        // And what it left behind is not measured against whatever takes its
        // number next.
        let again = vec![a_sample(300, 1, 5, 4_000)];
        let after = watcher
            .metrics_of(300, &again, started + Duration::from_secs(2), None)
            .expect("something is running under that number again");
        assert_eq!(
            after.cpu, None,
            "a rate needs two readings of the same process"
        );
    }

    /// A tree is measured process by process. Its total falls when a child ends,
    /// which is not the tree giving processor time back, and rises when a child's
    /// number is taken by a new process, which is not a second of work either.
    #[test]
    fn a_child_coming_or_going_is_not_read_as_work() {
        let mut watcher = Watcher::default();
        let at = Instant::now();
        let both = vec![
            started_at(80, 1, 100, 1_000, 10),
            started_at(81, 80, 500, 1_000, 20),
        ];
        watcher.metrics_of(80, &both, at, None);

        // The child ends, taking its 500 ticks out of the total, while the root
        // goes on working: 30 ticks of work, which the total would report as none.
        let alone = vec![started_at(80, 1, 130, 1_000, 10)];
        let after = watcher
            .metrics_of(80, &alone, at + Duration::from_secs(1), None)
            .expect("the root is still running");
        assert_eq!(
            after.cpu,
            Some(30. / ticks_a_second() * 100.),
            "the root's own 30 ticks, whatever the total says"
        );

        // And a new process takes the child's number, with time of its own behind
        // it. Only the time it had in that second is its work.
        let again = vec![
            started_at(80, 1, 130, 1_000, 10),
            started_at(81, 80, 25, 1_000, 90),
        ];
        let later = watcher
            .metrics_of(80, &again, at + Duration::from_secs(2), None)
            .expect("the root is still running");
        assert_eq!(
            later.cpu,
            Some(25. / ticks_a_second() * 100.),
            "a fresh process did its own 25 ticks of work, and no more"
        );
    }

    /// What a run had is not lent to whatever takes its number next.
    #[test]
    fn forgetting_the_last_reading_starts_the_reckoning_again() {
        let mut watcher = Watcher::default();
        let started = Instant::now();
        let running = vec![a_sample(700, 1, 100, 1_000)];
        watcher.metrics_of(700, &running, started, None);
        watcher.forget();
        let busier = vec![a_sample(700, 1, 400, 1_000)];
        let after = watcher
            .metrics_of(700, &busier, started + Duration::from_secs(1), None)
            .expect("it is running");
        assert_eq!(
            after.cpu, None,
            "with nothing to compare against there is no rate to report"
        );
    }

    /// The reading is done against a real machine here, not a fixture: this editor
    /// is a process tree of its own, so it can measure itself.
    #[test]
    fn this_very_process_can_be_measured() {
        // A machine with no /proc has nothing to say, which is its own answer.
        let Some(everything) = everything_running() else {
            return;
        };
        let mut watcher = Watcher::default();
        let metrics = watcher
            .metrics_of(std::process::id(), &everything, Instant::now(), None)
            .expect("this very process is running");
        assert_eq!(metrics.pid, std::process::id());
        assert!(metrics.processes >= 1, "at least the test runner itself");
        assert!(
            metrics.memory > 1024 * 1024,
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

    /// A tree's total says a build is busy; it does not say which of its
    /// processes is. Each row carries its own share, worked out against that
    /// process alone rather than against the tree.
    #[test]
    fn each_process_carries_its_own_share_of_the_processor() {
        let mut watcher = Watcher::default();
        let at = Instant::now();
        let running = vec![
            a_sample(1, 0, 0, 0),
            a_sample(10, 1, 100, 1_000),
            a_sample(11, 10, 100, 1_000),
        ];
        watcher.metrics_of(10, &running, at, None);

        let ticks = ticks_a_second() as u64;
        let busier = vec![
            a_sample(1, 0, 0, 0),
            a_sample(10, 1, 100, 1_000),
            a_sample(11, 10, 100 + ticks / 2, 1_000),
        ];
        let read = watcher
            .metrics_of(10, &busier, at + Duration::from_secs(1), None)
            .expect("the run is still going");

        let root = read
            .tree
            .iter()
            .find(|one| one.pid == 10)
            .expect("the root");
        let child = read
            .tree
            .iter()
            .find(|one| one.pid == 11)
            .expect("the child");
        assert_eq!(
            root.cpu,
            Some(0.),
            "the root did nothing in that second and says so"
        );
        let child_cpu = child.cpu.expect("the child was measured");
        assert!(
            (child_cpu - 50.).abs() < 1.,
            "the child had half a second of a core: {child_cpu}"
        );
        let total = read.cpu.expect("the tree was measured");
        assert!(
            (total - child_cpu).abs() < 0.01,
            "and the tree's total is what its processes add up to: {total} against {child_cpu}"
        );
    }

    /// The first reading of a run has nothing to measure a rate against, and
    /// says so for every row rather than reporting an idle tree.
    #[test]
    fn the_first_reading_gives_no_process_a_rate() {
        let mut watcher = Watcher::default();
        let read = watcher
            .metrics_of(
                10,
                &[a_sample(1, 0, 0, 0), a_sample(10, 1, 100, 1_000)],
                Instant::now(),
                None,
            )
            .expect("the run was found");

        assert_eq!(read.cpu, None);
        assert!(read.tree.iter().all(|one| one.cpu.is_none()));
    }

    /// How long a process has been alive is counted back from when the machine
    /// started it. Without the machine's own age there is nothing to count from,
    /// and nothing is what comes back.
    #[test]
    fn a_process_is_as_old_as_the_machine_less_when_it_started() {
        let mut watcher = Watcher::default();
        let started = (ticks_a_second() * 30.) as u64;
        let running = [a_sample(1, 0, 0, 0), started_at(10, 1, 5, 1_000, started)];

        let read = watcher
            .metrics_of(10, &running, Instant::now(), Some(Duration::from_secs(90)))
            .expect("the run was found");

        let uptime = read
            .uptime
            .expect("the machine said how long it had been up");
        assert!(
            (uptime.as_secs_f32() - 60.).abs() < 0.5,
            "started thirty seconds into a ninety-second machine: {uptime:?}"
        );
        assert_eq!(read.tree.first().and_then(|one| one.uptime), read.uptime);

        watcher.forget();
        let unknown = watcher
            .metrics_of(10, &running, Instant::now(), None)
            .expect("the run was found");
        assert_eq!(
            unknown.uptime, None,
            "and a machine that will not say its own age leaves the run's unknown"
        );
    }

    /// A reading that does not hold together -- a process the machine says
    /// started after the machine itself did -- leaves the age unknown rather
    /// than reporting a run that started this very instant.
    #[test]
    fn a_process_that_cannot_be_younger_than_it_says_has_no_age() {
        let mut watcher = Watcher::default();
        let started = (ticks_a_second() * 300.) as u64;
        let running = [a_sample(1, 0, 0, 0), started_at(10, 1, 5, 1_000, started)];

        let read = watcher
            .metrics_of(10, &running, Instant::now(), Some(Duration::from_secs(90)))
            .expect("the run was found");

        assert_eq!(read.uptime, None);
        assert_eq!(read.tree.first().and_then(|one| one.uptime), None);
    }

    /// The threads of the tree are what its processes run between them, not the
    /// root's alone.
    #[test]
    fn the_threads_reported_are_the_whole_trees() {
        let mut watcher = Watcher::default();
        let mut root = a_sample(10, 1, 0, 1_000);
        root.threads = 4;
        let mut child = a_sample(11, 10, 0, 1_000);
        child.threads = 6;

        let read = watcher
            .metrics_of(
                10,
                &[a_sample(1, 0, 0, 0), root, child],
                Instant::now(),
                None,
            )
            .expect("the run was found");

        assert_eq!(read.threads, 10);
    }
}
