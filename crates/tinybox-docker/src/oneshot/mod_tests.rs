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
    // Round 2 (owner decision 1): the environment is not on argv at all; a
    // run passes it as a mode-0600 `--env-file`.
    assert!(!argv.iter().any(|arg| arg == "-e" || arg == "MODE"));
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
        // Round 2: no `-e A -e REQ_KEY`; a run adds `--env-file <path>` here.
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
    // Round 2: a plain run is verified, so the fake answers the lifecycle and
    // records the `create` argv (the plain run's argv with `create` for `run`).
    let cli = one_shot_fake(tmp.path(), "start) echo out; echo err >&2; exit 3;;");
    let out = tmp.path().join("argv.out");
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
    assert_eq!(args[0], "create");
    assert_eq!(args[1..], strings(&spec.argv())[1..]);
}

#[tokio::test]
async fn run_times_out_with_the_pinned_message() {
    let tmp = tempfile::tempdir().unwrap();
    // Round 2: the attached client is `start -a`; `kill` and `rm` succeed.
    let cli = one_shot_fake(tmp.path(), "start) exec sleep 5;;");
    let got = cli
        .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_millis(500))
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
    // Round 2: the workload output now comes from `start -a`.
    let cli = one_shot_fake(
        tmp.path(),
        "start) head -c 1048575 /dev/zero | tr '\\0' 'a'; printf 'é'; head -c 10 /dev/zero | tr '\\0' 'b'; head -c 1100000 /dev/zero | tr '\\0' 'c' >&2;;",
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
    // Round 2 (owner decision 2): created-but-not-started containers too.
    assert_eq!(recorded(&out), s(&["ps", "-aq", "--filter", "label=l=1"]));
}

#[tokio::test]
async fn checked_label_cleanup_rejects_unsuccessful_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, out) = fake(
        tmp.path(),
        "if [ \"$1\" = ps ]; then echo ignored-id; exit 1; fi",
    );
    assert!(cli.kill_labelled_checked("l=1").await.is_err());
    // Round 2 (owner decision 2): created-but-not-started containers too.
    assert_eq!(recorded(&out), s(&["ps", "-aq", "--filter", "label=l=1"]));
}

#[tokio::test]
async fn checked_label_cleanup_requires_successful_kill() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, out) = fake(
        tmp.path(),
        "if [ \"$1\" = ps ]; then echo container-id; exit 0; fi; exit 1",
    );
    assert!(cli.kill_labelled_checked("l=1").await.is_err());
    // Round 2: an id `docker kill` refused is force-removed; that failed too.
    assert_eq!(recorded(&out), s(&["rm", "-f", "container-id"]));
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
    // Round 2: the run is verified, so the client that must die is the
    // attached `start -a`; `create` records the unique name to `name`.
    let (pid, name, calls) = (
        tmp.path().join("pid"),
        tmp.path().join("name"),
        tmp.path().join("kills"),
    );
    let cli = one_shot_fake(
        tmp.path(),
        &format!(
            "start) echo $$ > '{pid}'; exec sleep 30;;\n\
             kill) echo \"$*\" >> '{calls}';;",
            pid = pid.display(),
            calls = calls.display(),
        ),
    );
    let got = cli
        .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_millis(500))
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

// Round 2 (owner decision 1): the values now reach the container through a
// mode-0600 env file instead of the docker client's environment; they are
// still never on its argv.
#[tokio::test]
async fn audit_m1_env_values_reach_the_container_but_never_the_argv_or_client_env() {
    let tmp = tempfile::tempdir().unwrap();
    let cli = one_shot_fake(tmp.path(), "start) printf '%s' \"$API_TOKEN\";;");
    let spec = OneShot::new(tmp.path(), "true")
        .without_workspace()
        .with_env([(OsString::from("API_TOKEN"), OsString::from("s3cret"))]);

    let outcome = cli
        .run_one_shot(&spec, Duration::from_secs(5))
        .await
        .unwrap();

    let argv = recorded(&tmp.path().join("argv.out"));
    assert!(argv.iter().any(|arg| arg == "--env-file"), "{argv:?}");
    assert!(!argv.iter().any(|arg| arg.contains("s3cret")), "{argv:?}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("env-file")).unwrap(),
        "API_TOKEN=s3cret\n"
    );
    assert_eq!(
        outcome.stdout, "",
        "the client env must not carry the value"
    );
}

// ---- DEF-32: non-root user, process cap, and read-back of observed limits ----

fn hardened(workspace: &Path) -> OneShot {
    OneShot::new(workspace, "id -u")
        .with_label("openhuman.sandbox=true")
        .with_user(1000, 1000)
        .with_pids_limit(256)
}

// Merge with fork main: the fork's mandatory `--user`/`--pids-limit` slot
// (before `-m`) is pinned, so an explicit value replaces the default in place.
#[test]
fn def32_explicit_user_and_pids_limit_take_the_mandatory_slot() {
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
        "--user",
        "1000:1000",
        "--pids-limit",
        "64",
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
        "id -u",
    ]));
    assert_eq!(
        strings(&hardened(tmp.path()).with_pids_limit(64).argv()),
        expected
    );
}

// Merge with fork main: `--user` and `--pids-limit` are no longer opt-in; the
// fork's non-root identity and 256-process cap are the defaults.
#[test]
fn def32_user_and_pids_limit_default_to_the_fork_floor() {
    let tmp = tempfile::tempdir().unwrap();
    let argv = strings(&OneShot::new(tmp.path(), "true").argv());
    assert!(
        has_pair(&argv, "--user", &sandbox_user_for(Some(tmp.path()))),
        "{argv:?}"
    );
    assert!(has_pair(&argv, "--pids-limit", "256"), "{argv:?}");
}

#[test]
fn merge_a_root_user_is_never_honoured() {
    let tmp = tempfile::tempdir().unwrap();
    for (uid, gid) in [(0, 0), (0, 1000)] {
        let argv = strings(&OneShot::new(tmp.path(), "true").with_user(uid, gid).argv());
        assert!(
            has_pair(&argv, "--user", &sandbox_user_for(Some(tmp.path()))),
            "{argv:?}"
        );
    }
    // Round 2 (owner decision 6): a root primary group is not honoured either.
    let argv = strings(&OneShot::new(tmp.path(), "true").with_user(1000, 0).argv());
    assert!(
        has_pair(&argv, "--user", &sandbox_user_for(Some(tmp.path()))),
        "{argv:?}"
    );
}

#[test]
fn merge_pids_limit_may_tighten_but_never_loosen_the_cap() {
    let tmp = tempfile::tempdir().unwrap();
    for (asked, effective) in [(1, "1"), (64, "64"), (256, "256"), (257, "256"), (0, "1")] {
        let argv = strings(
            &OneShot::new(tmp.path(), "true")
                .with_pids_limit(asked)
                .argv(),
        );
        assert!(
            has_pair(&argv, "--pids-limit", effective),
            "{asked}: {argv:?}"
        );
    }
    assert_eq!(MAX_ONE_SHOT_PIDS_LIMIT, 256);
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
    // Merge with fork main: the defaults are the effective fork floor, not "unset".
    assert_eq!(
        OneShot::new(tmp.path(), "true").expected_limits(),
        ObservedLimits {
            memory_bytes: 512 * 1024 * 1024,
            pids_limit: Some(256),
            user: sandbox_user_for(Some(tmp.path())),
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

/// A shell body that prints a `docker info --format '{{json .}}'` document
/// with the given enforcement capabilities.
fn info_json(memory: bool, pids: bool, cgroup: &str, driver: &str, rootless: bool) -> String {
    let options = if rootless {
        r#"["name=seccomp,profile=builtin","name=rootless"]"#
    } else {
        r#"["name=seccomp,profile=builtin","name=cgroupns"]"#
    };
    format!(
        r#"printf '%s\n' '{{"MemoryLimit":{memory},"PidsLimit":{pids},"SwapLimit":true,"CgroupVersion":"{cgroup}","CgroupDriver":"{driver}","SecurityOptions":{options}}}'"#
    )
}

/// A daemon that enforces every limit a one-shot sets.
fn enforcing_info() -> String {
    info_json(true, true, "2", "systemd", false)
}

/// A fake client that appends each call (space separated) to `calls`, then
/// answers `create`, `inspect`, `start` and `rm` as scripted. `info` is
/// answered by an enforcing daemon and logged to `info-calls` instead.
fn fake_verified(dir: &Path, inspect: &str, start: &str) -> (DockerCli, PathBuf) {
    fake_verified_with(dir, &enforcing_info(), inspect, start, 0, 0)
}

/// [`fake_verified`] with a scripted `info` body and `kill` / `rm` exit codes.
fn fake_verified_with(
    dir: &Path,
    info: &str,
    inspect: &str,
    start: &str,
    kill: i32,
    rm: i32,
) -> (DockerCli, PathBuf) {
    let calls = dir.join("calls");
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = --tinybox-probe ] && exit 0\n\
         if [ \"$1\" = info ]; then echo \"$*\" >> '{infos}'; {info}; exit $?; fi\n\
         echo \"$*\" >> '{calls}'\n\
         case \"$1\" in\n\
         create) echo cid-1;;\n\
         inspect) printf '%s\\n' '{inspect}';;\n\
         start) {start};;\n\
         kill) exit {kill};;\n\
         rm) exit {rm};;\n\
         esac\n",
        calls = calls.display(),
        infos = dir.join("info-calls").display(),
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
            killed: None,
            duration_ms: got.outcome.duration_ms,
        }
    );
    assert_eq!(got.observed, spec.expected_limits());

    let log = calls(&log);
    assert_eq!(log.len(), 3, "{log:?}");
    // Merge with fork main: `create` carries the run's unique `tinybox-<uuid>`
    // name (not a per-run label), then exactly the spec's argv after `run`.
    let created: Vec<&str> = log[0].split(' ').collect();
    assert_eq!(created[0], "create");
    assert_eq!(created[1], "--rm");
    assert_eq!(created[2], "--name");
    assert!(created[3].starts_with("tinybox-"));
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

// Merge with fork main: timeout cleanup targets the run's unique name with
// `kill` (then `rm -f`), never a label sweep through `ps --filter`.
#[tokio::test]
async fn def32_verified_run_times_out_and_kills_by_its_own_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, log) = fake_verified(tmp.path(), "536870912 256 1000:1000", "exec sleep 5");
    let got = cli
        .run_one_shot_verified(&hardened(tmp.path()), Duration::from_millis(500))
        .await
        .unwrap();
    assert!(got.outcome.timed_out);
    assert_eq!(got.outcome.exit_code, -1);
    assert_eq!(got.outcome.killed, Some(true));
    let log = calls(&log);
    let name = log[0].split(' ').nth(3).unwrap().to_owned();
    assert!(name.starts_with("tinybox-"), "{log:?}");
    assert!(log.iter().any(|c| c == &format!("kill {name}")), "{log:?}");
    assert!(!log.iter().any(|c| c.starts_with("ps ")), "{log:?}");
}

#[tokio::test]
async fn merge_verified_run_checks_the_defaulted_floor() {
    let tmp = tempfile::tempdir().unwrap();
    let user = sandbox_user_for(Some(tmp.path()));
    let (cli, log) = fake_verified(tmp.path(), &format!("536870912 256 {user}"), "echo ok");
    let spec = OneShot::new(tmp.path(), "true");
    let got = cli
        .run_one_shot_verified(&spec, Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!(got.observed, spec.expected_limits());
    assert_eq!(
        calls(&log).last().map(String::as_str),
        Some("start -a cid-1")
    );

    // A daemon that reports a looser cap than the floor is refused.
    let tmp = tempfile::tempdir().unwrap();
    let user = sandbox_user_for(Some(tmp.path()));
    let (cli, log) = fake_verified(tmp.path(), &format!("536870912 4096 {user}"), "echo ran");
    assert!(
        cli.run_one_shot_verified(&OneShot::new(tmp.path(), "true"), Duration::from_secs(20))
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

// ---- PR #18 review: cleanup of a created container whose start fails ----

// A start that times out before (or after) the container reaches running
// removes the created container by its id, and only a confirmed stop or
// removal is reported as a kill.
#[tokio::test]
async fn review_a_verified_start_that_times_out_removes_its_container_by_id() {
    for (kill, rm, stopped) in [(0, 0, true), (1, 0, true), (1, 1, false)] {
        let tmp = tempfile::tempdir().unwrap();
        let (cli, log) = fake_verified_with(
            tmp.path(),
            &enforcing_info(),
            "536870912 256 1000:1000",
            "exec sleep 5",
            kill,
            rm,
        );
        let got = cli
            .run_one_shot_verified(&hardened(tmp.path()), Duration::from_millis(500))
            .await
            .unwrap();
        assert!(got.outcome.timed_out);
        assert_eq!(got.outcome.killed, Some(stopped), "{kill} {rm}");
        assert_eq!(
            got.outcome.stderr.contains("THE KILL FAILED"),
            !stopped,
            "{}",
            got.outcome.stderr
        );
        assert_eq!(
            got.outcome.stderr.contains("did not confirm removal"),
            rm != 0,
            "{}",
            got.outcome.stderr
        );
        let log = calls(&log);
        assert!(
            log.iter().any(|c| c == "rm -f cid-1"),
            "{kill} {rm}: {log:?}"
        );
        assert!(!log.iter().any(|c| c.starts_with("ps ")), "{log:?}");
    }
}

// ---- PR #18 review: the daemon must be able to enforce the limits ----

#[tokio::test]
async fn review_a_daemon_that_cannot_enforce_the_limits_is_refused_before_create() {
    for (info, reason) in [
        (info_json(true, false, "2", "systemd", false), "PidsLimit"),
        (info_json(false, true, "2", "systemd", false), "MemoryLimit"),
        (info_json(true, true, "1", "cgroupfs", true), "rootless"),
        (info_json(true, true, "2", "cgroupfs", true), "rootless"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (cli, log) = fake_verified_with(
            tmp.path(),
            &info,
            "536870912 256 1000:1000",
            "echo ran",
            0,
            0,
        );
        let err = cli
            .run_one_shot_verified(&hardened(tmp.path()), Duration::from_secs(20))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported, "{err}");
        assert!(err.to_string().contains("cannot be enforced"), "{err}");
        assert!(err.to_string().contains(reason), "{reason}: {err}");
        assert_eq!(calls(&log), Vec::<String>::new(), "{reason}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("info-calls")).unwrap(),
            "info --format {{json .}}\n"
        );
    }
}

#[tokio::test]
async fn review_an_unreadable_enforcement_probe_fails_closed() {
    for info in [
        "echo daemon down >&2; exit 1",
        "echo not json",
        r#"printf '%s\n' '{"MemoryLimit":true}'"#,
        r#"printf '%s\n' '{"MemoryLimit":"yes","PidsLimit":true}'"#,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (cli, log) = fake_verified_with(
            tmp.path(),
            info,
            "536870912 256 1000:1000",
            "echo ran",
            0,
            0,
        );
        let err = cli
            .run_one_shot_verified(&hardened(tmp.path()), Duration::from_secs(20))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("probe failed"), "{info}: {err}");
        assert_eq!(calls(&log), Vec::<String>::new(), "{info}");
    }
}

#[tokio::test]
async fn review_an_enforcing_daemon_is_probed_then_the_run_proceeds() {
    let tmp = tempfile::tempdir().unwrap();
    let (cli, log) = fake_verified(tmp.path(), "536870912 256 1000:1000", "echo ok");
    let got = cli
        .run_one_shot_verified(&hardened(tmp.path()), Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!(got.outcome.exit_code, 0);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("info-calls")).unwrap(),
        "info --format {{json .}}\n"
    );
    assert_eq!(
        calls(&log).last().map(String::as_str),
        Some("start -a cid-1")
    );

    // Round 2 (owner decision 4): Docker reads a zero memory limit as
    // unlimited, so it is refused as an invalid request before any call
    // (formerly it skipped the MemoryLimit check and ran).
    let tmp = tempfile::tempdir().unwrap();
    let (cli, log) = fake_verified_with(
        tmp.path(),
        &info_json(false, true, "2", "systemd", false),
        "0 256 1000:1000",
        "echo ok",
        0,
        0,
    );
    let err = cli
        .run_one_shot_verified(
            &hardened(tmp.path()).with_memory_mb(0),
            Duration::from_secs(20),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    assert_eq!(calls(&log), Vec::<String>::new());
    assert!(!tmp.path().join("info-calls").exists());
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

// ---- PR #18 round 2: owner decisions on the one-shot lifecycle ----

/// A fake client that answers the whole verified lifecycle. Every call is
/// logged to `calls` (space separated) and its process environment to
/// `envs`. `create` records its argv (NUL separated) to `argv.out`, its
/// `--name` to `name`, a copy and the mode of any `--env-file` to
/// `env-file` / `env-file.mode`, and derives the limits `inspect` reports
/// from its own `-m`, `--pids-limit` and `--user`. `arms` are extra `case`
/// arms tried first, so a test can script any verb.
fn one_shot_fake(dir: &Path, arms: &str) -> DockerCli {
    let d = dir.display();
    let info = enforcing_info();
    let script = format!(
        "#!/bin/sh
[ \"$1\" = --tinybox-probe ] && exit 0
{{ echo \"=== $1\"; env; }} >> '{d}/envs'
echo \"$*\" >> '{d}/calls'
case \"$1\" in
{arms}
info) {info};;
create)
  : > '{d}/argv.out'
  for a in \"$@\"; do printf '%s\\0' \"$a\" >> '{d}/argv.out'; done
  echo \"$4\" > '{d}/name'
  prev=; m=0; p=; u=
  for a in \"$@\"; do
    [ \"$a\" = -- ] && break
    case \"$prev\" in
    -m) m=${{a%m}};;
    --pids-limit) p=$a;;
    --user) u=$a;;
    --env-file) cat \"$a\" > '{d}/env-file'; stat -c %a \"$a\" > '{d}/env-file.mode';;
    esac
    prev=$a
  done
  echo \"$((m * 1048576)) $p $u\" > '{d}/limits'
  echo cid-1;;
inspect) cat '{d}/limits';;
esac
"
    );
    let path = dir.join("docker");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    wait_until_executable(&path);
    DockerCli::with_program(&path)
}

fn verbs(dir: &Path) -> Vec<String> {
    calls(&dir.join("calls"))
        .iter()
        .map(|call| call.split(' ').next().unwrap_or("").to_owned())
        .collect()
}

// Owner decision 3: a plain run is verified too. It probes enforcement,
// creates, reads the effective limits back, and only then starts.
#[tokio::test]
async fn round2_a_plain_run_probes_creates_reads_back_then_starts() {
    let tmp = tempfile::tempdir().unwrap();
    let cli = one_shot_fake(tmp.path(), "start) echo ran;;");
    let got = cli
        .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!(got.exit_code, 0);
    assert_eq!(got.stdout, "ran\n");
    assert_eq!(verbs(tmp.path()), ["info", "create", "inspect", "start"]);
    assert_eq!(
        calls(&tmp.path().join("calls")).last().map(String::as_str),
        Some("start -a cid-1")
    );
}

#[tokio::test]
async fn round2_a_plain_run_is_refused_when_limits_cannot_be_enforced_or_are_not_in_force() {
    let tmp = tempfile::tempdir().unwrap();
    let not_enforcing = format!("info) {};;", info_json(true, false, "2", "systemd", false));
    let cli = one_shot_fake(tmp.path(), &not_enforcing);
    let err = cli
        .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_secs(20))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported, "{err}");
    assert_eq!(verbs(tmp.path()), ["info"]);

    let tmp = tempfile::tempdir().unwrap();
    let cli = one_shot_fake(tmp.path(), "inspect) echo '536870912 <nil> ';;");
    let err = cli
        .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_secs(20))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not in force"), "{err}");
    assert_eq!(verbs(tmp.path()), ["info", "create", "inspect", "rm"]);
}

// Owner decision 3: the probe result is cached per process (keyed by the
// client and its Docker host configuration), and a failed probe is retried.
#[tokio::test]
async fn round2_the_probe_is_cached_and_re_probed_after_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let arms = format!(
        "info) if [ -e '{up}' ]; then {info}; else : > '{up}'; echo down >&2; exit 1; fi;;\nstart) echo ok;;",
        up = tmp.path().join("up").display(),
        info = enforcing_info(),
    );
    let cli = one_shot_fake(tmp.path(), &arms);
    let spec = hardened(tmp.path());
    let first = cli
        .run_one_shot_verified(&spec, Duration::from_secs(20))
        .await
        .unwrap_err();
    assert!(first.to_string().contains("probe failed"), "{first}");
    for _ in 0..2 {
        cli.run_one_shot(&spec, Duration::from_secs(20))
            .await
            .unwrap();
    }
    let infos = verbs(tmp.path()).iter().filter(|v| *v == "info").count();
    assert_eq!(infos, 2, "{:?}", calls(&tmp.path().join("calls")));
}

// Owner decision 1: request env reaches the container through a mode-0600
// env file, never the docker client's own process environment.
#[tokio::test]
async fn round2_request_env_never_reaches_the_docker_client_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let cli = one_shot_fake(tmp.path(), "start) echo ran;;");
    let spec = OneShot::new(tmp.path(), "true")
        .without_workspace()
        .with_env([
            (OsString::from("PATH"), OsString::from("/evil/bin")),
            (
                OsString::from("DOCKER_HOST"),
                OsString::from("tcp://evil.invalid:2375"),
            ),
            (OsString::from("DOCKER_CONTEXT"), OsString::from("evilctx")),
            (OsString::from("API_TOKEN"), OsString::from("s3cret value")),
        ]);
    let got = cli
        .run_one_shot(&spec, Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!(got.stdout, "ran\n");

    let envs = std::fs::read_to_string(tmp.path().join("envs")).unwrap_or_default();
    for verb in ["info", "create", "inspect", "start"] {
        assert!(envs.contains(&format!("=== {verb}\n")), "{verb}: {envs}");
    }
    for leaked in ["/evil/bin", "evil.invalid", "evilctx", "s3cret"] {
        assert!(!envs.contains(leaked), "{leaked} reached the client env");
    }
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("env-file")).unwrap_or_default(),
        "PATH=/evil/bin\nDOCKER_HOST=tcp://evil.invalid:2375\nDOCKER_CONTEXT=evilctx\nAPI_TOKEN=s3cret value\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("env-file.mode"))
            .unwrap_or_default()
            .trim(),
        "600"
    );
    let argv = recorded(&tmp.path().join("argv.out"));
    assert!(!argv.iter().any(|arg| arg.contains("s3cret")), "{argv:?}");
    assert!(!argv.iter().any(|arg| arg == "-e"), "{argv:?}");
    let path = argv
        .windows(2)
        .find(|pair| pair[0] == "--env-file")
        .map(|pair| pair[1].clone())
        .unwrap();
    assert!(
        !Path::new(&path).exists(),
        "the env file {path} must be deleted once the container exists"
    );
}

#[tokio::test]
async fn round2_unsafe_env_is_refused_before_any_docker_call() {
    let bad: [(OsString, OsString); 7] = [
        ("A".into(), "x\ny".into()),
        ("A".into(), "x\ry".into()),
        ("A".into(), OsString::from("x\0y")),
        ("A\nB".into(), "v".into()),
        ("A=B".into(), "v".into()),
        ("".into(), "v".into()),
        ("A B".into(), "v".into()),
    ];
    for (key, value) in bad {
        let tmp = tempfile::tempdir().unwrap();
        let cli = one_shot_fake(tmp.path(), "start) echo ran;;");
        let spec = OneShot::new(tmp.path(), "true").with_env([(key.clone(), value.clone())]);
        let err = cli
            .run_one_shot(&spec, Duration::from_secs(20))
            .await
            .unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "{key:?}={value:?}: {err}"
        );
        assert_eq!(verbs(tmp.path()), Vec::<String>::new(), "{key:?}");
    }
}

// Owner decision 4: Docker treats a zero memory or CPU limit as unlimited.
#[tokio::test]
async fn round2_zero_memory_or_cpus_is_an_invalid_request() {
    let specs: [fn(&Path) -> OneShot; 4] = [
        |w: &Path| OneShot::new(w, "true").with_memory_mb(0),
        |w: &Path| OneShot::new(w, "true").with_cpus(0.0),
        |w: &Path| OneShot::new(w, "true").with_cpus(-1.0),
        |w: &Path| OneShot::new(w, "true").with_cpus(f64::NAN),
    ];
    for spec in specs {
        let tmp = tempfile::tempdir().unwrap();
        let cli = one_shot_fake(tmp.path(), "start) echo ran;;");
        let spec = spec(tmp.path());
        let plain = cli
            .run_one_shot(&spec, Duration::from_secs(20))
            .await
            .unwrap_err();
        assert_eq!(plain.kind(), std::io::ErrorKind::InvalidInput, "{plain}");
        let verified = cli
            .run_one_shot_verified(&spec, Duration::from_secs(20))
            .await
            .unwrap_err();
        assert_eq!(
            verified.kind(),
            std::io::ErrorKind::InvalidInput,
            "{verified}"
        );
        assert_eq!(verbs(tmp.path()), Vec::<String>::new());
    }
}

// Owner decision 5: every pre-start `rm -f` result is in the error. Docker
// 27 exits 0 for `rm -f` of a missing container and says "No such
// container" on stderr; that is reported as absent, never as a removal.
#[tokio::test]
async fn round2_every_pre_start_cleanup_result_is_reported() {
    let no_such = "echo 'Error response from daemon: No such container: x' >&2; exit 0";
    for (rm, says) in [
        ("exit 0", "was removed"),
        (no_such, "No such container"),
        ("exit 1", "CLEANUP FAILED"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let arms = format!("create) echo boom >&2; exit 1;;\nrm) {rm};;");
        let cli = one_shot_fake(tmp.path(), &arms);
        let err = cli
            .run_one_shot(&hardened(tmp.path()), Duration::from_secs(20))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("docker create failed: boom"), "{message}");
        assert!(message.contains(says), "{rm}: {message}");
        assert_eq!(
            message.contains("was removed"),
            says == "was removed",
            "{message}"
        );

        let tmp = tempfile::tempdir().unwrap();
        let arms = format!("inspect) echo garbage;;\nrm) {rm};;");
        let cli = one_shot_fake(tmp.path(), &arms);
        let message = cli
            .run_one_shot_verified(&hardened(tmp.path()), Duration::from_secs(20))
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains(says), "{rm}: {message}");
        assert_eq!(verbs(tmp.path()), ["info", "create", "inspect", "rm"]);
    }
}

// Owner decision 5: `killed` is true only for a kill Docker confirmed, or a
// removal Docker confirmed of a container that never ran. "No such
// container" is not a confirmed removal.
#[tokio::test]
async fn round2_a_timeout_reports_killed_only_for_a_confirmed_kill_or_removal() {
    let no_such = "echo 'Error response from daemon: No such container: cid-1' >&2; exit 0";
    for (kill, rm, killed) in [
        ("exit 0", "exit 0", true),
        ("exit 1", "exit 0", true),
        ("exit 1", no_such, false),
        ("exit 1", "exit 1", false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let arms = format!("start) exec sleep 5;;\nkill) {kill};;\nrm) {rm};;");
        let cli = one_shot_fake(tmp.path(), &arms);
        let got = cli
            .run_one_shot(&OneShot::new(tmp.path(), "x"), Duration::from_millis(500))
            .await
            .unwrap();
        assert!(got.timed_out);
        assert_eq!(got.killed, Some(killed), "{kill} / {rm}: {}", got.stderr);
        assert_eq!(
            got.stderr.contains("THE KILL FAILED"),
            !killed,
            "{}",
            got.stderr
        );
    }
}

// Owner decision 2: label cleanup includes created-but-never-started
// containers (`ps -aq`); one that cannot be killed is removed instead.
#[tokio::test]
async fn round2_label_cleanup_includes_created_but_not_started_containers() {
    let tmp = tempfile::tempdir().unwrap();
    let cli = one_shot_fake(
        tmp.path(),
        "ps) echo created-1;;\nkill) echo 'container created-1 is not running' >&2; exit 1;;\nrm) echo created-1;;",
    );
    assert_eq!(cli.kill_labelled_checked("l=1").await.unwrap(), 1);
    assert_eq!(
        calls(&tmp.path().join("calls")),
        [
            "ps -aq --filter label=l=1",
            "kill created-1",
            "rm -f created-1"
        ]
    );

    let tmp = tempfile::tempdir().unwrap();
    let cli = one_shot_fake(
        tmp.path(),
        "ps) echo created-1;;\nkill) exit 1;;\nrm) echo busy >&2; exit 1;;",
    );
    assert!(cli.kill_labelled_checked("l=1").await.is_err());
}

// Owner decision 6: neither uid 0 nor the root group is honoured.
#[test]
fn round2_a_root_primary_group_is_never_honoured() {
    let tmp = tempfile::tempdir().unwrap();
    for (uid, gid) in [(1000, 0), (0, 0), (0, 1000)] {
        let argv = strings(&OneShot::new(tmp.path(), "true").with_user(uid, gid).argv());
        assert!(
            has_pair(&argv, "--user", &sandbox_user_for(Some(tmp.path()))),
            "{uid}:{gid}: {argv:?}"
        );
    }
}

// ---- PR #18 round 2 remediation ----

/// Docker 27 answers `docker info` for an unreachable daemon with exit 0 and
/// JSON whose `ServerErrors` is non-empty and whose `MemoryLimit` and
/// `PidsLimit` are false. That is a failed probe (fail closed, ordinary
/// error), never an enforcement refusal (`Unsupported`).
#[tokio::test]
async fn round2r_an_unreachable_daemon_is_a_failed_probe_not_a_refusal() {
    let tmp = tempfile::tempdir().unwrap();
    let unreachable = r#"{"ID":"","ServerVersion":"","MemoryLimit":false,"PidsLimit":false,"ServerErrors":["Cannot connect to the Docker daemon at unix:///tmp/none.sock. Is the docker daemon running?"]}"#;
    let arms = format!("info) printf '%s\\n' '{unreachable}';;");
    let cli = one_shot_fake(tmp.path(), &arms);
    let err = cli
        .run_one_shot_verified(&hardened(tmp.path()), Duration::from_secs(20))
        .await
        .unwrap_err();
    assert_ne!(err.kind(), std::io::ErrorKind::Unsupported, "{err}");
    assert!(err.to_string().contains("probe failed"), "{err}");
    assert_eq!(verbs(tmp.path()), ["info"]);
}

/// A workspace owned by a non-root uid in the root group (gid 0) must not
/// hand the workload the root group through the default identity.
#[test]
fn round2r_a_workspace_in_the_root_group_never_yields_gid_0() {
    let tmp = tempfile::tempdir().unwrap();
    if let Err(error) = std::os::unix::fs::chown(tmp.path(), Some(1000), Some(0)) {
        eprintln!("skipped: chown needs root ({error})");
        return;
    }
    let user = sandbox_user_for(Some(tmp.path()));
    assert_ne!(user, "1000:0");
    assert!(!user.ends_with(":0"), "{user}");
    let spec = OneShot::new(tmp.path(), "x");
    let argv = strings(&spec.argv());
    assert!(!has_pair(&argv, "--user", "1000:0"), "{argv:?}");
    assert!(!spec.expected_limits().user.ends_with(":0"));
}
