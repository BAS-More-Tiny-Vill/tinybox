//! Shell startup isolation, quoting and pipeline-status regressions.
use super::*;

fn request(preserve_pipeline_status: bool) -> SelectShellRequest {
    SelectShellRequest {
        command: "false | true".into(),
        preserve_pipeline_status,
        runtime_path: None,
        python_utf8: false,
    }
}

#[test]
fn bash_surfaces_pipe_failures_and_preservation_keeps_the_original_payload() {
    let selected = selection_for(&request(false), ShellFlavor::Posix, Some("/bin/bash"));
    assert_eq!(
        selected.arguments,
        ["-p", "-c", "set -o pipefail\nfalse | true"]
    );
    let preserved = selection_for(&request(true), ShellFlavor::Posix, Some("/bin/bash"));
    assert_eq!(preserved.arguments, ["-p", "-c", "false | true"]);
}

#[test]
fn sh_fallback_never_receives_bash_only_flags() {
    let selected = selection_for(&request(false), ShellFlavor::Posix, None);
    assert_eq!(selected.program, "/bin/sh");
    assert_eq!(selected.arguments, ["-c", "false | true"]);
    assert_eq!(selected.raw_arguments, None);
    assert_eq!(selected.bootstrap_environment, Vec::<String>::new());
}

#[test]
fn cmd_preserves_quoted_payload_and_lists_bootstrap_names_without_values() {
    let payload = r#"echo %USERPROFILE% > "C:\Program Files (x86)\out.txt""#;
    let selected = selection_for(
        &SelectShellRequest {
            command: payload.into(),
            preserve_pipeline_status: false,
            runtime_path: None,
            python_utf8: false,
        },
        ShellFlavor::Cmd,
        None,
    );
    assert_eq!(selected.program, cmd_path());
    assert_eq!(selected.arguments, ["/D", "/E:ON", "/V:OFF", "/S", "/C"]);
    assert_eq!(selected.raw_arguments, Some(format!("\"{payload}\"")));
    assert_eq!(
        selected.bootstrap_environment,
        WINDOWS_BOOTSTRAP_ENVIRONMENT
    );
}

#[test]
#[cfg(windows)]
fn actual_cmd_raw_payload_keeps_a_quoted_executable_and_argument()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let executable = root.path().join("tool with spaces.exe");
    std::fs::copy(std::env::current_exe()?, &executable)?;
    let output = root.path().join("quoted executable output.txt");
    let requested = SelectShellRequest {
        command: format!(
            "\"{}\" \"--list\" > \"{}\"",
            executable.display(),
            output.display()
        ),
        preserve_pipeline_status: false,
        runtime_path: None,
        python_utf8: false,
    };
    assert!(build_shell_command(&requested).status()?.success());
    assert!(
        std::fs::read_to_string(output)?
            .contains("actual_cmd_raw_payload_keeps_a_quoted_executable_and_argument")
    );
    Ok(())
}

#[test]
fn selected_shell_and_native_builder_agree() {
    let requested = request(false);
    let selected = select_shell(&requested);
    let command = build_shell_command(&requested);
    assert_eq!(
        command.get_program(),
        std::ffi::OsStr::new(&selected.program)
    );
}

#[test]
#[cfg(unix)]
fn actual_bash_pipeline_failure_is_visible_unless_preservation_is_requested()
-> Result<(), Box<dyn std::error::Error>> {
    // The fallback is tested above; execution only asserts Bash semantics on
    // hosts whose selected shell actually supports them.
    if bash_path().is_none() {
        return Ok(());
    }
    assert!(!build_shell_command(&request(false)).status()?.success());
    assert!(build_shell_command(&request(true)).status()?.success());
    Ok(())
}

#[test]
#[cfg(windows)]
fn actual_cmd_raw_payload_keeps_quoted_redirection_paths() -> Result<(), Box<dyn std::error::Error>>
{
    let root = tempfile::tempdir()?;
    let output = root.path().join("output with spaces (fixture).txt");
    let requested = SelectShellRequest {
        command: format!("echo fixture > \"{}\"", output.display()),
        preserve_pipeline_status: false,
        runtime_path: None,
        python_utf8: false,
    };
    assert!(build_shell_command(&requested).status()?.success());
    assert_eq!(std::fs::read_to_string(output)?.trim(), "fixture");
    Ok(())
}

#[test]
#[cfg(unix)]
fn bash_startup_environment_cannot_replace_the_requested_payload()
-> Result<(), Box<dyn std::error::Error>> {
    const CHILD: &str = "TINYBOX_SHELL_ENV_REGRESSION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let request = SelectShellRequest {
            command: "exit 0".into(),
            preserve_pipeline_status: false,
            runtime_path: None,
            python_utf8: false,
        };
        assert!(build_shell_command(&request).status()?.success());
        return Ok(());
    }
    let fixture = tempfile::tempdir()?;
    let startup = fixture.path().join("startup.sh");
    std::fs::write(&startup, "exit 77\n")?;
    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "local::shell::tests::bash_startup_environment_cannot_replace_the_requested_payload",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("BASH_ENV", &startup)
        .status()?;
    assert!(
        status.success(),
        "startup environment changed the child payload result"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn inherited_bash_options_cannot_abort_the_requested_payload()
-> Result<(), Box<dyn std::error::Error>> {
    const CHILD: &str = "TINYBOX_SHELL_OPTIONS_REGRESSION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let request = SelectShellRequest {
            command: "false; echo reached".into(),
            preserve_pipeline_status: false,
            runtime_path: None,
            python_utf8: false,
        };
        let output = build_shell_command(&request).output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout, b"reached\n");
        return Ok(());
    }
    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "local::shell::tests::inherited_bash_options_cannot_abort_the_requested_payload",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("SHELLOPTS", "errexit:nounset")
        .env("BASHOPTS", "failglob")
        .status()?;
    assert!(
        status.success(),
        "inherited Bash options changed payload semantics"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn exported_bash_functions_cannot_hide_pipeline_failures() -> Result<(), Box<dyn std::error::Error>>
{
    const CHILD: &str = "TINYBOX_SHELL_FUNCTION_REGRESSION_CHILD";
    if bash_path().is_none() {
        return Ok(());
    }
    if std::env::var_os(CHILD).is_some() {
        assert!(!build_shell_command(&request(false)).status()?.success());
        return Ok(());
    }
    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "local::shell::tests::exported_bash_functions_cannot_hide_pipeline_failures",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("BASH_FUNC_false%%", "() { return 0; }")
        .status()?;
    assert!(
        status.success(),
        "exported function hid the failing pipeline stage"
    );
    Ok(())
}

#[test]
fn bash_discovery_rechecks_availability() -> Result<(), Box<dyn std::error::Error>> {
    // A changing candidate list represents installation/removal without altering
    // global system paths or relying on the machine's installed shell.
    let fixture = tempfile::NamedTempFile::new()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    let candidate_string = fixture.path().to_string_lossy().into_owned();
    let candidate = candidate_string.as_str();
    assert_eq!(bash_path_in(&[candidate]), Some(candidate));
    fixture.close()?;
    assert_eq!(bash_path_in(&[candidate]), None);
    std::fs::write(candidate, "fixture")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(candidate, std::fs::Permissions::from_mode(0o700))?;
    }
    let discovered = bash_path_in(&[candidate]);
    std::fs::remove_file(candidate)?;
    assert_eq!(discovered, Some(candidate));
    Ok(())
}

#[test]
#[cfg(unix)]
fn fallback_shell_survives_an_overridden_payload_path() -> Result<(), Box<dyn std::error::Error>> {
    let selected = selection_for(
        &SelectShellRequest {
            command: "exit 0".into(),
            preserve_pipeline_status: false,
            runtime_path: None,
            python_utf8: false,
        },
        ShellFlavor::Posix,
        None,
    );
    let status = command_for(selected).env("PATH", "").status()?;
    assert!(status.success());
    Ok(())
}

#[test]
fn cmd_selection_uses_the_parent_system_directory() {
    let selected = selection_for(&request(false), ShellFlavor::Cmd, None);
    let root = std::env::var_os("SystemRoot").map_or_else(
        || std::path::PathBuf::from(r"C:\Windows"),
        std::path::PathBuf::from,
    );
    assert_eq!(
        std::path::Path::new(&selected.program),
        root.join("System32").join("cmd.exe")
    );
}

#[test]
#[cfg(windows)]
fn cmd_startup_ignores_the_payload_path() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    // A test runner is a valid executable but cannot interpret cmd's switches.
    std::fs::copy(std::env::current_exe()?, fixture.path().join("cmd.exe"))?;
    let requested = SelectShellRequest {
        command: "echo %PATH%".into(),
        preserve_pipeline_status: false,
        runtime_path: None,
        python_utf8: false,
    };
    let output = build_shell_command(&requested)
        .env("PATH", fixture.path())
        .output()?;
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)?.trim(),
        fixture.path().to_string_lossy()
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn inherited_posix_mode_cannot_change_bash_command_substitution()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(bash) = bash_path() else {
        return Ok(());
    };
    let selected = selection_for(
        &SelectShellRequest {
            command: "set -e; x=$(false; echo reached); echo \"$x\"".into(),
            preserve_pipeline_status: true,
            runtime_path: None,
            python_utf8: false,
        },
        ShellFlavor::Posix,
        Some(bash),
    );
    // Supply the ambient value before applying the invocation's removal list.
    let mut command = std::process::Command::new(&selected.program);
    command
        .args(&selected.arguments)
        .env("POSIXLY_CORRECT", "1");
    for name in selected.remove_environment {
        command.env_remove(name);
    }
    let output = command.output()?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"reached\n");
    Ok(())
}

#[test]
#[cfg(unix)]
fn bash_discovery_skips_non_executable_files() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let first = tempfile::NamedTempFile::new()?;
    let second = tempfile::NamedTempFile::new()?;
    std::fs::set_permissions(second.path(), std::fs::Permissions::from_mode(0o700))?;
    let first = first.path().to_str().ok_or("non utf8 fixture")?;
    let second = second.path().to_str().ok_or("non utf8 fixture")?;
    assert_eq!(bash_path_in(&[first, second]), Some(second));
    assert_eq!(bash_path_in(&[first]), None);
    Ok(())
}

#[test]
fn runtime_path_is_exported_after_login_profiles_and_after_pipefail() {
    let mut req = request(false);
    req.command = "echo hi".into();
    req.runtime_path = Some("/opt/it's/bin:/usr/bin".into());
    let posix = selection_for(&req, ShellFlavor::Posix, Some("/bin/bash"));
    assert_eq!(
        posix.arguments.last().map(String::as_str),
        Some("set -o pipefail\nexport PATH='/opt/it'\\''s/bin:/usr/bin'\necho hi")
    );
    let sh = selection_for(&req, ShellFlavor::Posix, None);
    assert_eq!(
        sh.arguments.last().map(String::as_str),
        Some("export PATH='/opt/it'\\''s/bin:/usr/bin'\necho hi")
    );
}

#[test]
fn runtime_path_never_changes_a_cmd_payload() {
    let mut req = request(false);
    req.runtime_path = Some("C:\\bin".into());
    let cmd = selection_for(&req, ShellFlavor::Cmd, None);
    assert_eq!(cmd.raw_arguments.as_deref(), Some("\"false | true\""));
}
