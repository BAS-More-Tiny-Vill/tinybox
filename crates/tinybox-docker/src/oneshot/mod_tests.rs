//! One-shot argv and execution, against a fake `docker` binary so no daemon is
//! needed. The argv literals here are the command line the original host
//! sandbox produced; they must not drift.

#![allow(clippy::unwrap_used, missing_docs)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;

fn strings(argv: &[OsString]) -> Vec<String> {
    argv.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

fn canon(p: &Path) -> String {
    p.canonicalize().unwrap().display().to_string()
}

fn has_pair(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2)
        .any(|pair| pair[0] == flag && pair[1] == value)
}

/// A fake client that records its argv (NUL separated) then runs `body`.
fn fake(dir: &Path, body: &str) -> (DockerCli, PathBuf) {
    let out = dir.join("argv.out");
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = --tinybox-probe ] && exit 0\n: > '{out}'\nfor a in \"$@\"; do printf '%s\\0' \"$a\" >> '{out}'; done\n{body}\n",
        out = out.display()
    );
    let path = dir.join("docker");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    wait_until_executable(&path);
    (DockerCli::with_program(&path), out)
}

/// Another test thread may `fork` while our write handle is still open, and the
/// child keeps that handle until it `exec`s; executing the script in that window
/// fails with `ETXTBSY`. Probe (a no-op for the script) until the window closes.
pub(super) fn wait_until_executable(path: &Path) {
    for _ in 0..200 {
        match std::process::Command::new(path)
            .arg("--tinybox-probe")
            .status()
        {
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => return,
        }
    }
}

fn recorded(out: &Path) -> Vec<String> {
    std::fs::read(out)
        .unwrap()
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

#[test]
fn defaults_produce_the_pinned_command_line() {
    let tmp = tempfile::tempdir().unwrap();
    let spec = OneShot::new(tmp.path(), "echo hi && ls").with_label("openhuman.sandbox=true");
    let mut expected = s(&[
        "run",
        "--rm",
        "--label",
        "openhuman.sandbox=true",
        "--network",
        "none",
        "--cap-drop",
        "ALL",
        "-m",
        "512m",
        "--cpus",
        "1",
        "--read-only",
        "--tmpfs",
        "/tmp:rw,noexec,nosuid,size=64m",
        "--tmpfs",
        "/var/tmp:rw,noexec,nosuid,size=64m",
        "--security-opt",
        "no-new-privileges",
        "-v",
    ]);
    expected.push(format!("{}:/workspace", canon(tmp.path())));
    expected.extend(s(&[
        "-w",
        "/workspace",
        "--",
        "alpine:3.20",
        "sh",
        "-c",
        "echo hi && ls",
    ]));
    let index = expected
        .iter()
        .position(|argument| argument == "-m")
        .unwrap();
    expected.splice(
        index..index,
        [
            "--user".to_owned(),
            sandbox_user_for(Some(tmp.path())),
            "--pids-limit".to_owned(),
            "256".to_owned(),
        ],
    );
    assert_eq!(strings(&spec.argv()), expected);
}

#[test]
fn without_workspace_preserves_isolation_flags_without_mount_or_workdir() {
    let tmp = tempfile::tempdir().unwrap();
    let spec = OneShot::new(tmp.path(), "printf '%s' \"$MODE\"")
        .without_workspace()
        .with_network("bridge")
        .with_memory_mb(2048)
        .with_cpus(2.5)
        .with_read_only_rootfs(false)
        .with_login_shell()
        .with_env([(OsString::from("MODE"), OsString::from("host-cwd"))]);
    let argv = strings(&spec.argv());

    assert!(has_pair(&argv, "--network", "bridge"));
    assert!(has_pair(&argv, "-m", "2048m"));
    assert!(has_pair(&argv, "--cpus", "2.5"));
    assert!(has_pair(&argv, "-e", "MODE"));
    assert!(!argv.iter().any(|arg| arg.contains("host-cwd")));
    assert!(!argv.iter().any(|arg| arg == "-v"));
    assert!(!argv.iter().any(|arg| arg == "-w"));
    assert!(!argv.iter().any(|arg| arg == "--read-only"));
    assert_eq!(argv[0], "run");
    assert_eq!(argv[1], "--rm");
    assert_eq!(
        argv[argv.len() - 3..],
        ["sh", "-lc", "printf '%s' \"$MODE\""]
    );
}

#[test]
fn overrides_mounts_and_env_are_ordered() {
    let tmp = tempfile::tempdir().unwrap();
    let ro = tempfile::tempdir().unwrap();
    let spec = OneShot::new(tmp.path(), "make test")
        .with_label("l=1")
        .with_image("debian:12")
        .with_network("bridge")
        .with_memory_mb(2048)
        .with_cpus(2.5)
        .with_read_only_rootfs(false)
        .with_extra_cap_drops(["NET_RAW".to_owned(), "MKNOD".to_owned()])
        .with_read_only_mounts([
            ro.path().to_path_buf(),
            PathBuf::from("/definitely/not/here"),
        ])
        .with_env([
            (OsString::from("A"), OsString::from("1")),
            (OsString::from("REQ_KEY"), OsString::from("req value=1")),
        ]);
    let mut expected = s(&[
        "run",
        "--rm",
        "--label",
        "l=1",
        "--network",
        "bridge",
        "--cap-drop",
        "ALL",
        "--cap-drop",
        "NET_RAW",
        "--cap-drop",
        "MKNOD",
        "-m",
        "2048m",
        "--cpus",
        "2.5",
        "--security-opt",
        "no-new-privileges",
        "-v",
    ]);
    expected.push(format!("{}:/workspace", canon(tmp.path())));
    expected.extend(s(&["-w", "/workspace", "-v"]));
    expected.push(format!("{0}:{0}:ro", canon(ro.path())));
    expected.extend(s(&[
        "-v",
        "/definitely/not/here:/definitely/not/here:ro",
        "-e",
        "A",
        "-e",
        "REQ_KEY",
        "--",
        "debian:12",
        "sh",
        "-c",
        "make test",
    ]));
    let index = expected
        .iter()
        .position(|argument| argument == "-m")
        .unwrap();
    expected.splice(
        index..index,
        [
            "--user".to_owned(),
            sandbox_user_for(Some(tmp.path())),
            "--pids-limit".to_owned(),
            "256".to_owned(),
        ],
    );
    assert_eq!(strings(&spec.argv()), expected);
}

#[test]
fn unlabelled_run_omits_the_label_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let argv = strings(&OneShot::new(tmp.path(), "true").argv());
    assert!(!argv.contains(&"--label".to_owned()));
}

#[tokio::test]
async fn run_reports_exit_code_and_streams() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, out) = fake(tmp.path(), "echo out; echo err >&2; exit 3");
    let spec = OneShot::new(tmp.path(), "x");
    let got = cli
        .run_one_shot(&spec, Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!(
        got,
        OneShotOutcome {
            exit_code: 3,
            stdout: "out\n".into(),
            stderr: "err\n".into(),
            timed_out: false,
            killed: None,
            duration_ms: got.duration_ms,
        }
    );
    let mut args = recorded(&out);
    assert_eq!(args[2], "--name");
    assert!(args[3].starts_with("tinybox-"));
    args.drain(2..4);
    assert_eq!(args, strings(&spec.argv()));
}

#[tokio::test]
async fn run_times_out_with_the_pinned_message() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, _) = fake(
        tmp.path(),
        "if [ \"$1\" = kill ]; then exit 0; fi; exec sleep 5",
    );
    let got = cli
        .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_millis(300))
        .await
        .unwrap();
    assert!(got.timed_out);
    assert_eq!(got.exit_code, -1);
    assert_eq!(got.stdout.len(), 0);
    assert_eq!(
        got.stderr,
        "Command timed out after 0s and the container was killed"
    );
}

#[tokio::test]
async fn output_is_capped_on_a_character_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    // 1_048_575 ASCII bytes then a 2-byte char straddling the cap.
    let (cli, _) = fake(
        tmp.path(),
        "head -c 1048575 /dev/zero | tr '\\0' 'a'; printf 'é'; head -c 10 /dev/zero | tr '\\0' 'b'; head -c 1100000 /dev/zero | tr '\\0' 'c' >&2",
    );
    let got = cli
        .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_secs(20))
        .await
        .unwrap();
    assert!(got.stdout.ends_with("\n... [output truncated at 1MB]"));
    assert_eq!(
        got.stdout.len(),
        1_048_575 + "\n... [output truncated at 1MB]".len()
    );
    assert_eq!(
        got.stderr.len(),
        MAX_OUTPUT_BYTES + "\n... [stderr truncated at 1MB]".len()
    );
}

#[tokio::test]
async fn missing_client_is_an_io_error() {
    let tmp = tempfile::tempdir().unwrap();
    let cli = DockerCli::with_program(tmp.path().join("nope"));
    assert!(
        cli.run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_secs(5))
            .await
            .is_err()
    );
    assert!(!cli.is_available().await);
}

#[tokio::test]
async fn availability_probe_asks_for_the_server_version() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, out) = fake(tmp.path(), "exit 0");
    assert!(cli.is_available().await);
    assert_eq!(
        recorded(&out),
        s(&["info", "--format", "{{.ServerVersion}}"])
    );
    let tmp = tempfile::tempdir().unwrap();
    let (cli, _) = fake(tmp.path(), "exit 1");
    assert!(!cli.is_available().await);
}

#[tokio::test]
async fn kill_labelled_lists_then_kills() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, out) = fake(
        tmp.path(),
        "if [ \"$1\" = ps ]; then echo abc; echo def; fi",
    );
    assert_eq!(cli.kill_labelled("l=1").await.unwrap(), 2);
    assert_eq!(recorded(&out), s(&["kill", "abc", "def"]));

    let tmp = tempfile::tempdir().unwrap();
    let (cli, out) = fake(tmp.path(), "exit 0");
    assert_eq!(cli.kill_labelled("l=1").await.unwrap(), 0);
    assert_eq!(recorded(&out), s(&["ps", "-q", "--filter", "label=l=1"]));
}

#[tokio::test]
async fn checked_label_cleanup_rejects_unsuccessful_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, out) = fake(
        tmp.path(),
        "if [ \"$1\" = ps ]; then echo ignored-id; exit 1; fi",
    );
    assert!(cli.kill_labelled_checked("l=1").await.is_err());
    assert_eq!(recorded(&out), s(&["ps", "-q", "--filter", "label=l=1"]));
}

#[tokio::test]
async fn checked_label_cleanup_requires_successful_kill() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, out) = fake(
        tmp.path(),
        "if [ \"$1\" = ps ]; then echo container-id; exit 0; fi; exit 1",
    );
    assert!(cli.kill_labelled_checked("l=1").await.is_err());
    assert_eq!(recorded(&out), s(&["kill", "container-id"]));
}

#[tokio::test]
async fn legacy_label_cleanup_keeps_its_best_effort_result() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, _) = fake(
        tmp.path(),
        "if [ \"$1\" = ps ]; then echo container-id; exit 0; fi; exit 1",
    );
    assert_eq!(cli.kill_labelled("l=1").await.unwrap(), 1);
}

#[test]
fn an_image_that_looks_like_a_flag_stays_a_positional() {
    let tmp = tempfile::tempdir().unwrap();
    let argv = strings(
        &OneShot::new(tmp.path(), "true")
            .with_image("--privileged")
            .argv(),
    );
    let image = argv.iter().position(|arg| arg == "--privileged").unwrap();
    assert_eq!(argv[image - 1], "--", "{argv:?}");
}

#[tokio::test]
async fn audit_h6_a_timed_out_run_kills_its_client_and_its_container() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, name, calls) = (
        tmp.path().join("pid"),
        tmp.path().join("name"),
        tmp.path().join("calls"),
    );
    let (cli, _) = fake(
        tmp.path(),
        &format!(
            "case \"$1\" in\n\
             run) echo $$ > '{pid}'; echo \"$4\" > '{name}'; exec sleep 30;;\n\
             kill) echo \"$*\" >> '{calls}';;\n\
             esac",
            pid = pid.display(),
            name = name.display(),
            calls = calls.display(),
        ),
    );
    let got = cli
        .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_millis(300))
        .await
        .unwrap();
    assert!(got.timed_out);
    assert_eq!(got.killed, Some(true));

    // The fork addresses only this invocation, by its unique container name.
    let name = std::fs::read_to_string(&name).unwrap();
    assert!(name.starts_with("tinybox-"), "{name}");
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap_or_default(),
        format!("kill {name}"),
        "the container must be killed by this run's own name"
    );

    let pid = std::fs::read_to_string(&pid).unwrap().trim().to_owned();
    // Alive = listed and not a zombie (an unreaped killed child still answers `kill -0`).
    let alive = || {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        !stat.is_empty() && !stat.starts_with('Z')
    };
    for _ in 0..20 {
        if !alive() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let still_alive = alive();
    if still_alive {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid])
            .status();
    }
    assert!(
        !still_alive,
        "docker client pid {pid} still alive 1s after the timeout reported it killed"
    );
}

#[tokio::test]
async fn audit_m1_env_values_reach_the_client_but_never_its_argv() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, out) = fake(tmp.path(), "printf '%s' \"$API_TOKEN\"");
    let spec = OneShot::new(tmp.path(), "true")
        .without_workspace()
        .with_env([(OsString::from("API_TOKEN"), OsString::from("s3cret"))]);

    let outcome = cli
        .run_one_shot(&spec, Duration::from_secs(5))
        .await
        .unwrap();

    let argv = recorded(&out);
    assert!(has_pair(&argv, "-e", "API_TOKEN"), "{argv:?}");
    assert!(!argv.iter().any(|arg| arg.contains("s3cret")), "{argv:?}");
    assert_eq!(outcome.stdout, "s3cret");
}
