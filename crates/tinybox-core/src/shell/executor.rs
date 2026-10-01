//! Detecting bases that execute arbitrary code (interpreters,
//! shells, `xargs`, Windows `LOLBins`, …). Basename normalisation itself is
//! `tinybox_core::shell::scan`.

use super::scan::normalized_command_name;

fn is_python_command(command: &str) -> bool {
    let command = normalized_command_name(command);
    command == "python"
        || command == "pythonw"
        || command
            .strip_prefix("pythonw")
            .and_then(|suffix| suffix.chars().next())
            .is_some_and(|ch| ch.is_ascii_digit())
        || command
            .strip_prefix("python")
            .and_then(|suffix| suffix.chars().next())
            .is_some_and(|ch| ch.is_ascii_digit())
}

/// True when `command` (any path / `.exe` form) is a base that runs arbitrary
/// code: interpreters, shells, `xargs`, `env`, JS runtimes and Windows launchers.
#[must_use]
pub fn is_command_executor(command: &str) -> bool {
    let command = normalized_command_name(command);
    is_python_command(command.as_str())
        || matches!(
            command.as_str(),
            "xargs"
                | "awk"
                | "gawk"
                | "mawk"
                | "nawk"
                | "perl"
                | "ruby"
                | "bash"
                | "sh"
                | "dash"
                | "zsh"
                | "ksh"
                | "fish"
                | "env"
                // JS/TS runtimes (the `node_exec`/`npm_exec` shell equivalents)
                | "node"
                | "nodejs"
                | "deno"
                | "bun"
                // Windows / PowerShell arbitrary-code launchers + LOLBins
                | "iex"
                | "invoke-expression"
                | "cmd"
                | "pwsh"
                | "powershell"
                | "wscript"
                | "cscript"
                | "mshta"
                | "rundll32"
                | "start-process"
        )
}
