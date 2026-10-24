use super::*;

fn mounts(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

#[test]
fn ordinary_run_has_no_issues() {
    assert_eq!(
        run_policy_issues(
            Some("none"),
            Some("/home/u/project"),
            &mounts(&["/usr/lib"])
        )
        .len(),
        0
    );
    assert_eq!(run_policy_issues(None, None, &[]).len(), 0);
}

#[test]
fn host_network_is_reported() {
    let issues = run_policy_issues(Some("host"), Some("/work"), &[]);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].contains("host network"));
}

#[test]
fn dangerous_read_only_mounts_are_reported() {
    let issues = run_policy_issues(
        None,
        Some("/work"),
        &mounts(&["/", "/etc", "/var/run/docker.sock", "/proc", "/sys", "/usr"]),
    );
    assert_eq!(issues.len(), 5);
}

#[test]
fn trailing_separator_does_not_hide_a_dangerous_path() {
    assert_eq!(
        run_policy_issues(None, Some("/etc/"), &mounts(&["/sys/"])).len(),
        2
    );
}

#[test]
fn dangerous_workspace_is_reported() {
    let issues = run_policy_issues(None, Some("/"), &[]);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].contains("Workspace"));
}

#[test]
fn skipped_workspace_is_not_checked() {
    assert_eq!(run_policy_issues(None, None, &[]).len(), 0);
}

#[test]
fn subpaths_of_dangerous_roots_are_allowed() {
    assert_eq!(
        run_policy_issues(None, Some("/etc/nginx"), &mounts(&["/proc/self"])).len(),
        0
    );
}
