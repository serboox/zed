use anyhow::{Context as _, Result, bail};
use futures::io::copy;
use std::time::{Duration, Instant};

/// Which local `kubectl` operation opens the path to the database. The user
/// picks this explicitly based on which RBAC verb (`pods/portforward` or
/// `pods/exec`) their account actually has -- there is deliberately no
/// automatic detection or fallback between the two, since that would make
/// this tool infer and route around whatever permission boundary a cluster
/// administrator has set. Each mode is only ever used when selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KubernetesTunnelMode {
    /// `kubectl port-forward` -- requires the `portforward` RBAC verb.
    PortForward,
    /// `kubectl exec` piping a relay command's stdio -- requires the `exec`
    /// RBAC verb. Used when port-forward is not a permission the user has.
    Exec(KubernetesRelayCommand),
}

/// The relay binary run inside the target container to bridge the exec
/// session's stdio to a TCP connection. Must already exist in the container
/// image; there is no way to install one from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KubernetesRelayCommand {
    Socat,
    Nc,
}

impl KubernetesRelayCommand {
    /// The `kubectl exec -- <program> <args...>` tail for relaying stdio to
    /// `remote_host:remote_port`, as reached from inside the target
    /// container's own network namespace.
    fn exec_args(self, remote_host: &str, remote_port: u16) -> Vec<String> {
        match self {
            KubernetesRelayCommand::Socat => vec![
                "socat".to_string(),
                "-,ignoreeof".to_string(),
                format!("TCP:{remote_host}:{remote_port}"),
            ],
            KubernetesRelayCommand::Nc => {
                vec![
                    "nc".to_string(),
                    remote_host.to_string(),
                    remote_port.to_string(),
                ]
            }
        }
    }

    /// The binary name to probe for with `kubectl exec ... -- which <name>`.
    fn binary_name(self) -> &'static str {
        match self {
            KubernetesRelayCommand::Socat => "socat",
            KubernetesRelayCommand::Nc => "nc",
        }
    }
}

/// What `kubectl` should forward to or exec into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KubernetesTarget {
    Pod(String),
    Service(String),
}

impl KubernetesTarget {
    /// The `pod/<name>` or `svc/<name>` resource argument `kubectl` expects.
    fn resource_arg(&self) -> String {
        match self {
            KubernetesTarget::Pod(name) => format!("pod/{name}"),
            KubernetesTarget::Service(name) => format!("svc/{name}"),
        }
    }

    /// `kubectl exec` only ever targets a specific pod, never a service --
    /// callers must not construct an `Exec` mode tunnel against a `Service`
    /// target. Returns the pod name, or `None` if this target is a service.
    fn pod_name(&self) -> Option<&str> {
        match self {
            KubernetesTarget::Pod(name) => Some(name),
            KubernetesTarget::Service(_) => None,
        }
    }
}

/// Common `kubectl` connection flags (context/namespace/kubeconfig), shared
/// by both port-forward and exec argument builders.
fn common_kubectl_args(
    context: &str,
    namespace: &str,
    kubeconfig_path: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "--context".to_string(),
        context.to_string(),
        "-n".to_string(),
        namespace.to_string(),
    ];
    if let Some(path) = kubeconfig_path {
        args.push("--kubeconfig".to_string());
        args.push(path.to_string());
    }
    args
}

/// The `kubectl port-forward` argument list, computed independently of
/// process spawning so it is directly unit-testable.
fn port_forward_args(
    context: &str,
    namespace: &str,
    kubeconfig_path: Option<&str>,
    target: &KubernetesTarget,
    local_port: u16,
    remote_port: u16,
) -> Vec<String> {
    let mut args = vec![
        "port-forward".to_string(),
        target.resource_arg(),
        format!("{local_port}:{remote_port}"),
    ];
    args.extend(common_kubectl_args(context, namespace, kubeconfig_path));
    args
}

/// The `kubectl exec -i <pod> ... -- which <binary>` argument list used to
/// probe for the relay binary's presence before committing to the exec-relay
/// path.
fn exec_probe_args(
    context: &str,
    namespace: &str,
    kubeconfig_path: Option<&str>,
    pod: &str,
    relay: KubernetesRelayCommand,
) -> Vec<String> {
    let mut args = vec!["exec".to_string(), pod.to_string()];
    args.extend(common_kubectl_args(context, namespace, kubeconfig_path));
    args.push("--".to_string());
    args.push("which".to_string());
    args.push(relay.binary_name().to_string());
    args
}

/// The `kubectl exec -i <pod> ... -- <relay> <args...>` argument list that
/// actually bridges stdio to `remote_host:remote_port`.
fn exec_relay_args(
    context: &str,
    namespace: &str,
    kubeconfig_path: Option<&str>,
    pod: &str,
    relay: KubernetesRelayCommand,
    remote_host: &str,
    remote_port: u16,
) -> Vec<String> {
    let mut args = vec!["exec".to_string(), "-i".to_string(), pod.to_string()];
    args.extend(common_kubectl_args(context, namespace, kubeconfig_path));
    args.push("--".to_string());
    args.extend(relay.exec_args(remote_host, remote_port));
    args
}

/// A note surfaced in the connection UI when the driver is known to do
/// client-side cluster peer discovery, which a single-pod tunnel cannot
/// satisfy on its own. Not an error -- ad hoc single-node access typically
/// still works -- just an honest disclosure of a cluster-topology limitation
/// this tool cannot fix from the client side.
pub fn kubernetes_tunnel_caveat(driver: crate::DatabaseDriver) -> Option<&'static str> {
    match driver {
        crate::DatabaseDriver::Aerospike => Some(
            "Aerospike clusters with more than one node may not be fully reachable through a \
             single-pod tunnel: the client discovers and connects directly to every cluster \
             node's address after the initial handshake. This works out of the box only if the \
             Aerospike Kubernetes Operator's alternate-access-address feature is configured on \
             the server, or the cluster has a single node. Single-node ad hoc queries through \
             this tunnel typically work regardless.",
        ),
        _ => None,
    }
}

fn find_free_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").context("Could not bind to find a free port")?;
    Ok(listener.local_addr()?.port())
}

async fn wait_for_local_port(local_port: u16, timeout: Duration) -> Result<()> {
    smol::unblock(move || -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], local_port)),
                Duration::from_millis(100),
            )
            .is_ok()
            {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(anyhow::anyhow!(
                    "Kubernetes tunnel on local port {local_port} did not become ready within {}s",
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    })
    .await
}

/// Holds a live tunnel from a local TCP port into a Kubernetes cluster,
/// established via the user's own `kubectl` binary and kubeconfig (including
/// whatever cloud-provider exec-credential auth plugin it already has
/// configured) -- this tool never talks to the Kubernetes API directly and
/// never reimplements kubeconfig/auth handling.
pub enum KubernetesTunnel {
    /// A single `kubectl port-forward` child forwards the local port for as
    /// long as the tunnel lives.
    PortForward {
        local_port: u16,
        _process: smol::process::Child,
    },
    /// A local `TcpListener` accepts connections and, per connection, spawns
    /// a fresh `kubectl exec` relay child whose stdio is spliced to that
    /// connection. The listener task runs until the tunnel is dropped.
    Exec {
        local_port: u16,
        _listener_task: smol::Task<()>,
    },
}

impl Drop for KubernetesTunnel {
    fn drop(&mut self) {
        if let KubernetesTunnel::PortForward { _process, .. } = self {
            let _ = _process.kill();
        }
    }
}

impl KubernetesTunnel {
    pub fn local_port(&self) -> u16 {
        match self {
            KubernetesTunnel::PortForward { local_port, .. } => *local_port,
            KubernetesTunnel::Exec { local_port, .. } => *local_port,
        }
    }

    /// Establishes a tunnel using the user-selected `mode`. There is no
    /// fallback between modes -- the caller has already picked the one that
    /// matches the RBAC permission they know they have.
    pub async fn establish(
        mode: KubernetesTunnelMode,
        context: &str,
        namespace: &str,
        kubeconfig_path: Option<&str>,
        target: KubernetesTarget,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<Self> {
        match mode {
            KubernetesTunnelMode::PortForward => {
                Self::establish_port_forward(
                    context,
                    namespace,
                    kubeconfig_path,
                    target,
                    remote_port,
                )
                .await
            }
            KubernetesTunnelMode::Exec(relay) => {
                Self::establish_exec_relay(
                    context,
                    namespace,
                    kubeconfig_path,
                    target,
                    relay,
                    remote_host,
                    remote_port,
                )
                .await
            }
        }
    }

    async fn establish_port_forward(
        context: &str,
        namespace: &str,
        kubeconfig_path: Option<&str>,
        target: KubernetesTarget,
        remote_port: u16,
    ) -> Result<Self> {
        let local_port = find_free_port()?;
        let args = port_forward_args(
            context,
            namespace,
            kubeconfig_path,
            &target,
            local_port,
            remote_port,
        );

        let mut process = smol::process::Command::new("kubectl")
            .args(&args)
            .stdin(smol::process::Stdio::null())
            .stdout(smol::process::Stdio::null())
            .stderr(smol::process::Stdio::null())
            .spawn()
            .context("Failed to spawn kubectl; ensure 'kubectl' is in PATH")?;

        wait_for_local_port(local_port, Duration::from_secs(10)).await?;

        // `find_free_port` releases the port before `kubectl` binds it, so a
        // connect succeeding on `local_port` does not by itself prove it is
        // `kubectl`'s listener -- another process could have grabbed the
        // port in that window. If `kubectl` has already exited (e.g. it
        // failed to bind because something else took the port), surface
        // that instead of silently returning a tunnel to the wrong service.
        if let Some(status) = process
            .try_status()
            .context("Failed to check kubectl port-forward's status")?
        {
            bail!(
                "kubectl port-forward exited immediately ({status}) -- local port {local_port} \
                 may have been taken by another process before kubectl could bind it"
            );
        }

        Ok(KubernetesTunnel::PortForward {
            local_port,
            _process: process,
        })
    }

    async fn establish_exec_relay(
        context: &str,
        namespace: &str,
        kubeconfig_path: Option<&str>,
        target: KubernetesTarget,
        relay: KubernetesRelayCommand,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<Self> {
        let pod = target
            .pod_name()
            .context("Exec mode requires a Pod target, not a Service")?
            .to_string();

        let probe_args = exec_probe_args(context, namespace, kubeconfig_path, &pod, relay);
        let probe = smol::process::Command::new("kubectl")
            .args(&probe_args)
            .stdin(smol::process::Stdio::null())
            .output()
            .await
            .context("Failed to run kubectl exec to probe for the relay binary")?;
        if !probe.status.success() {
            anyhow::bail!(
                "'{}' was not found in pod '{pod}' -- cannot tunnel via exec. Install it in the \
                 container image, or use Port-forward mode instead.",
                relay.binary_name()
            );
        }

        let listener = smol::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("Could not bind a local port for the exec-relay tunnel")?;
        let local_port = listener.local_addr()?.port();

        let context = context.to_string();
        let namespace = namespace.to_string();
        let kubeconfig_path = kubeconfig_path.map(str::to_string);
        let remote_host = remote_host.to_string();

        let listener_task = smol::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let args = exec_relay_args(
                    &context,
                    &namespace,
                    kubeconfig_path.as_deref(),
                    &pod,
                    relay,
                    &remote_host,
                    remote_port,
                );
                let Ok(mut child) = smol::process::Command::new("kubectl")
                    .args(&args)
                    .stdin(smol::process::Stdio::piped())
                    .stdout(smol::process::Stdio::piped())
                    .stderr(smol::process::Stdio::null())
                    .spawn()
                else {
                    continue;
                };
                let Some(child_stdin) = child.stdin.take() else {
                    continue;
                };
                let Some(child_stdout) = child.stdout.take() else {
                    continue;
                };
                let (tcp_read, tcp_write) = smol::io::split(stream);
                smol::spawn(async move {
                    let mut child_stdin = child_stdin;
                    let _ = copy(tcp_read, &mut child_stdin).await;
                })
                .detach();
                smol::spawn(async move {
                    let mut tcp_write = tcp_write;
                    let _ = copy(child_stdout, &mut tcp_write).await;
                })
                .detach();
                smol::spawn(async move {
                    let _ = child.status().await;
                })
                .detach();
            }
        });

        Ok(KubernetesTunnel::Exec {
            local_port,
            _listener_task: listener_task,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_forward_args_target_a_pod_with_local_to_remote_port_mapping() {
        let target = KubernetesTarget::Pod("aerospike-0".to_string());
        let args = port_forward_args("prod-cluster", "db", None, &target, 15000, 3000);
        assert_eq!(
            args,
            vec![
                "port-forward",
                "pod/aerospike-0",
                "15000:3000",
                "--context",
                "prod-cluster",
                "-n",
                "db",
            ]
        );
    }

    #[test]
    fn port_forward_args_target_a_service_and_include_kubeconfig_when_set() {
        let target = KubernetesTarget::Service("aerospike".to_string());
        let args = port_forward_args(
            "prod-cluster",
            "db",
            Some("/home/alice/.kube/config"),
            &target,
            15000,
            3000,
        );
        assert!(args.contains(&"svc/aerospike".to_string()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--kubeconfig", "/home/alice/.kube/config"])
        );
    }

    #[test]
    fn exec_probe_args_ask_which_for_the_selected_relay_binary() {
        let args = exec_probe_args(
            "prod-cluster",
            "db",
            None,
            "aerospike-0",
            KubernetesRelayCommand::Socat,
        );
        assert_eq!(
            args,
            vec![
                "exec",
                "aerospike-0",
                "--context",
                "prod-cluster",
                "-n",
                "db",
                "--",
                "which",
                "socat",
            ]
        );
    }

    #[test]
    fn exec_relay_args_use_socat_ignoreeof_to_bridge_stdio_to_the_remote_address() {
        let args = exec_relay_args(
            "prod-cluster",
            "db",
            None,
            "aerospike-0",
            KubernetesRelayCommand::Socat,
            "localhost",
            3000,
        );
        assert_eq!(
            args,
            vec![
                "exec",
                "-i",
                "aerospike-0",
                "--context",
                "prod-cluster",
                "-n",
                "db",
                "--",
                "socat",
                "-,ignoreeof",
                "TCP:localhost:3000",
            ]
        );
    }

    #[test]
    fn exec_relay_args_use_nc_when_selected_instead_of_socat() {
        let args = exec_relay_args(
            "prod-cluster",
            "db",
            None,
            "aerospike-0",
            KubernetesRelayCommand::Nc,
            "localhost",
            3000,
        );
        assert!(args.ends_with(&[
            "nc".to_string(),
            "localhost".to_string(),
            "3000".to_string()
        ]));
    }

    #[test]
    fn kubernetes_tunnel_caveat_warns_only_for_aerospike() {
        assert!(kubernetes_tunnel_caveat(crate::DatabaseDriver::Aerospike).is_some());
        assert!(kubernetes_tunnel_caveat(crate::DatabaseDriver::MySQL).is_none());
        assert!(kubernetes_tunnel_caveat(crate::DatabaseDriver::MongoDB).is_none());
    }

    #[test]
    fn exec_target_has_no_pod_name_for_a_service() {
        let target = KubernetesTarget::Service("aerospike".to_string());
        assert_eq!(target.pod_name(), None);
    }

    #[test]
    fn pod_target_resource_arg_is_prefixed_with_pod_slash() {
        let target = KubernetesTarget::Pod("aerospike-0".to_string());
        assert_eq!(target.resource_arg(), "pod/aerospike-0");
    }
}
