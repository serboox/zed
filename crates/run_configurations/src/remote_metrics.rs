use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use gpui::BackgroundExecutor;

use crate::over_ssh::{RemoteRun, quoted};
use crate::process_metrics::{Sample, sample_with_page_size, ticks_a_second};

/// How long the far side has to answer before its run is measured locally for that tick.
const GIVEN_TO_ANSWER: Duration = Duration::from_secs(2);

const PROCESSES: &str = "#processes";
const MARKED: &str = "#marked";

/// What the far side says about one run.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteReading {
    pub root: u32,
    pub samples: Vec<Sample>,
    pub uptime: Option<Duration>,
}

/// The script the far side runs, in POSIX `sh` and without forking per process. The run
/// is found by the token in its processes' environment, and everything below them is
/// taken along. A process keeps the environment it started with, so the token is on what
/// the run started, not on its shell. Only the run is sent back.
fn collector(token: &str) -> String {
    format!(
        r##"read -r up _ < /proc/uptime; echo "#uptime $up"
pagesize=$(getconf PAGESIZE 2>/dev/null)
if [ -z "$pagesize" ]; then
  while read -r key value _; do
    if [ "$key" = "KernelPageSize:" ]; then pagesize=$((value * 1024)); break; fi
  done < /proc/self/smaps
fi
echo "#pagesize $pagesize"
echo "#clktck $(getconf CLK_TCK 2>/dev/null)"
echo '{MARKED}'
marked=$(grep -lsxzF "ZED_REMOTE_RUN={token}" /proc/[0-9]*/environ)
echo "$marked"
echo '{PROCESSES}'
members=" "
for f in $marked; do p=${{f#/proc/}}; members="$members${{p%/environ}} "; done
parents=""
for p in /proc/[0-9]*; do
  read -r s < "$p/stat" 2>/dev/null || continue
  rest=${{s##*) }}; set -- $rest; parents="$parents ${{p#/proc/}}:$2"
done
changed=1
while [ "$changed" = 1 ]; do
  changed=0
  for entry in $parents; do
    pid=${{entry%%:*}}; parent=${{entry##*:}}
    case "$members" in *" $pid "*) continue ;; esac
    case "$members" in *" $parent "*) members="$members$pid "; changed=1 ;; esac
  done
done
for pid in $members; do
  {{ read -r s < /proc/$pid/stat && read -r m < /proc/$pid/statm; }} 2>/dev/null || continue
  printf '@%s\n%s\n%s\n' "$pid" "$s" "$m"
done"##
    )
}

/// Reads what the collector printed. Nothing comes back when no process of the run is
/// there, because a number then would be a guess.
pub fn parse(output: &str) -> Option<RemoteReading> {
    let mut uptime = None;
    let mut page_size = None;
    let mut remote_ticks_a_second = None;
    let mut marked = HashSet::<u32>::new();
    let mut unread = Vec::new();
    let mut in_marked = false;
    let mut lines = output.lines();
    while let Some(line) = lines.next() {
        if let Some(seconds) = line.strip_prefix("#uptime ") {
            uptime = seconds
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
        } else if let Some(size) = line.strip_prefix("#pagesize ") {
            page_size = size.trim().parse::<u64>().ok().filter(|size| *size > 0);
        } else if let Some(rate) = line.strip_prefix("#clktck ") {
            remote_ticks_a_second = rate.trim().parse::<u64>().ok().filter(|rate| *rate > 0);
        } else if line == MARKED {
            in_marked = true;
        } else if line == PROCESSES {
            in_marked = false;
        } else if in_marked {
            let pid = line
                .strip_prefix("/proc/")
                .and_then(|rest| rest.strip_suffix("/environ"))
                .and_then(|pid| pid.parse::<u32>().ok());
            marked.extend(pid);
        } else if let Some(pid) = line.strip_prefix('@') {
            let (Some(stat), Some(statm)) = (lines.next(), lines.next()) else {
                break;
            };
            unread.push((pid, stat, statm));
        }
    }
    // Memory in pages of a size nobody said would be a guess, so there is none.
    let page_size = page_size?;
    let local_ticks_a_second = ticks_a_second() as f64;
    let samples: Vec<Sample> = unread
        .into_iter()
        .filter_map(|(pid, stat, statm)| {
            let mut sample = sample_with_page_size(stat, statm, page_size)?;
            if pid.parse() != Ok(sample.pid) {
                return None;
            }
            // Ticks are a fact about the machine that counted them. One that
            // did not say how long a tick is counts them the way this one does.
            if let Some(remote) = remote_ticks_a_second {
                let scale =
                    |ticks: u64| (ticks as f64 * local_ticks_a_second / remote as f64) as u64;
                sample.ticks = scale(sample.ticks);
                sample.started = scale(sample.started);
            }
            Some(sample)
        })
        .collect();
    let known: HashSet<u32> = samples.iter().map(|sample| sample.pid).collect();
    let root = samples
        .iter()
        .filter(|sample| marked.contains(&sample.pid) && !marked.contains(&sample.parent))
        .map(|sample| sample.pid)
        .min()
        .or_else(|| {
            marked
                .iter()
                .copied()
                .filter(|pid| known.contains(pid))
                .min()
        })?;
    Some(RemoteReading {
        root,
        samples,
        uptime,
    })
}

/// Asks the far side about `run` over the connection its own `ssh` opened. Nothing comes
/// back when that connection is gone or does not answer in time.
pub async fn read(run: &RemoteRun, executor: &BackgroundExecutor) -> Option<RemoteReading> {
    read_within(run, executor.timer(GIVEN_TO_ANSWER)).await
}

/// [`read`] with its own clock, so tests of real processes need not use the test executor's.
async fn read_within(
    run: &RemoteRun,
    time_is_up: impl Future<Output = ()>,
) -> Option<RemoteReading> {
    if !run.control_path.exists() {
        return None;
    }
    let mut command = smol::process::Command::new(&run.program);
    command
        .arg("-o")
        .arg(format!("ControlPath={}", run.control_path.display()))
        .args(["-o", "ControlMaster=no", "-o", "BatchMode=yes"])
        .args(["-o", "ConnectTimeout=3"]);
    if let Some(port) = run.port {
        command.arg("-p").arg(port.to_string());
    }
    command
        .arg(&run.destination)
        .arg("--")
        .arg(format!("sh -c {}", quoted(&collector(&run.token))))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = smol::future::or(async { command.output().await.ok() }, async {
        time_is_up.await;
        None
    })
    .await?;
    if !output.status.success() {
        return None;
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

/// Does here what ssh does on the far side: drops the options up to `--` and runs the
/// rest through a shell.
#[cfg(test)]
pub(crate) fn a_stand_in_for_ssh(directory: &std::path::Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join("ssh");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.arguments\"\nwhile [ \"$#\" -gt 0 ] && [ \"$1\" != \"--\" ]; do shift; done\nshift\nexec sh -c \"$*\"\n",
    )
    .expect("the stand-in is written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("and can be run");
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
pub(crate) fn a_run_over(program: String, directory: &std::path::Path, token: &str) -> RemoteRun {
    let control_path = directory.join(format!("zed-ssh-{token}"));
    std::fs::write(&control_path, "").expect("the connection is there");
    RemoteRun {
        program,
        destination: "deploy@build.example.com".into(),
        port: Some(2222),
        control_path,
        token: token.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "{pid} ({name}) S {parent} 1 1 0 -1 4194560 100 0 0 0 {ticks} 0 0 0 20 0 1 0 {started} 1000000 100 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0";

    fn stat(pid: u32, name: &str, parent: u32, ticks: u64) -> String {
        STAT.replace("{pid}", &pid.to_string())
            .replace("{name}", name)
            .replace("{parent}", &parent.to_string())
            .replace("{ticks}", &ticks.to_string())
            .replace("{started}", &(pid as u64 * 10).to_string())
    }

    fn process(pid: u32, name: &str, parent: u32, ticks: u64, pages: u64) -> String {
        format!(
            "@{pid}\n{}\n{} {pages} 0 0 0 0 0\n",
            stat(pid, name, parent, ticks),
            pages * 2
        )
    }

    fn output(marked: &[u32], processes: &[String]) -> String {
        let mut output = "#uptime 5000.25\n#pagesize 65536\n#marked\n".to_string();
        for pid in marked {
            output.push_str(&format!("/proc/{pid}/environ\n"));
        }
        output.push_str("#processes\n");
        for process in processes {
            output.push_str(process);
        }
        output
    }

    /// The root is the run's process whose parent is not one of the run's.
    #[test]
    fn the_root_is_the_run_process_whose_parent_is_not_in_the_run() {
        let reading = parse(&output(
            &[5000, 301, 302],
            &[
                process(1, "systemd", 0, 10, 10),
                process(200, "sshd", 1, 10, 10),
                process(5000, "bash", 200, 5, 50),
                process(301, "api", 5000, 50, 100),
                process(302, "worker", 301, 30, 200),
                process(400, "other", 1, 999, 999),
            ],
        ))
        .expect("the run is there");
        assert_eq!(
            reading.root, 5000,
            "a process numbered above its children is still their root"
        );
        assert_eq!(reading.samples.len(), 6);
        assert_eq!(reading.uptime, Some(Duration::from_secs_f64(5000.25)));
    }

    /// A page is what the far side says it is.
    #[test]
    fn memory_is_counted_in_the_page_of_the_machine_that_counted() {
        let reading = parse(&output(&[300], &[process(300, "api", 1, 5, 100)])).expect("there");
        assert_eq!(reading.samples[0].memory, 100 * 65536);
    }

    /// No marked process is no run, and no number is better than a guess.
    #[test]
    fn a_run_nobody_carries_the_token_of_is_not_measured() {
        assert_eq!(parse(&output(&[], &[process(300, "api", 1, 5, 100)])), None);
        assert_eq!(
            parse(&output(&[999], &[process(300, "api", 1, 5, 100)])),
            None,
            "a marked process the machine then had nothing to say about is gone"
        );
        assert_eq!(parse(""), None);
    }

    /// A number of pages of a size nobody said would be a guess.
    #[test]
    fn a_reading_with_no_page_size_is_not_measured() {
        let without = output(&[300], &[process(300, "api", 1, 5, 100)])
            .replace("#pagesize 65536\n", "#pagesize \n");
        assert_eq!(parse(&without), None);
    }

    /// A line the far side garbles must not bring the poll down, however large
    /// the number in it.
    #[test]
    fn an_absurd_uptime_is_no_uptime_and_no_panic() {
        let text = output(&[300], &[process(300, "api", 1, 5, 100)])
            .replace("#uptime 5000.25", "#uptime 1e100");
        let reading = parse(&text).expect("the run is there");
        assert_eq!(reading.uptime, None);
    }

    /// Ticks are counted in the far side's tick, and are given in this one's.
    #[test]
    fn ticks_are_given_in_the_tick_of_this_machine() {
        let same = output(&[300], &[process(300, "api", 1, 1000, 100)]);
        let local = parse(&same).expect("there").samples[0].ticks;
        assert_eq!(local, 1000, "no rate said: the same tick");
        let twice_as_fast = same.replace(
            "#pagesize 65536\n",
            &format!("#pagesize 65536\n#clktck {}\n", ticks_a_second() as u64 * 2),
        );
        let scaled = parse(&twice_as_fast).expect("there").samples[0].ticks;
        assert_eq!(
            scaled, 500,
            "a machine that ticks twice as often counted twice as many"
        );
    }

    /// A garbled line costs that process and no other.
    #[test]
    fn a_garbled_process_costs_only_itself() {
        let mut text = output(&[300], &[process(300, "api", 1, 5, 100)]);
        text.push_str("@301\nnot a stat line\n1 2 3\n");
        text.push_str(&process(302, "after", 300, 1, 1));
        let reading = parse(&text).expect("the run is still there");
        assert_eq!(reading.root, 300);
        assert!(reading.samples.iter().any(|sample| sample.pid == 302));
        assert!(!reading.samples.iter().any(|sample| sample.pid == 301));
    }

    /// The collector, run for real against this machine's `/proc`, finds what
    /// carries its token and everything below it, and sends back nothing else.
    #[test]
    fn the_collector_sends_the_run_and_what_it_started_and_nothing_else() {
        let token = "abc123tok456";
        // The shell carries the token, and starts a program that has dropped it.
        let mut child = smol::process::Command::new("sh")
            .arg("-c")
            .arg("env -u ZED_REMOTE_RUN sleep 6 & wait")
            .env("ZED_REMOTE_RUN", token)
            .spawn()
            .expect("a run carrying the token");
        std::thread::sleep(Duration::from_millis(300));

        let ask = |token: &str| {
            smol::block_on(
                smol::process::Command::new("sh")
                    .arg("-c")
                    .arg(collector(token))
                    .output(),
            )
            .expect("the collector runs")
        };
        let answered = ask(token);
        let other = ask("someothertoken");
        let carrier = child.id();
        let reading = parse(&String::from_utf8_lossy(&answered.stdout));
        // What the run started is ended by the number the collector gave it; a
        // program the collector did not find ends by itself within seconds.
        for sample in reading.iter().flat_map(|reading| &reading.samples) {
            if &*sample.name == "sleep" {
                unsafe { libc::kill(sample.pid as i32, libc::SIGKILL) };
            }
        }
        child.kill().ok();
        smol::block_on(child.status()).ok();

        assert!(answered.status.success());
        let reading = reading.expect("the token is carried");
        assert_eq!(reading.root, carrier);
        let names: Vec<&str> = reading
            .samples
            .iter()
            .map(|sample| &*sample.name as &str)
            .collect();
        assert!(
            names.contains(&"sleep"),
            "what the run started is taken along although it dropped the token: {names:?}"
        );
        assert!(
            reading.samples.len() <= 4,
            "only the run is sent, not the machine: {names:?}"
        );
        assert!(
            !reading
                .samples
                .iter()
                .any(|sample| sample.pid == std::process::id()),
            "nothing beside the run is in it"
        );
        assert!(reading.uptime.is_some());
        assert_eq!(
            parse(&String::from_utf8_lossy(&other.stdout)),
            None,
            "a token nobody carries finds nothing"
        );
    }

    /// The whole question, asked the way the editor asks it: a command to a
    /// machine, its answer read back. Only the machine is this one.
    #[test]
    fn a_run_is_read_over_the_connection_its_ssh_opened() {
        let directory = tempfile::tempdir().expect("a directory");
        let program = a_stand_in_for_ssh(directory.path());
        let token = "readoverconn";
        let run = a_run_over(program, directory.path(), token);
        let mut child = smol::process::Command::new("sleep")
            .arg("30")
            .env("ZED_REMOTE_RUN", token)
            .spawn()
            .expect("a run carrying the token");

        let reading = smol::block_on(read_within(&run, std::future::pending()));
        child.kill().ok();
        smol::block_on(child.status()).ok();

        let reading = reading.expect("the far side answered");
        assert_eq!(reading.root, child.id());
        assert!(
            reading
                .samples
                .iter()
                .any(|sample| sample.pid == child.id())
        );
        assert!(reading.uptime.is_some());

        let asked = std::fs::read_to_string(format!("{}.arguments", run.program)).expect("ssh ran");
        let asked: Vec<&str> = asked.lines().collect();
        for wanted in [
            format!("ControlPath={}", run.control_path.display()).as_str(),
            "ControlMaster=no",
            "BatchMode=yes",
            "deploy@build.example.com",
            "2222",
        ] {
            assert!(
                asked.contains(&wanted),
                "{wanted} is asked of ssh: {asked:?}"
            );
        }
        assert!(
            asked.iter().any(|line| line.starts_with("sh -c '")),
            "the script goes through `sh`, whatever shell the far side logs in with"
        );
    }

    /// A connection that is gone is a run that is over: nothing is asked, and
    /// nothing is logged in to instead.
    #[test]
    fn a_connection_that_is_gone_is_not_asked_and_not_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("a directory");
        let marker = directory.path().join("was-called");
        let program = directory.path().join("ssh");
        std::fs::write(&program, format!("#!/bin/sh\ntouch {}\n", marker.display()))
            .expect("written");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("set");
        let mut run = a_run_over(
            program.to_string_lossy().into_owned(),
            directory.path(),
            "goneaway",
        );
        std::fs::remove_file(&run.control_path).expect("the connection ends with the run");
        run.port = None;

        assert_eq!(
            smol::block_on(read_within(&run, std::future::pending())),
            None
        );
        assert!(!marker.exists(), "ssh was not started at all");
    }

    /// An answer that comes too late is no answer: the run is measured by its
    /// local client for that tick rather than holding every other run up.
    #[test]
    fn a_far_side_that_does_not_answer_in_time_is_given_up_on() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("a directory");
        let program = directory.path().join("ssh");
        std::fs::write(&program, "#!/bin/sh\nexec sleep 20\n").expect("written");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("set");
        let run = a_run_over(
            program.to_string_lossy().into_owned(),
            directory.path(),
            "slowanswer",
        );

        let started = std::time::Instant::now();
        let brief = smol::unblock(|| std::thread::sleep(Duration::from_millis(300)));
        assert_eq!(smol::block_on(read_within(&run, brief)), None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
