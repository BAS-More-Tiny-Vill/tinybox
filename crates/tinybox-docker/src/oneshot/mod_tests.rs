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
fn wait_until_executable(path: &Path) {
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
            timed_out: false
        }
    );
    // The run carries its own label right after `run --rm`.
    let mut got = recorded(&out);
    let run_label: Vec<String> = got.drain(2..4).collect();
    assert_eq!(run_label[0], "--label");
    assert!(run_label[1].starts_with("ai.tinyhumans.tinybox.oneshot="));
    assert_eq!(got, strings(&spec.argv()));
}

#[tokio::test]
async fn run_times_out_with_the_pinned_message() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, _) = fake(tmp.path(), "[ \"$1\" = run ] && exec sleep 5");
    let got = cli
        .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_millis(300))
        .await
        .unwrap();
    assert!(got.timed_out);
    assert_eq!(got.exit_code, -1);
    assert_eq!(got.stdout.len(), 0);
    assert_eq!(got.stderr, "Command timed out after 0s and was killed");
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
    let (pid, label, calls) = (
        tmp.path().join("pid"),
        tmp.path().join("label"),
        tmp.path().join("calls"),
    );
    let (cli, _) = fake(
        tmp.path(),
        &format!(
            "case \"$1\" in\n\
             run) echo $$ > '{pid}'; echo \"$4\" > '{label}'; exec sleep 30;;\n\
             ps) echo \"$*\" >> '{calls}'; echo cid1;;\n\
             kill) echo \"$*\" >> '{calls}';;\n\
             esac",
            pid = pid.display(),
            label = label.display(),
            calls = calls.display(),
        ),
    );
    let got = cli
        .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_millis(300))
        .await
        .unwrap();
    assert!(got.timed_out);

    let label = std::fs::read_to_string(&label).unwrap();
    assert!(
        label.starts_with("ai.tinyhumans.tinybox.oneshot="),
        "{label}"
    );
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap_or_default(),
        format!("ps -q --filter label={label}kill cid1\n"),
        "the container must be killed by this run's own label"
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

// ---- DEF-32: non-root user, process cap, and read-back of observed limits ----

fn hardened(workspace: &Path) -> OneShot {
    OneShot::new(workspace, "id -u")
        .with_label("openhuman.sandbox=true")
        .with_user(1000, 1000)
        .with_pids_limit(256)
}

#[test]
fn def32_user_and_pids_limit_follow_the_cpu_limit() {
    let tmp = tempfile::tempdir().unwrap();
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
        "--pids-limit",
        "256",
        "--user",
        "1000:1000",
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
        "id -u",
    ]));
    assert_eq!(strings(&hardened(tmp.path()).argv()), expected);
}

#[test]
fn def32_user_and_pids_limit_are_opt_in() {
    let tmp = tempfile::tempdir().unwrap();
    let argv = strings(&OneShot::new(tmp.path(), "true").argv());
    assert!(
        !argv.iter().any(|a| a == "--user" || a == "--pids-limit"),
        "{argv:?}"
    );
}

#[test]
fn def32_expected_limits_describe_the_spec() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        hardened(tmp.path()).with_memory_mb(256).expected_limits(),
        ObservedLimits {
            memory_bytes: 256 * 1024 * 1024,
            pids_limit: Some(256),
            user: "1000:1000".into(),
        }
    );
    assert_eq!(
        OneShot::new(tmp.path(), "true").expected_limits(),
        ObservedLimits {
            memory_bytes: 512 * 1024 * 1024,
            pids_limit: None,
            user: String::new(),
        }
    );
}

#[test]
fn def32_observed_limits_parse_docker_inspect_output() {
    assert_eq!(
        ObservedLimits::parse("536870912 256 1000:1000\n"),
        Some(ObservedLimits {
            memory_bytes: 536_870_912,
            pids_limit: Some(256),
            user: "1000:1000".into(),
        })
    );
    // An unset user prints nothing; an unset pids limit prints `<nil>`, `0` or `-1`.
    for unset in ["<nil>", "0", "-1"] {
        assert_eq!(
            ObservedLimits::parse(&format!("536870912 {unset} \n")),
            Some(ObservedLimits {
                memory_bytes: 536_870_912,
                pids_limit: None,
                user: String::new(),
            }),
            "{unset}"
        );
    }
    assert_eq!(ObservedLimits::parse(""), None);
    assert_eq!(ObservedLimits::parse("lots of garbage here"), None);
    assert_eq!(ObservedLimits::parse("1 2 3:3 extra"), None);
}

/// A fake client that appends each call (space separated) to `calls`, then
/// answers `create`, `inspect`, `start` and `rm` as scripted.
fn fake_verified(dir: &Path, inspect: &str, start: &str) -> (DockerCli, PathBuf) {
    let calls = dir.join("calls");
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = --tinybox-probe ] && exit 0\necho \"$*\" >> '{calls}'\n\
         case \"$1\" in\n\
         create) echo cid-1;;\n\
         inspect) printf '%s\\n' '{inspect}';;\n\
         start) {start};;\n\
         esac\n",
        calls = calls.display(),
    );
    let path = dir.join("docker");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    wait_until_executable(&path);
    (DockerCli::with_program(&path), calls)
}

fn calls(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn def32_verified_run_creates_reads_back_then_starts() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, log) = fake_verified(
        tmp.path(),
        "536870912 256 1000:1000",
        "echo 1000; echo warn >&2; exit 3",
    );
    let spec = hardened(tmp.path());
    let got = cli
        .run_one_shot_verified(&spec, Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!(
        got.outcome,
        OneShotOutcome {
            exit_code: 3,
            stdout: "1000\n".into(),
            stderr: "warn\n".into(),
            timed_out: false,
        }
    );
    assert_eq!(got.observed, spec.expected_limits());

    let log = calls(&log);
    assert_eq!(log.len(), 3, "{log:?}");
    // `create` carries the run's own label, then exactly the spec's argv after `run`.
    let created: Vec<&str> = log[0].split(' ').collect();
    assert_eq!(created[0], "create");
    assert_eq!(created[1], "--rm");
    assert_eq!(created[2], "--label");
    assert!(created[3].starts_with("ai.tinyhumans.tinybox.oneshot="));
    let tail: Vec<String> = strings(&spec.argv())[2..]
        .iter()
        .flat_map(|a| a.split(' ').map(str::to_owned).collect::<Vec<_>>())
        .collect();
    assert_eq!(
        created[4..],
        tail.iter().map(String::as_str).collect::<Vec<_>>()[..]
    );
    assert_eq!(
        log[1],
        format!("inspect --format {OBSERVED_LIMITS_FORMAT} cid-1")
    );
    assert_eq!(log[2], "start -a cid-1");
}

#[tokio::test]
async fn def32_verified_run_refuses_a_container_whose_limits_are_not_in_force() {
    let tmp = tempfile::tempdir().unwrap();
    // The daemon reports no pids limit and no user: the flags did not take.
    let (cli, log) = fake_verified(tmp.path(), "536870912 <nil> ", "echo ran");
    let err = cli
        .run_one_shot_verified(&hardened(tmp.path()), Duration::from_secs(20))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not in force"), "{err}");
    let log = calls(&log);
    assert!(!log.iter().any(|c| c.starts_with("start")), "{log:?}");
    assert_eq!(
        log.last().map(String::as_str),
        Some("rm -f cid-1"),
        "{log:?}"
    );
}

#[tokio::test]
async fn def32_verified_run_refuses_unreadable_inspect_output() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, log) = fake_verified(tmp.path(), "not a limit line at all", "echo ran");
    assert!(
        cli.run_one_shot_verified(&hardened(tmp.path()), Duration::from_secs(20))
            .await
            .is_err()
    );
    let log = calls(&log);
    assert!(!log.iter().any(|c| c.starts_with("start")), "{log:?}");
    assert_eq!(
        log.last().map(String::as_str),
        Some("rm -f cid-1"),
        "{log:?}"
    );
}

#[tokio::test]
async fn def32_verified_run_times_out_and_kills_by_its_own_label() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, log) = fake_verified(tmp.path(), "536870912 256 1000:1000", "exec sleep 5");
    let got = cli
        .run_one_shot_verified(&hardened(tmp.path()), Duration::from_millis(500))
        .await
        .unwrap();
    assert!(got.outcome.timed_out);
    assert_eq!(got.outcome.exit_code, -1);
    let log = calls(&log);
    let label = log[0].split(' ').nth(3).unwrap().to_owned();
    assert!(
        log.iter()
            .any(|c| c == &format!("ps -q --filter label={label}")),
        "{log:?}"
    );
}

/// Real daemon: set `TINYBOX_DOCKER_E2E=1` on a host with Docker.
#[tokio::test]
async fn def32_e2e_limits_are_in_force_on_a_real_daemon() {
    if std::env::var_os("TINYBOX_DOCKER_E2E").is_none() {
        eprintln!("skipped: TINYBOX_DOCKER_E2E is not set");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let spec = OneShot::new(tmp.path(), "id -u; id -g; cat /sys/fs/cgroup/pids.max")
        .without_workspace()
        .with_user(65534, 65534)
        .with_pids_limit(64)
        .with_memory_mb(128);
    let got = DockerCli::new()
        .run_one_shot_verified(&spec, Duration::from_secs(120))
        .await
        .unwrap();
    assert_eq!(got.outcome.exit_code, 0, "{:?}", got.outcome);
    assert_eq!(got.outcome.stdout, "65534\n65534\n64\n");
    assert_eq!(
        got.observed,
        ObservedLimits {
            memory_bytes: 128 * 1024 * 1024,
            pids_limit: Some(64),
            user: "65534:65534".into(),
        }
    );
}
