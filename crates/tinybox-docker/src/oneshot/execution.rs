//! Supervised one-shot ownership and bounded timeout cleanup.

use std::io;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{DockerCli, MAX_OUTPUT_BYTES, OneShot, OneShotOutcome, cap};

/// The cleanup commands have their own short bounds, independent of the job.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn run(
    cli: DockerCli,
    spec: OneShot,
    timeout: Duration,
) -> io::Result<OneShotOutcome> {
    let name = format!("tinybox-{}", uuid::Uuid::new_v4());
    let mut args = spec.argv();
    args.splice(2..2, ["--name".into(), name.clone().into()]);
    let started = Instant::now();
    let mut child = Command::new(&cli.program)
        .args(args)
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
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
    match tokio::time::timeout(timeout, Box::pin(execution)).await {
        Ok(Ok((stdout, stderr, status))) => Ok(OneShotOutcome {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
            timed_out: false,
            killed: None,
            duration_ms: elapsed_ms(started),
        }),
        Ok(Err(error)) => {
            let _ = stop_container(&cli, &name).await;
            Err(error)
        }
        Err(_) => {
            // Killing the client cannot stop its daemon-owned container.
            // Address only this invocation, never every job sharing a label.
            let _ = child.start_kill();
            let killed = stop_container(&cli, &name).await;
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
