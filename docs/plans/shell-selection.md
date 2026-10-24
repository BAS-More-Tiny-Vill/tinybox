# Shell selection implementation sequence

[Specification](../specs/shell-selection.md).
This records the discovery and native execution slices and their verification order.

1. Add SelectShellRequest, ShellFlavor and ShellInvocation in
   crates/tinybox-bus/src/shell_selection.rs. Re-export from src/lib.rs,
   append SelectShell to METHODS, advance the additive contract to 1.12,
   and cover defaults, serialization and method ordering in lib_tests.rs.
2. Implement platform selection and the native quote-preserving builder in
   crates/tinybox-host/src/local/shell.rs; centralize exports in src/lib.rs.
   Portable regressions in local/shell_tests.rs pin non-login startup,
   Bash pipefail, preservation, sh fallback and cmd /D /E:ON /V:OFF /S /C raw wrapping.
   The startup regressions failed before the review corrections.
3. Add module dispatch and manifest declaration in
   crates/tinybox-module/src/tinybus_module/mod.rs. Extend
   tests/grants_manifest_e2e.rs to build the actual artifact with --locked
   --offline, enforce production admission and invoke SelectShell.
4. Execute pipeline fixtures on Unix and quoted executable, quoted argument
   and redirection fixtures on Windows. Keep fallible tests returning Result;
   portable selection fixtures also cover unavailable-Bash fallback.
5. Update the module README's method table, contract version and startup
   semantics. Run formatting, affected tests, full Clippy/build and the
   hosted cross-platform matrix before claiming complete validation.

Local verification:

- cargo fmt --all -- --check
- cargo test --locked -p tinybox-bus -p tinybox-host -p tinybox-module --all-features
- cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
- cargo build --locked --workspace --all-targets --all-features

6. Contract 1.13 appends StartShellExec(SpawnRequest, SelectShellRequest).
   Reuse the live supervisor with a native-shell collector, keep shell text in
   the idempotency identity, resolve resource cwd/environment, and reject
   non-local or non-passthrough resources. Empty argv distinguishes shell
   execution from ordinary StartExec. Cover identity/admission in stream tests
   and execute through the compiled artifact on Unix and Windows.

Discovery and supervised native execution are implemented. Hosted validation
is reported on the pull request; Windows behavior requires its native matrix.

Windows explicitly enables extensions and disables delayed expansion.
Unix builders remove BASH_ENV, ENV, SHELLOPTS and BASHOPTS; ShellInvocation
returns those removal names. Bash uses -p to ignore imported environment
functions. Each discovery probes the standard paths again so shell installation
or removal is reflected immediately. Providers apply them after approved environment overrides.
