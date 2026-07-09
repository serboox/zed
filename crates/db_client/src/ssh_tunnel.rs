use anyhow::{Context as _, Result};
use std::io::Write as _;
use std::time::{Duration, Instant};

/// How the tunnel authenticates to the SSH bastion host.
pub enum SshAuth<'a> {
    /// `-i <path>`, relying on the key's own passphrase-less agent or none at all.
    KeyFile(Option<&'a str>),
    /// Fed to `ssh` non-interactively via `SSH_ASKPASS`, never on the command
    /// line (would leak through `ps`) and never written to disk.
    Password(&'a str),
}

const ASKPASS_ENV_VAR: &str = "ZED_DB_CLIENT_SSH_PASSWORD";

pub struct SshTunnel {
    local_port: u16,
    _process: smol::process::Child,
    // Kept alive for the process's lifetime; the askpass script only exists
    // while `ssh` might still need to read it (e.g. on a slow handshake).
    _askpass_dir: Option<tempfile::TempDir>,
}

/// The `ssh` argument list for a given auth mode, computed independently of
/// process spawning so the auth-mode branching is directly unit-testable.
fn ssh_args(
    ssh_port: u16,
    ssh_username: Option<&str>,
    ssh_host: &str,
    auth: &SshAuth<'_>,
    forward: String,
) -> Vec<String> {
    let mut args = vec![
        "-N".to_string(),
        "-L".to_string(),
        forward,
        "-p".to_string(),
        ssh_port.to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=60".to_string(),
    ];
    match auth {
        SshAuth::KeyFile(key_path) => {
            // BatchMode disables all interactive/askpass prompting, which is
            // exactly what we want for key-only auth: fail fast instead of
            // hanging on a password prompt nothing will answer.
            args.push("-o".to_string());
            args.push("BatchMode=yes".to_string());
            if let Some(key) = key_path {
                args.push("-i".to_string());
                args.push(key.to_string());
            }
        }
        SshAuth::Password(_) => {}
    }
    let host_arg = if let Some(user) = ssh_username {
        format!("{user}@{ssh_host}")
    } else {
        ssh_host.to_string()
    };
    args.push(host_arg);
    args
}

impl SshTunnel {
    pub async fn establish(
        ssh_host: &str,
        ssh_port: u16,
        ssh_username: Option<&str>,
        auth: SshAuth<'_>,
        db_host: &str,
        db_port: u16,
    ) -> Result<Self> {
        let local_port = find_free_port()?;
        let forward = format!("{local_port}:{db_host}:{db_port}");
        let args = ssh_args(ssh_port, ssh_username, ssh_host, &auth, forward);

        let mut std_cmd = std::process::Command::new("ssh");
        let askpass_dir = match auth {
            SshAuth::KeyFile(_) => None,
            SshAuth::Password(password) => {
                let (script_path, dir) = write_askpass_script()?;
                std_cmd.env(ASKPASS_ENV_VAR, password);
                std_cmd.env("SSH_ASKPASS", &script_path);
                // Forces askpass even though stdin/stdout are null and no
                // controlling tty is attached (OpenSSH 8.4+); older clients
                // fall back to DISPLAY-gated behavior, hence setting both.
                std_cmd.env("SSH_ASKPASS_REQUIRE", "force");
                std_cmd.env("DISPLAY", ":0");
                Some(dir)
            }
        };

        std_cmd.args(&args);
        let mut smol_cmd = smol::process::Command::from(std_cmd);
        let process = smol_cmd
            .stdin(smol::process::Stdio::null())
            .stdout(smol::process::Stdio::null())
            .stderr(smol::process::Stdio::null())
            .spawn()
            .context("Failed to spawn SSH process; ensure 'ssh' is in PATH")?;

        let ssh_host_for_error = ssh_host.to_string();
        smol::unblock(move || -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], local_port)),
                    Duration::from_millis(100),
                )
                .is_ok()
                {
                    break;
                }
                if Instant::now() > deadline {
                    return Err(anyhow::anyhow!(
                        "SSH tunnel to {}:{} did not become ready within 10 seconds",
                        ssh_host_for_error,
                        ssh_port
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        })
        .await?;

        Ok(Self {
            local_port,
            _process: process,
            _askpass_dir: askpass_dir,
        })
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        let _ = self._process.kill();
    }
}

fn find_free_port() -> Result<u16> {
    use std::net::TcpListener;
    let listener =
        TcpListener::bind("127.0.0.1:0").context("Could not bind to find a free port")?;
    Ok(listener.local_addr()?.port())
}

/// Writes a private (0700) temp-dir script that `ssh` runs as `SSH_ASKPASS`.
/// It only echoes an env var that we set on `ssh`'s own child process
/// environment -- the password never appears in argv (visible to any local
/// user via `ps`) and is never written to disk.
fn write_askpass_script() -> Result<(std::path::PathBuf, tempfile::TempDir)> {
    let dir = tempfile::Builder::new()
        .prefix("zed-db-client-askpass")
        .tempdir()
        .context("creating askpass temp dir")?;
    let script_path = dir.path().join("askpass.sh");
    let script = format!("#!/bin/sh\nprintf '%s' \"${ASKPASS_ENV_VAR}\"\n");
    {
        let mut file = std::fs::File::create(&script_path).context("creating askpass script")?;
        file.write_all(script.as_bytes())
            .context("writing askpass script")?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .context("marking askpass script executable")?;
    }
    Ok((script_path, dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_file_auth_forces_batch_mode_and_passes_the_key_path() {
        let args = ssh_args(
            22,
            Some("alice"),
            "bastion.example.com",
            &SshAuth::KeyFile(Some("/home/alice/.ssh/id_rsa")),
            "1234:db.internal:3306".to_string(),
        );
        assert!(
            args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]),
            "key-file auth must force BatchMode=yes so a missing/bad key fails \
             fast instead of hanging on an unanswerable prompt: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["-i", "/home/alice/.ssh/id_rsa"]),
            "the configured key path must be passed via -i: {args:?}"
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("alice@bastion.example.com")
        );
    }

    #[test]
    fn password_auth_does_not_force_batch_mode_or_leak_a_key_flag() {
        let args = ssh_args(
            22,
            Some("alice"),
            "bastion.example.com",
            &SshAuth::Password("hunter2"),
            "1234:db.internal:3306".to_string(),
        );
        assert!(
            !args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]),
            "BatchMode=yes would suppress the SSH_ASKPASS prompt password auth relies \
             on: {args:?}"
        );
        assert!(
            !args.contains(&"-i".to_string()),
            "password auth must not also pass a key-file flag: {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("hunter2")),
            "the password must never appear in the argv passed to ssh (visible via \
             `ps` to any local user): {args:?}"
        );
    }

    #[test]
    fn askpass_script_echoes_the_env_var_and_never_embeds_the_password() {
        let (script_path, _dir) = write_askpass_script().expect("write askpass script");
        let script = std::fs::read_to_string(&script_path).expect("read askpass script");
        assert!(
            script.contains(&format!("${ASKPASS_ENV_VAR}")),
            "the script must read the password from the env var, not a literal: {script:?}"
        );
        assert!(
            !script.to_lowercase().contains("password") || script.contains(ASKPASS_ENV_VAR),
            "sanity check: script content is the env-var reference, not a hardcoded secret"
        );
    }
}
