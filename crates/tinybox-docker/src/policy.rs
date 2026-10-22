//! Safety policy for one-shot Docker runs.
//!
//! The checks are pure: they inspect the request's network mode and mount
//! paths and never touch the daemon or the filesystem, so a host can rely on
//! the module refusing a dangerous run before any container exists.

use std::path::Path;

/// Host paths that must never be bind-mounted into a one-shot container.
pub const DANGEROUS_MOUNTS: &[&str] = &["/", "/etc", "/var/run/docker.sock", "/proc", "/sys"];

/// The network mode that shares the host's network stack.
pub const HOST_NETWORK: &str = "host";

/// Describe every way a one-shot run would weaken container isolation.
///
/// `workspace` is `None` for runs that mount no workspace. An empty result
/// means the run may proceed.
#[must_use]
pub fn run_policy_issues(
    network: Option<&str>,
    workspace: Option<&str>,
    read_only_mounts: &[String],
) -> Vec<String> {
    let mut issues = Vec::new();
    if network == Some(HOST_NETWORK) {
        issues.push("Docker run uses the host network, which defeats network isolation".into());
    }
    for mount in read_only_mounts {
        if is_dangerous(mount) {
            issues.push(format!("Dangerous read-only mount: {mount}"));
        }
    }
    if let Some(workspace) = workspace
        && is_dangerous(workspace)
    {
        issues.push(format!("Workspace is a dangerous path: {workspace}"));
    }
    issues
}

fn is_dangerous(path: &str) -> bool {
    // Compare components so a trailing separator cannot disguise `/etc/`.
    let normalized: Vec<_> = Path::new(path).components().collect();
    DANGEROUS_MOUNTS.iter().any(|dangerous| {
        Path::new(dangerous)
            .components()
            .eq(normalized.iter().copied())
    })
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
