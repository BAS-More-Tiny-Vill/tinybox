//! Module Docker operation tests use a local fake CLI, never a daemon.

use std::path::Path;

use tinybox_bus::{
    CleanupDockerRequest, DockerRunRequest, DockerStatusRequest, MAX_DOCKER_RUN_TIMEOUT_MS,
};
use tinybox_docker::DockerCli;

use super::*;

/// Publish a fake CLI through an immutable launcher and confirm execution.
fn write_cli_fixture(path: &Path, script: impl AsRef<str>) -> std::io::Result<std::path::PathBuf> {
    let script = script.as_ref().replacen(
        "#!/bin/sh\n",
        "#!/bin/sh\n[ \"$1\" = --tinybox-probe ] && exit 0\n",
        1,
    );
    // Execute an immutable launcher: inherited writers can safely retain the
    // generated script body because the shell reads it as data.
    let body = path.with_extension("script");
    std::fs::write(&body, script)?;
    std::os::unix::fs::symlink(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/docker-cli.sh"),
        path,
    )?;
    probe_cli_fixture(|| {
        std::process::Command::new(path)
            .arg("--tinybox-probe")
            .status()
    })?;
    Ok(body)
}

/// Confirm the fixture is executable without recording a Docker invocation.
fn probe_cli_fixture(
    mut probe: impl FnMut() -> std::io::Result<std::process::ExitStatus>,
) -> std::io::Result<()> {
    // Confirm launch before the test starts. Preserve the existing probe
    // contract: ETXTBSY alone is retryable, without a deadline.
    loop {
        match probe() {
            Err(error) if error.raw_os_error() == Some(26) => {
                std::thread::yield_now();
            }
            result => {
                let status = result?;
                return if status.success() {
                    Ok(())
                } else {
                    Err(std::io::Error::other("fake Docker CLI probe failed"))
                };
            }
        }
    }
}

#[test]
fn fixture_probe_waits_for_inherited_writers_without_an_attempt_limit() -> std::io::Result<()> {
    use std::os::unix::process::ExitStatusExt;
    let mut busy = 300;
    probe_cli_fixture(|| {
        if busy > 0 {
            busy -= 1;
            Err(std::io::Error::from_raw_os_error(26))
        } else {
            Ok(std::process::ExitStatus::from_raw(0))
        }
    })?;
    assert_eq!(busy, 0);
    assert!(probe_cli_fixture(|| Err(std::io::Error::from_raw_os_error(2))).is_err());
    assert!(probe_cli_fixture(|| Ok(std::process::ExitStatus::from_raw(256))).is_err());
    Ok(())
}

fn canonical(path: &Path) -> std::io::Result<String> {
    path.canonicalize().map(|path| path.display().to_string())
}

/// A fake CLI body for the always-verified run: `info` reports a daemon that
/// enforces limits, `create` records its argv (NUL separated) into
/// `argv_file`, and copies any `--env-file` it was given to `<argv_file>.env`
/// (the module deletes its own copy once `create` has read it), `inspect`
/// prints `inspect`, and `start` prints `out` / `err` and exits with
/// `exit_code`.
fn settings_cli_script(argv_file: &Path, inspect: &str, exit_code: i32) -> String {
    format!(
        "#!/bin/sh\ncase \"$1\" in\n\
         info) echo '{ENFORCING_INFO}';;\n\
         create) : > '{argv}'; prev=; for a in \"$@\"; do printf '%s\\0' \"$a\" >> '{argv}'; [ \"$prev\" = --env-file ] && cp \"$a\" '{argv}.env'; prev=$a; done; echo cid-7;;\n\
         inspect) echo '{inspect}';;\n\
         start) printf out; printf err >&2; exit {exit_code};;\n\
         *) exit 9;;\n\
         esac\n",
        argv = argv_file.display(),
    )
}

fn has_pair(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2)
        .any(|pair| pair[0] == flag && pair[1] == value)
}

#[tokio::test]
async fn docker_status_uses_the_module_owned_cli_probe()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let calls = temp.path().join("calls");
    let script_path = temp.path().join("docker");
    write_cli_fixture(
        &script_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n[ \"$1\" = info ] && exit 0\nexit 1\n",
            calls.display()
        ),
    )?;

    let status = status_with_cli(
        &DockerCli::with_program(&script_path),
        DockerStatusRequest { timeout_ms: 10_000 },
    )
    .await?;

    assert!(status.available);
    assert_eq!(
        std::fs::read_to_string(calls)?,
        "info --format {{.ServerVersion}}\n"
    );
    Ok(())
}

#[tokio::test]
async fn docker_status_reports_an_unavailable_daemon_without_failing_the_probe()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let script_path = temp.path().join("docker");
    write_cli_fixture(&script_path, "#!/bin/sh\nexit 1\n")?;

    let status = status_with_cli(
        &DockerCli::with_program(&script_path),
        DockerStatusRequest { timeout_ms: 10_000 },
    )
    .await?;

    assert!(!status.available);
    Ok(())
}

#[tokio::test]
async fn docker_cleanup_only_kills_ids_returned_for_the_requested_label()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let calls = temp.path().join("calls");
    let script_path = temp.path().join("docker");
    write_cli_fixture(
        &script_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = ps ]; then printf 'box-a\\nbox-b\\n'; exit 0; fi\n[ \"$1\" = kill ] && exit 0\nexit 1\n",
            calls.display()
        ),
    )?;

    let result = cleanup_with_cli(
        &DockerCli::with_program(&script_path),
        CleanupDockerRequest {
            label: "openhuman.sandbox=true".into(),
            timeout_ms: 10_000,
        },
    )
    .await?;

    assert_eq!(result.containers_stopped, 2);
    assert_eq!(
        std::fs::read_to_string(calls)?,
        // Round 2: `-a` also lists created-but-never-started containers.
        "ps -aq --filter label=openhuman.sandbox=true\nkill box-a box-b\n"
    );
    Ok(())
}

#[tokio::test]
async fn docker_cleanup_reports_failed_kill_instead_of_claiming_stopped()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let script_path = temp.path().join("docker");
    write_cli_fixture(
        &script_path,
        "#!/bin/sh\n[ \"$1\" = ps ] && { printf 'box-a\\n'; exit 0; }\n[ \"$1\" = kill ] && exit 1\nexit 2\n",
    )?;

    let Err(error) = cleanup_with_cli(
        &DockerCli::with_program(&script_path),
        CleanupDockerRequest {
            label: "openhuman.sandbox=true".into(),
            timeout_ms: 10_000,
        },
    )
    .await
    else {
        return Err(
            std::io::Error::other("failed Docker kill was reported as successful cleanup").into(),
        );
    };

    assert_eq!(error.wire_name(), tinybox_bus::DOCKER_CONTROL_FAILED);
    Ok(())
}

#[tokio::test]
async fn docker_cleanup_does_not_kill_ids_from_a_failed_listing()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let calls = temp.path().join("calls");
    let script_path = temp.path().join("docker");
    write_cli_fixture(
        &script_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n[ \"$1\" = ps ] && {{ printf 'box-a\\n'; exit 1; }}\nexit 0\n",
            calls.display()
        ),
    )?;

    let Err(error) = cleanup_with_cli(
        &DockerCli::with_program(&script_path),
        CleanupDockerRequest {
            label: "openhuman.sandbox=true".into(),
            timeout_ms: 10_000,
        },
    )
    .await
    else {
        return Err(
            std::io::Error::other("failed listing used partial stdout as container ids").into(),
        );
    };

    assert_eq!(error.wire_name(), tinybox_bus::DOCKER_CONTROL_FAILED);
    assert_eq!(
        std::fs::read_to_string(calls)?,
        "ps -aq --filter label=openhuman.sandbox=true\n"
    );
    Ok(())
}

#[tokio::test]
async fn docker_cleanup_with_no_matching_containers_does_not_kill()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let calls = temp.path().join("calls");
    let script_path = temp.path().join("docker");
    write_cli_fixture(
        &script_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n[ \"$1\" = ps ] && exit 0\nexit 2\n",
            calls.display()
        ),
    )?;

    let result = cleanup_with_cli(
        &DockerCli::with_program(&script_path),
        CleanupDockerRequest {
            label: "openhuman.sandbox=true".into(),
            timeout_ms: 10_000,
        },
    )
    .await?;

    assert_eq!(result.containers_stopped, 0);
    assert_eq!(
        std::fs::read_to_string(calls)?,
        "ps -aq --filter label=openhuman.sandbox=true\n"
    );
    Ok(())
}

#[tokio::test]
async fn docker_control_rejects_invalid_deadline_and_label_before_cli_call()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let calls = temp.path().join("calls");
    let script_path = temp.path().join("docker");
    write_cli_fixture(
        &script_path,
        format!(
            "#!/bin/sh\nprintf called >> '{}'\nexit 0\n",
            calls.display()
        ),
    )?;
    let cli = DockerCli::with_program(&script_path);

    let Err(error) = status_with_cli(&cli, DockerStatusRequest { timeout_ms: 0 }).await else {
        return Err(std::io::Error::other("zero deadline was accepted").into());
    };
    assert_eq!(error.wire_name(), tinybox_bus::INVALID_DOCKER_CONTROL);

    let Err(error) = status_with_cli(
        &cli,
        DockerStatusRequest {
            timeout_ms: tinybox_bus::MAX_DOCKER_CONTROL_TIMEOUT_MS + 1,
        },
    )
    .await
    else {
        return Err(std::io::Error::other("deadline above the module limit was accepted").into());
    };
    assert_eq!(error.wire_name(), tinybox_bus::INVALID_DOCKER_CONTROL);

    let Err(error) = cleanup_with_cli(
        &cli,
        CleanupDockerRequest {
            label: "openhuman.sandbox=true\n--privileged".into(),
            timeout_ms: 10_000,
        },
    )
    .await
    else {
        return Err(std::io::Error::other("label control characters were accepted").into());
    };
    assert_eq!(error.wire_name(), tinybox_bus::INVALID_DOCKER_CONTROL);
    assert!(!calls.exists());
    Ok(())
}

#[tokio::test]
async fn run_docker_preserves_one_shot_settings_output_and_timeout()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let workspace = temp.path().join("workspace");
    let readonly = temp.path().join("readonly");
    std::fs::create_dir_all(&workspace)?;
    std::fs::create_dir_all(&readonly)?;
    let argv_file = temp.path().join("argv.bin");
    let script_path = temp.path().join("docker");
    // Round 2 (always verify): this fake used to answer a single `docker run`;
    // every run is now probed, created, read back and started, so it answers
    // each step and records `create`'s argv (which carries every setting).
    let user = default_user(Some(&workspace))?;
    write_cli_fixture(
        &script_path,
        settings_cli_script(&argv_file, &format!("2147483648 256 {user}"), 3),
    )?;

    let request = DockerRunRequest {
        workspace: workspace.display().to_string(),
        image: Some("debian:12".into()),
        command: "make test".into(),
        login_shell: false,
        network: Some("bridge".into()),
        memory_limit_mb: Some(2048),
        cpus: Some(2.5),
        read_only_rootfs: Some(false),
        read_only_mounts: vec![readonly.display().to_string()],
        extra_cap_drops: vec!["NET_RAW".into(), "MKNOD".into()],
        env: [
            ("MODE".into(), "test".into()),
            ("SPACE".into(), "a b".into()),
        ]
        .into(),
        label: Some("openhuman.sandbox=true".into()),
        timeout_ms: 120_000,
        user: None,
        pids_limit: None,
    };
    let output = run_with_cli(&DockerCli::with_program(&script_path), request, true).await?;
    assert_eq!(output.exit_code, 3);
    assert_eq!(output.stdout, "out");
    assert_eq!(output.stderr, "err");
    assert!(!output.timed_out);
    assert_eq!(output.killed, None);
    assert!(output.duration_ms.is_some());
    assert_eq!(
        output.observed_limits,
        Some(tinybox_bus::DockerObservedLimits {
            memory_bytes: 2048 * 1024 * 1024,
            pids_limit: Some(256),
            user: user.clone(),
        })
    );

    let argv = create_argv(&argv_file)?;
    assert_eq!(argv[0], "create", "{argv:?}");
    assert!(has_pair(&argv, "--user", &user), "{argv:?}");
    assert!(has_pair(&argv, "--pids-limit", "256"), "{argv:?}");
    assert!(has_pair(&argv, "--network", "bridge"));
    assert!(has_pair(&argv, "--cap-drop", "NET_RAW"));
    assert!(has_pair(&argv, "--cap-drop", "MKNOD"));
    assert!(has_pair(&argv, "-m", "2048m"));
    assert!(has_pair(&argv, "--cpus", "2.5"));
    assert!(!argv.iter().any(|argument| argument == "--read-only"));
    assert!(has_pair(
        &argv,
        "-v",
        &format!("{}:{}:ro", canonical(&readonly)?, canonical(&readonly)?)
    ));
    // Round 2 (env-file): the environment used to be named with `-e NAME` and
    // passed through the client's own environment. It now reaches only the
    // container through `--env-file`; neither names nor values sit on argv.
    assert_env_only_in_env_file(&argv, &argv_file, "MODE=test\nSPACE=a b\n")?;
    assert!(!argv.iter().any(|argument| argument.contains("a b")));
    assert!(has_pair(&argv, "--label", "openhuman.sandbox=true"));
    assert!(has_pair(&argv, "-w", "/workspace"));
    Ok(())
}

/// The create argv passes exactly one `--env-file`, never `-e`, and the file
/// Docker read held `expected`.
fn assert_env_only_in_env_file(
    argv: &[String],
    argv_file: &Path,
    expected: &str,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    assert!(!argv.iter().any(|argument| argument == "-e"), "{argv:?}");
    assert_eq!(
        argv.iter()
            .filter(|argument| *argument == "--env-file")
            .count(),
        1,
        "{argv:?}"
    );
    assert_eq!(
        std::fs::read_to_string(argv_file.with_extension("bin.env"))?,
        expected
    );
    Ok(())
}

#[tokio::test]
async fn run_without_workspace_preserves_docker_settings_without_host_mounts()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let argv_file = temp.path().join("argv.bin");
    let script_path = temp.path().join("docker");
    // Round 2 (always verify): see `run_docker_preserves_one_shot_settings_output_and_timeout`.
    write_cli_fixture(
        &script_path,
        settings_cli_script(&argv_file, "1610612736 256 65534:65534", 0),
    )?;

    let request = DockerRunRequest {
        workspace: String::new(),
        image: Some("local/test-image:latest".into()),
        command: "printf '%s' \"$MODE\"".into(),
        login_shell: true,
        network: Some("bridge".into()),
        memory_limit_mb: Some(1536),
        cpus: Some(1.5),
        read_only_rootfs: Some(false),
        read_only_mounts: Vec::new(),
        extra_cap_drops: vec!["NET_RAW".into()],
        env: [("MODE".into(), "preserved".into())].into(),
        label: Some("openhuman.sandbox=true".into()),
        timeout_ms: 12_000,
        user: None,
        pids_limit: None,
    };
    let output = run_without_workspace(&DockerCli::with_program(&script_path), request).await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, "out");
    assert_eq!(output.stderr, "err");
    assert!(!output.timed_out);
    assert_eq!(output.killed, None);
    assert!(output.duration_ms.is_some());
    assert_eq!(
        output.observed_limits,
        Some(tinybox_bus::DockerObservedLimits {
            memory_bytes: 1536 * 1024 * 1024,
            pids_limit: Some(256),
            user: "65534:65534".into(),
        })
    );

    let argv = create_argv(&argv_file)?;
    assert_eq!(argv[0], "create", "{argv:?}");
    assert!(has_pair(&argv, "--user", "65534:65534"), "{argv:?}");
    assert!(has_pair(&argv, "--network", "bridge"));
    assert!(has_pair(&argv, "-m", "1536m"));
    assert!(has_pair(&argv, "--cpus", "1.5"));
    assert!(has_pair(&argv, "--cap-drop", "NET_RAW"));
    // Round 2 (env-file): see `run_docker_preserves_one_shot_settings_output_and_timeout`.
    assert_env_only_in_env_file(&argv, &argv_file, "MODE=preserved\n")?;
    assert!(!argv.iter().any(|argument| argument.contains("preserved")));
    assert!(has_pair(&argv, "--label", "openhuman.sandbox=true"));
    assert!(!argv.iter().any(|argument| argument == "-v"));
    assert!(!argv.iter().any(|argument| argument == "-w"));
    assert!(!argv.iter().any(|argument| argument == "--read-only"));
    assert_eq!(
        argv[argv.len() - 3..],
        ["sh", "-lc", "printf '%s' \"$MODE\""]
    );
    Ok(())
}

#[test]
fn validation_rejects_unbounded_runtime() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let request = DockerRunRequest {
        workspace: "/tmp".into(),
        image: None,
        command: "true".into(),
        login_shell: false,
        network: None,
        memory_limit_mb: None,
        cpus: None,
        read_only_rootfs: None,
        read_only_mounts: Vec::new(),
        extra_cap_drops: Vec::new(),
        env: std::collections::BTreeMap::new(),
        label: None,
        timeout_ms: MAX_DOCKER_RUN_TIMEOUT_MS + 1,
        user: None,
        pids_limit: None,
    };
    let error = validate(&request, true)
        .err()
        .ok_or("runtime should be bounded")?;
    assert_eq!(error.wire_name(), tinybox_bus::INVALID_DOCKER_RUN);
    Ok(())
}

#[test]
fn no_workspace_request_rejects_an_ambiguous_workspace_path()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let request = DockerRunRequest {
        workspace: "/host/private".into(),
        image: None,
        command: "true".into(),
        login_shell: false,
        network: None,
        memory_limit_mb: None,
        cpus: None,
        read_only_rootfs: None,
        read_only_mounts: Vec::new(),
        extra_cap_drops: Vec::new(),
        env: std::collections::BTreeMap::new(),
        label: None,
        timeout_ms: 1000,
        user: None,
        pids_limit: None,
    };
    let error = validate(&request, false)
        .err()
        .ok_or("workspace must be absent")?;
    assert_eq!(error.wire_name(), tinybox_bus::INVALID_DOCKER_RUN);
    Ok(())
}

#[tokio::test]
async fn docker_fixture_runs_while_an_inherited_script_writer_is_open()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let script_path = temp.path().join("docker");
    let body = write_cli_fixture(&script_path, "#!/bin/sh\nexit 0\n")?;
    // A concurrently forked process can retain a writer until its exec.
    let writer = std::fs::OpenOptions::new().write(true).open(body)?;
    let status = status_with_cli(
        &DockerCli::with_program(&script_path),
        DockerStatusRequest { timeout_ms: 10_000 },
    )
    .await?;
    drop(writer);
    assert!(status.available);
    Ok(())
}

fn policy_request(network: Option<&str>, mounts: &[&str], workspace: &str) -> DockerRunRequest {
    DockerRunRequest {
        workspace: workspace.into(),
        image: None,
        command: "true".into(),
        login_shell: false,
        network: network.map(str::to_owned),
        memory_limit_mb: None,
        cpus: None,
        read_only_rootfs: None,
        read_only_mounts: mounts.iter().map(|m| (*m).to_owned()).collect(),
        extra_cap_drops: Vec::new(),
        env: std::collections::BTreeMap::new(),
        label: None,
        timeout_ms: 1000,
        user: None,
        pids_limit: None,
    }
}

#[test]
fn docker_run_refuses_host_network_and_dangerous_mounts() {
    for request in [
        policy_request(Some("host"), &[], "/work"),
        policy_request(None, &["/var/run/docker.sock"], "/work"),
        policy_request(None, &["/etc"], "/work"),
        policy_request(None, &[], "/"),
    ] {
        assert!(matches!(
            validate(&request, true),
            Err(error) if error.wire_name() == tinybox_bus::INVALID_DOCKER_RUN
        ));
    }
}

#[test]
fn docker_run_accepts_ordinary_isolated_requests() {
    assert!(validate(&policy_request(Some("none"), &["/usr/lib"], "/work"), true).is_ok());
    assert!(validate(&policy_request(Some("bridge"), &[], ""), false).is_ok());
}

#[test]
fn workspace_less_run_still_refuses_host_network() {
    assert!(validate(&policy_request(Some("host"), &[], ""), false).is_err());
}

// ---- DEF-32: --user, --pids-limit, and the read-back reported to the host ----

/// What a daemon that enforces memory and process limits reports for
/// `docker info --format '{{json .}}'` (the fields the probe reads).
const ENFORCING_INFO: &str = r#"{"MemoryLimit":true,"PidsLimit":true,"SwapLimit":true,"CgroupVersion":"2","CgroupDriver":"systemd","SecurityOptions":["name=seccomp,profile=builtin","name=cgroupns"]}"#;

/// The same daemon without the pids controller: `--pids-limit` is stored but
/// not enforced, so every run must be refused before anything is created.
const NO_PIDS_INFO: &str = r#"{"MemoryLimit":true,"PidsLimit":false,"SwapLimit":true,"CgroupVersion":"2","CgroupDriver":"systemd","SecurityOptions":["name=seccomp,profile=builtin","name=cgroupns"]}"#;

/// A fake CLI for the verified path: records `create`'s argv (NUL separated)
/// and every call, answers `info` with a daemon that enforces limits, and
/// answers `inspect` with `inspect`.
fn verified_cli(
    dir: &Path,
    inspect: &str,
) -> std::io::Result<(DockerCli, std::path::PathBuf, std::path::PathBuf)> {
    verified_cli_with_info(dir, inspect, &format!("echo '{ENFORCING_INFO}'"))
}

/// [`verified_cli`] with `info_arm` as the shell body of the `info` answer.
fn verified_cli_with_info(
    dir: &Path,
    inspect: &str,
    info_arm: &str,
) -> std::io::Result<(DockerCli, std::path::PathBuf, std::path::PathBuf)> {
    let argv_file = dir.join("create.bin");
    let calls = dir.join("calls");
    let script_path = dir.join("docker");
    write_cli_fixture(
        &script_path,
        format!(
            "#!/bin/sh\necho \"$1\" >> '{calls}'\ncase \"$1\" in\n\
             info) {info_arm};;\n\
             create) : > '{argv}'; for a in \"$@\"; do printf '%s\\0' \"$a\" >> '{argv}'; done; echo cid-9;;\n\
             inspect) echo '{inspect}';;\n\
             start) printf out; printf err >&2; exit 4;;\n\
             esac\n",
            calls = calls.display(),
            argv = argv_file.display(),
        ),
    )?;
    Ok((DockerCli::with_program(&script_path), argv_file, calls))
}

/// The module's default identity for `workspace`: its non-root owner, else
/// `65534:65534` (also the identity for runs without a workspace).
fn default_user(workspace: Option<&Path>) -> std::io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let Some(workspace) = workspace else {
        return Ok("65534:65534".to_owned());
    };
    let metadata = std::fs::metadata(workspace)?;
    Ok(if metadata.uid() == 0 {
        "65534:65534".to_owned()
    } else {
        format!("{}:{}", metadata.uid(), metadata.gid())
    })
}

fn create_argv(argv_file: &Path) -> std::io::Result<Vec<String>> {
    Ok(std::fs::read(argv_file)?
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect())
}

fn hardened_request(workspace: &Path) -> DockerRunRequest {
    let mut request = policy_request(None, &[], &workspace.display().to_string());
    request.user = Some("1000:1000".into());
    request.pids_limit = Some(256);
    request.timeout_ms = 20_000;
    request
}

#[tokio::test]
async fn def32_user_and_pids_limit_reach_the_container_and_are_read_back()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (cli, argv_file, calls) = verified_cli(temp.path(), "536870912 256 1000:1000")?;
    let output = run_with_cli(&cli, hardened_request(temp.path()), true).await?;
    assert_eq!(output.exit_code, 4);
    assert_eq!(output.stdout, "out");
    assert_eq!(output.stderr, "err");
    assert_eq!(
        output.observed_limits,
        Some(tinybox_bus::DockerObservedLimits {
            memory_bytes: 536_870_912,
            pids_limit: Some(256),
            user: "1000:1000".into(),
        })
    );
    let argv: Vec<String> = std::fs::read(&argv_file)?
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    assert_eq!(argv[0], "create");
    assert!(has_pair(&argv, "--user", "1000:1000"), "{argv:?}");
    assert!(has_pair(&argv, "--pids-limit", "256"), "{argv:?}");
    assert_eq!(
        std::fs::read_to_string(&calls)?,
        "info\ncreate\ninspect\nstart\n"
    );
    Ok(())
}

#[tokio::test]
async fn def32_a_run_whose_limits_are_not_in_force_never_starts()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (cli, _, calls) = verified_cli(temp.path(), "536870912 <nil> ")?;
    let error = run_with_cli(&cli, hardened_request(temp.path()), true)
        .await
        .err()
        .ok_or("a run without its limits in force must fail")?;
    assert_eq!(error.wire_name(), tinybox_bus::INVALID_DOCKER_RUN);
    assert_eq!(
        std::fs::read_to_string(&calls)?,
        "info\ncreate\ninspect\nrm\n"
    );
    Ok(())
}

// Round 2 (always verify): this test used to assert that a request without
// `user` / `pids_limit` took the plain `docker run` path and reported no
// read-back. Every run is now probed and read back, so it asserts the opposite.
#[tokio::test]
async fn def32_requests_without_user_or_pids_are_verified_and_report_the_default_read_back()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let user = default_user(Some(temp.path()))?;
    let (cli, argv_file, calls) = verified_cli(temp.path(), &format!("536870912 256 {user}"))?;
    let mut request = policy_request(None, &[], &temp.path().display().to_string());
    request.timeout_ms = 20_000;
    let output = run_with_cli(&cli, request, true).await?;
    assert_eq!(
        output.observed_limits,
        Some(tinybox_bus::DockerObservedLimits {
            memory_bytes: 536_870_912,
            pids_limit: Some(256),
            user: user.clone(),
        })
    );
    assert_eq!(output.exit_code, 4);
    let argv = create_argv(&argv_file)?;
    assert!(has_pair(&argv, "--user", &user), "{argv:?}");
    assert!(has_pair(&argv, "--pids-limit", "256"), "{argv:?}");
    assert_eq!(
        std::fs::read_to_string(&calls)?,
        "info\ncreate\ninspect\nstart\n"
    );
    Ok(())
}

#[tokio::test]
async fn round2_enforcement_refusal_has_its_own_error_and_creates_nothing()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (cli, _, calls) = verified_cli_with_info(
        temp.path(),
        "536870912 256 1000:1000",
        &format!("echo '{NO_PIDS_INFO}'"),
    )?;
    let error = run_with_cli(&cli, hardened_request(temp.path()), true)
        .await
        .err()
        .ok_or("a daemon without the pids controller must refuse the run")?;
    assert_eq!(
        error.wire_name(),
        tinybox_bus::DOCKER_LIMITS_UNENFORCEABLE,
        "{error:?}"
    );
    assert_ne!(
        tinybox_bus::DOCKER_LIMITS_UNENFORCEABLE,
        tinybox_bus::INVALID_DOCKER_RUN
    );
    assert!(error.to_string().contains("PidsLimit=false"), "{error}");
    // Refused before anything was created: nothing to remove, nothing started.
    assert_eq!(std::fs::read_to_string(&calls)?, "info\n");

    // A plain request is refused the same way: the floor applies to every run.
    std::fs::remove_file(&calls)?;
    let mut plain = policy_request(None, &[], &temp.path().display().to_string());
    plain.timeout_ms = 20_000;
    let error = run_with_cli(&cli, plain, true)
        .await
        .err()
        .ok_or("the process floor must be enforceable for plain runs too")?;
    assert_eq!(error.wire_name(), tinybox_bus::DOCKER_LIMITS_UNENFORCEABLE);
    assert_eq!(std::fs::read_to_string(&calls)?, "info\n");
    Ok(())
}

#[tokio::test]
async fn round2_a_failed_or_slow_probe_is_not_reported_as_an_enforcement_refusal()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    // A probe that fails is a verification failure, not a refusal.
    let temp = tempfile::tempdir()?;
    let (cli, _, calls) =
        verified_cli_with_info(temp.path(), "unused", "echo daemon down >&2; exit 1")?;
    let error = run_with_cli(&cli, hardened_request(temp.path()), true)
        .await
        .err()
        .ok_or("a failed probe must refuse the run")?;
    assert_eq!(
        error.wire_name(),
        tinybox_bus::INVALID_DOCKER_RUN,
        "{error:?}"
    );
    assert!(
        error.to_string().contains("could not be verified"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&calls)?, "info\n");

    // A probe that overruns the deadline is reported as a deadline, not a refusal.
    let slow = tempfile::tempdir()?;
    let (cli, _, _) = verified_cli_with_info(slow.path(), "unused", "exec sleep 10")?;
    let mut request = hardened_request(slow.path());
    request.timeout_ms = 300;
    let error = run_with_cli(&cli, request, true)
        .await
        .err()
        .ok_or("a probe past the deadline must refuse the run")?;
    assert_eq!(
        error.wire_name(),
        tinybox_bus::INVALID_DOCKER_RUN,
        "{error:?}"
    );
    assert!(
        error.to_string().contains("exceeded its deadline"),
        "{error}"
    );
    Ok(())
}

#[test]
fn def32_user_must_be_numeric_uid_gid_and_pids_limit_positive() {
    let with = |user: Option<&str>, pids: Option<u64>| {
        let mut request = policy_request(None, &[], "/work");
        request.user = user.map(str::to_owned);
        request.pids_limit = pids;
        validate(&request, true)
    };
    assert!(with(Some("1000:1000"), Some(256)).is_ok());
    assert!(with(Some("65534:65534"), None).is_ok());
    for bad in [
        "root",
        "1000",
        "1000:",
        ":1000",
        "a:b",
        "1000:1000:1",
        "-1:0",
        " 1:1",
    ] {
        assert!(
            matches!(with(Some(bad), None), Err(e) if e.wire_name() == tinybox_bus::INVALID_DOCKER_RUN),
            "{bad}"
        );
    }
    assert!(with(None, Some(0)).is_err());
}

#[test]
fn merge_explicit_user_stays_non_root_and_pids_limit_cannot_loosen_the_cap() {
    let with = |user: Option<&str>, pids: Option<u64>| {
        let mut request = policy_request(None, &[], "/work");
        request.user = user.map(str::to_owned);
        request.pids_limit = pids;
        validate(&request, true)
    };
    for root in ["0:0", "0:1000", "00:1000"] {
        assert!(
            matches!(with(Some(root), None), Err(e) if e.wire_name() == tinybox_bus::INVALID_DOCKER_RUN),
            "{root}"
        );
    }
    assert!(with(None, Some(1)).is_ok());
    assert!(with(None, Some(tinybox_docker::MAX_ONE_SHOT_PIDS_LIMIT)).is_ok());
    for loose in [tinybox_docker::MAX_ONE_SHOT_PIDS_LIMIT + 1, u64::MAX] {
        assert!(
            matches!(with(None, Some(loose)), Err(e) if e.wire_name() == tinybox_bus::INVALID_DOCKER_RUN),
            "{loose}"
        );
    }
}

#[tokio::test]
async fn merge_an_explicit_pids_limit_alone_reads_back_the_default_user()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let user = {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(&workspace)?;
        if metadata.uid() == 0 {
            "65534:65534".to_owned()
        } else {
            format!("{}:{}", metadata.uid(), metadata.gid())
        }
    };
    let (cli, argv_file, calls) = verified_cli(temp.path(), &format!("536870912 64 {user}"))?;
    let mut request = policy_request(None, &[], &workspace.display().to_string());
    request.pids_limit = Some(64);
    request.timeout_ms = 20_000;
    let output = run_with_cli(&cli, request, true).await?;
    assert_eq!(
        output.observed_limits,
        Some(tinybox_bus::DockerObservedLimits {
            memory_bytes: 536_870_912,
            pids_limit: Some(64),
            user: user.clone(),
        })
    );
    assert_eq!(output.killed, None);
    assert!(output.duration_ms.is_some());
    let argv: Vec<String> = std::fs::read(&argv_file)?
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    assert_eq!(argv[2], "--name", "{argv:?}");
    assert!(argv[3].starts_with("tinybox-"), "{argv:?}");
    assert!(has_pair(&argv, "--user", &user), "{argv:?}");
    assert!(has_pair(&argv, "--pids-limit", "64"), "{argv:?}");
    assert_eq!(
        std::fs::read_to_string(&calls)?,
        "info\ncreate\ninspect\nstart\n"
    );
    Ok(())
}

#[test]
fn round2_explicit_user_refuses_gid_0_and_memory_limit_refuses_0() {
    let with = |user: Option<&str>, memory: Option<u64>| {
        let mut request = policy_request(None, &[], "/work");
        request.user = user.map(str::to_owned);
        request.memory_limit_mb = memory;
        validate(&request, true)
    };
    for root_group in ["1000:0", "1000:00", "65534:0"] {
        assert!(
            matches!(with(Some(root_group), None), Err(e) if e.wire_name() == tinybox_bus::INVALID_DOCKER_RUN),
            "{root_group}"
        );
    }
    assert!(with(Some("1000:1"), None).is_ok());
    assert!(matches!(
        with(None, Some(0)),
        Err(e) if e.wire_name() == tinybox_bus::INVALID_DOCKER_RUN
    ));
    assert!(with(None, Some(1)).is_ok());
    assert!(with(None, None).is_ok());
}

// ---- Codex P1 on PR #18: the module advertises the hardened Docker run ----

#[test]
fn module_advertises_the_hardened_docker_run_capability() {
    for platform in [
        super::super::Platform::Linux,
        super::super::Platform::Unix,
        super::super::Platform::Windows,
        super::super::Platform::Other,
    ] {
        let capabilities = super::super::capabilities_for(platform);
        assert!(
            tinybox_bus::supports_hardened_docker_run(&capabilities),
            "{capabilities:?}"
        );
        assert_eq!(
            capabilities.features,
            vec![tinybox_bus::HARDENED_DOCKER_RUN_FEATURE.to_owned()]
        );
        // Round 2: the floor holds on every run, so a host that requires it
        // may send this module plain requests too.
        assert!(tinybox_bus::docker_run_request_is_supported_with_policy(
            &capabilities,
            &policy_request(None, &[], "/work"),
            tinybox_bus::HardenedDockerRunPolicy::Always,
        ));
    }
}

#[tokio::test]
async fn hardened_request_round_trips_to_the_module_and_reports_observed_limits()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    // The host reads the module description off the wire and checks the
    // capability before it sends user / pids_limit.
    let capabilities: tinybox_bus::ModuleCapabilities =
        serde_json::from_value(serde_json::to_value(super::super::capabilities_for(
            super::super::Platform::current(),
        ))?)?;
    let temp = tempfile::tempdir()?;
    let mut sent = hardened_request(temp.path());
    sent.pids_limit = Some(64);
    assert!(tinybox_bus::docker_run_request_is_supported(
        &capabilities,
        &sent
    ));

    let received: DockerRunRequest = serde_json::from_value(serde_json::to_value(&sent)?)?;
    assert_eq!(received.user.as_deref(), Some("1000:1000"));
    assert_eq!(received.pids_limit, Some(64));

    let (cli, argv_file, _) = verified_cli(temp.path(), "536870912 64 1000:1000")?;
    let output = run_with_cli(&cli, received, true).await?;
    let returned: tinybox_bus::DockerRunOutput =
        serde_json::from_value(serde_json::to_value(&output)?)?;
    assert_eq!(
        returned.observed_limits,
        Some(tinybox_bus::DockerObservedLimits {
            memory_bytes: 536_870_912,
            pids_limit: Some(64),
            user: "1000:1000".into(),
        })
    );
    let argv: Vec<String> = std::fs::read(&argv_file)?
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    assert!(has_pair(&argv, "--user", "1000:1000"), "{argv:?}");
    assert!(has_pair(&argv, "--pids-limit", "64"), "{argv:?}");
    Ok(())
}
