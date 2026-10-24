use super::*;
use std::os::unix::fs::PermissionsExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixture(kill: i32, remove: i32) -> io::Result<(tempfile::TempDir, DockerCli)> {
    let dir = tempfile::tempdir()?;
    let program = dir.path().join("docker");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\n[ \"$1\" = --tinybox-probe ] && exit 0\nprintf '%s ' \"$@\" >> '{0}/calls'\nprintf '\\n' >> '{0}/calls'\ncase \"$1\" in\nrun) exec sleep 5;;\nkill) exit {kill};;\nrm) exit {remove};;\nesac\n",
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
        let result = cli.run_one_shot(&spec, Duration::from_millis(50)).await?;
        assert!(result.timed_out);
        assert_eq!(result.killed, Some(stopped));
        assert!(result.duration_ms >= 50);
        assert_eq!(result.stderr.contains("THE KILL FAILED"), !stopped);
        let calls = std::fs::read_to_string(dir.path().join("calls"))?;
        let lines: Vec<_> = calls.lines().collect();
        let name = lines[0].split_whitespace().nth(3).ok_or("missing name")?;
        assert!(name.starts_with("tinybox-"));
        assert_eq!(lines[1].trim(), format!("kill {name}"));
        if kill != 0 {
            assert_eq!(lines[2].trim(), format!("rm -f {name}"));
        } else {
            assert_eq!(lines.len(), 2);
        }
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
    let (a, b) = tokio::join!(
        first_cli.run_one_shot(&first_spec, Duration::from_millis(50)),
        second_cli.run_one_shot(&second_spec, Duration::from_millis(50)),
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
    let wait =
        tokio::spawn(async move { cli.run_one_shot(&spec, Duration::from_millis(100)).await });
    let calls = dir.path().join("calls");
    for _ in 0..100 {
        if calls.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(calls.exists());
    wait.abort();
    for _ in 0..100 {
        if std::fs::read_to_string(&calls)?.contains("\nkill ") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("abandoned container did not receive targeted cleanup".into())
}

#[tokio::test]
async fn missing_cleanup_client_cannot_claim_a_stop() {
    let cli = DockerCli::with_program("/definitely/missing/tinybox-docker");
    assert!(!stop_container(&cli, "owned-container").await);
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
