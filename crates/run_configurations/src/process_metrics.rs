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
    /// The threads the machine runs this process on, busiest first.
    pub thread_readings: Vec<ThreadReading>,
}

/// One thread of a process, as the machine schedules it.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadReading {
    pub tid: u32,
    /// What the thread calls itself, as `/proc/<pid>/task/<tid>/comm` spells it.
    pub name: Arc<str>,
    /// Percentage of one core this thread is using. None until two readings
    /// have been taken of it.
    pub cpu: Option<f32>,
    /// What the machine says it is doing: `R`, `S`, `D`, `Z`, `T`, `I`.
    pub state: char,
    /// Processor time it has had since it started, user and system together.
    pub cpu_time: Duration,
    /// How long it has been running. None when the machine's clock and the
    /// thread's start do not hold together.
    pub uptime: Option<Duration>,
    /// Its nice value, from -20 (first in line) to 19 (last).
    pub nice: Option<i64>,
    /// The core it last ran on.
    pub last_core: Option<u32>,
    pub switches: Option<ContextSwitches>,
}

/// How often the scheduler took a thread off its core: because it waited for
/// something (voluntary), or because its time was up (involuntary). A thread
/// with many involuntary switches wants more processor than it gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextSwitches {
    pub voluntary: u64,
    pub involuntary: u64,
}

/// Reads the switch counts out of `/proc/<pid>/task/<tid>/status`.
pub fn context_switches_of(status: &str) -> Option<ContextSwitches> {
    let count_of = |key: &str| {
        status.lines().find_map(|line| {
            line.strip_prefix(key)?
                .strip_prefix(':')?
                .trim()
                .parse::<u64>()
                .ok()
        })
    };
    Some(ContextSwitches {
        voluntary: count_of("voluntary_ctxt_switches")?,
        involuntary: count_of("nonvoluntary_ctxt_switches")?,
    })
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
    /// Every thread the process was running, read the same moment.
    pub thread_samples: Vec<ThreadSample>,
}

/// What the machine says about one thread, before any of it is turned into a
/// rate. The same idea as `Sample`, for `/proc/<pid>/task/<tid>` instead of
/// `/proc/<pid>`.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadSample {
    pub tid: u32,
    /// What the thread calls itself: `/proc/<pid>/task/<tid>/comm` when it has
    /// something to say, the name from `stat` otherwise.
    pub name: Arc<str>,
    pub ticks: u64,
    pub state: char,
    /// When the machine started it, in ticks since it itself booted -- a
    /// reused tid is told apart from the thread that had it before the same
    /// way a reused pid is.
    pub started: u64,
    pub nice: Option<i64>,
    pub last_core: Option<u32>,
    pub switches: Option<ContextSwitches>,
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

/// The fields `sample_of` and `thread_sample_of` both read out of a `stat`
/// line: `/proc/<pid>/stat` and `/proc/<pid>/task/<tid>/stat` are the same
/// layout, just for a different id.
struct StatFields {
    id: u32,
    name: Arc<str>,
    state: char,
    parent: u32,
    ticks: u64,
    threads: u64,
    started: u64,
    nice: Option<i64>,
    last_core: Option<u32>,
}

/// The name may hold spaces and brackets -- `(a b) c` is a real name -- so the
/// fields after it are found from the *last* `)`, never by splitting the
/// whole line.
fn stat_fields(stat: &str) -> Option<StatFields> {
    let opens = stat.find('(')?;
    let closes = stat.rfind(')')?;
    let name: Arc<str> = stat.get(opens + 1..closes)?.into();
    let id: u32 = stat[..stat.find(' ')?].trim().parse().ok()?;
    let after_name: Vec<&str> = stat[closes + 1..].split_whitespace().collect();
    // After the name come: state, ppid, pgrp, ... utime is the 12th, stime the
    // 13th, counting the state as the first.
    let state: char = after_name.first()?.chars().next()?;
    let parent: u32 = after_name.get(1)?.parse().ok()?;
    let utime: u64 = after_name.get(11)?.parse().ok()?;
    let stime: u64 = after_name.get(12)?.parse().ok()?;
    let threads: u64 = after_name.get(17)?.parse().ok()?;
    let started: u64 = after_name.get(19)?.parse().ok()?;
    let nice: Option<i64> = after_name.get(16).and_then(|field| field.parse().ok());
    let last_core: Option<u32> = after_name.get(36).and_then(|field| field.parse().ok());
    Some(StatFields {
        id,
        name,
        state,
        parent,
        ticks: utime.saturating_add(stime),
        threads,
        started,
        nice,
        last_core,
    })
}

/// Reads `/proc/<pid>/stat` and `/proc/<pid>/statm` into a sample.
pub fn sample_of(stat: &str, statm: &str) -> Option<Sample> {
    let fields = stat_fields(stat)?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(Sample {
        pid: fields.id,
        parent: fields.parent,
        name: fields.name,
        ticks: fields.ticks,
        memory: pages.saturating_mul(page_size()),
        threads: fields.threads,
        state: fields.state,
        started: fields.started,
        thread_samples: Vec::new(),
    })
}

/// Reads `/proc/<pid>/task/<tid>/stat` into a thread sample. `comm` is
/// `/proc/<pid>/task/<tid>/comm`, which usually names a thread better than the
/// name in `stat`; when it has nothing to say the name from `stat` is kept.
pub fn thread_sample_of(stat: &str, comm: &str) -> Option<ThreadSample> {
    let fields = stat_fields(stat)?;
    let trimmed = comm.trim();
    let name: Arc<str> = match trimmed.is_empty() {
        true => fields.name,
        false => trimmed.into(),
    };
    Some(ThreadSample {
        tid: fields.id,
        name,
        state: fields.state,
        started: fields.started,
        ticks: fields.ticks,
        nice: fields.nice,
        last_core: fields.last_core,
        switches: None,
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

/// Which thread a reading is of, the same idea as `Who` for a thread: a tid is
/// told apart from whatever had it before by the process it belongs to and the
/// moment it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ThreadWho {
    pid: u32,
    tid: u32,
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
    /// Each thread of the tree and the time it had then, the same way `ticks`
    /// holds it for processes. Rebuilt fresh from what is currently running on
    /// every reading, so a thread that has ended is not still around to measure
    /// whatever takes its tid next.
    thread_ticks: HashMap<ThreadWho, u64>,
}

/// Sorts thread readings busiest first. A thread with no rate yet -- the
/// first reading of it -- sorts last rather than first or wherever an
/// unordered `None` would happen to land; ties are broken by tid so the order
/// does not jitter from one reading to the next.
fn sort_threads(threads: &mut [ThreadReading]) {
    threads.sort_by(|a, b| match (a.cpu, b.cpu) {
        (Some(left), Some(right)) => right
            .partial_cmp(&left)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.tid.cmp(&b.tid)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.tid.cmp(&b.tid),
    });
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
        let mut thread_ticks: HashMap<ThreadWho, u64> = HashMap::new();
        let holdings: Vec<ProcessReading> = tree
            .iter()
            .map(|sample| {
                let mut thread_readings: Vec<ThreadReading> = sample
                    .thread_samples
                    .iter()
                    .map(|thread| {
                        let who = ThreadWho {
                            pid: sample.pid,
                            tid: thread.tid,
                            started: thread.started,
                        };
                        thread_ticks.insert(who, thread.ticks);
                        let cpu = since.map(|(last, seconds)| {
                            let before = last.thread_ticks.get(&who).copied().unwrap_or(0);
                            thread.ticks.saturating_sub(before) as f32 / ticks_a_second() / seconds
                                * 100.
                        });
                        let uptime = machine_uptime.and_then(|up| {
                            up.checked_sub(seconds(thread.started as f32 / ticks_a_second())?)
                        });
                        ThreadReading {
                            tid: thread.tid,
                            name: thread.name.clone(),
                            cpu,
                            state: thread.state,
                            cpu_time: seconds(thread.ticks as f32 / ticks_a_second())
                                .unwrap_or_default(),
                            uptime,
                            nice: thread.nice,
                            last_core: thread.last_core,
                            switches: thread.switches,
                        }
                    })
                    .collect();
                sort_threads(&mut thread_readings);
                ProcessReading {
                    pid: sample.pid,
                    parent: sample.parent,
                    name: sample.name.clone(),
                    memory: sample.memory,
                    cpu: rate_of(&who_of(sample), sample.ticks),
                    threads: sample.threads,
                    state: sample.state,
                    uptime: alive_since(sample),
                    thread_readings,
                }
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
            thread_ticks,
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
            && let Some(mut sample) = sample_of(&stat, &statm)
        {
            sample.thread_samples = threads_of(sample.pid);
            samples.push(sample);
        }
    }
    match samples.is_empty() {
        true => None,
        false => Some(samples),
    }
}

/// Every thread of `pid`, read from `/proc/<pid>/task`.
///
/// A thread that vanishes between listing the directory and reading its files
/// is skipped, the same way a process is in `everything_running` -- threads
/// come and go constantly, and that is not an error worth reporting.
fn threads_of(pid: u32) -> Vec<ThreadSample> {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut threads = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.chars().all(|character| character.is_ascii_digit()) {
            continue;
        }
        let stat = fs::read_to_string(entry.path().join("stat"));
        let comm = fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        if let Ok(stat) = stat
            && let Some(mut thread) = thread_sample_of(&stat, &comm)
        {
            thread.switches = fs::read_to_string(entry.path().join("status"))
                .ok()
                .and_then(|status| context_switches_of(&status));
            threads.push(thread);
        }
    }
    threads
}

/// One process of a run, caught the moment the run is asked to stop.
///
/// The terminal ends the process group in its foreground and the shell, and
/// nothing else: a program that moved into a group or session of its own, or
/// takes its time over a polite signal, is still running when the next run
/// starts beside it. Catching the whole tree first is what lets the rest be
/// ended too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caught {
    pub pid: u32,
    started: u64,
}

/// Every process of the tree under `root`, `root` itself included.
pub fn processes_under(root: u32) -> Vec<Caught> {
    let Some(everything) = everything_running() else {
        return Vec::new();
    };
    caught_in(root, &everything)
}

fn caught_in(root: u32, everything: &[Sample]) -> Vec<Caught> {
    tree_of(root, everything)
        .into_iter()
        .map(|sample| Caught {
            pid: sample.pid,
            started: sample.started,
        })
        .collect()
}

/// Whether `stat` is still the process that was caught. A zombie has ended,
/// and a process started under the same pid since is somebody else's.
fn is_still(caught: &Caught, stat: &str) -> bool {
    stat_fields(stat).is_some_and(|fields| {
        fields.id == caught.pid && fields.started == caught.started && fields.state != 'Z'
    })
}

/// The caught processes that are still running.
pub fn still_running(caught: &[Caught]) -> Vec<Caught> {
    caught
        .iter()
        .filter(|caught| {
            fs::read_to_string(format!("/proc/{}/stat", caught.pid))
                .is_ok_and(|stat| is_still(caught, &stat))
        })
        .copied()
        .collect()
}

/// Sends `signal` to each caught process. One that has ended in the meantime
/// answers `ESRCH`, which is the outcome being asked for, so it is not reported.
#[cfg(unix)]
pub fn signal(caught: &[Caught], signal: libc::c_int) {
    for caught in caught {
        unsafe {
            libc::kill(caught.pid as libc::pid_t, signal);
        }
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

    /// A pid is only the caught process while it started at the same moment
    /// and has not ended: signalling a new process under a reused pid would
    /// end somebody else's program.
    #[test]
    fn a_caught_process_is_told_apart_from_a_new_one_under_its_pid() {
        let caught = Caught {
            pid: 4242,
            started: 12345,
        };
        let running = "4242 (server) S 99 4242 4242 0 -1 0 0 0 0 0 1 1 0 0 20 0 1 0 12345 0 0";
        let reused = "4242 (server) S 99 4242 4242 0 -1 0 0 0 0 0 1 1 0 0 20 0 1 0 99999 0 0";
        let ended = "4242 (server) Z 99 4242 4242 0 -1 0 0 0 0 0 1 1 0 0 20 0 1 0 12345 0 0";
        assert!(is_still(&caught, running));
        assert!(
            !is_still(&caught, reused),
            "a new process under the same pid"
        );
        assert!(!is_still(&caught, ended), "a zombie has already ended");
    }

    #[test]
    fn the_whole_tree_is_caught_and_nothing_beside_it() {
        let everything = vec![
            started_at(10, 1, 0, 0, 7),
            started_at(11, 10, 0, 0, 8),
            started_at(12, 11, 0, 0, 9),
            started_at(20, 1, 0, 0, 5),
        ];
        let caught = caught_in(10, &everything);
        assert_eq!(
            caught.iter().map(|caught| caught.pid).collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
        assert_eq!(caught[2].started, 9);
    }

    /// The case the terminal alone gets wrong: a program started in a session
    /// of its own outlives the end of the process that started it, and is only
    /// ended because it was caught beforehand.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_program_in_a_session_of_its_own_is_caught_and_ended() {
        let mut starter = smol::process::Command::new("sh")
            .args(["-c", "setsid sleep 120 & wait"])
            .spawn()
            .expect("sh starts");
        let root = starter.id();
        let mut caught = Vec::new();
        for _ in 0..200 {
            caught = processes_under(root);
            if caught.len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let own_session: Vec<Caught> = caught
            .iter()
            .filter(|caught| caught.pid != root)
            .copied()
            .collect();
        assert!(
            !own_session.is_empty(),
            "the program started by the shell is caught with it: {caught:?}"
        );

        starter.kill().expect("the shell is ended");
        smol::block_on(starter.status()).expect("the shell is reaped");
        assert_eq!(
            still_running(&own_session),
            own_session,
            "ending the shell alone leaves its program running"
        );

        signal(&own_session, libc::SIGKILL);
        let mut left = own_session;
        for _ in 0..200 {
            left = still_running(&left);
            if left.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(left.is_empty(), "the caught program is ended: {left:?}");
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
            thread_samples: Vec::new(),
        }
    }

    fn a_thread(tid: u32, ticks: u64, started: u64) -> ThreadSample {
        ThreadSample {
            tid,
            name: "a thread".into(),
            ticks,
            state: 'R',
            started,
            nice: None,
            last_core: None,
            switches: None,
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

    /// A thread name with spaces and brackets parses the same way a process
    /// name does; `comm`, when it has something to say, wins over it.
    #[test]
    fn a_threads_switches_are_read_from_its_status() {
        let status = "Name:\tworker\nState:\tS (sleeping)\n\
                      voluntary_ctxt_switches:\t1520\n\
                      nonvoluntary_ctxt_switches:\t37\n";
        assert_eq!(
            context_switches_of(status),
            Some(ContextSwitches {
                voluntary: 1520,
                involuntary: 37
            })
        );
        assert_eq!(context_switches_of("Name:\tworker\n"), None);
    }

    #[test]
    fn a_thread_name_with_spaces_parses() {
        let stat = "77 (worker one (2)) R 10 10 10 0 -1 0 100 0 0 0 5 3 0 0 20 0 3 0 999 0 0";
        let thread = thread_sample_of(stat, "").expect("the line reads");
        assert_eq!(thread.tid, 77);
        assert_eq!(
            &*thread.name, "worker one (2)",
            "the name from stat when comm has nothing"
        );
        assert_eq!(thread.state, 'R');
        assert_eq!(thread.ticks, 8, "user and system time together");
        assert_eq!(thread.started, 999);

        let named = thread_sample_of(stat, "short name\n").expect("the line reads");
        assert_eq!(
            &*named.name, "short name",
            "comm wins, and its newline is trimmed"
        );
    }

    /// A rate for a thread needs two readings of it, the same as a process.
    #[test]
    fn a_thread_rate_is_computed_from_two_readings() {
        let mut watcher = Watcher::default();
        let at = Instant::now();
        let mut root = a_sample(10, 1, 0, 1_000);
        root.thread_samples = vec![a_thread(11, 100, 5)];
        watcher.metrics_of(10, &[a_sample(1, 0, 0, 0), root.clone()], at, None);

        let ticks = ticks_a_second() as u64;
        let mut busier = root;
        busier.thread_samples = vec![a_thread(11, 100 + ticks / 2, 5)];
        let read = watcher
            .metrics_of(
                10,
                &[a_sample(1, 0, 0, 0), busier],
                at + Duration::from_secs(1),
                None,
            )
            .expect("the run is still going");

        let process = read
            .tree
            .iter()
            .find(|one| one.pid == 10)
            .expect("the root");
        let thread = process.thread_readings.first().expect("the thread");
        let cpu = thread.cpu.expect("the thread was measured");
        assert!((cpu - 50.).abs() < 1., "half a second of a core: {cpu}");
    }

    /// The first reading of a thread has nothing to measure a rate against.
    #[test]
    fn the_first_reading_gives_no_thread_a_rate() {
        let mut watcher = Watcher::default();
        let mut root = a_sample(10, 1, 0, 1_000);
        root.thread_samples = vec![a_thread(11, 100, 5)];
        let read = watcher
            .metrics_of(10, &[a_sample(1, 0, 0, 0), root], Instant::now(), None)
            .expect("the run was found");
        let process = read
            .tree
            .iter()
            .find(|one| one.pid == 10)
            .expect("the root");
        assert!(
            process
                .thread_readings
                .iter()
                .all(|thread| thread.cpu.is_none())
        );
    }

    /// Tids come back too. A different thread under the same number must not be
    /// measured against the processor time of the one that had it before.
    #[test]
    fn a_reused_tid_is_not_measured_against_the_old_thread() {
        let mut watcher = Watcher::default();
        let at = Instant::now();
        let mut busy = a_sample(10, 1, 0, 1_000);
        busy.thread_samples = vec![a_thread(11, 10_000, 5)];
        watcher.metrics_of(10, &[a_sample(1, 0, 0, 0), busy], at, None);

        // A different thread altogether takes the same tid, started later, with
        // far less time behind it.
        let ticks = ticks_a_second() as u64;
        let mut fresh = a_sample(10, 1, 0, 1_000);
        fresh.thread_samples = vec![a_thread(11, ticks, 900)];
        let read = watcher
            .metrics_of(
                10,
                &[a_sample(1, 0, 0, 0), fresh],
                at + Duration::from_secs(1),
                None,
            )
            .expect("the run is still going");

        let process = read
            .tree
            .iter()
            .find(|one| one.pid == 10)
            .expect("the root");
        let thread = process.thread_readings.first().expect("the thread");
        let cpu = thread
            .cpu
            .expect("a new thread is measured from when it appeared");
        assert!(
            (cpu - 100.).abs() < 1.,
            "a whole core's worth of its own ticks, not measured against the old thread's 10,000: {cpu}"
        );
    }

    /// A thread that is gone for a reading is not still around to lend its old
    /// ticks to whatever takes its tid next.
    #[test]
    fn a_gone_threads_baseline_is_forgotten() {
        let mut watcher = Watcher::default();
        let at = Instant::now();
        let mut root = a_sample(10, 1, 0, 1_000);
        root.thread_samples = vec![a_thread(11, 1_000, 5)];
        watcher.metrics_of(10, &[a_sample(1, 0, 0, 0), root], at, None);

        // The thread is gone for one reading.
        let empty_root = a_sample(10, 1, 0, 1_000);
        watcher.metrics_of(
            10,
            &[a_sample(1, 0, 0, 0), empty_root],
            at + Duration::from_secs(1),
            None,
        );

        // The same tid and start come back. If its old baseline had lingered,
        // this would be measured against the 1,000 ticks from the first
        // reading rather than as a thread appearing fresh.
        let ticks = ticks_a_second() as u64;
        let mut back = a_sample(10, 1, 0, 1_000);
        back.thread_samples = vec![a_thread(11, ticks, 5)];
        let read = watcher
            .metrics_of(
                10,
                &[a_sample(1, 0, 0, 0), back],
                at + Duration::from_secs(2),
                None,
            )
            .expect("the run is still going");

        let process = read
            .tree
            .iter()
            .find(|one| one.pid == 10)
            .expect("the root");
        let thread = process.thread_readings.first().expect("the thread");
        let cpu = thread
            .cpu
            .expect("a thread reappearing is measured from zero, not skipped");
        assert!(
            (cpu - 100.).abs() < 1.,
            "measured against zero, not against the 1,000 ticks the old baseline had: {cpu}"
        );
    }

    /// A build is busy on one thread and idle on another; the busiest one is
    /// what a reader wants to see first.
    #[test]
    fn thread_readings_are_ordered_busiest_first() {
        let mut watcher = Watcher::default();
        let at = Instant::now();
        let mut root = a_sample(10, 1, 0, 1_000);
        root.thread_samples = vec![a_thread(20, 0, 5), a_thread(21, 0, 5), a_thread(22, 0, 5)];
        watcher.metrics_of(10, &[a_sample(1, 0, 0, 0), root], at, None);

        let ticks = ticks_a_second() as u64;
        let mut busier = a_sample(10, 1, 0, 1_000);
        busier.thread_samples = vec![
            a_thread(20, ticks / 4, 5),
            a_thread(21, ticks / 2, 5),
            a_thread(22, 0, 5),
        ];
        let read = watcher
            .metrics_of(
                10,
                &[a_sample(1, 0, 0, 0), busier],
                at + Duration::from_secs(1),
                None,
            )
            .expect("the run is still going");

        let process = read
            .tree
            .iter()
            .find(|one| one.pid == 10)
            .expect("the root");
        let order: Vec<u32> = process
            .thread_readings
            .iter()
            .map(|thread| thread.tid)
            .collect();
        assert_eq!(
            order,
            vec![21, 20, 22],
            "busiest first, an idle thread last"
        );
    }

    /// With no rate yet for any thread, tid is the only thing left to order by.
    #[test]
    fn threads_with_no_rate_yet_sort_last_by_tid() {
        let mut watcher = Watcher::default();
        let mut root = a_sample(10, 1, 0, 1_000);
        root.thread_samples = vec![a_thread(30, 0, 5), a_thread(10, 0, 5), a_thread(20, 0, 5)];
        let read = watcher
            .metrics_of(10, &[a_sample(1, 0, 0, 0), root], Instant::now(), None)
            .expect("the run was found");
        let process = read
            .tree
            .iter()
            .find(|one| one.pid == 10)
            .expect("the root");
        let order: Vec<u32> = process
            .thread_readings
            .iter()
            .map(|thread| thread.tid)
            .collect();
        assert_eq!(
            order,
            vec![10, 20, 30],
            "no rate yet anywhere, so tid breaks every tie"
        );
    }

    /// The reading is done against a real machine here, not a fixture: this
    /// editor's own process has threads of its own to find.
    #[test]
    fn this_very_process_can_have_its_threads_read() {
        // A machine with no /proc has nothing to say, which is its own answer.
        if everything_running().is_none() {
            return;
        }
        let threads = threads_of(std::process::id());
        assert!(
            !threads.is_empty(),
            "a running process has at least one thread"
        );
    }
}
