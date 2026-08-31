use std::collections::HashMap;

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
    // The far side is asked for a terminal of its own, so a program that reads
    // input or paints progress behaves as it would if it were run there by
    // hand.
    ssh_args.push("-t".to_string());
    ssh_args.push(machine.destination());
    ssh_args.push("--".to_string());
    ssh_args.push(script);
    ("ssh".to_string(), ssh_args)
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
    let (program, args) = run_over_ssh(machine, &command, &resolved.args, cwd.as_deref(), &env);

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
fn quoted(value: &str) -> String {
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
}
