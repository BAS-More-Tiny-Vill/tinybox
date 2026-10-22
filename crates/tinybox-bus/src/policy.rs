//! Standalone Docker run policy vocabulary; validating grants no execution access.

use serde::{Deserialize, Serialize};

/// The parts of a Docker run that decide whether it weakens isolation.
///
/// These mirror the same-named `DockerRunRequest` fields so a host can ask the
/// module for the verdict `RunDocker` would reach without starting anything.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DockerPolicyRequest {
    /// Docker network mode; `None` is the isolated default.
    #[serde(default)]
    pub network: Option<String>,
    /// Host workspace path, or `None` for a run that mounts none.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Additional read-only host paths.
    #[serde(default)]
    pub read_only_mounts: Vec<String>,
}

/// Every way the checked run would weaken container isolation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DockerPolicyReport {
    /// Human-readable findings; empty means the run may proceed.
    pub issues: Vec<String>,
}
