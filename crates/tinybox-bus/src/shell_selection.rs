//! Platform shell invocation vocabulary; selecting a shell grants no execution access.

use serde::{Deserialize, Serialize};

/// Command text and the desired pipeline exit-status convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectShellRequest {
    /// Text interpreted by the platform shell.
    pub command: String,
    /// Preserve the shell's ordinary last-stage pipeline status rather than
    /// enabling Bash pipefail. Windows cmd has no pipefail mode.
    #[serde(default)]
    pub preserve_pipeline_status: bool,
    /// Managed `PATH` restored after POSIX login profiles run, as a leading
    /// `export PATH=...` in the payload. Ignored by Windows cmd.
    #[serde(default)]
    pub runtime_path: Option<String>,
    /// Native shell execution only: default `PYTHONUTF8=1` and
    /// `PYTHONIOENCODING=utf-8` unless the parent environment sets them.
    #[serde(default)]
    pub python_utf8: bool,
}

/// Shell syntax used by the module machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellFlavor {
    /// Windows cmd syntax, including percent-variable expansion.
    Cmd,
    /// POSIX shell syntax, using Bash when available.
    Posix,
}

/// Selected invocation, returned for discovery and consumed inside `TinyBox`.
/// Hosts execute commands through the module rather than spawning this plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellInvocation {
    /// Syntax understood by the selected shell.
    pub flavor: ShellFlavor,
    /// Shell executable on the module machine.
    pub program: String,
    /// Ordinary process arguments, excluding the executable.
    pub arguments: Vec<String>,
    /// Windows command payload appended verbatim after ordinary arguments.
    /// Rust's ordinary argument escaping is incompatible with cmd quotes.
    pub raw_arguments: Option<String>,
    /// Parent environment names needed to bootstrap Windows child tools.
    /// Values are never returned by shell discovery.
    pub bootstrap_environment: Vec<String>,
    /// Remove these startup-hook variables after applying the approved environment.
    #[serde(default)]
    pub remove_environment: Vec<String>,
}
