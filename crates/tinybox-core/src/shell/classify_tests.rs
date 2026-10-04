//! Tests for the command classifier, executor list, env guard and hidden
//! execution check.

use super::classify::{CommandClass, classify_segment, has_hidden_execution};
use super::env_guard::has_dangerous_env_prefix;
use super::executor::is_command_executor;

fn class(base: &str, args: &[&str]) -> CommandClass {
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    let joined = format!("{base} {}", args.join(" "));
    classify_segment(base, &args, &joined)
}

#[test]
fn class_ordering_is_low_to_high() {
    assert!(CommandClass::Read < CommandClass::Write);
    assert!(CommandClass::Write < CommandClass::Network);
    assert!(CommandClass::Network < CommandClass::Install);
    assert!(CommandClass::Install < CommandClass::Destructive);
}

#[test]
fn read_only_bases_are_read() {
    for base in ["ls", "cat", "grep", "pwd", "get-childitem", "dir"] {
        assert_eq!(class(base, &[]), CommandClass::Read, "{base}");
    }
}

#[test]
fn unknown_base_fails_closed_to_write() {
    assert_eq!(class("frobnicate", &["--now"]), CommandClass::Write);
    assert_eq!(class("tee", &["out"]), CommandClass::Write);
}

#[test]
fn network_and_destructive_bases() {
    assert_eq!(class("curl", &["x"]), CommandClass::Network);
    assert_eq!(class("ssh", &["host"]), CommandClass::Network);
    assert_eq!(class("sudo", &["ls"]), CommandClass::Destructive);
    assert_eq!(class("diskpart", &[]), CommandClass::Destructive);
}

#[test]
fn catastrophic_patterns_win_over_the_base() {
    assert_eq!(class("rm", &["-rf", "/"]), CommandClass::Destructive);
    assert_eq!(class("rm", &["-fr", "/"]), CommandClass::Destructive);
    assert_eq!(
        classify_segment("bash", &[], ":(){:|:&};:"),
        CommandClass::Destructive
    );
}

#[test]
fn vcs_and_package_tools_are_verb_sensitive() {
    assert_eq!(class("git", &["status"]), CommandClass::Read);
    assert_eq!(class("git", &["push"]), CommandClass::Write);
    assert_eq!(class("git", &[]), CommandClass::Write);
    assert_eq!(class("npm", &["ls"]), CommandClass::Read);
    assert_eq!(class("pnpm", &["run", "build"]), CommandClass::Write);
    assert_eq!(class("cargo", &["metadata"]), CommandClass::Read);
    assert_eq!(class("cargo", &["build"]), CommandClass::Write);
}

#[test]
fn installs_are_their_own_bucket() {
    assert_eq!(class("apt-get", &["install", "x"]), CommandClass::Install);
    assert_eq!(class("cargo", &["install", "x"]), CommandClass::Install);
    assert_eq!(class("npm", &["i", "-g", "x"]), CommandClass::Install);
    assert_eq!(class("npm", &["install", "x"]), CommandClass::Write);
    assert_eq!(
        class("yarn", &["global", "add", "x"]),
        CommandClass::Install
    );
    assert_eq!(class("pacman", &["-syu"]), CommandClass::Install);
    assert_eq!(class("pacman", &["-ss", "x"]), CommandClass::Write);
    assert_eq!(class("brew", &["list"]), CommandClass::Write);
}

#[test]
fn find_is_read_unless_it_executes_or_writes() {
    assert_eq!(class("find", &[".", "-name", "x"]), CommandClass::Read);
    for flag in [
        "-exec",
        "-delete",
        "-fprint",
        "-fprintf",
        "'-fprint'",
        "\"-fls\"",
    ] {
        assert_eq!(class("find", &[".", flag]), CommandClass::Write, "{flag}");
    }
}

#[test]
fn executors_classify_as_write() {
    for base in ["python3", "bash", "node", "powershell", "xargs"] {
        assert_eq!(class(base, &["-c", "x"]), CommandClass::Write, "{base}");
    }
}

#[test]
fn command_executor_list() {
    for base in [
        "python",
        "python3",
        "pythonw",
        "pythonw3",
        "/usr/bin/perl",
        "BASH",
        "env",
        "node.exe",
        "mshta",
        "xargs",
    ] {
        assert!(is_command_executor(base), "{base}");
    }
    for base in ["ls", "pythonista", "git", "pythonx"] {
        assert!(!is_command_executor(base), "{base}");
    }
}

#[test]
fn dangerous_env_prefixes() {
    assert!(has_dangerous_env_prefix("GIT_PAGER=evil git log"));
    assert!(has_dangerous_env_prefix("FOO=1 ld_preload=x ls"));
    assert!(has_dangerous_env_prefix("  PATH=/tmp ls"));
    assert!(!has_dangerous_env_prefix("FOO=1 ls"));
    assert!(!has_dangerous_env_prefix("ls PATH=x"));
    assert!(!has_dangerous_env_prefix("=x ls"));
    assert!(!has_dangerous_env_prefix(""));
}

#[test]
fn hidden_execution_structures() {
    assert!(has_hidden_execution("echo $(rm -rf ~)"));
    assert!(has_hidden_execution("echo `id`"));
    assert!(has_hidden_execution("diff <(a) <(b)"));
    assert!(has_hidden_execution("tee >(cat)"));
    assert!(has_hidden_execution("sleep 9 &"));
    assert!(!has_hidden_execution("ls 2>&1"));
    assert!(!has_hidden_execution("echo a > out.txt"));
    assert!(!has_hidden_execution("echo 'a & b'"));
    assert!(!has_hidden_execution(
        "cat <<'EOF'\nChicken & Spinach `x` $(y)\nEOF"
    ));
    assert!(has_hidden_execution("cat <<EOF\n$(y)\nEOF"));
}
