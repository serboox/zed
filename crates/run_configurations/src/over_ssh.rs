use std::collections::HashMap;
use std::path::PathBuf;

/// A machine to run something on, spelled the way `ssh` itself spells it.
///
/// Deliberately not the editor's own remote-connection type: that one
/// describes a whole remote *project* -- a password, forwarded ports, its own
/// binary uploaded to the far side -- and none of that is needed to run one
/// command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl Machine {
    /// Reads `host`, `user@host`, `user@host:port` or `host:port`.
    ///
    /// A port that is not a number is left as part of the host rather than
    /// silently dropped: an address the reader typed wrongly should fail to
    /// connect and say so, not connect somewhere else.
    pub fn parse(said: &str) -> Option<Self> {
        let said = said.trim();
        if said.is_empty() {
            return None;
        }
        let (user, rest) = match said.split_once('@') {
            Some((user, rest)) if !user.is_empty() && !rest.is_empty() => {
                (Some(user.to_string()), rest)
            }
            _ => (None, said),
        };
        // An address of several colons is an IPv6 one, where none of them mean
        // a port; the only way to give such an address a port is the bracketed
        // form, which is also how `ssh` itself spells it. Anything else takes a
        // port only from a single trailing `:digits`.
        let (host, port) = if let Some(rest) = rest.strip_prefix('[') {
            match rest.split_once(']') {
                Some((inside, after)) => match after.strip_prefix(':') {
                    Some(port) => match port.parse::<u16>() {
                        Ok(port) => (inside, Some(port)),
                        Err(_) => (inside, None),
                    },
                    None => (inside, None),
                },
                None => (rest, None),
            }
        } else if rest.matches(':').count() == 1 {
            match rest.split_once(':') {
                Some((host, port)) if !host.is_empty() && !port.is_empty() => {
                    match port.parse::<u16>() {
                        Ok(port) => (host, Some(port)),
                        Err(_) => (rest, None),
                    }
                }
                _ => (rest, None),
            }
        } else {
            (rest, None)
        };
        if host.is_empty() {
            return None;
        }
        Some(Self {
            user,
            host: host.to_string(),
            port,
        })
    }

    /// What `ssh` is given as its destination.
    pub fn destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// The command that runs `command` with `args` on `machine`.
///
/// Everything the run needs has to travel: a working directory and an
/// environment set on this side would apply to this machine, not to that one.
/// They are therefore written into the one line the far side's shell is asked
/// to run, and every piece of it is quoted, so a path or a value with a space
/// in it arrives whole.
///
/// No working directory means the far side's own login directory rather than a
/// guess. A configuration written for this machine usually says
/// `$ZED_WORKTREE_ROOT`, which names a path that need not exist over there at
/// all, so the form asks for a remote directory instead of assuming one.
pub fn run_over_ssh(
    machine: &Machine,
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &[(String, String)],
) -> (String, Vec<String>) {
    compose(machine, None, command, args, cwd, env)
}

/// Lets the editor find a run again on the far side: a token only its processes carry,
/// and the path of the ssh connection that later questions reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTag {
    pub token: String,
    pub control_path: PathBuf,
}

/// What a socket path may come to, with room to spare under the 104 bytes of the
/// shortest limit a system has.
const LONGEST_SOCKET_PATH: usize = 100;

impl RunTag {
    pub fn fresh() -> Self {
        Self::in_one_of(
            [
                std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
                Some(std::env::temp_dir()),
                Some(PathBuf::from("/tmp")),
            ]
            .into_iter()
            .flatten(),
        )
    }

    /// The first of `directories` the socket path fits in, for a runtime or
    /// temporary directory of any length.
    fn in_one_of(directories: impl IntoIterator<Item = PathBuf>) -> Self {
        let token = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let name = format!("zed-ssh-{token}");
        let directories: Vec<PathBuf> = directories.into_iter().collect();
        let directory = directories
            .iter()
            .find(|directory| directory.join(&name).as_os_str().len() <= LONGEST_SOCKET_PATH)
            .or(directories.last())
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        Self {
            control_path: directory.join(name),
            token,
        }
    }
}

const RUN_TOKEN_VARIABLE: &str = "ZED_REMOTE_RUN";

/// [`run_over_ssh`] for a run the editor will measure on the far side.
pub fn run_tagged_over_ssh(
    machine: &Machine,
    tag: &RunTag,
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &[(String, String)],
) -> (String, Vec<String>) {
    compose(machine, Some(tag), command, args, cwd, env)
}

fn compose(
    machine: &Machine,
    tag: Option<&RunTag>,
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &[(String, String)],
) -> (String, Vec<String>) {
    let mut script = String::new();
    if let Some(cwd) = cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) {
        script.push_str("cd ");
        script.push_str(&quoted(cwd));
        script.push_str(" && ");
    }
    for (name, value) in env {
        script.push_str("export ");
        script.push_str(name);
        script.push('=');
        script.push_str(&quoted(value));
        script.push_str(" && ");
    }
    // After the run's own variables, so none of them can take the token's place.
    if let Some(tag) = tag {
        script.push_str(&format!("export {RUN_TOKEN_VARIABLE}='{}' && ", tag.token));
    }
    script.push_str(command);
    for arg in args {
        script.push(' ');
        script.push_str(&quoted(arg));
    }

    let mut ssh_args = Vec::new();
    if let Some(port) = machine.port {
        ssh_args.push("-p".to_string());
        ssh_args.push(port.to_string());
    }
    if let Some(tag) = tag {
        // The connection ends with the run, whatever the reader's configuration says.
        for option in [
            "ControlMaster=auto".to_string(),
            format!("ControlPath={}", tag.control_path.display()),
            "ControlPersist=no".to_string(),
        ] {
            ssh_args.push("-o".to_string());
            ssh_args.push(option);
        }
    }
    // The far side is asked for a terminal of its own, so a program that reads
    // input or paints progress behaves as it would if it were run there by
    // hand.
    ssh_args.push("-t".to_string());
    ssh_args.push(machine.destination());
    ssh_args.push("--".to_string());
    ssh_args.push(script);
    ("ssh".to_string(), ssh_args)
}

/// The machine a run was sent to, read back out of the command that
/// [`run_over_ssh`] made: `ssh`, then the options, then `--` and the script. A
/// run of anything else, or an `ssh` the reader wrote without the `--`, says
/// nothing about where it goes.
pub fn destination_of(command: Option<&str>, args: &[String]) -> Option<String> {
    let program = std::path::Path::new(command?).file_name()?.to_str()?;
    if program != "ssh" {
        return None;
    }
    let separator = args.iter().position(|arg| arg == "--")?;
    args.get(separator.checked_sub(1)?).cloned()
}

/// A run sent over ssh, read back from its command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRun {
    pub program: String,
    pub destination: String,
    pub port: Option<u16>,
    pub control_path: PathBuf,
    pub token: String,
}

/// The [`RemoteRun`] a command made by [`run_tagged_over_ssh`] describes. Any other
/// run says nothing and is measured by its local client.
pub fn remote_run_of(command: Option<&str>, args: &[String]) -> Option<RemoteRun> {
    let program = command?;
    let destination = destination_of(Some(program), args)?;
    let separator = args.iter().position(|arg| arg == "--")?;
    let port = args
        .iter()
        .take(separator)
        .position(|arg| arg == "-p")
        .and_then(|at| args.get(at + 1)?.parse().ok());
    let control_path = args
        .iter()
        .take(separator)
        .find_map(|arg| arg.strip_prefix("ControlPath="))?;
    let script = args.get(separator + 1)?;
    let marker = format!("export {RUN_TOKEN_VARIABLE}='");
    let after = &script[script.find(&marker)? + marker.len()..];
    let token = after[..after.find('\'')?].to_string();
    if token.is_empty()
        || !token
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(RemoteRun {
        program: program.to_string(),
        destination,
        port,
        control_path: PathBuf::from(control_path),
        token,
    })
}

/// Rewrites a resolved run so it happens on `machine` instead of here.
///
/// `from_the_file` is whatever a named environment file held, read on this side
/// because that is where the file is; the configuration's own variables win
/// over it, exactly as they do for a run on this machine.
///
/// The editor's own context variables are left behind. They name paths of this
/// machine -- a worktree root, the file that happens to be open -- and
/// exporting them over there would state, in the far side's environment, places
/// that do not exist on it.
pub fn send_to(
    machine: &Machine,
    resolved: &mut task::SpawnInTerminal,
    from_the_file: HashMap<String, String>,
) {
    let mut env: Vec<(String, String)> = from_the_file
        .into_iter()
        .chain(resolved.env.clone())
        .filter(|(name, _)| !name.starts_with("ZED_"))
        .collect::<HashMap<_, _>>()
        .into_iter()
        .collect();
    // Sorted so the same configuration composes the same command every time,
    // which is what makes the command shown in the terminal worth reading.
    env.sort();

    let command = resolved.command.clone().unwrap_or_default();
    let cwd = resolved
        .cwd
        .as_ref()
        .map(|cwd| cwd.to_string_lossy().into_owned());
    let (program, args) = run_tagged_over_ssh(
        machine,
        &RunTag::fresh(),
        &command,
        &resolved.args,
        cwd.as_deref(),
        &env,
    );

    // Named so a remote run is never mistaken for a local one, in the tab and
    // in the line the terminal prints above the output.
    let on = machine.destination();
    resolved.label = format!("{} on {on}", resolved.label);
    resolved.full_label = format!("{} on {on}", resolved.full_label);
    resolved.command_label = format!("{} on {on}", resolved.command_label);

    resolved.command = Some(program);
    resolved.args = args;
    // Cleared: a directory and an environment set here would apply to this
    // machine. They have travelled into the command instead.
    resolved.cwd = None;
    resolved.env = HashMap::default();
    resolved.env_file = None;
}

/// One argument, quoted for a POSIX shell.
///
/// Single quotes take everything literally, which is what is wanted; the only
/// character they cannot hold is a single quote itself, so each one is closed,
/// escaped and reopened.
pub(crate) fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_is_read_the_way_ssh_spells_one() {
        assert_eq!(
            Machine::parse("build.example.com"),
            Some(Machine {
                user: None,
                host: "build.example.com".into(),
                port: None
            })
        );
        assert_eq!(
            Machine::parse("deploy@build.example.com"),
            Some(Machine {
                user: Some("deploy".into()),
                host: "build.example.com".into(),
                port: None
            })
        );
        assert_eq!(
            Machine::parse("deploy@build.example.com:2222"),
            Some(Machine {
                user: Some("deploy".into()),
                host: "build.example.com".into(),
                port: Some(2222)
            })
        );
        assert_eq!(
            Machine::parse("  build.example.com  ")
                .map(|machine| machine.host)
                .as_deref(),
            Some("build.example.com")
        );
    }

    /// Nothing typed means this machine, which is what every configuration
    /// written before a machine could be named means.
    #[test]
    fn nothing_typed_is_not_a_machine() {
        assert_eq!(Machine::parse(""), None);
        assert_eq!(Machine::parse("   "), None);
    }

    /// A colon is not always a port. An address that is not one has to fail to
    /// connect and say so, rather than quietly become a different address.
    #[test]
    fn only_a_trailing_number_is_a_port() {
        let named = Machine::parse("build.example.com:whatever").expect("still an address");
        assert_eq!(named.host, "build.example.com:whatever");
        assert_eq!(named.port, None);

        let sixed = Machine::parse("fe80::1").expect("an address of colons");
        assert_eq!(sixed.host, "fe80::1");
        assert_eq!(
            sixed.port, None,
            "the last colon of an IPv6 address is part of the address"
        );

        // The bracketed form is how such an address is given a port, and how
        // `ssh` itself spells it.
        let bracketed = Machine::parse("[fe80::1]:2222").expect("a bracketed address");
        assert_eq!(bracketed.host, "fe80::1");
        assert_eq!(bracketed.port, Some(2222));
        let bare = Machine::parse("[fe80::1]").expect("a bracketed address with no port");
        assert_eq!(bare.host, "fe80::1");
        assert_eq!(bare.port, None);
        assert_eq!(
            Machine::parse("deploy@[fe80::1]:22").map(|machine| machine.destination()),
            Some("deploy@fe80::1".to_string())
        );
    }

    #[test]
    fn the_working_directory_and_the_environment_travel_with_the_command() {
        let machine = Machine::parse("deploy@build.example.com:2222").expect("a machine");
        let (program, args) = run_over_ssh(
            &machine,
            "go",
            &["run".to_string(), "./cmd/api".to_string()],
            Some("/srv/app"),
            &[("PORT".to_string(), "8080".to_string())],
        );
        assert_eq!(program, "ssh");
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], "2222");
        assert_eq!(args[3], "deploy@build.example.com");
        assert_eq!(args[4], "--");
        assert_eq!(
            args[5],
            "cd '/srv/app' && export PORT='8080' && go 'run' './cmd/api'"
        );
    }

    /// A run sent over ssh can be told from the command it became; a run of
    /// anything else, and an `ssh` with no script to run, cannot.
    #[test]
    fn a_run_sent_over_ssh_says_where_it_was_sent() {
        let machine = Machine::parse("deploy@build.example.com:2222").expect("a machine");
        let (program, args) = run_over_ssh(&machine, "go", &[], None, &[]);
        assert_eq!(
            destination_of(Some(&program), &args),
            Some("deploy@build.example.com".to_string())
        );
        assert_eq!(
            destination_of(Some("/usr/bin/ssh"), &args),
            Some("deploy@build.example.com".to_string()),
            "the program may be spelled with a path"
        );
        assert_eq!(destination_of(Some("go"), &args), None);
        assert_eq!(destination_of(None, &args), None);
        assert_eq!(
            destination_of(Some("ssh"), &["host".to_string(), "uptime".to_string()]),
            None,
            "an ssh written by hand has no `--` to read the destination from"
        );
    }

    /// A path or a value with a space in it has to arrive whole.
    #[test]
    fn a_space_survives_the_journey() {
        let machine = Machine::parse("host").expect("a machine");
        let (_, args) = run_over_ssh(
            &machine,
            "./run",
            &["--name".to_string(), "the whole thing".to_string()],
            Some("/srv/my app"),
            &[("GREETING".to_string(), "hello there".to_string())],
        );
        let script = args.last().expect("the script");
        assert!(script.contains("cd '/srv/my app'"), "{script}");
        assert!(script.contains("export GREETING='hello there'"), "{script}");
        assert!(script.contains("'the whole thing'"), "{script}");
    }

    /// A single quote is the one character single quotes cannot hold, and a
    /// value carrying one must not be able to end the quoting and become
    /// another command.
    #[test]
    fn a_quote_in_a_value_cannot_break_out_of_its_quoting() {
        let machine = Machine::parse("host").expect("a machine");
        let (_, args) = run_over_ssh(
            &machine,
            "echo",
            &[],
            None,
            &[("MESSAGE".to_string(), "it's; rm -rf /".to_string())],
        );
        let script = args.last().expect("the script");
        assert_eq!(script, r"export MESSAGE='it'\''s; rm -rf /' && echo");
    }

    /// No directory means the far side's own login directory. A configuration
    /// written for this machine names a path that need not exist over there.
    #[test]
    fn no_working_directory_sends_no_cd_at_all() {
        let machine = Machine::parse("host").expect("a machine");
        let (_, args) = run_over_ssh(&machine, "uptime", &[], None, &[]);
        assert_eq!(args.last().map(String::as_str), Some("uptime"));
        let (_, blank) = run_over_ssh(&machine, "uptime", &[], Some("   "), &[]);
        assert_eq!(blank.last().map(String::as_str), Some("uptime"));
    }
    fn a_tag() -> RunTag {
        RunTag {
            token: "abc123def456".into(),
            control_path: PathBuf::from("/run/user/1000/zed-ssh-abc123def456"),
        }
    }

    /// A tagged run can be found again from its command, and its script is unchanged.
    #[test]
    fn a_tagged_run_can_be_found_again_from_its_command() {
        let machine = Machine::parse("deploy@build.example.com:2222").expect("a machine");
        let (program, args) = run_tagged_over_ssh(
            &machine,
            &a_tag(),
            "go",
            &["run".to_string()],
            Some("/srv/app"),
            &[("PORT".to_string(), "8080".to_string())],
        );
        assert_eq!(
            remote_run_of(Some(&program), &args),
            Some(RemoteRun {
                program: "ssh".into(),
                destination: "deploy@build.example.com".into(),
                port: Some(2222),
                control_path: a_tag().control_path,
                token: "abc123def456".into(),
            })
        );
        let script = args.last().expect("a script");
        assert_eq!(
            script,
            "cd '/srv/app' && export PORT='8080' && export ZED_REMOTE_RUN='abc123def456' && go 'run'"
        );
        for option in [
            "ControlMaster=auto",
            "ControlPath=/run/user/1000/zed-ssh-abc123def456",
            "ControlPersist=no",
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "-o" && pair[1] == option),
                "{option} is asked of ssh: {args:?}"
            );
        }
    }

    /// A run the editor did not tag has nothing to ask about.
    #[test]
    fn a_run_with_no_tag_has_nothing_to_ask() {
        let machine = Machine::parse("deploy@build.example.com").expect("a machine");
        let (program, args) = run_over_ssh(&machine, "go", &[], None, &[]);
        assert_eq!(remote_run_of(Some(&program), &args), None);
        assert_eq!(
            remote_run_of(Some("ssh"), &["host".to_string(), "uptime".to_string()]),
            None
        );
        let (program, mut args) = run_tagged_over_ssh(&machine, &a_tag(), "go", &[], None, &[]);
        let script = args.last_mut().expect("a script");
        *script = script.replace("abc123def456", "a b");
        assert_eq!(
            remote_run_of(Some(&program), &args),
            None,
            "a token that is not plain letters and digits is not one the editor made"
        );
    }

    /// A tag is short enough for a socket path, and no two runs share one.
    #[test]
    fn a_fresh_tag_is_short_and_not_shared() {
        let (first, second) = (RunTag::fresh(), RunTag::fresh());
        assert_ne!(first, second);
        assert_eq!(first.token.len(), 12);
        assert!(first.control_path.as_os_str().len() < 100);
        assert!(
            first
                .control_path
                .ends_with(format!("zed-ssh-{}", first.token))
        );
    }
    /// A directory too long for a socket path is passed over for one that fits.
    #[test]
    fn a_directory_too_long_for_a_socket_is_passed_over() {
        let too_long = PathBuf::from(format!("/run/user/1000/{}", "x".repeat(120)));
        let tag = RunTag::in_one_of([too_long, PathBuf::from("/tmp")]);
        assert_eq!(
            tag.control_path.parent(),
            Some(std::path::Path::new("/tmp"))
        );
        let only_long = PathBuf::from(format!("/run/user/1000/{}", "y".repeat(120)));
        assert!(
            RunTag::in_one_of([only_long.clone()])
                .control_path
                .starts_with(&only_long),
            "with nothing better the last directory is used as it is"
        );
    }

    /// A run the editor sends over ssh is always tagged.
    #[test]
    fn a_run_sent_to_a_machine_can_be_asked_about() {
        let machine = Machine::parse("deploy@build.example.com").expect("a machine");
        let mut resolved = task::SpawnInTerminal {
            label: "api".into(),
            full_label: "api".into(),
            command_label: "go run".into(),
            command: Some("go".into()),
            args: vec!["run".into()],
            ..Default::default()
        };
        send_to(&machine, &mut resolved, HashMap::new());

        let remote = remote_run_of(resolved.command.as_deref(), &resolved.args)
            .expect("the run says how to ask about it");
        assert_eq!(remote.destination, "deploy@build.example.com");
        assert_eq!(resolved.label, "api on deploy@build.example.com");
    }
}
