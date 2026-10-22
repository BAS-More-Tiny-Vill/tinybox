# Native shell execution implementation order

[Specification](../specs/native-shell-execution.md).

1. Reproduce the absent module consumer through the unconditional real-artifact
   test in crates/tinybox-module/tests/grants_manifest_e2e.rs.
2. Add ShellExecRequest in crates/tinybox-bus/src/shell_execution.rs, append
   StartNativeShellExec and contract 1.14, and add TimedOut output failure.
3. Extend the existing collectors in tinybox-host/src/limited/{mod,jailed}.rs
   with observed standard-command bridges; retain owned kill/reap behavior.
4. Implement validation, environment and scratch ownership in the module's
   native_shell directory. Reuse native_jail preparation for strict confinement.
5. Integrate shell collectors and exact request identity in resources_stream.rs.
   Reuse existing reservations, output journals and resource cleanup.
6. Add behavioral tests beside each module and actual artifact execution.
   Validate formatting, affected/full tests, Clippy, build, coverage and
   cross-platform CI through the workflow runner.

Implementation is complete; upstream review and cross-platform CI remain. No host gitlink or availability claim is
part of this upstream slice.
