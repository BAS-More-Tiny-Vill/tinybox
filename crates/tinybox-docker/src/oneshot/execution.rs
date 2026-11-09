//! Supervised one-shot ownership and bounded timeout cleanup.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Write};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{
    DockerCli, MAX_OUTPUT_BYTES, NO_SUCH_CONTAINER, OBSERVED_LIMITS_FORMAT, ObservedLimits,
    OneShot, OneShotOutcome, VerifiedOneShot, cap, first_line,
};

/// The cleanup commands have their own short bounds, independent of the job.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// A plain run is a verified run whose observed limits the caller does not
/// need: the probe, read-back and refusal all apply.
pub(super) async fn run(
    cli: DockerCli,
    spec: OneShot,
    timeout: Duration,
) -> io::Result<OneShotOutcome> {
    run_verified(cli, spec, timeout)
        .await
        .map(|verified| verified.outcome)
}

/// Validate the spec, probe that the daemon can enforce the limits, create
/// the container, read its limits back, and start it only when the effective
/// limits are in force. `timeout` bounds every step.
pub(super) async fn run_verified(
    cli: DockerCli,
    spec: OneShot,
    timeout: Duration,
) -> io::Result<VerifiedOneShot> {
    spec.validate()?;
    let name = unique_name();
    let started = Instant::now();
    let deadline = started + timeout;
    // A stored HostConfig is not a kernel control. Refuse before anything is
    // created when the daemon reports it cannot enforce a limit we rely on.
    let enforcement = probe_enforcement(&cli, deadline).await?;
    if let Some(reason) = enforcement.refusal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("container limits cannot be enforced by this Docker daemon: {reason}"),
        ));
    }
    // The workload environment goes to the container through a mode-0600
    // file; the client itself runs with this process's environment only.
    let env_file = write_env_file(&spec)?;
    let mut args = spec.argv_with_env_file(env_file.as_ref().map(tempfile::NamedTempFile::path));
    args[0] = OsString::from("create");
    args.splice(2..2, ["--name".into(), name.clone().into()]);
    let mut create = Command::new(&cli.program);
    create.args(args);
    let created = bounded_output(create, deadline, "docker create").await;
    // `docker create` has read the file into the container's configuration
    // (or failed, or been killed at its deadline): delete it now.
    drop(env_file);
    let created = match created {
        Ok(created) if created.success => created,
        Ok(created) => {
            let removal = remove(&cli, &name).await;
            return Err(io::Error::other(format!(
                "docker create failed: {}; {}",
                first_line(created.stderr.as_bytes()),
                removal.report(&name)
            )));
        }
        Err(error) => {
            // A create that overran its deadline may still have made the container.
            let removal = remove(&cli, &name).await;
            return Err(with_removal(&error, removal, &name));
        }
    };
    let id = created.stdout.trim().to_owned();
    if id.is_empty() || id.contains(char::is_whitespace) {
        let removal = remove(&cli, &name).await;
        return Err(with_removal(
            &io::Error::other("docker create returned no container id"),
            removal,
            &name,
        ));
    }

    let observed = match read_back(&cli, &id, deadline).await {
        Ok(observed) => observed,
        Err(error) => {
            let removal = remove(&cli, &id).await;
            return Err(with_removal(&error, removal, &id));
        }
    };
    let expected = spec.expected_limits();
    if observed != expected {
        let removal = remove(&cli, &id).await;
        return Err(with_removal(
            &io::Error::other(format!(
                "container limits are not in force: expected {expected:?}, observed {observed:?}"
            )),
            removal,
            &id,
        ));
    }

    let mut start = Command::new(&cli.program);
    start.args(["start", "-a", &id]);
    let remaining = deadline.saturating_duration_since(Instant::now());
    let outcome = attach(
        &cli,
        start,
        Owned::Created {
            name: &name,
            id: &id,
        },
        started,
        remaining,
        timeout,
    )
    .await?;
    Ok(VerifiedOneShot { outcome, observed })
}

fn unique_name() -> String {
    format!("tinybox-{}", uuid::Uuid::new_v4())
}

/// Write the spec's (already validated) environment to a mode-0600 temporary
/// `--env-file`, one `NAME=value` line each. Docker reads such a file
/// literally: no quoting, and a line without `=` would copy the client's own
/// variable, which is why every line has one. Dropping the handle deletes
/// the file.
fn write_env_file(spec: &OneShot) -> io::Result<Option<tempfile::NamedTempFile>> {
    if spec.env.is_empty() {
        return Ok(None);
    }
    let mut file = tempfile::Builder::new()
        .prefix("tinybox-oneshot-env-")
        .tempfile()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let mut contents = String::new();
    for (key, value) in &spec.env {
        let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid one-shot request: environment is not UTF-8",
            ));
        };
        contents.push_str(key);
        contents.push('=');
        contents.push_str(value);
        contents.push('\n');
    }
    file.write_all(contents.as_bytes())?;
    file.flush()?;
    Ok(Some(file))
}

/// The `docker info --format` template for the enforcement probe.
const ENFORCEMENT_PROBE_FORMAT: &str = "{{json .}}";

/// The client environment that selects a daemon (and so decides what the
/// probe describes).
const DOCKER_HOST_CONFIG: [&str; 5] = [
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CERT_PATH",
];

/// Which daemon a probe describes: the client program plus its host config.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProbeKey {
    program: OsString,
    host_config: Vec<Option<OsString>>,
}

impl ProbeKey {
    fn current(cli: &DockerCli) -> Self {
        Self {
            program: cli.program.clone(),
            host_config: DOCKER_HOST_CONFIG.iter().map(std::env::var_os).collect(),
        }
    }
}

/// Probes that reported full enforcement, for the life of the process.
fn probe_cache() -> &'static Mutex<HashMap<ProbeKey, LimitEnforcement>> {
    static CACHE: OnceLock<Mutex<HashMap<ProbeKey, LimitEnforcement>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// What `docker info` reports the daemon can enforce (Engine API `Info`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct LimitEnforcement {
    /// `MemoryLimit`: the memory controller is available.
    memory_limit: bool,
    /// `PidsLimit`: the pids controller is available.
    pids_limit: bool,
    /// `CgroupVersion`, `"1"` or `"2"`; empty when not reported.
    cgroup_version: String,
    /// `CgroupDriver`, `"systemd"` or `"cgroupfs"`; empty when not reported.
    cgroup_driver: String,
    /// `SecurityOptions` contains `name=rootless`.
    rootless: bool,
}

impl LimitEnforcement {
    /// Parse `docker info --format '{{json .}}'`. `MemoryLimit` and
    /// `PidsLimit` must be present booleans; anything else is `None`, so an
    /// unreadable probe never reads as "enforced".
    fn parse(json: &str) -> Option<Self> {
        let info: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
        let text = |key: &str| {
            info.get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let rootless = info
            .get("SecurityOptions")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|options| {
                options
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .any(|option| option.split(',').any(|part| part == "name=rootless"))
            });
        Some(Self {
            memory_limit: info.get("MemoryLimit")?.as_bool()?,
            pids_limit: info.get("PidsLimit")?.as_bool()?,
            cgroup_version: text("CgroupVersion"),
            cgroup_driver: text("CgroupDriver"),
            rootless,
        })
    }

    /// Why a one-shot's limits cannot be enforced, or `None` when they can.
    /// The process cap and the memory limit always apply: a zero memory
    /// limit is refused as an invalid request before the probe.
    fn refusal(&self) -> Option<String> {
        if !self.pids_limit {
            return Some(
                "docker info reports PidsLimit=false, so --pids-limit is not enforced".into(),
            );
        }
        if !self.memory_limit {
            return Some("docker info reports MemoryLimit=false, so -m is not enforced".into());
        }
        if self.rootless && (self.cgroup_version != "2" || self.cgroup_driver != "systemd") {
            return Some(format!(
                "a rootless daemon enforces limits only with cgroup v2 and the systemd driver, \
                 but reports CgroupVersion={:?} CgroupDriver={:?}",
                self.cgroup_version, self.cgroup_driver
            ));
        }
        None
    }
}

/// What the daemon can enforce: a cached full-enforcement probe for this
/// client and host config, or a fresh probe. Only a probe that reports full
/// enforcement is cached, so a failed, unreadable or refusing probe is
/// repeated on the next run.
async fn probe_enforcement(cli: &DockerCli, deadline: Instant) -> io::Result<LimitEnforcement> {
    let key = ProbeKey::current(cli);
    let cached = probe_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).cloned());
    if let Some(cached) = cached {
        return Ok(cached);
    }
    let probed = probe_daemon(cli, deadline).await?;
    if probed.refusal().is_none()
        && let Ok(mut cache) = probe_cache().lock()
    {
        cache.insert(key, probed.clone());
    }
    Ok(probed)
}

/// Ask the daemon what it can enforce, before `deadline`. Fails closed: a
/// probe that errors, times out, or cannot be read is an error.
async fn probe_daemon(cli: &DockerCli, deadline: Instant) -> io::Result<LimitEnforcement> {
    let mut info = Command::new(&cli.program);
    info.args(["info", "--format", ENFORCEMENT_PROBE_FORMAT]);
    let out = bounded_output(info, deadline, "docker info")
        .await
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("limit enforcement probe failed: {error}"),
            )
        })?;
    if !out.success {
        return Err(io::Error::other(format!(
            "limit enforcement probe failed: docker info: {}",
            first_line(out.stderr.as_bytes())
        )));
    }
    // An unreachable daemon still exits 0 (Docker 27) with MemoryLimit and
    // PidsLimit false: that is a failed probe, not an enforcement refusal.
    if let Some(server_error) = server_error(&out.stdout) {
        return Err(io::Error::other(format!(
            "limit enforcement probe failed: docker info: {server_error}"
        )));
    }
    LimitEnforcement::parse(&out.stdout).ok_or_else(|| {
        io::Error::other(
            "limit enforcement probe failed: docker info did not report MemoryLimit and PidsLimit",
        )
    })
}

/// The first entry of a non-empty `ServerErrors` in `docker info` JSON: the
/// client could not get the facts from the daemon, so none can be trusted.
fn server_error(json: &str) -> Option<String> {
    let info: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let errors = info.get("ServerErrors")?.as_array()?;
    let first = errors.first()?;
    Some(
        first
            .as_str()
            .map_or_else(|| first.to_string(), str::to_owned),
    )
}

/// The container an attached client owns, which decides its failure cleanup.
#[derive(Debug, Clone, Copy)]
enum Owned<'a> {
    /// `docker start -a` of a container already created with this `id`.
    Created { name: &'a str, id: &'a str },
}

/// What `docker rm -f` said about a container.
///
/// Docker 27 exits 0 for `rm -f` of a container that does not exist and
/// prints `No such container` on stderr; older clients exit 1 with the same
/// message. So that message means [`Removal::Absent`] whatever the exit
/// code, and only an exit 0 without it is a confirmed [`Removal::Removed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Removal {
    /// Docker confirmed it removed the container.
    Removed,
    /// Docker reported there was no such container to remove.
    Absent,
    /// Docker did not confirm anything (failure, timeout, no client).
    Unconfirmed,
}

impl Removal {
    /// The cleanup result, for an error message.
    fn report(self, target: &str) -> String {
        match self {
            Self::Removed => format!("container {target} was removed"),
            Self::Absent => format!(
                "docker rm -f {target} reported No such container, so no container {target} exists"
            ),
            Self::Unconfirmed => format!(
                "CLEANUP FAILED: docker rm -f {target} did not confirm removal, \
                 the container may still exist"
            ),
        }
    }

    /// The cleanup result, as a suffix for a timeout message.
    fn timeout_note(self, target: &str) -> String {
        match self {
            Self::Removed => String::new(),
            Self::Absent => format!("; docker rm -f {target} reported No such container"),
            Self::Unconfirmed => format!("; docker rm -f {target} did not confirm removal"),
        }
    }
}

fn with_removal(error: &io::Error, removal: Removal, target: &str) -> io::Error {
    io::Error::new(error.kind(), format!("{error}; {}", removal.report(target)))
}

/// Force-remove `target` (a name or id) and say what Docker confirmed.
async fn remove(cli: &DockerCli, target: &str) -> Removal {
    let mut rm = Command::new(&cli.program);
    rm.args(["rm", "-f", target]);
    match bounded_output(rm, Instant::now() + CLEANUP_TIMEOUT, "docker rm -f").await {
        Ok(out) if out.stderr.contains(NO_SUCH_CONTAINER) => Removal::Absent,
        Ok(out) if out.success => Removal::Removed,
        _ => Removal::Unconfirmed,
    }
}

/// Clean up after an attached client failed with `error`, and return the
/// error to report. A created container is removed by id; the error says
/// whether Docker confirmed the removal, and never claims it when it did not.
async fn after_failure(cli: &DockerCli, owned: Owned<'_>, error: &io::Error) -> io::Error {
    let Owned::Created { id, .. } = owned;
    with_removal(error, remove(cli, id).await, id)
}

/// Run an attached client (`start -a`) to completion or `remaining`, draining
/// output with bounded memory. On a timeout it stops only the container it
/// owns; the created container is also removed by id whenever the start does
/// not complete. `timeout` is the caller's whole budget, for the timeout
/// message.
async fn attach(
    cli: &DockerCli,
    mut command: Command,
    owned: Owned<'_>,
    started: Instant,
    remaining: Duration,
    timeout: Duration,
) -> io::Result<OneShotOutcome> {
    let spawned = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return Err(after_failure(cli, owned, &error).await),
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.start_kill();
        return Err(after_failure(cli, owned, &io::Error::other("missing output pipe")).await);
    };
    let execution = async {
        tokio::try_join!(
            read_output(stdout, "\n... [output truncated at 1MB]"),
            read_output(stderr, "\n... [stderr truncated at 1MB]"),
            child.wait(),
        )
    };
    match tokio::time::timeout(remaining, Box::pin(execution)).await {
        Ok(Ok((stdout, stderr, status))) => Ok(OneShotOutcome {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
            timed_out: false,
            killed: None,
            duration_ms: elapsed_ms(started),
        }),
        Ok(Err(error)) => Err(after_failure(cli, owned, &error).await),
        Err(_) => {
            // Killing the client cannot stop its daemon-owned container.
            // Address only this invocation, never every job sharing a label.
            let _ = child.start_kill();
            let Owned::Created { name, id } = owned;
            let (killed, removal) = stop_container(cli, name, id).await;
            Ok(OneShotOutcome {
                exit_code: -1,
                stdout: String::new(),
                stderr: format!(
                    "Command timed out after {}s and {}{}",
                    timeout.as_secs(),
                    if killed {
                        "the container was killed"
                    } else {
                        "THE KILL FAILED - container may still be running"
                    },
                    removal.timeout_note(id)
                ),
                timed_out: true,
                killed: Some(killed),
                duration_ms: elapsed_ms(started),
            })
        }
    }
}

async fn read_back(cli: &DockerCli, id: &str, deadline: Instant) -> io::Result<ObservedLimits> {
    let mut inspect = Command::new(&cli.program);
    inspect.args(["inspect", "--format", OBSERVED_LIMITS_FORMAT, id]);
    let out = bounded_output(inspect, deadline, "docker inspect").await?;
    if !out.success {
        return Err(io::Error::other(format!(
            "docker inspect failed: {}",
            first_line(out.stderr.as_bytes())
        )));
    }
    ObservedLimits::parse(&out.stdout).ok_or_else(|| {
        io::Error::other(format!(
            "docker inspect output is not a limit line: {:?}",
            first_line(out.stdout.as_bytes())
        ))
    })
}

/// Captured output of a short control command (`create`, `inspect`, `rm`).
struct Captured {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run a short control command to completion before `deadline`, draining its
/// output with the same bounded memory as the workload.
async fn bounded_output(
    mut command: Command,
    deadline: Instant,
    what: &str,
) -> io::Result<Captured> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr pipe"))?;
    let execution = async {
        tokio::try_join!(
            read_output(stdout, "\n... [output truncated at 1MB]"),
            read_output(stderr, "\n... [stderr truncated at 1MB]"),
            child.wait(),
        )
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let (stdout, stderr, status) = tokio::time::timeout(remaining, Box::pin(execution))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{what} exceeded its deadline"),
            )
        })??;
    Ok(Captured {
        success: status.success(),
        stdout,
        stderr,
    })
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Stop the owned container after a timeout: kill it by its unique `name`,
/// then remove it by `id` (a container that never reached running cannot be
/// killed, and `--rm` removes only on exit). It counts as killed only when
/// Docker confirmed the kill, or confirmed the removal.
async fn stop_container(cli: &DockerCli, name: &str, id: &str) -> (bool, Removal) {
    let killed = control(cli, &["kill", name]).await;
    let removal = remove(cli, id).await;
    (killed || removal == Removal::Removed, removal)
}

async fn control(cli: &DockerCli, args: &[&str]) -> bool {
    tokio::time::timeout(
        CLEANUP_TIMEOUT,
        Command::new(&cli.program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .is_ok_and(|result| result.is_ok_and(|status| status.success()))
}

async fn read_output(mut reader: impl AsyncRead + Unpin, note: &str) -> io::Result<String> {
    // Keep enough extra bytes to finish a UTF-8 character at the boundary.
    // Continue draining so a verbose child never blocks on a full pipe.
    let limit = MAX_OUTPUT_BYTES + 4;
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let take = count.min(limit.saturating_sub(kept.len()));
        kept.extend_from_slice(&buffer[..take]);
    }
    let mut text = String::from_utf8_lossy(&kept).into_owned();
    cap(&mut text, note);
    Ok(text)
}

#[cfg(all(test, unix))]
#[path = "execution_tests.rs"]
mod tests;
