//! One-shot containers: one `docker run --rm` per command.
//!
//! The long-lived [`DockerSandbox`](crate::DockerSandbox) keeps a container
//! open and `docker exec`s into it. Some callers want the opposite contract:
//! every command gets a **fresh** container, nothing persists between commands,
//! and the container removes itself when the command exits. That is what this
//! module provides, and it deliberately reproduces one exact command line — the
//! flags below, in this order — because callers pin it.
//!
//! The command line is built by [`OneShot::argv`], a pure function (apart from
//! resolving symlinks in mount paths), so it can be tested without a daemon.
//! [`DockerCli`] runs it, probes the daemon, and kills labelled stragglers; its
//! program name is injectable so tests can substitute a fake binary.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
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

    /// Run the command as numeric `uid:gid` (`--user`) instead of the image's
    /// default user, which for most images is root.
    #[must_use]
    pub fn with_user(mut self, uid: u32, gid: u32) -> Self {
        self.user = Some(format!("{uid}:{gid}"));
        self
    }

    /// Cap the number of processes in the container (`--pids-limit`), so a
    /// fork bomb exhausts the container's budget rather than the host's
    /// process table. The memory limit does not contain one.
    #[must_use]
    pub fn with_pids_limit(mut self, pids_limit: u64) -> Self {
        self.pids_limit = Some(pids_limit);
        self
    }

    /// The limits Docker should report for a container started from this
    /// spec, in the shape [`ObservedLimits::parse`] returns.
    #[must_use]
    pub fn expected_limits(&self) -> ObservedLimits {
        ObservedLimits {
            memory_bytes: self.memory_mb.saturating_mul(1024 * 1024),
            pids_limit: self
                .pids_limit
                .map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            user: self.user.clone().unwrap_or_default(),
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

    /// The memory limit in mebibytes.
    #[must_use]
    pub fn with_memory_mb(mut self, memory_mb: u64) -> Self {
        self.memory_mb = memory_mb;
        self
    }

    /// The CPU limit in cores.
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

    /// Environment variables injected with `-e`, in order.
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
    #[must_use]
    pub fn argv(&self) -> Vec<OsString> {
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
        push("-m");
        push(&format!("{}m", self.memory_mb));
        push("--cpus");
        push(&self.cpus.to_string());
        // Opt-in, so callers that pin the default command line see no change.
        if let Some(pids_limit) = self.pids_limit {
            push("--pids-limit");
            push(&pids_limit.to_string());
        }
        if let Some(user) = &self.user {
            push("--user");
            push(user);
        }
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

        // Names only: the values reach the docker client through its own
        // environment (see `run_one_shot`), so they never sit on an argv that
        // any local user can read from the process table.
        for (key, _) in &self.env {
            argv.push(OsString::from("-e"));
            argv.push(key.clone());
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

    /// Run `spec` in a fresh container and wait for it, up to `timeout`.
    ///
    /// A command that exits non-zero is a success here, reported in
    /// [`OneShotOutcome::exit_code`]. Hitting the deadline is also an outcome
    /// ([`OneShotOutcome::timed_out`]): the local client is killed, and so is the
    /// container, found by a label unique to this run (killing an attached
    /// `docker run` does not stop its container).
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the `docker` client cannot be started.
    pub async fn run_one_shot(
        &self,
        spec: &OneShot,
        timeout: Duration,
    ) -> io::Result<OneShotOutcome> {
        // Per run, not `spec.label`: callers share one label across concurrent runs.
        let run_label = format!("ai.tinyhumans.tinybox.oneshot={}", uuid::Uuid::new_v4());
        let mut argv = spec.argv();
        argv.splice(
            2..2,
            [OsString::from("--label"), OsString::from(&run_label)],
        );
        let mut cmd = Command::new(&self.program);
        cmd.args(argv);
        cmd.envs(spec.env.iter().map(|(key, value)| (key, value)));
        self.wait_attached(cmd, timeout, &run_label).await
    }

    /// Like [`Self::run_one_shot`], but reads the container's limits back
    /// before the command runs, and refuses to run it when they are not the
    /// ones `spec` asked for.
    ///
    /// The container is `docker create`d (same flags, same per-run label),
    /// `docker inspect`ed with [`OBSERVED_LIMITS_FORMAT`], compared with
    /// [`OneShot::expected_limits`], and only then `docker start -a`ed. A
    /// mismatch, or inspect output that cannot be read, removes the created
    /// container and returns an error; the command never starts. `timeout`
    /// covers all of these steps.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the client cannot be started, `create` or
    /// `inspect` fails or exceeds the deadline, or the observed limits are not
    /// in force.
    pub async fn run_one_shot_verified(
        &self,
        spec: &OneShot,
        timeout: Duration,
    ) -> io::Result<VerifiedOneShot> {
        let deadline = tokio::time::Instant::now() + timeout;
        let run_label = format!("ai.tinyhumans.tinybox.oneshot={}", uuid::Uuid::new_v4());
        let mut argv = spec.argv();
        argv[0] = OsString::from("create");
        argv.splice(
            2..2,
            [OsString::from("--label"), OsString::from(&run_label)],
        );
        let mut create = Command::new(&self.program);
        create.args(argv);
        create.envs(spec.env.iter().map(|(key, value)| (key, value)));
        let created = self
            .bounded_output(create, deadline, "docker create")
            .await?;
        if !created.status.success() {
            return Err(io::Error::other(format!(
                "docker create failed: {}",
                first_line(&created.stderr)
            )));
        }
        let id = String::from_utf8_lossy(&created.stdout).trim().to_owned();
        if id.is_empty() || id.contains(char::is_whitespace) {
            return Err(io::Error::other("docker create returned no container id"));
        }

        let observed = match self.read_back(&id, deadline).await {
            Ok(observed) => observed,
            Err(e) => {
                self.remove(&id).await;
                return Err(e);
            }
        };
        let expected = spec.expected_limits();
        if observed != expected {
            self.remove(&id).await;
            return Err(io::Error::other(format!(
                "container limits are not in force: expected {expected:?}, observed {observed:?}"
            )));
        }

        let mut start = Command::new(&self.program);
        start.args(["start", "-a", &id]);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let outcome = self.wait_attached(start, remaining, &run_label).await?;
        Ok(VerifiedOneShot { outcome, observed })
    }

    async fn read_back(
        &self,
        id: &str,
        deadline: tokio::time::Instant,
    ) -> io::Result<ObservedLimits> {
        let mut inspect = Command::new(&self.program);
        inspect.args(["inspect", "--format", OBSERVED_LIMITS_FORMAT, id]);
        let out = self
            .bounded_output(inspect, deadline, "docker inspect")
            .await?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "docker inspect failed: {}",
                first_line(&out.stderr)
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        ObservedLimits::parse(&text).ok_or_else(|| {
            io::Error::other(format!(
                "docker inspect output is not a limit line: {:?}",
                first_line(&out.stdout)
            ))
        })
    }

    async fn bounded_output(
        &self,
        mut cmd: Command,
        deadline: tokio::time::Instant,
        what: &str,
    ) -> io::Result<std::process::Output> {
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        tokio::time::timeout_at(deadline, cmd.output())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{what} exceeded its deadline"),
                )
            })?
    }

    /// Best effort: force-remove a created container that must not run.
    async fn remove(&self, id: &str) {
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new(&self.program)
                .args(["rm", "-f", id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
    }

    /// Run an attached client (`run` or `start -a`) to completion or `timeout`,
    /// killing the container by `run_label` on a timeout.
    async fn wait_attached(
        &self,
        mut cmd: Command,
        timeout: Duration,
        run_label: &str,
    ) -> io::Result<OneShotOutcome> {
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
                cap(&mut stdout, "\n... [output truncated at 1MB]");
                cap(&mut stderr, "\n... [stderr truncated at 1MB]");
                Ok(OneShotOutcome {
                    exit_code: output.status.code().unwrap_or(-1),
                    stdout,
                    stderr,
                    timed_out: false,
                })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // ponytail: `ps -q` misses a container created but not yet
                // started; `ps -aq` + `rm -f` if that window matters.
                let _ =
                    tokio::time::timeout(Duration::from_secs(10), self.kill_labelled(run_label))
                        .await;
                Ok(OneShotOutcome {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!(
                        "Command timed out after {}s and was killed",
                        timeout.as_secs()
                    ),
                    timed_out: true,
                })
            }
        }
    }

    /// Kill every running container carrying `label` (`key=value`), returning
    /// how many identifiers Docker accepted the request for.
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
            .args(["ps", "-q", "--filter", &format!("label={label}")])
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

        let mut kill = Command::new(&self.program);
        kill.arg("kill");
        kill.args(&ids);
        // Best effort: a container that exited in the meantime is not an error.
        let status = kill
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await;
        if checked {
            let status = status?;
            if !status.success() {
                return Err(io::Error::other("Docker kill command failed"));
            }
        }
        Ok(u32::try_from(ids.len()).unwrap_or(u32::MAX))
    }
}

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
