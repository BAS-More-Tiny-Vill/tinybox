//! Reserved native shell execution; authorization remains caller-owned.
use crate::{NativeJailConfig, ResourceId, SelectShellRequest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Start a local shell under module-owned supervision and output replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellExecRequest {
    /// Caller-known process reservation.
    pub process: ResourceId,
    /// Existing local passthrough resource.
    pub resource: ResourceId,
    /// Shell payload and pipeline-status convention.
    pub shell: SelectShellRequest,
    /// Working directory inside the resource; absent means its root.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Approved overrides on the module's safe parent environment snapshot.
    #[serde(default)]
    pub extra_env: BTreeMap<String, String>,
    /// Approved existing absolute private state directory for owned scratch.
    pub state_dir: String,
    /// Positive runtime budget, default thirty seconds, at most twenty-four hours.
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Binary input; absent closes stdin.
    #[serde(default)]
    pub stdin: Option<Vec<u8>>,
    /// Explicit native confinement; absent is ordinary local execution.
    #[serde(default)]
    pub jail: Option<NativeJailConfig>,
    /// Windows only: start the child without a console window. Ignored on
    /// other platforms.
    #[serde(default)]
    pub hide_window: bool,
}
const fn default_timeout() -> u64 {
    30_000
}
