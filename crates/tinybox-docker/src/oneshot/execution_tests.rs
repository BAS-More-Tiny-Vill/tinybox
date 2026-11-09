use super::*;
use std::os::unix::fs::PermissionsExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;

// Round 2: every run is verified, so the fixture answers the enforcement
// probe (not logged), `create`, `inspect` (the spec's effective defaults) and
// an attached `start` that outlives the deadline.
fn fixture(kill: i32, remove: i32) -> io::Result<(tempfile::TempDir, DockerCli)> {
    let dir = tempfile::tempdir()?;
    let program = dir.path().join("docker");
    let user = super::super::sandbox_user_for(Some(dir.path()));
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\n[ \"$1\" = --tinybox-probe ] && exit 0\n[ \"$1\" = info ] && {{ printf '%s\\n' '{{\"MemoryLimit\":true,\"PidsLimit\":true}}'; exit 0; }}\nprintf '%s ' \"$@\" >> '{0}/calls'\nprintf '\\n' >> '{0}/calls'\ncase \"$1\" in\ncreate) echo cid-1;;\ninspect) echo '536870912 256 {user}';;\nstart) exec sleep 5;;\nkill) exit {kill};;\nrm) exit {remove};;\nesac\n",
            dir.path().display(),
        ),
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
    super::super::test::wait_until_executable(&program);
    let cli = DockerCli::with_program(program);
    Ok((dir, cli))
}

#[tokio::test]
async fn timeout_cleanup_targets_only_its_unique_container() -> TestResult {
    for (kill, remove, stopped) in [(0, 0, true), (1, 0, true), (1, 1, false)] {
        let (dir, cli) = fixture(kill, remove)?;
        let spec = OneShot::new(dir.path(), "untrusted").with_label("shared=label");
        // Round 2: the budget also covers probe, create and inspect.
        let result = cli.run_one_shot(&spec, Duration::from_millis(500)).await?;
        assert!(result.timed_out);
        assert_eq!(result.killed, Some(stopped));
        assert!(result.duration_ms >= 500);
        assert_eq!(result.stderr.contains("THE KILL FAILED"), !stopped);
        let calls = std::fs::read_to_string(dir.path().join("calls"))?;
        let lines: Vec<_> = calls.lines().map(str::trim).collect();
        let name = lines[0].split_whitespace().nth(3).ok_or("missing name")?;
        assert!(name.starts_with("tinybox-"));
        // Round 2: create, inspect, start, then kill by name and remove by id.
        assert!(lines[0].starts_with("create --rm --name tinybox-"));
        assert_eq!(lines[2], "start -a cid-1");
        assert_eq!(lines[3], format!("kill {name}"));
        assert_eq!(lines[4], "rm -f cid-1");
        assert_eq!(lines.len(), 5);
        assert!(!calls.contains("ps --filter"));
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_jobs_never_reuse_a_container_name() -> TestResult {
    let (first, first_cli) = fixture(0, 0)?;
    let (second, second_cli) = fixture(0, 0)?;
    let first_spec = OneShot::new(first.path(), "first");
    let second_spec = OneShot::new(second.path(), "second");
    // Round 2: the budget also covers probe, create and inspect.
    let (a, b) = tokio::join!(
        first_cli.run_one_shot(&first_spec, Duration::from_millis(500)),
        second_cli.run_one_shot(&second_spec, Duration::from_millis(500)),
    );
    assert!(a?.killed == Some(true) && b?.killed == Some(true));
    let first_calls = std::fs::read_to_string(first.path().join("calls"))?;
    let second_calls = std::fs::read_to_string(second.path().join("calls"))?;
    assert_ne!(
        first_calls.split_whitespace().nth(3),
        second_calls.split_whitespace().nth(3)
    );
    Ok(())
}

#[tokio::test]
async fn dropping_the_wait_keeps_timeout_supervision() -> TestResult {
    let (dir, cli) = fixture(0, 0)?;
    let spec = OneShot::new(dir.path(), "abandoned");
    // Round 2: the budget also covers probe, create and inspect.
    let wait =
        tokio::spawn(async move { cli.run_one_shot(&spec, Duration::from_millis(500)).await });
    let calls = dir.path().join("calls");
    for _ in 0..100 {
        if calls.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(calls.exists());
    wait.abort();
    for _ in 0..300 {
        if std::fs::read_to_string(&calls)?.contains("\nkill ") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("abandoned container did not receive targeted cleanup".into())
}

// PR #18 review: a created container whose `start -a` fails with an I/O error
// (here, the client cannot be spawned) is removed by id, and the error says
// whether Docker confirmed it; a failed removal is never reported as done.
#[tokio::test]
async fn a_created_container_whose_start_fails_is_removed_by_id() -> TestResult {
    for (remove, removed) in [(0, true), (1, false)] {
        let (dir, cli) = fixture(0, remove)?;
        let error = attach(
            &cli,
            Command::new("/definitely/missing/tinybox-docker"),
            Owned::Created {
                name: "tinybox-owned",
                id: "cid-1",
            },
            Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await
        .err()
        .ok_or("a start that cannot spawn must fail")?;
        let calls = std::fs::read_to_string(dir.path().join("calls"))?;
        let lines: Vec<_> = calls.lines().map(str::trim).collect();
        assert_eq!(lines, ["rm -f cid-1"], "{remove}");
        let message = error.to_string();
        assert_eq!(message.contains("was removed"), removed, "{message}");
        assert_eq!(message.contains("CLEANUP FAILED"), !removed, "{message}");
    }
    Ok(())
}

#[test]
fn enforcement_probe_parses_engine_info_and_fails_closed() {
    let parse = LimitEnforcement::parse;
    let enforcing = parse(
        r#"{"MemoryLimit":true,"PidsLimit":true,"SwapLimit":false,"CgroupVersion":"2","CgroupDriver":"systemd","SecurityOptions":["name=seccomp,profile=builtin","name=rootless"]}"#,
    );
    assert_eq!(
        enforcing,
        Some(LimitEnforcement {
            memory_limit: true,
            pids_limit: true,
            cgroup_version: "2".into(),
            cgroup_driver: "systemd".into(),
            rootless: true,
        })
    );
    assert_eq!(enforcing.and_then(|e| e.refusal()), None);
    for unreadable in [
        "",
        "not json",
        "{}",
        r#"{"MemoryLimit":true}"#,
        r#"{"PidsLimit":true}"#,
        r#"{"MemoryLimit":1,"PidsLimit":true}"#,
        r#"{"MemoryLimit":true,"PidsLimit":"true"}"#,
    ] {
        assert_eq!(parse(unreadable), None, "{unreadable}");
    }
    // Round 2 (owner decision 4): a memory limit always applies (zero is
    // refused as invalid), so the refusal no longer depends on the spec.
    let refuse = |json: &str| parse(json).and_then(|e| e.refusal());
    assert!(
        refuse(r#"{"MemoryLimit":true,"PidsLimit":false}"#)
            .is_some_and(|r| r.contains("PidsLimit"))
    );
    assert!(
        refuse(r#"{"MemoryLimit":false,"PidsLimit":true}"#)
            .is_some_and(|r| r.contains("MemoryLimit"))
    );
    assert!(
        refuse(
            r#"{"MemoryLimit":true,"PidsLimit":true,"CgroupVersion":"1","CgroupDriver":"cgroupfs","SecurityOptions":["name=rootless"]}"#,
        )
        .is_some_and(|r| r.contains("rootless"))
    );
}

#[tokio::test]
async fn missing_cleanup_client_cannot_claim_a_stop() {
    let cli = DockerCli::with_program("/definitely/missing/tinybox-docker");
    // Round 2: the stop also removes by id; neither step can be confirmed.
    let (killed, removal) = stop_container(&cli, "owned-container", "owned-id").await;
    assert!(!killed);
    assert_eq!(removal, Removal::Unconfirmed);
}

#[test]
fn identity_is_nonroot_and_process_count_is_bounded() -> TestResult {
    let dir = tempfile::tempdir()?;
    let spec = OneShot::new(dir.path(), "true");
    let args: Vec<_> = spec
        .argv()
        .into_iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let user = args
        .windows(2)
        .find(|a| a[0] == "--user")
        .ok_or("missing user")?;
    assert_ne!(user[1].split(':').next(), Some("0"));
    assert!(args.windows(2).any(|a| a == ["--pids-limit", "256"]));
    assert_eq!(super::super::sandbox_user_for(None), "65534:65534");
    assert_eq!(
        super::super::sandbox_user_for(Some(std::path::Path::new("/"))),
        "65534:65534"
    );
    assert_eq!(
        super::super::sandbox_user_for(Some(std::path::Path::new("/definitely/missing/workspace"))),
        "65534:65534"
    );
    Ok(())
}
