//! One-shot containers: one `docker run --rm` per command.
//!
//! The long-lived [`DockerSandbox`](crate::DockerSandbox) keeps a container
//! open and `docker exec`s into it. Some callers want the opposite contract:
//! every command gets a **fresh** container, nothing persists between commands,
//! and the container removes itself when the command exits. That is what this
//! module provides. Its command line retains explicit resource and mount policy,
//! with a mandatory non-root identity and process-count limit. A caller may
//! choose another non-root identity or a tighter process cap, never root or a
//! looser cap. Every run, [`DockerCli::run_one_shot`] as well as
//! [`DockerCli::run_one_shot_verified`], probes that the daemon can enforce
//! the limits and reads the effective limits back before the command starts.
//! Request environment reaches the container through a mode-0600 env file,
//! never the docker client's own process environment.
//!
//! The command line is built by [`OneShot::argv`], a pure function (apart from
//! resolving symlinks in mount paths), so it can be tested without a daemon.
//! [`DockerCli`] runs it, probes the daemon, and kills labelled stragglers; its
//! program name is injectable so tests can substitute a fake binary.

mod execution;

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Where the workspace appears inside a one-shot container.
pub const ONE_SHOT_WORKSPACE: &str = "/workspace";

/// The image used when the caller names none.
pub const DEFAULT_ONE_SHOT_IMAGE: &str = "alpine:3.20";

/// The network mode used when the caller names none: no network at all.
pub const DEFAULT_ONE_SHOT_NETWORK: &str = "none";

/// The memory limit, in mebibytes, used when the caller names none.
pub const DEFAULT_ONE_SHOT_MEMORY_MB: u64 = 512;

/// The CPU limit, in cores, used when the caller names none.
pub const DEFAULT_ONE_SHOT_CPUS: f64 = 1.0;

/// How many bytes of stdout or stderr a [`OneShotOutcome`] keeps (1 MiB).
pub const MAX_OUTPUT_BYTES: usize = 1_048_576;

/// The process cap every one-shot container runs with. A caller may tighten it
/// with [`OneShot::with_pids_limit`], never loosen it.
pub const MAX_ONE_SHOT_PIDS_LIMIT: u64 = 256;

/// The `docker inspect --format` template that reads a container's limits
/// back: memory in bytes, the pids limit, and the configured user.
pub const OBSERVED_LIMITS_FORMAT: &str =
    "{{.HostConfig.Memory}} {{.HostConfig.PidsLimit}} {{.Config.User}}";

/// The limits Docker reports for a container, read back with
/// [`OBSERVED_LIMITS_FORMAT`].
///
/// "The flag was passed" and "the limit is in force" are different claims;
/// only the second is a control. [`DockerCli::run_one_shot_verified`] compares
/// this against [`OneShot::expected_limits`] before the command starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedLimits {
    /// `HostConfig.Memory`, in bytes.
    pub memory_bytes: u64,
    /// `HostConfig.PidsLimit`; `None` when Docker reports it unset or unlimited.
    pub pids_limit: Option<i64>,
    /// `Config.User`; empty when the image default (usually root) applies.
    pub user: String,
}

impl ObservedLimits {
    /// Parse one line of `docker inspect --format` [`OBSERVED_LIMITS_FORMAT`]
    /// output. Returns `None` for anything that is not exactly that shape.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split_whitespace();
        let memory_bytes = fields.next()?.parse().ok()?;
        let pids_limit = match fields.next()? {
            "<nil>" => None,
            value => {
                let n: i64 = value.parse().ok()?;
                (n > 0).then_some(n)
            }
        };
        let user = fields.next().unwrap_or("").to_owned();
        if fields.next().is_some() {
            return None;
        }
        Some(Self {
            memory_bytes,
            pids_limit,
            user,
        })
    }
}

/// A one-shot run whose limits were read back before the command started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedOneShot {
    /// What the command did.
    pub outcome: OneShotOutcome,
    /// The limits Docker reported for the container.
    pub observed: ObservedLimits,
}

/// A single containerized command: the image, limits, mounts, and environment
/// it runs with.
///
/// Build one with [`OneShot::new`] (safe defaults: no network, read-only root
/// filesystem, all capabilities dropped) and adjust it with the `with_*`
/// methods.
#[derive(Debug, Clone, PartialEq)]
pub struct OneShot {
    image: String,
    network: String,
    extra_cap_drops: Vec<String>,
    memory_mb: u64,
    cpus: f64,
    read_only_rootfs: bool,
    workspace: PathBuf,
    mount_workspace: bool,
    read_only_mounts: Vec<PathBuf>,
    env: Vec<(OsString, OsString)>,
    label: Option<String>,
    command: String,
    login_shell: bool,
    user: Option<String>,
    pids_limit: Option<u64>,
}

impl OneShot {
    /// Run `command` (through `sh -c`) with `workspace` mounted read-write at
    /// [`ONE_SHOT_WORKSPACE`].
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>, command: impl Into<String>) -> Self {
        Self {
            image: DEFAULT_ONE_SHOT_IMAGE.to_owned(),
            network: DEFAULT_ONE_SHOT_NETWORK.to_owned(),
            extra_cap_drops: Vec::new(),
            memory_mb: DEFAULT_ONE_SHOT_MEMORY_MB,
            cpus: DEFAULT_ONE_SHOT_CPUS,
            read_only_rootfs: true,
            workspace: workspace.into(),
            mount_workspace: true,
            read_only_mounts: Vec::new(),
            env: Vec::new(),
            label: None,
            command: command.into(),
            login_shell: false,
            user: None,
            pids_limit: None,
        }
    }

    /// Run the command as numeric `uid:gid` (`--user`) instead of the default
    /// non-root workspace owner (or `65534:65534`). Root is never honoured:
    /// a `uid` 0 or a primary `gid` 0 (the root group) keeps that non-root
    /// default.
    #[must_use]
    pub fn with_user(mut self, uid: u32, gid: u32) -> Self {
        self.user = (uid != 0 && gid != 0).then(|| format!("{uid}:{gid}"));
        self
    }

    /// Tighten the process cap (`--pids-limit`) below the mandatory
    /// [`MAX_ONE_SHOT_PIDS_LIMIT`]. The value is clamped to
    /// `1..=MAX_ONE_SHOT_PIDS_LIMIT`, so it can never loosen the cap.
    #[must_use]
    pub fn with_pids_limit(mut self, pids_limit: u64) -> Self {
        self.pids_limit = Some(pids_limit);
        self
    }

    /// The `--user` the container runs as: the caller's non-root identity, or
    /// the non-root workspace owner, or `65534:65534`.
    fn effective_user(&self) -> String {
        self.user.clone().unwrap_or_else(|| {
            sandbox_user_for(self.mount_workspace.then_some(self.workspace.as_path()))
        })
    }

    /// The `--pids-limit` the container runs with; never above the mandatory cap.
    fn effective_pids_limit(&self) -> u64 {
        self.pids_limit.map_or(MAX_ONE_SHOT_PIDS_LIMIT, |n| {
            n.clamp(1, MAX_ONE_SHOT_PIDS_LIMIT)
        })
    }

    /// The limits Docker should report for a container started from this
    /// spec, in the shape [`ObservedLimits::parse`] returns. These are the
    /// effective values, whether the caller supplied them or they defaulted.
    #[must_use]
    pub fn expected_limits(&self) -> ObservedLimits {
        ObservedLimits {
            memory_bytes: self.memory_mb.saturating_mul(1024 * 1024),
            pids_limit: Some(i64::try_from(self.effective_pids_limit()).unwrap_or(i64::MAX)),
            user: self.effective_user(),
        }
    }

    /// Run without mounting a host workspace or changing the container working directory.
    #[must_use]
    pub fn without_workspace(mut self) -> Self {
        self.mount_workspace = false;
        self
    }

    /// Execute through a login POSIX shell (`sh -lc`).
    #[must_use]
    pub fn with_login_shell(mut self) -> Self {
        self.login_shell = true;
        self
    }

    /// The image to run in.
    #[must_use]
    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    /// The `--network` mode (`none`, `bridge`, ...).
    #[must_use]
    pub fn with_network(mut self, network: impl Into<String>) -> Self {
        self.network = network.into();
        self
    }

    /// Capabilities to drop in addition to the unconditional `--cap-drop ALL`.
    #[must_use]
    pub fn with_extra_cap_drops(mut self, caps: impl IntoIterator<Item = String>) -> Self {
        self.extra_cap_drops = caps.into_iter().collect();
        self
    }

    /// The memory limit in mebibytes. Docker reads `0` as "unlimited", so a
    /// run with a zero limit is refused as an invalid request.
    #[must_use]
    pub fn with_memory_mb(mut self, memory_mb: u64) -> Self {
        self.memory_mb = memory_mb;
        self
    }

    /// The CPU limit in cores. Docker reads `0` as "unlimited", so a run with
    /// a limit that is not a positive finite number is refused as an invalid
    /// request.
    #[must_use]
    pub fn with_cpus(mut self, cpus: f64) -> Self {
        self.cpus = cpus;
        self
    }

    /// Whether the root filesystem is read-only (with small `/tmp` and
    /// `/var/tmp` tmpfs mounts so the command can still scratch).
    #[must_use]
    pub fn with_read_only_rootfs(mut self, read_only: bool) -> Self {
        self.read_only_rootfs = read_only;
        self
    }

    /// Extra host paths bind-mounted read-only at the same path in the
    /// container.
    #[must_use]
    pub fn with_read_only_mounts(mut self, mounts: impl IntoIterator<Item = PathBuf>) -> Self {
        self.read_only_mounts = mounts.into_iter().collect();
        self
    }

    /// Environment variables for the workload, in order.
    ///
    /// They reach the container only: the run writes them to a mode-0600
    /// temporary `--env-file` that is deleted once `docker create` has read
    /// it (or the run fails), and every docker client call runs with this
    /// process's own environment. A request can therefore never choose the
    /// client binary or daemon (`PATH`, `DOCKER_HOST`, `DOCKER_CONTEXT`,
    /// `DOCKER_CONFIG`, ...). Names must be non-empty, without `=`,
    /// whitespace, a leading `#`, or NUL; names and values must be UTF-8
    /// without newline, carriage return or NUL. Anything else is refused as
    /// an invalid request before Docker is called.
    #[must_use]
    pub fn with_env(mut self, env: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        self.env = env.into_iter().collect();
        self
    }

    /// A `--label` applied to the container, so stragglers can be found by
    /// [`DockerCli::kill_labelled`].
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The arguments after `docker`: `run --rm ...`.
    ///
    /// Mount sources are resolved with [`Path::canonicalize`] (falling back to
    /// the path as given when it does not exist yet), because Docker needs an
    /// absolute path with symlinks resolved.
    ///
    /// The environment is not on this command line: neither its values nor
    /// bare `-e NAME` flags (which would copy the client's own variables into
    /// the container). A run passes it as `--env-file` in the same place; see
    /// [`Self::with_env`].
    #[must_use]
    pub fn argv(&self) -> Vec<OsString> {
        self.argv_with_env_file(None)
    }

    /// [`Self::argv`], with `--env-file <env_file>` after the mounts.
    pub(super) fn argv_with_env_file(&self, env_file: Option<&Path>) -> Vec<OsString> {
        let mut argv: Vec<OsString> = Vec::new();
        let mut push = |s: &str| argv.push(OsString::from(s));

        push("run");
        push("--rm");
        if let Some(label) = &self.label {
            push("--label");
            push(label);
        }
        push("--network");
        push(&self.network);
        // Drop every capability, then any named extras.
        push("--cap-drop");
        push("ALL");
        for cap in &self.extra_cap_drops {
            push("--cap-drop");
            push(cap);
        }
        // Mandatory: a non-root identity and a process cap on every run. A
        // caller-supplied user or pids limit replaces only the default value.
        push("--user");
        push(&self.effective_user());
        push("--pids-limit");
        push(&self.effective_pids_limit().to_string());
        push("-m");
        push(&format!("{}m", self.memory_mb));
        push("--cpus");
        push(&self.cpus.to_string());
        if self.read_only_rootfs {
            push("--read-only");
            push("--tmpfs");
            push("/tmp:rw,noexec,nosuid,size=64m");
            push("--tmpfs");
            push("/var/tmp:rw,noexec,nosuid,size=64m");
        }
        // Stop setuid/setgid escalation inside the container.
        push("--security-opt");
        push("no-new-privileges");

        if self.mount_workspace {
            let workspace = resolve(&self.workspace);
            push("-v");
            push(&format!("{}:{ONE_SHOT_WORKSPACE}", workspace.display()));
            push("-w");
            push(ONE_SHOT_WORKSPACE);
        }

        for ro in &self.read_only_mounts {
            let ro = resolve(ro);
            push("-v");
            push(&format!("{0}:{0}:ro", ro.display()));
        }

        // Only the path: the values sit in a mode-0600 file, never on an argv
        // that any local user can read from the process table, and never in
        // the docker client's own environment.
        if let Some(env_file) = env_file {
            argv.push(OsString::from("--env-file"));
            argv.push(env_file.as_os_str().to_owned());
        }

        let mut push = |s: &str| argv.push(OsString::from(s));
        // End docker's options so an image like `--privileged` stays a positional.
        push("--");
        push(&self.image);
        push("sh");
        push(if self.login_shell { "-lc" } else { "-c" });
        push(&self.command);
        argv
    }

    /// Refuse a spec Docker would run with weaker limits than it states, or
    /// whose environment cannot be written safely to an env file.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`]; the message never contains a value.
    pub(super) fn validate(&self) -> io::Result<()> {
        if self.memory_mb == 0 {
            return Err(invalid_request(
                "a memory limit of 0 MiB means unlimited to Docker",
            ));
        }
        if !(self.cpus.is_finite() && self.cpus > 0.0) {
            return Err(invalid_request(
                "a CPU limit must be a positive number; 0 means unlimited to Docker",
            ));
        }
        for (index, (key, value)) in self.env.iter().enumerate() {
            let control = |c: char| matches!(c, '\n' | '\r' | '\0');
            let key_ok = key.to_str().is_some_and(|key| {
                !key.is_empty()
                    && !key.starts_with('#')
                    && !key
                        .chars()
                        .any(|c| c == '=' || c.is_whitespace() || control(c))
            });
            if !key_ok {
                return Err(invalid_request(&format!(
                    "environment entry {index} has an unsafe name"
                )));
            }
            if value
                .to_str()
                .is_none_or(|value| value.chars().any(control))
            {
                return Err(invalid_request(&format!(
                    "environment entry {index} has a value with a newline, NUL or non-UTF-8 byte"
                )));
            }
        }
        Ok(())
    }
}

fn invalid_request(message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid one-shot request: {message}"),
    )
}

/// Use the non-root workspace owner; never inherit a root identity.
fn sandbox_user_for(workspace: Option<&Path>) -> String {
    #[cfg(unix)]
    if let Some(workspace) = workspace {
        use std::os::unix::fs::MetadataExt;
        // Neither the root user nor the root group: a workspace owned by
        // 1000:0 would otherwise hand the workload gid 0.
        if let Ok(metadata) = workspace.metadata()
            && metadata.uid() != 0
            && metadata.gid() != 0
        {
            return format!("{}:{}", metadata.uid(), metadata.gid());
        }
    }
    #[cfg(not(unix))]
    let _ = workspace;
    "65534:65534".to_owned()
}

fn resolve(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// What a one-shot container did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneShotOutcome {
    /// The exit status, or `-1` when the process was killed by a signal or the
    /// run timed out.
    pub exit_code: i32,
    /// Standard output, lossily decoded and capped at [`MAX_OUTPUT_BYTES`].
    pub stdout: String,
    /// Standard error, lossily decoded and capped at [`MAX_OUTPUT_BYTES`]; on a
    /// timeout, a message saying so.
    pub stderr: String,
    /// Whether the run exceeded its deadline.
    pub timed_out: bool,
    /// Whether targeted timeout cleanup confirmed the workload stopped.
    /// `None` means no timeout occurred; `Some(false)` must never imply a kill.
    pub killed: Option<bool>,
    /// Observed elapsed time, including bounded timeout cleanup.
    pub duration_ms: u64,
}

/// A handle on the local `docker` command-line client.
#[derive(Debug, Clone)]
pub struct DockerCli {
    program: OsString,
}

impl Default for DockerCli {
    fn default() -> Self {
        Self::new()
    }
}

impl DockerCli {
    /// Use `docker` from `PATH`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_program("docker")
    }

    /// Use a different client binary, which is how tests substitute a fake.
    #[must_use]
    pub fn with_program(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
        }
    }

    /// Whether the daemon answers `docker info`.
    ///
    /// The probe has no deadline of its own: a wedged daemon blocks it, so
    /// callers that must not hang wrap it in a timeout.
    pub async fn is_available(&self) -> bool {
        let result = Command::new(&self.program)
            .args(["info", "--format", "{{.ServerVersion}}"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;
        result.is_ok_and(|output| output.status.success())
    }

    /// Run a fresh, non-root container with a process cap and a unique name.
    ///
    /// This is [`Self::run_one_shot_verified`] without the observed limits in
    /// the result: the enforcement probe, the `create` / `inspect` read-back
    /// of the effective limits and the refusal to start otherwise all apply.
    /// The run is supervised through its deadline even if the caller drops its
    /// wait. Timeout cleanup kills only this container, removes it by id, and
    /// reports whether Docker confirmed the stop. Output is drained with
    /// bounded memory; cleanup commands have their own short deadlines.
    ///
    /// # Errors
    ///
    /// As [`Self::run_one_shot_verified`].
    pub async fn run_one_shot(
        &self,
        spec: &OneShot,
        timeout: Duration,
    ) -> io::Result<OneShotOutcome> {
        tokio::spawn(execution::run(self.clone(), spec.clone(), timeout))
            .await
            .map_err(io::Error::other)?
    }

    /// Like [`Self::run_one_shot`], but first checks that the daemon can
    /// enforce the limits, then reads the container's limits back before the
    /// command runs, and refuses to run it when they are not the effective
    /// ones [`OneShot::expected_limits`] describes.
    ///
    /// The spec is validated first ([`OneShot::with_memory_mb`],
    /// [`OneShot::with_cpus`], [`OneShot::with_env`]); a refused spec is an
    /// [`io::ErrorKind::InvalidInput`] error and Docker is not called.
    ///
    /// A stored `HostConfig` is not proof that a kernel controller enforces
    /// it (a rootless daemon without cgroup v2 and systemd ignores `-m` and
    /// `--pids-limit`). So `docker info --format '{{json .}}'` is probed first:
    /// `PidsLimit` and `MemoryLimit` must be true (a memory limit always
    /// applies, since zero is refused), and a rootless daemon must report
    /// cgroup v2 with the systemd driver. Otherwise the run is refused with
    /// [`io::ErrorKind::Unsupported`] before anything is created. A probe that
    /// fails, times out or cannot be read also refuses the run. A probe that
    /// reports full enforcement is cached for the life of the process, keyed
    /// by the client program and its `DOCKER_HOST`, `DOCKER_CONTEXT`,
    /// `DOCKER_CONFIG`, `DOCKER_TLS_VERIFY` and `DOCKER_CERT_PATH`; any other
    /// probe result is not cached, so the next run probes again.
    ///
    /// The container is then `docker create`d (same flags, same unique
    /// `tinybox-<uuid>` name), `docker inspect`ed with
    /// [`OBSERVED_LIMITS_FORMAT`], compared, and only then `docker start -a`ed
    /// under the same supervision, bounded draining and targeted timeout
    /// cleanup as [`Self::run_one_shot`]. A mismatch, or inspect output that
    /// cannot be read, removes the created container and returns an error; the
    /// command never starts. Whenever the start does not complete (an I/O
    /// error, or the deadline passing before or after the container reaches
    /// running) the created container is removed by id with `docker rm -f`.
    /// `timeout` covers all of these steps.
    ///
    /// Every `docker rm -f` result is reported, never discarded: an error
    /// says the container was removed, that Docker reported `No such
    /// container` (absent), or `CLEANUP FAILED`. Docker 27 exits 0 for
    /// `rm -f` of a missing container and prints `No such container` on
    /// stderr (older clients exit 1); either way that stderr is read as
    /// absent, not as a removal. A timed-out outcome reports `killed` true
    /// only when Docker confirmed the kill of the running container, or
    /// confirmed (exit 0 without `No such container`) the removal of the
    /// created container.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the daemon cannot enforce the limits or the
    /// probe fails, the client cannot be started, `create` or `inspect` fails
    /// or exceeds the deadline, the observed limits are not in force, or the
    /// start fails (the message says whether the container was removed).
    pub async fn run_one_shot_verified(
        &self,
        spec: &OneShot,
        timeout: Duration,
    ) -> io::Result<VerifiedOneShot> {
        tokio::spawn(execution::run_verified(self.clone(), spec.clone(), timeout))
            .await
            .map_err(io::Error::other)?
    }

    /// Kill every container carrying `label` (`key=value`), returning how many
    /// identifiers Docker accepted the request for.
    ///
    /// The listing is `docker ps -aq`, so it includes containers that were
    /// created but never started (a verified run between `create` and
    /// `start`). Those cannot be killed, so when `docker kill` does not
    /// succeed for every id they are removed with `docker rm -f`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the listing command cannot be started.
    pub async fn kill_labelled(&self, label: &str) -> io::Result<u32> {
        self.kill_labelled_inner(label, false).await
    }

    /// Kill matching containers and require Docker to confirm both listing
    /// and kill completion. Use this when a lifecycle API reports success.
    /// The historical [`Self::kill_labelled`] method retains its best-effort
    /// behavior for existing callers.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when Docker cannot be started, the container
    /// listing fails, or any requested container kill is not confirmed.
    pub async fn kill_labelled_checked(&self, label: &str) -> io::Result<u32> {
        self.kill_labelled_inner(label, true).await
    }

    async fn kill_labelled_inner(&self, label: &str, checked: bool) -> io::Result<u32> {
        let output = Command::new(&self.program)
            .args(["ps", "-aq", "--filter", &format!("label={label}")])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await?;

        if checked && !output.status.success() {
            return Err(io::Error::other("Docker container listing failed"));
        }

        let ids: Vec<&str> = std::str::from_utf8(&output.stdout)
            .unwrap_or("")
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        if ids.is_empty() {
            return Ok(0);
        }

        let count = u32::try_from(ids.len()).unwrap_or(u32::MAX);
        let mut kill = Command::new(&self.program);
        kill.arg("kill");
        kill.args(&ids);
        let killed = kill
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await;
        if killed.as_ref().is_ok_and(std::process::ExitStatus::success) {
            return Ok(count);
        }
        // A created-but-never-started (or already exited) container cannot be
        // killed; forced removal stops and removes whatever remains.
        let mut remove = Command::new(&self.program);
        remove.args(["rm", "-f"]);
        remove.args(&ids);
        let removed = remove
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await;
        if checked && !removed_or_absent(&removed?) {
            return Err(io::Error::other(
                "Docker kill command failed and docker rm -f did not confirm removal",
            ));
        }
        Ok(count)
    }
}

/// Whether a forced removal left none of its ids behind, read with Docker's
/// `No such container` semantics: Docker 27 exits 0 for a missing container
/// and says so on stderr, older clients exit 1. A failure is accepted only
/// when every stderr line is such a report.
fn removed_or_absent(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut lines = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .peekable();
    output.status.success()
        || (lines.peek().is_some() && lines.all(|line| line.contains(NO_SUCH_CONTAINER)))
}

/// How Docker says a container to remove does not exist.
pub(super) const NO_SUCH_CONTAINER: &str = "No such container";

/// The first line of client output, bounded, for an error message.
fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

fn cap(text: &mut String, note: &str) {
    if text.len() > MAX_OUTPUT_BYTES {
        // Cut on a character boundary: `String::truncate` panics inside one.
        let mut end = MAX_OUTPUT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(note);
    }
}

#[cfg(all(test, unix))]
#[path = "mod_tests.rs"]
mod test;
