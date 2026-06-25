use anyhow::{Context as _, Result};
use std::time::{Duration, Instant};

pub struct SshTunnel {
    local_port: u16,
    _process: smol::process::Child,
}

impl SshTunnel {
    pub async fn establish(
        ssh_host: &str,
        ssh_port: u16,
        ssh_username: Option<&str>,
        ssh_key_path: Option<&str>,
        db_host: &str,
        db_port: u16,
    ) -> Result<Self> {
        let local_port = find_free_port()?;
        let forward = format!("{local_port}:{db_host}:{db_port}");
        let mut args = vec![
            "-N".to_string(),
            "-L".to_string(),
            forward,
            "-p".to_string(),
            ssh_port.to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=10".to_string(),
            "-o".to_string(),
            "ServerAliveInterval=60".to_string(),
        ];
        if let Some(key) = ssh_key_path {
            args.push("-i".to_string());
            args.push(key.to_string());
        }
        let host_arg = if let Some(user) = ssh_username {
            format!("{user}@{ssh_host}")
        } else {
            ssh_host.to_string()
        };
        args.push(host_arg);

        let mut std_cmd = std::process::Command::new("ssh");
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
