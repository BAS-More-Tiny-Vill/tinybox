//! Tests that drive a real Docker daemon.
//!
//! Gated behind `TINYBOX_LIVE_DOCKER=1` and named `live_*` so an ordinary
//! `cargo test` skips them: they need a daemon, pull images, and are far slower
//! than the unit suite. Everything about argument construction is covered
//! without a daemon in `src/sandbox/test.rs`; what is here is what only a real
//! daemon can answer.
//!
//! The isolation tests assert **negatively** — that the box *cannot* see the
//! host — because a positive assertion would pass just as happily against a
//! sandbox that confines nothing.
//!
//! Run them with:
//!
//! ```sh
//! TINYBOX_LIVE_DOCKER=1 cargo test -p tinybox-docker --test live_docker
//! ```
//!
//! CI also sets `TINYBOX_REQUIRE_LIVE_DOCKER=1`; in that mode the focused
//! connectivity test fails if the opt-in gate was not enabled.

use std::{
    io::{Read, Write},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream},
    process::Command,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use tinybox_core::{
    BoxSpec, Error, ExecRequest, Host as _, HostRef, MemoryStore, NetworkPolicy, Placement,
    PortMapping, Result, Sandbox, SandboxRef, WorkspaceSource,
};
use tinybox_docker::{DockerSandbox, EgressResolver, NativeLinuxDockerEgressController};
use tinybox_host::LocalHost;

/// The image every live test uses. Small, and it ships a shell.
const IMAGE: &str = "alpine:3";

/// Whether the live suite should run at all.
fn enabled() -> bool {
    std::env::var_os("TINYBOX_LIVE_DOCKER").is_some()
}

/// Enforce the CI opt-in contract and report whether live Docker tests should run.
fn live_docker_enabled() -> Result<bool> {
    let enabled = enabled();
    if std::env::var_os("TINYBOX_REQUIRE_LIVE_DOCKER").is_some() && !enabled {
        return Err(Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "run required live network test",
            message: "TINYBOX_REQUIRE_LIVE_DOCKER is set but TINYBOX_LIVE_DOCKER is not enabled"
                .to_owned(),
        });
    }
    Ok(enabled)
}

/// Return the default bridge gateway used as the controlled listener address.
fn docker_bridge_gateway() -> Result<Ipv4Addr> {
    let output = Command::new("docker")
        .args([
            "network",
            "inspect",
            "bridge",
            "--format",
            "{{(index .IPAM.Config 0).Gateway}}",
        ])
        .output()
        .map_err(|error| Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "inspect the default bridge gateway",
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "inspect the default bridge gateway",
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|error| Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "parse the default bridge gateway",
            message: format!(
                "{}: {error}",
                String::from_utf8_lossy(&output.stdout).trim()
            ),
        })
}

/// Accept one connection before `deadline`, polling a nonblocking listener.
fn accept_before(listener: &TcpListener, deadline: Instant) -> std::io::Result<TcpStream> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out waiting for the Docker network probe",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Connect to a published guest port and collect its response before timeout.
fn exchange_before(address: SocketAddr, deadline: Instant) -> Result<Vec<u8>> {
    loop {
        if Instant::now() >= deadline {
            return Err(Error::Backend {
                sandbox: "docker".to_owned(),
                operation: "read published-port response",
                message: "timed out waiting for published-ok response".to_owned(),
            });
        }
        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Ok(mut stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .map_err(|error| Error::io("set published-port read timeout", &error))?;
                stream
                    .write_all(b"published-port probe\n")
                    .map_err(|error| Error::io("request published-port response", &error))?;
                let mut response = Vec::new();
                let mut chunk = [0; 1024];
                loop {
                    let read = match stream.read(&mut chunk) {
                        Ok(read) => read,
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) && Instant::now() < deadline =>
                        {
                            continue;
                        }
                        Err(error)
                            if error.kind() == std::io::ErrorKind::ConnectionReset
                                && Instant::now() < deadline =>
                        {
                            break;
                        }
                        Err(error) => {
                            return Err(Error::io("read published-port response", &error));
                        }
                    };
                    if read == 0 {
                        if response
                            .windows(b"published-ok\n".len())
                            .any(|window| window == b"published-ok\n")
                        {
                            return Ok(response);
                        }
                        break;
                    }
                    response.extend_from_slice(&chunk[..read]);
                    if response
                        .windows(b"published-ok\n".len())
                        .any(|window| window == b"published-ok\n")
                    {
                        return Ok(response);
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(Error::Backend {
                        sandbox: "docker".to_owned(),
                        operation: "connect to published container port",
                        message: error.to_string(),
                    });
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(Error::io("connect to published container port", &error)),
        }
    }
}

/// Build the bounded HTTP request the container uses to probe the listener.
fn http_probe_command(address: SocketAddr) -> String {
    format!("wget -q -T 3 -O - http://{address}/probe")
}

/// Confirm the host can connect before using this listener to test container policy.
fn prove_host_can_reach_listener(listener: &TcpListener, address: SocketAddr) -> Result<()> {
    let host = TcpStream::connect_timeout(&address, Duration::from_secs(3)).map_err(|error| {
        Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "connect the host positive control",
            message: error.to_string(),
        }
    })?;
    let _accepted_host =
        accept_before(listener, Instant::now() + Duration::from_secs(3)).map_err(|error| {
            Error::Backend {
                sandbox: "docker".to_owned(),
                operation: "accept the host positive control",
                message: error.to_string(),
            }
        })?;
    drop(host);
    Ok(())
}

/// A sandbox in its own namespace.
///
/// Every test allocates `box-0` from a fresh store, so without a per-test
/// namespace they would all fight over one container name on the daemon — which
/// is exactly the collision the namespace exists to prevent.
fn sandbox(namespace: &str) -> Result<DockerSandbox> {
    DockerSandbox::with_namespace(
        Arc::new(LocalHost::new()),
        Arc::new(MemoryStore::new()),
        namespace,
    )
}

fn spec_from(source: WorkspaceSource) -> Result<BoxSpec> {
    Ok(BoxSpec::new(
        Placement::new(HostRef::new("local")?, SandboxRef::new("docker")?),
        source,
    ))
}

fn spec() -> Result<BoxSpec> {
    spec_from(WorkspaceSource::OciImage(IMAGE.to_owned()))
}

/// Run `argv` in a box and return its trimmed standard output.
async fn output_of(
    sandbox: &DockerSandbox,
    id: &tinybox_core::BoxId,
    argv: &[&str],
) -> Result<String> {
    let output = sandbox.exec(id, &ExecRequest::new(argv.to_vec())).await?;
    Ok(output.stdout_lossy().trim().to_owned())
}

#[tokio::test]
async fn live_a_box_runs_a_command_in_a_real_container() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let sandbox = sandbox("ns-abraciarc")?;
    let info = sandbox.create(&spec()?).await?;

    let result = output_of(&sandbox, &info.id, &["echo", "from-the-container"]).await;

    sandbox.destroy(&info.id).await?;
    assert_eq!(result?, "from-the-container");
    Ok(())
}

#[tokio::test]
async fn live_the_box_cannot_see_host_processes() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let sandbox = sandbox("ns-tbcshp")?;
    let info = sandbox.create(&spec()?).await?;

    let result = output_of(
        &sandbox,
        &info.id,
        &["sh", "-c", "ls /proc | grep -c '^[0-9]*$'"],
    )
    .await;

    sandbox.destroy(&info.id).await?;

    // The host has hundreds of processes. A PID-namespaced container sees only
    // its own handful, so a large count here means the isolation is not real.
    let visible: usize = result?.parse().unwrap_or(usize::MAX);
    assert!(
        visible < 20,
        "expected a private process table, saw {visible} processes"
    );
    Ok(())
}

#[tokio::test]
async fn live_the_box_has_its_own_root_filesystem() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let sandbox = sandbox("ns-tbhiorf")?;
    let info = sandbox.create(&spec()?).await?;

    // A file that exists on the host checkout must not be visible inside.
    let result = output_of(
        &sandbox,
        &info.id,
        &[
            "sh",
            "-c",
            "test -e /etc/alpine-release && echo alpine; test -e /home/enamakel && echo LEAKED",
        ],
    )
    .await;

    sandbox.destroy(&info.id).await?;
    let seen = result?;
    assert!(seen.contains("alpine"));
    assert!(!seen.contains("LEAKED"), "the host filesystem was visible");
    Ok(())
}

#[tokio::test]
async fn live_a_denied_network_really_is_denied() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let sandbox = sandbox("ns-adnrid")?;
    let info = sandbox.create(&spec()?).await?;

    // With `--network none` the container has only a loopback interface.
    let result = output_of(&sandbox, &info.id, &["sh", "-c", "ip -o link | wc -l"]).await;

    sandbox.destroy(&info.id).await?;
    let interfaces: usize = result?.parse().unwrap_or(usize::MAX);
    assert_eq!(interfaces, 1, "expected loopback only");
    Ok(())
}

#[tokio::test]
async fn live_egress_is_available_when_the_policy_allows_it() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let sandbox = sandbox("ns-eiawtpai")?;
    let info = sandbox
        .create(&spec()?.with_network(NetworkPolicy::Egress))
        .await?;

    let result = output_of(&sandbox, &info.id, &["sh", "-c", "ip -o link | wc -l"]).await;

    sandbox.destroy(&info.id).await?;
    let interfaces: usize = result?.parse().unwrap_or(0);
    assert!(interfaces > 1, "expected more than loopback");
    Ok(())
}

/// Checks host, Egress, and default-Denied connectivity to a controlled listener.
#[tokio::test]
async fn live_network_policy_controls_host_listener_connectivity() -> Result<()> {
    if !live_docker_enabled()? {
        return Ok(());
    }

    let gateway = docker_bridge_gateway()?;
    let listener = TcpListener::bind((gateway, 0)).map_err(|error| Error::Backend {
        sandbox: "docker".to_owned(),
        operation: "bind the controlled bridge-gateway listener",
        message: error.to_string(),
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "configure the controlled listener",
            message: error.to_string(),
        })?;
    let address = listener.local_addr().map_err(|error| Error::Backend {
        sandbox: "docker".to_owned(),
        operation: "read the controlled listener address",
        message: error.to_string(),
    })?;

    // Prove the local endpoint is reachable before it is used as a sandbox
    // policy probe.
    prove_host_can_reach_listener(&listener, address)?;

    let egress_sandbox = sandbox("ns-docker-network-egress")?;
    let egress_info = egress_sandbox
        .create(&spec()?.with_network(NetworkPolicy::Egress))
        .await?;
    let egress_listener = listener.try_clone().map_err(|error| Error::Backend {
        sandbox: "docker".to_owned(),
        operation: "clone the controlled listener",
        message: error.to_string(),
    })?;
    let egress_server = thread::spawn(move || -> std::io::Result<()> {
        let mut stream = accept_before(&egress_listener, Instant::now() + Duration::from_secs(5))?;
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request)?;
        stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\ndocker-ok",
        )
    });
    let egress_result = egress_sandbox
        .exec(
            &egress_info.id,
            &ExecRequest::new(["sh", "-c", &http_probe_command(address)]),
        )
        .await;
    let egress_server_result = egress_server.join().map_err(|_| Error::Backend {
        sandbox: "docker".to_owned(),
        operation: "join the Egress listener",
        message: "listener thread panicked".to_owned(),
    })?;
    let egress_destroy = egress_sandbox.destroy(&egress_info.id).await;
    let egress = egress_result?;
    egress_destroy?;
    assert_eq!(
        egress.exit_code,
        0,
        "Egress container could not reach the listener: {}",
        egress.stderr_lossy()
    );
    egress_server_result.map_err(|error| Error::Backend {
        sandbox: "docker".to_owned(),
        operation: "accept the Egress container connection",
        message: error.to_string(),
    })?;
    assert_eq!(egress.stdout_lossy().trim(), "docker-ok");

    let denied_sandbox = sandbox("ns-docker-network-denied")?;
    let denied_info = denied_sandbox.create(&spec()?).await?;
    let denied = denied_sandbox
        .exec(
            &denied_info.id,
            &ExecRequest::new(["sh", "-c", &http_probe_command(address)]),
        )
        .await;
    let no_denied_connection = matches!(
        accept_before(&listener, Instant::now() + Duration::from_millis(250)),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut
    );
    let denied_destroy = denied_sandbox.destroy(&denied_info.id).await;
    let denied = denied?;
    denied_destroy?;
    assert_ne!(
        denied.exit_code, 0,
        "default-Denied container unexpectedly fetched the local response"
    );
    assert!(
        no_denied_connection,
        "default-Denied container established a connection to the controlled listener"
    );
    live_allowlist_pins_destinations_and_blocks_other_raw_tcp().await?;
    Ok(())
}

#[derive(Debug)]
struct FixedResolver(Vec<std::net::IpAddr>);

impl EgressResolver for FixedResolver {
    fn resolve(&self, _hostname: &str) -> std::io::Result<Vec<std::net::IpAddr>> {
        Ok(self.0.clone())
    }
}

fn docker_command(arguments: &[&str]) -> Result<String> {
    let output = Command::new("docker")
        .args(arguments)
        .output()
        .map_err(|error| Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "run the controlled egress listener",
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "run the controlled egress listener",
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn ipv6_address_inspect_template(network: &str) -> String {
    format!("{{{{(index .NetworkSettings.Networks \"{network}\").GlobalIPv6Address}}}}")
}

fn ipv6_mac_inspect_template(network: &str) -> String {
    format!("{{{{(index .NetworkSettings.Networks \"{network}\").MacAddress}}}}")
}

fn stale_neighbor_args(address: &str, mac: &str, bridge: &str) -> Vec<String> {
    [
        "-6", "neigh", "replace", address, "lladdr", mac, "nud", "stale", "dev", bridge,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn allowlist_probe_ports() -> tinybox_core::EgressAllowlist {
    tinybox_core::EgressAllowlist::new(["listener.test"], [18373, 18376])
}

const TARGET_LISTENER_SCRIPT: &str = "(while true; do nc -l -p 18373 >/dev/null; echo hit >> /tmp/allowed; done) & (while true; do nc -l -p 18374 >/dev/null; echo hit >> /tmp/blocked; done) & (while true; do nc -l -p 18376 >/dev/null; echo hit >> /tmp/allowed-v6; done) & (while true; do nc -u -l -p 53 >/dev/null; echo hit >> /tmp/dns; done) & wait";
fn ipv6_listener_probe(address: Ipv6Addr) -> String {
    format!(
        "unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy; nc -z -w 3 {address} 18376"
    )
}

#[test]
fn live_allowlist_covers_both_listener_probe_ports() {
    let allowlist = allowlist_probe_ports();
    assert!(allowlist.ports.contains(&18373));
    assert!(allowlist.ports.contains(&18376));
}

#[test]
fn alpine_busybox_nc_uses_literal_ipv6_without_unsupported_flag() {
    let address = Ipv6Addr::new(0xfd42, 0, 0, 0, 0, 0, 0, 0x1234);
    let probe = ipv6_listener_probe(address);
    assert!(TARGET_LISTENER_SCRIPT.contains("nc -l -p 18376"));
    assert!(!TARGET_LISTENER_SCRIPT.contains("nc -6"));
    assert!(probe.contains("nc -z -w 3 fd42::1234 18376"));
    assert!(!probe.contains("listener.test"));
    assert!(!probe.contains("nc -6"));
}

#[test]
fn ipv6_address_inspect_template_quotes_the_network_name() {
    assert_eq!(
        ipv6_address_inspect_template("controlled-v6"),
        r#"{{(index .NetworkSettings.Networks "controlled-v6").GlobalIPv6Address}}"#
    );
}

#[test]
fn stale_neighbor_command_targets_the_sandbox_ipv6_peer() {
    assert_eq!(
        ipv6_mac_inspect_template("managed-v6"),
        r#"{{(index .NetworkSettings.Networks "managed-v6").MacAddress}}"#
    );
    assert_eq!(
        stale_neighbor_args("fd42::10", "02:42:ac:11:00:02", "br-tinybox"),
        [
            "-6",
            "neigh",
            "replace",
            "fd42::10",
            "lladdr",
            "02:42:ac:11:00:02",
            "nud",
            "stale",
            "dev",
            "br-tinybox"
        ]
    );
}

#[test]
fn exchange_before_retries_a_connection_closed_before_the_response() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| Error::io("bind response retry test listener", &error))?;
    let address = listener
        .local_addr()
        .map_err(|error| Error::io("read response retry test address", &error))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| Error::io("set response retry listener nonblocking", &error))?;
    let server = thread::spawn(move || -> std::io::Result<()> {
        let (mut first, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => break Err(error)?,
            }
        };
        let mut request = [0; 64];
        let _ = first.read(&mut request)?;
        first.shutdown(std::net::Shutdown::Write)?;
        drop(first);
        let deadline = Instant::now() + Duration::from_secs(2);
        let (mut second, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => break Err(error)?,
            }
        };
        let mut request = [0; 64];
        let _ = second.read(&mut request)?;
        second.write_all(b"published-ok\n")?;
        Ok(())
    });

    let response = exchange_before(address, Instant::now() + Duration::from_secs(2))?;
    server
        .join()
        .map_err(|_| Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "join response retry test server",
            message: "server thread panicked".to_owned(),
        })?
        .map_err(|error| Error::io("serve response retry test", &error))?;
    assert!(
        response
            .windows(b"published-ok\n".len())
            .any(|window| window == b"published-ok\n")
    );
    Ok(())
}

/// A disposable Linux Engine proof for the native firewall controller.
///
/// Both listeners run in a separate, test-owned bridge container, so this
/// exercises raw TCP from the sandbox without opening a path to the Docker
/// host or relying on a public service.
// Keep setup, all three probes, assertions, and cleanup together so cleanup
// runs on every assertion path in the live integration test.
#[allow(clippy::too_many_lines)]
async fn live_allowlist_pins_destinations_and_blocks_other_raw_tcp() -> Result<()> {
    const ALLOWED_PORT: &str = "18373";
    const BLOCKED_PORT: &str = "18374";

    if !live_docker_enabled()? {
        return Ok(());
    }

    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let target_name = format!("tinybox-egress-target-{}", &suffix[..12]);
    let target_network = format!("tinybox-egress-v6-{}", &suffix[..12]);
    let target_subnet = format!("fd42:{}:5678::/64", &suffix[..4]);
    let gateway = docker_bridge_gateway()?;
    let host_listener = TcpListener::bind((gateway, 0)).map_err(|error| Error::Backend {
        sandbox: "docker".to_owned(),
        operation: "bind the host-gateway negative-control listener",
        message: error.to_string(),
    })?;
    host_listener
        .set_nonblocking(true)
        .map_err(|error| Error::io("configure host-gateway listener", &error))?;
    let host_address = host_listener
        .local_addr()
        .map_err(|error| Error::io("read host-gateway listener address", &error))?;
    let image = docker_command(&[
        "run",
        "-d",
        "--name",
        &target_name,
        IMAGE,
        "sh",
        "-c",
        TARGET_LISTENER_SCRIPT,
    ]);
    let _ = image?;
    if let Err(error) = docker_command(&[
        "network",
        "create",
        "--driver",
        "bridge",
        "--ipv6",
        "--subnet",
        &target_subnet,
        &target_network,
    ]) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &target_name])
            .status();
        return Err(error);
    }
    let result = async {
        docker_command(&["network", "connect", &target_network, &target_name])?;
        let ipv4 = docker_command(&[
            "inspect",
            "--format",
            "{{(index .NetworkSettings.Networks \"bridge\").IPAddress}}",
            &target_name,
        ])?
        .parse::<std::net::IpAddr>()
        .map_err(|error| Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "read the controlled listener address",
            message: error.to_string(),
        })?;
        let ipv6 = docker_command(&[
            "inspect",
            "--format",
            &ipv6_address_inspect_template(&target_network),
            &target_name,
        ])?
        .parse::<Ipv6Addr>()
        .map_err(|error| Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "read the controlled IPv6 listener address",
            message: error.to_string(),
        })?;

        let resolver: Arc<dyn EgressResolver> =
            Arc::new(FixedResolver(vec![ipv4, std::net::IpAddr::V6(ipv6)]));
        let namespace = format!("ns-egress-{}", &suffix[..8]);
        let sandbox = sandbox(&namespace)?
            .with_egress_controller(Arc::new(NativeLinuxDockerEgressController::new(resolver)));
        let allowlist = allowlist_probe_ports();
        let info = sandbox
            .create(
                &spec()?
                    .with_network(NetworkPolicy::Allowlist(allowlist))
                    .with_port(PortMapping::dynamic(18375)),
            )
            .await?;

        let probes = async {
        let container = format!("tinybox-{namespace}-{}", info.id);
        docker_command(&[
            "exec",
            "-d",
            &container,
                "sh",
                "-c",
                "while true; do printf 'published-ok\\n' | nc -l -p 18375; done",
        ])?;
        let published_port = sandbox
            .published_ports(&info.id)
            .await?
            .into_iter()
            .find(|mapping| mapping.guest == 18375)
            .and_then(|mapping| mapping.host)
            .ok_or_else(|| Error::Backend {
                sandbox: "docker".to_owned(),
                operation: "inspect published container port",
                message: "Docker did not assign a host port".to_owned(),
            })?;
        let published_response = exchange_before(
            SocketAddr::from((Ipv4Addr::LOCALHOST, published_port)),
            Instant::now() + Duration::from_secs(5),
        )?;

        let allowed = sandbox
            .exec(
                &info.id,
                &ExecRequest::new([
                    "sh",
                    "-c",
                    &format!("unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy; nc -z -w 3 listener.test {ALLOWED_PORT}"),
                ]),
            )
            .await;
        let allowed_v6 = sandbox
            .exec(
                &info.id,
                &ExecRequest::new([
                    "sh",
                    "-c",
                    &ipv6_listener_probe(ipv6),
                ]),
            )
            .await;
        let egress_network = docker_command(&[
            "inspect",
            "--format",
            "{{range $name, $endpoint := .NetworkSettings.Networks}}{{$name}}{{end}}",
            &container,
        ])?;
        let sandbox_ipv6 = docker_command(&[
            "inspect",
            "--format",
            &ipv6_address_inspect_template(&egress_network),
            &container,
        ])?;
        let sandbox_mac = docker_command(&[
            "inspect",
            "--format",
            &ipv6_mac_inspect_template(&egress_network),
            &container,
        ])?;
        let bridge = docker_command(&[
            "network",
            "inspect",
            "--format",
            "{{index .Options \"com.docker.network.bridge.name\"}}",
            &egress_network,
        ])?;
        let mut stale_args = vec!["-n".to_owned(), "ip".to_owned()];
        stale_args.extend(stale_neighbor_args(&sandbox_ipv6, &sandbox_mac, &bridge));
        let stale_neighbor = Command::new("sudo")
            .args(&stale_args)
            .output()
            .map_err(|error| Error::io("mark sandbox IPv6 neighbor stale", &error))?;
        if !stale_neighbor.status.success() {
            return Err(Error::Backend {
                sandbox: "docker".to_owned(),
                operation: "mark sandbox IPv6 neighbor stale",
                message: String::from_utf8_lossy(&stale_neighbor.stderr).trim().to_owned(),
            });
        }
        let allowed_v6_revalidated = sandbox
            .exec(
                &info.id,
                &ExecRequest::new(["sh", "-c", &ipv6_listener_probe(ipv6)]),
            )
            .await;
        let blocked = sandbox
            .exec(
                &info.id,
                &ExecRequest::new([
                    "sh",
                    "-c",
                    &format!("unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy; nc -z -w 2 {ipv4} {BLOCKED_PORT}"),
                ]),
            )
            .await;
        let host_gateway = sandbox
            .exec(
                &info.id,
                &ExecRequest::new([
                    "sh",
                    "-c",
                    &format!("unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy; nc -z -w 2 {} {}", host_address.ip(), host_address.port()),
                ]),
            )
            .await;
        let dns_blocked = sandbox
            .exec(
                &info.id,
                &ExecRequest::new([
                    "sh",
                    "-c",
                    "unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy; nslookup -timeout=1 -retry=1 unlisted-name.test >/dev/null 2>&1",
                ]),
            )
            .await;
        let direct_dns = sandbox
            .exec(
                &info.id,
                &ExecRequest::new([
                    "sh",
                    "-c",
                    &format!("printf probe | nc -u -w 1 {ipv4} 53 || true"),
                ]),
            )
            .await;
        let host_listener_untouched = matches!(
            accept_before(&host_listener, Instant::now() + Duration::from_millis(250)),
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut
        );
        let allowed_hits =
            docker_command(&["exec", &target_name, "sh", "-c", "test -s /tmp/allowed"]);
        let allowed_v6_hits = docker_command(&[
            "exec",
            &target_name,
            "sh",
            "-c",
            "test -s /tmp/allowed-v6",
        ]);
        let blocked_hits = Command::new("docker")
            .args(["exec", &target_name, "sh", "-c", "test ! -s /tmp/blocked"])
            .status()
            .map_err(|error| Error::io("check forbidden listener", &error));
        let dns_hits = Command::new("docker")
            .args(["exec", &target_name, "sh", "-c", "test ! -s /tmp/dns"])
            .status()
            .map_err(|error| Error::io("check forbidden DNS listener", &error));

        let allowed = allowed?;
        let allowed_v6 = allowed_v6?;
        let allowed_v6_revalidated = allowed_v6_revalidated?;
        let blocked = blocked?;
        let host_gateway = host_gateway?;
        let dns_blocked = dns_blocked?;
        direct_dns?;
        allowed_hits?;
        allowed_v6_hits?;
        let blocked_hits = blocked_hits?;
        let dns_hits = dns_hits?;
        if allowed.exit_code != 0
            || allowed_v6.exit_code != 0
            || allowed_v6_revalidated.exit_code != 0
            || blocked.exit_code == 0
            || host_gateway.exit_code == 0
            || dns_blocked.exit_code == 0
            || !published_response
                .windows(b"published-ok\n".len())
                .any(|window| window == b"published-ok\n")
            || !blocked_hits.success()
            || !dns_hits.success()
            || !host_listener_untouched
        {
            return Err(Error::Backend {
                sandbox: "docker".to_owned(),
                operation: "verify native egress allowlist",
                message: format!(
                    "allowed v4 exit={}, allowed v6 exit={}, revalidated allowed v6 exit={}, blocked exit={}, host-gateway exit={}, unlisted DNS lookup exit={}, published response={published_response:?}, forbidden TCP hit={}, forbidden DNS hit={}, host listener touched={}",
                    allowed.exit_code,
                    allowed_v6.exit_code,
                    allowed_v6_revalidated.exit_code,
                    blocked.exit_code,
                    host_gateway.exit_code,
                    dns_blocked.exit_code,
                    !blocked_hits.success(),
                    !dns_hits.success(),
                    !host_listener_untouched
                ),
            });
        }
        Ok(())
        }
        .await;
        let destroy = sandbox.destroy(&info.id).await;
        probes?;
        destroy?;
        Ok(())
    }
    .await;
    let cleanup = Command::new("docker")
        .args(["rm", "-f", &target_name])
        .status()
        .map_err(|error| Error::io("remove controlled listener", &error));
    let network_cleanup = Command::new("docker")
        .args(["network", "rm", &target_network])
        .status()
        .map_err(|error| Error::io("remove controlled IPv6 listener network", &error));
    result?;
    let cleanup = cleanup?;
    let network_cleanup = network_cleanup?;
    if !cleanup.success() || !network_cleanup.success() {
        return Err(Error::Backend {
            sandbox: "docker".to_owned(),
            operation: "remove controlled listeners",
            message: "docker rm or docker network rm returned a failure status".to_owned(),
        });
    }
    Ok(())
}

#[tokio::test]
async fn live_a_memory_limit_is_actually_enforced() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let sandbox = sandbox("ns-amliae")?;
    let spec = spec()?.with_resources(tinybox_core::Resources {
        memory_bytes: 64 * 1024 * 1024,
        ..tinybox_core::Resources::DEFAULT
    });
    let info = sandbox.create(&spec).await?;

    // Reading `memory.max` would only prove the cgroup was *configured*, and an
    // earlier version of this test did exactly that. So this allocates for
    // real. The allocation is anonymous memory held by the shell rather than a
    // file write: a write to `/tmp` measures disk in Docker and memory under
    // namespaces, which made an earlier probe silently test nothing at all.
    let over = output_of(
        &sandbox,
        &info.id,
        &["sh", "-c", "x=$(yes a | head -c 100000000); echo SURVIVED"],
    )
    .await;

    // ...and a modest allocation under the same cap still works, so this is
    // measuring a limit rather than a broken container.
    let under = output_of(
        &sandbox,
        &info.id,
        &["sh", "-c", "x=$(yes a | head -c 8000000); echo ok"],
    )
    .await;

    sandbox.destroy(&info.id).await?;
    assert!(
        !over?.contains("SURVIVED"),
        "a 100 MiB allocation was not stopped by a 64 MiB cap"
    );
    assert_eq!(under?, "ok");
    Ok(())
}

#[tokio::test]
async fn live_a_local_directory_is_visible_inside_the_box() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let dir = tempfile::tempdir().map_err(|error| Error::io("tempdir", &error))?;
    std::fs::write(dir.path().join("note.txt"), "mounted\n")
        .map_err(|error| Error::io("write", &error))?;

    let sandbox = sandbox("ns-localdir")?;
    let info = sandbox
        .create(&spec_from(WorkspaceSource::LocalDir(dir.path().into()))?)
        .await?;

    // The bind mount is also the working directory, so a bare filename resolves.
    let result = output_of(&sandbox, &info.id, &["cat", "note.txt"]).await;

    sandbox.destroy(&info.id).await?;
    assert_eq!(result?, "mounted");
    Ok(())
}

#[tokio::test]
async fn live_a_snapshot_captures_a_write_and_a_fork_starts_from_it() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let sandbox = sandbox("ns-ascawaafsfi")?;
    let parent = sandbox.create(&spec()?).await?;

    // Write into the parent, then capture it.
    sandbox
        .exec(
            &parent.id,
            &ExecRequest::new(["sh", "-c", "echo captured > /marker"]),
        )
        .await?;
    let snapshot = sandbox.snapshot(&parent.id).await?;

    let forked = sandbox.fork(&snapshot, &spec()?).await?;

    // The fork inherits the parent's filesystem...
    let inherited = output_of(&sandbox, &forked.id, &["cat", "/marker"]).await;

    // ...and writes in the fork do not reach back into the parent.
    sandbox
        .exec(
            &forked.id,
            &ExecRequest::new(["sh", "-c", "echo fork-only > /fork-marker"]),
        )
        .await?;
    let parent_sees_fork = output_of(
        &sandbox,
        &parent.id,
        &[
            "sh",
            "-c",
            "test -e /fork-marker && echo LEAKED || echo isolated",
        ],
    )
    .await;

    sandbox.destroy(&forked.id).await?;
    sandbox.destroy(&parent.id).await?;

    assert_eq!(inherited?, "captured");
    assert_eq!(parent_sees_fork?, "isolated");
    Ok(())
}

#[tokio::test]
async fn live_a_destroyed_box_leaves_no_container_behind() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let sandbox = sandbox("ns-adblncb")?;
    let info = sandbox.create(&spec()?).await?;
    let name = tinybox_docker::container_name(sandbox.namespace(), &info.id);

    sandbox.destroy(&info.id).await?;

    let listed = LocalHost::new()
        .run(&ExecRequest::new([
            "docker",
            "ps",
            "--all",
            "--quiet",
            "--filter",
            &format!("name=^{name}$"),
        ]))
        .await?;

    assert!(
        listed.stdout_lossy().trim().is_empty(),
        "container {name} survived destroy"
    );
    Ok(())
}

#[tokio::test]
async fn live_a_missing_image_is_reported_with_dockers_diagnostic() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let sandbox = sandbox("ns-amiirwdd")?;

    let outcome = sandbox
        .create(&spec_from(WorkspaceSource::OciImage(
            "tinybox.invalid/no-such-image:0".to_owned(),
        ))?)
        .await;

    assert!(matches!(
        outcome,
        Err(Error::Backend {
            operation: "create the container",
            ..
        })
    ));
    Ok(())
}
