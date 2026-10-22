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
        "ps -q --filter label=openhuman.sandbox=true\nkill box-a box-b\n"
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
        "ps -q --filter label=openhuman.sandbox=true\n"
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
        "ps -q --filter label=openhuman.sandbox=true\n"
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
    write_cli_fixture(
        &script_path,
        format!(
            "#!/bin/sh\n: > '{}'\nfor a in \"$@\"; do printf '%s\\0' \"$a\" >> '{}'; done\nprintf out\nprintf err >&2\nexit 3\n",
            argv_file.display(),
            argv_file.display(),
        ),
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
    };
    let output = run_with_cli(&DockerCli::with_program(&script_path), request, true).await?;
    assert_eq!(output.exit_code, 3);
    assert_eq!(output.stdout, "out");
    assert_eq!(output.stderr, "err");
    assert!(!output.timed_out);

    let argv: Vec<String> = std::fs::read(&argv_file)?
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
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
    assert!(has_pair(&argv, "-e", "MODE=test"));
    assert!(has_pair(&argv, "-e", "SPACE=a b"));
    assert!(has_pair(&argv, "--label", "openhuman.sandbox=true"));
    assert!(has_pair(&argv, "-w", "/workspace"));
    Ok(())
}

#[tokio::test]
async fn run_without_workspace_preserves_docker_settings_without_host_mounts()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let argv_file = temp.path().join("argv.bin");
    let script_path = temp.path().join("docker");
    write_cli_fixture(
        &script_path,
        format!(
            "#!/bin/sh\n: > '{}'\nfor a in \"$@\"; do printf '%s\\0' \"$a\" >> '{}'; done\nprintf out\nprintf err >&2\nexit 0\n",
            argv_file.display(),
            argv_file.display(),
        ),
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
    };
    let output = run_without_workspace(&DockerCli::with_program(&script_path), request).await?;
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, "out");
    assert_eq!(output.stderr, "err");
    assert!(!output.timed_out);

    let argv: Vec<String> = std::fs::read(&argv_file)?
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    assert!(has_pair(&argv, "--network", "bridge"));
    assert!(has_pair(&argv, "-m", "1536m"));
    assert!(has_pair(&argv, "--cpus", "1.5"));
    assert!(has_pair(&argv, "--cap-drop", "NET_RAW"));
    assert!(has_pair(&argv, "-e", "MODE=preserved"));
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
