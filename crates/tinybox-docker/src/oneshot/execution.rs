//! Supervised one-shot ownership and bounded timeout cleanup.

use std::ffi::OsString;
use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{
    DockerCli, MAX_OUTPUT_BYTES, OBSERVED_LIMITS_FORMAT, ObservedLimits, OneShot, OneShotOutcome,
    VerifiedOneShot, cap, first_line,
};

/// The cleanup commands have their own short bounds, independent of the job.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn run(
    cli: DockerCli,
    spec: OneShot,
    timeout: Duration,
) -> io::Result<OneShotOutcome> {
    let name = unique_name();
    let mut args = spec.argv();
    args.splice(2..2, ["--name".into(), name.clone().into()]);
    let started = Instant::now();
    let mut command = Command::new(&cli.program);
    command
        .args(args)
        .envs(spec.env.iter().map(|(key, value)| (key, value)));
    attach(&cli, command, &name, started, timeout, timeout).await
}

/// Create the container, read its limits back, and start it only when the
/// effective limits are in force. `timeout` bounds every step.
pub(super) async fn run_verified(
    cli: DockerCli,
    spec: OneShot,
    timeout: Duration,
) -> io::Result<VerifiedOneShot> {
    let name = unique_name();
    let started = Instant::now();
    let deadline = started + timeout;
    let mut args = spec.argv();
    args[0] = OsString::from("create");
    args.splice(2..2, ["--name".into(), name.clone().into()]);
    let mut create = Command::new(&cli.program);
    create
        .args(args)
        .envs(spec.env.iter().map(|(key, value)| (key, value)));
    let created = match bounded_output(create, deadline, "docker create").await {
        Ok(created) if created.success => created,
        Ok(created) => {
            let _ = control(&cli, &["rm", "-f", &name]).await;
            return Err(io::Error::other(format!(
                "docker create failed: {}",
                first_line(created.stderr.as_bytes())
            )));
        }
        Err(error) => {
            // A create that overran its deadline may still have made the container.
            let _ = control(&cli, &["rm", "-f", &name]).await;
            return Err(error);
        }
    };
    let id = created.stdout.trim().to_owned();
    if id.is_empty() || id.contains(char::is_whitespace) {
        let _ = control(&cli, &["rm", "-f", &name]).await;
        return Err(io::Error::other("docker create returned no container id"));
    }

    let observed = match read_back(&cli, &id, deadline).await {
        Ok(observed) => observed,
        Err(error) => {
            let _ = control(&cli, &["rm", "-f", &id]).await;
            return Err(error);
        }
    };
    let expected = spec.expected_limits();
    if observed != expected {
        let _ = control(&cli, &["rm", "-f", &id]).await;
        return Err(io::Error::other(format!(
            "container limits are not in force: expected {expected:?}, observed {observed:?}"
        )));
    }

    let mut start = Command::new(&cli.program);
    start.args(["start", "-a", &id]);
    let remaining = deadline.saturating_duration_since(Instant::now());
    let outcome = attach(&cli, start, &name, started, remaining, timeout).await?;
    Ok(VerifiedOneShot { outcome, observed })
}

fn unique_name() -> String {
    format!("tinybox-{}", uuid::Uuid::new_v4())
}

/// Run an attached client (`run` or `start -a`) to completion or `remaining`,
/// draining output with bounded memory and, on a timeout, stopping only the
/// container called `name`. `timeout` is the caller's whole budget, for the
/// timeout message.
async fn attach(
    cli: &DockerCli,
    mut command: Command,
    name: &str,
    started: Instant,
    remaining: Duration,
    timeout: Duration,
) -> io::Result<OneShotOutcome> {
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
    match tokio::time::timeout(remaining, Box::pin(execution)).await {
        Ok(Ok((stdout, stderr, status))) => Ok(OneShotOutcome {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
            timed_out: false,
            killed: None,
            duration_ms: elapsed_ms(started),
        }),
        Ok(Err(error)) => {
            let _ = stop_container(cli, name).await;
            Err(error)
        }
        Err(_) => {
            // Killing the client cannot stop its daemon-owned container.
            // Address only this invocation, never every job sharing a label.
            let _ = child.start_kill();
            let killed = stop_container(cli, name).await;
            Ok(OneShotOutcome {
                exit_code: -1,
                stdout: String::new(),
                stderr: format!(
                    "Command timed out after {}s and {}",
                    timeout.as_secs(),
                    if killed {
                        "the container was killed"
                    } else {
                        "THE KILL FAILED - container may still be running"
                    }
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

/// Captured output of a short control command (`create`, `inspect`).
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

async fn stop_container(cli: &DockerCli, name: &str) -> bool {
    control(cli, &["kill", name]).await || control(cli, &["rm", "-f", name]).await
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
