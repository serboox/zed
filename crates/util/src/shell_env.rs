use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use collections::HashMap;
use serde::Deserialize;

use crate::shell::ShellKind;

fn parse_env_map_from_noisy_output(output: &str) -> Result<collections::HashMap<String, String>> {
    for (position, _) in output.match_indices('{') {
        let candidate = &output[position..];
        let mut deserializer = serde_json::Deserializer::from_str(candidate);
        if let Ok(env_map) = HashMap::<String, String>::deserialize(&mut deserializer) {
            return Ok(env_map);
        }
    }
    anyhow::bail!("Failed to find JSON in shell output: {output}")
}

pub fn print_env() {
    let env_vars: HashMap<String, String> = std::env::vars().collect();
    let json = serde_json::to_string_pretty(&env_vars).unwrap_or_else(|err| {
        eprintln!("Error serializing environment variables: {}", err);
        std::process::exit(1);
    });
    println!("{}", json);
}

/// How long a shell should be given to print the environment before it is
/// given up on.
///
/// A login shell carrying a version manager or two takes a second, and a cold
/// one on a slow disk several, so this is generous. What it defends against is
/// not slowness but a shell that never finishes at all: one whose rc file
/// loops, or -- far more common -- one that prints everything asked of it and
/// then does not exit, or leaves a background process holding the pipe. Eleven
/// of those, one per directory of a restored session, once left an editor
/// reporting "Restoring terminals…" for as long as it was left running.
///
/// It is only the number. The waiting itself belongs to the caller, which is
/// why [`capture`] takes a future rather than a duration: a timer made here
/// would be one no test could control, and the callers all hold an executor
/// whose timers a test can move at will.
pub const NO_LONGER_THAN: Duration = Duration::from_secs(15);

/// Capture all environment variables from the login shell in the given
/// directory, giving up on the shell if `give_up_on_it` finishes first.
///
/// Giving up is not the same as failing. Everything the shell printed before
/// then is still read, so a shell that printed a whole environment and then
/// failed to exit is believed -- which is the difference between a terminal
/// that opens a few seconds late and one that never opens.
pub async fn capture(
    shell_path: impl AsRef<Path>,
    args: &[String],
    directory: impl AsRef<Path>,
    give_up_on_it: impl Future<Output = ()>,
) -> Result<collections::HashMap<String, String>> {
    #[cfg(windows)]
    return capture_windows(shell_path.as_ref(), args, directory.as_ref(), give_up_on_it).await;
    #[cfg(unix)]
    return capture_unix(shell_path.as_ref(), args, directory.as_ref(), give_up_on_it).await;
}

/// Try to parse the environment output before checking the exit status.
/// The user's shell rc files may contain commands that fail (e.g. editor
/// integrations that call posix_spawnp outside a real PTY), causing a
/// non-zero exit status even though `zed --printenv` ran successfully and
/// produced valid output on its separate fd.
fn parse_env_output(
    env_output: &str,
    status: &std::process::ExitStatus,
    successful_capture_warning: impl FnOnce() -> String,
    failed_capture_error: impl FnOnce() -> String,
) -> Result<collections::HashMap<String, String>> {
    match parse_env_map_from_noisy_output(env_output) {
        Ok(env_map) => {
            if !status.success() {
                log::warn!("{}", successful_capture_warning());
            }
            Ok(env_map)
        }
        Err(parse_error) => {
            if !status.success() {
                anyhow::bail!(
                    "{}. Failed to deserialize environment variables from json: {parse_error}. output: {env_output}",
                    failed_capture_error(),
                );
            }

            anyhow::bail!(
                "Failed to deserialize environment variables from json: {parse_error}. output: {env_output}"
            );
        }
    }
}

#[cfg(unix)]
async fn capture_unix(
    shell_path: &Path,
    args: &[String],
    directory: &Path,
    give_up_on_it: impl Future<Output = ()>,
) -> Result<collections::HashMap<String, String>> {
    use std::os::unix::process::CommandExt;

    use crate::command::new_std_command;

    let shell_kind = ShellKind::new(shell_path, false);
    let quoted_zed_path = super::get_shell_safe_zed_path(shell_kind)?;

    let mut command_string = String::new();
    let mut command = new_std_command(shell_path);
    command.args(args);
    // In some shells, file descriptors greater than 2 cannot be used in interactive mode,
    // so file descriptor 0 (stdin) is used instead. This impacts zsh, old bash; perhaps others.
    // See: https://github.com/zed-industries/zed/pull/32136#issuecomment-2999645482
    const FD_STDIN: std::os::fd::RawFd = 0;
    const FD_STDOUT: std::os::fd::RawFd = 1;
    const FD_STDERR: std::os::fd::RawFd = 2;

    let (fd_num, redir) = match shell_kind {
        ShellKind::Rc => (FD_STDIN, format!(">[1={}]", FD_STDIN)), // `[1=0]`
        ShellKind::Nushell | ShellKind::Tcsh => (FD_STDOUT, "".to_string()),
        // xonsh doesn't support redirecting to stdin, and control sequences are printed to
        // stdout on startup
        ShellKind::Xonsh => (FD_STDERR, "o>e".to_string()),
        ShellKind::PowerShell => (FD_STDIN, format!(">{}", FD_STDIN)),
        _ => (FD_STDIN, format!(">&{}", FD_STDIN)), // `>&0`
    };

    match shell_kind {
        ShellKind::Csh | ShellKind::Tcsh => {
            // For csh/tcsh, login shell requires passing `-` as 0th argument (instead of `-l`)
            command.arg0("-");
        }
        ShellKind::Fish => {
            // in fish, asdf, direnv attach to the `fish_prompt` event
            command_string.push_str("emit fish_prompt;");
            command.arg("-l");
        }
        _ => {
            command.arg("-l");
        }
    }

    match shell_kind {
        // Nushell does not allow non-interactive login shells.
        // Instead of doing "-l -i -c '<command>'"
        // use "-l -e '<command>; exit'" instead
        ShellKind::Nushell => command.arg("-e"),
        _ => command.args(["-i", "-c"]),
    };

    // Prefix with "./" if the path starts with "-" to prevent cd from interpreting it as a flag
    let dir_str = directory.to_string_lossy();
    let dir_str = if dir_str.starts_with('-') {
        format!("./{dir_str}").into()
    } else {
        dir_str
    };
    let quoted_dir = shell_kind
        .try_quote(&dir_str)
        .context("unexpected null in directory name")?;

    // cd into the directory, triggering directory specific side-effects (asdf, direnv, etc)
    command_string.push_str(&format!("cd {};", quoted_dir));
    if let Some(prefix) = shell_kind.command_prefix() {
        command_string.push(prefix);
    }
    command_string.push_str(&format!("{} --printenv {}", quoted_zed_path, redir));

    if let ShellKind::Nushell = shell_kind {
        command_string.push_str("; exit");
    }

    command.arg(&command_string);

    super::set_pre_exec_to_start_new_session(&mut command);

    let (env_output, process_output) = spawn_and_read_fd(command, fd_num, give_up_on_it)
        .await
        .with_context(|| format!("running a login shell in {}", directory.display()))?;
    let env_output = String::from_utf8_lossy(&env_output);

    parse_env_output(
        &env_output,
        &process_output.status,
        || {
            format!(
                "login shell exited with {} but environment was captured successfully. stderr: {:?}",
                process_output.status,
                String::from_utf8_lossy(&process_output.stderr),
            )
        },
        || {
            format!(
                "login shell exited with {}. stdout: {:?}, stderr: {:?}",
                process_output.status,
                String::from_utf8_lossy(&process_output.stdout),
                String::from_utf8_lossy(&process_output.stderr),
            )
        },
    )
}

/// Runs the shell and reads what it wrote to `child_fd`, giving up on it if
/// `give_up_on_it` finishes first.
///
/// Nothing here waits on anything the shell controls once the bound is up, and
/// that is the whole design. The obvious shape -- read to the end of the pipe,
/// then collect the process -- waits on the shell twice over, because both the
/// read and collecting the output end only when every write end of every pipe
/// is closed. A shell that prints a whole environment and then does not exit
/// closes none of them, and neither does one that exits leaving a background
/// process behind, so either waits forever. So the bytes are gathered as they
/// arrive into a buffer this function holds, and giving up means taking what is
/// in that buffer: a shell that printed a whole environment is still believed,
/// which is the difference between a terminal that opens a few seconds late and
/// one that never opens.
///
/// The read is also moved off this thread. It is a blocking read, and a
/// blocking read cannot be cancelled: a future sitting inside one is not at an
/// await point, so dropping it does nothing at all and the thread it occupies
/// is lost until the pipe closes. Off this thread that costs a thread from the
/// pool that exists for exactly such reads; on it, it cost the editor one of
/// the few threads that run everything else.
#[cfg(unix)]
async fn spawn_and_read_fd(
    mut command: std::process::Command,
    child_fd: std::os::fd::RawFd,
    give_up_on_it: impl Future<Output = ()>,
) -> anyhow::Result<(Vec<u8>, std::process::Output)> {
    use command_fds::{CommandFdExt, FdMapping};
    use futures::AsyncReadExt as _;
    use futures::future::Either;
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};

    let (reader, writer) = std::io::pipe()?;

    command.fd_mappings(vec![FdMapping {
        parent_fd: writer.into(),
        child_fd,
    }])?;

    let mut process = smol::process::Command::from(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Written to as it arrives rather than returned at the end, so that giving
    // up on the read still leaves what the shell had already printed.
    let printed = Arc::new(Mutex::new(Vec::new()));
    let reading = {
        let printed = printed.clone();
        let mut reader = smol::Unblock::new(reader);
        async move {
            let mut chunk = [0u8; 8192];
            loop {
                let read = reader.read(&mut chunk).await?;
                if read == 0 {
                    return anyhow::Ok(());
                }
                match printed.lock() {
                    Ok(mut printed) => printed.extend_from_slice(&chunk[..read]),
                    // Nothing here panics while holding the lock, so this is
                    // unreachable; dropping the rest is still better than
                    // unwrapping into a panic on a background thread.
                    Err(_) => return anyhow::Ok(()),
                }
            }
        }
    };

    // Kept rather than consumed, because both waits below are on the same
    // patience: `select` hands the unfinished future back, so what is left of
    // it bounds the second wait too.
    let mut give_up_on_it = Box::pin(give_up_on_it);

    let gave_up =
        match futures::future::select(std::pin::pin!(reading), give_up_on_it.as_mut()).await {
            Either::Left((read, _)) => {
                read?;
                false
            }
            Either::Right(_) => {
                log::warn!("the shell did not finish in time; using whatever it printed");
                stop_it(&mut process);
                true
            }
        };

    let buffer = match printed.lock() {
        Ok(printed) => printed.clone(),
        Err(_) => Vec::new(),
    };

    if gave_up {
        // Nothing more is waited on once the patience is gone -- and the
        // patience itself must not be asked again, since a future that has
        // finished may not be polled a second time.
        return Ok((buffer, stopped_status()));
    }

    // Collecting the output is bounded by the same patience, and needs to be:
    // it reads the shell's stdout and stderr to their ends, which a process the
    // shell left behind holds open just as it holds the environment's pipe
    // open. Nothing is killed on this path -- the shell may well have exited
    // already, and its process group is named by a pid that is then no longer
    // its own.
    match futures::future::select(std::pin::pin!(process.output()), give_up_on_it).await {
        Either::Left((output, _)) => Ok((buffer, output?)),
        // A shell that had to be given up on did not succeed, whatever it had
        // already reported before it was: the caller reads the status to decide
        // how loudly to complain about output it cannot parse.
        Either::Right(_) => Ok((buffer, stopped_status())),
    }
}

/// Kills the shell, and everything it started where that can be done safely.
///
/// The whole process group is what wants killing, because what fails to finish
/// is usually not the shell but something it launched. But the group is named
/// by the shell's own pid, and once the shell has exited that pid belongs to
/// whoever the system hands it to next -- killing that group could kill an
/// unrelated process. So the group is only killed while the shell is still
/// running, which is precisely the case where killing it achieves anything;
/// where it has already gone, its children are left alone and the read they are
/// holding open is simply abandoned.
#[cfg(unix)]
fn stop_it(process: &mut smol::process::Child) {
    let still_running = matches!(process.try_status(), Ok(None));
    if !still_running {
        return;
    }
    let group = process.id() as i32;
    // SAFETY: `group` is our own child, still running, and made a group leader
    // by `set_pre_exec_to_start_new_session`, so its pid is its group id. A
    // failure means it has gone in between, which is the wanted state anyway.
    unsafe {
        libc::killpg(group, libc::SIGKILL);
    }
    process.kill().ok();
}

/// Stands in for the exit status of a shell that was stopped rather than left
/// to finish. It reports the signal that stopped it, so it is not a success.
#[cfg(unix)]
fn stopped_status() -> std::process::Output {
    use std::os::unix::process::ExitStatusExt as _;

    std::process::Output {
        status: std::process::ExitStatus::from_raw(libc::SIGKILL),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

#[cfg(windows)]
async fn capture_windows(
    shell_path: &Path,
    args: &[String],
    directory: &Path,
    give_up_on_it: impl Future<Output = ()>,
) -> Result<collections::HashMap<String, String>> {
    use std::process::Stdio;

    let zed_path =
        std::env::current_exe().context("Failed to determine current zed executable path.")?;

    let shell_kind = ShellKind::new(shell_path, true);
    // Prefix with "./" if the path starts with "-" to prevent cd from interpreting it as a flag
    let directory_string = directory.display().to_string();
    let directory_string = if directory_string.starts_with('-') {
        format!("./{directory_string}")
    } else {
        directory_string
    };
    let zed_path_string = zed_path.display().to_string();
    let quote_for_shell = |value: &str| {
        shell_kind
            .try_quote(value)
            .map(|quoted| quoted.into_owned())
            .context("unexpected null in directory name")
    };
    let mut cmd = crate::command::new_command(shell_path);
    cmd.args(args);
    let quoted_directory = quote_for_shell(&directory_string)?;
    let quoted_zed_path = quote_for_shell(&zed_path_string)?;
    let cmd = match shell_kind {
        ShellKind::Csh
        | ShellKind::Tcsh
        | ShellKind::Rc
        | ShellKind::Fish
        | ShellKind::Xonsh
        | ShellKind::Posix => cmd.args([
            "-l",
            "-i",
            "-c",
            &format!("cd {}; {} --printenv", quoted_directory, quoted_zed_path),
        ]),
        ShellKind::PowerShell | ShellKind::Pwsh => cmd.args([
            "-NonInteractive",
            "-NoProfile",
            "-Command",
            &format!(
                "Set-Location {}; & {} --printenv",
                quoted_directory, quoted_zed_path
            ),
        ]),
        ShellKind::Elvish => cmd.args([
            "-c",
            &format!("cd {}; {} --printenv", quoted_directory, quoted_zed_path),
        ]),
        ShellKind::Nushell => {
            let zed_command = shell_kind
                .prepend_command_prefix(&quoted_zed_path)
                .into_owned();
            cmd.args([
                "-c",
                &format!("cd {}; {} --printenv", quoted_directory, zed_command),
            ])
        }
        ShellKind::Cmd => {
            let dir = directory_string.trim_end_matches('\\');
            cmd.args(["/d", "/c", "cd", dir, "&&", &zed_path_string, "--printenv"])
        }
    }
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    // Bounded for the same reason the unix path is: collecting the output reads
    // stdout and stderr to their ends, and a process the shell leaves behind
    // holds those open long after the shell itself has gone. Here the wait is
    // simply abandoned -- there is no separate pipe whose partial contents
    // would be worth keeping.
    let output =
        match futures::future::select(std::pin::pin!(cmd.output()), std::pin::pin!(give_up_on_it))
            .await
        {
            futures::future::Either::Left((output, _)) => {
                output.with_context(|| format!("command {cmd:?}"))?
            }
            futures::future::Either::Right(_) => {
                anyhow::bail!("command {cmd:?} did not finish in time");
            }
        };
    let env_output = String::from_utf8_lossy(&output.stdout);

    parse_env_output(
        &env_output,
        &output.status,
        || {
            format!(
                "Command {cmd:?} exited with {} but environment was captured successfully. stderr: {:?}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
            )
        },
        || {
            format!(
                "Command {cmd:?} failed with {}. stdout: {:?}, stderr: {:?}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use std::process::ExitStatus;

    use super::*;
    use crate::path;

    #[cfg(unix)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;

        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: u32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;

        ExitStatus::from_raw(code)
    }

    #[test]
    fn parse_env_output_accepts_valid_env_when_shell_exits_nonzero() {
        let env_json = serde_json::json!({
            "PATH": path!("/usr/bin"),
            "SHELL": path!("/bin/zsh"),
        });
        let env_output = format!("shell startup noise\n{env_json}\nshell shutdown noise");

        let env_map = parse_env_output(
            &env_output,
            &exit_status(1),
            || "shell exited with 1 but environment was captured successfully".to_string(),
            || panic!("failed capture error should not be evaluated for valid environment output"),
        )
        .expect("valid environment output should be returned despite non-zero shell exit");
        assert_eq!(
            env_map.get("PATH").map(String::as_str),
            Some(path!("/usr/bin"))
        );
        assert_eq!(
            env_map.get("SHELL").map(String::as_str),
            Some(path!("/bin/zsh"))
        );
    }

    /// A real wait of `milliseconds`, made without a timer.
    ///
    /// `smol::Timer` is disallowed in this repository because it is a clock no
    /// test can move, and the answer for production code is the executor's own
    /// timer -- which is why the bound reaches this module as a future rather
    /// than as a duration. These tests drive real shells, so they need a real
    /// wait rather than a controllable one, and a sleep handed to the blocking
    /// pool is one without reaching for the disallowed clock.
    #[cfg(unix)]
    fn patience_of(milliseconds: u64) -> impl std::future::Future<Output = ()> {
        smol::unblock(move || std::thread::sleep(Duration::from_millis(milliseconds)))
    }

    /// A shell that exits on its own is read whole, with no waiting for the
    /// bound. Here so that bounding the read cannot quietly break the case
    /// that always worked.
    #[cfg(unix)]
    #[test]
    fn a_shell_that_finishes_is_read_whole_and_at_once() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", r#"printf '{"A":"1"}' >&0"#]);
        crate::set_pre_exec_to_start_new_session(&mut command);

        let started = std::time::Instant::now();
        let (buffer, output) =
            smol::block_on(spawn_and_read_fd(command, 0, std::future::pending()))
                .expect("a shell that exits is read");

        assert_eq!(String::from_utf8_lossy(&buffer), r#"{"A":"1"}"#);
        assert!(output.status.success(), "{:?}", output.status);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it waited {:?} for a shell that had already finished",
            started.elapsed()
        );
    }

    /// The reported failure, reproduced: the shell prints the whole
    /// environment and then never exits. Before the read was bounded this
    /// hung for as long as the editor was left running, and the terminal it
    /// was for never opened. What must happen instead is that the wait ends
    /// and the environment is used, because it was printed in full.
    #[cfg(unix)]
    #[test]
    fn a_shell_that_prints_everything_and_never_exits_is_stopped_and_believed() {
        let mut command = std::process::Command::new("/bin/sh");
        // Long enough that the bound is what ends the wait, short enough that
        // an unbounded read fails this test rather than hanging a test run.
        command.args(["-c", r#"printf '{"A":"1"}' >&0; sleep 30"#]);
        crate::set_pre_exec_to_start_new_session(&mut command);

        let started = std::time::Instant::now();
        let (buffer, _) = smol::block_on(spawn_and_read_fd(command, 0, patience_of(300)))
            .expect("what the shell printed is handed over rather than lost");

        assert_eq!(
            String::from_utf8_lossy(&buffer),
            r#"{"A":"1"}"#,
            "the environment was printed in full, so it is the environment"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it waited {:?} on a shell that was never going to exit",
            started.elapsed()
        );
    }

    /// A shell that exits but leaves something behind holding the pipe is the
    /// same hang wearing a different hat, and the harder one: the shell is
    /// gone, so its process group cannot be killed safely -- that group is
    /// named by a pid the system may already have handed to somebody else.
    /// What must still happen is that the wait ends and the environment the
    /// shell did print is used. The child is deliberately left running, and
    /// this test leaves it to be reaped when it finishes.
    #[cfg(unix)]
    #[test]
    fn a_process_outliving_the_shell_does_not_hold_the_read_open() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", r#"sleep 30 & printf '{"A":"1"}' >&0; exit 0"#]);
        crate::set_pre_exec_to_start_new_session(&mut command);

        let started = std::time::Instant::now();
        let (buffer, output) = smol::block_on(spawn_and_read_fd(command, 0, patience_of(300)))
            .expect("what the shell printed is handed over rather than waited on");

        assert_eq!(
            String::from_utf8_lossy(&buffer),
            r#"{"A":"1"}"#,
            "the environment was printed in full before the shell exited"
        );
        assert!(
            !output.status.success(),
            "a shell that had to be given up on did not succeed, so its output \
             is reported as advice rather than as a whole answer"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it waited {:?} on a pipe held open by an outliving child",
            started.elapsed()
        );
    }
}
