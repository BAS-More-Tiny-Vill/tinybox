# Contract 1.18: Docker policy validation and native shell window control

Status: implemented; cross-platform CI pending. Owner: TinyBox.
Sequence: [contract 1.18 plan](../plans/contract-1-18.md).

## ValidateDockerPolicy

Contract 1.18 appends `ValidateDockerPolicy(DockerPolicyRequest) ->
DockerPolicyReport`. The request carries the three inputs of the 1.17 run check:
an optional network mode, an optional workspace path and the read-only mount
paths. The reply lists every finding as a human-readable string; an empty list
means the run may proceed. The member is the standalone form of the check
`RunDocker` and `RunDockerWithoutWorkspace` already apply: it calls
`tinybox_docker::run_policy_issues`, so a host and the run members cannot
disagree. It needs no Docker daemon, starts nothing and grants no access.

A host with its own policy object maps it onto the request and applies its own
decision to the findings. The module does not decide what the host does with a
finding; it only reports it.

## ShellExecRequest.hide_window

`StartNativeShellExec` gains `hide_window: bool` (serde default false). On
Windows a true value starts the shell child with `CREATE_NO_WINDOW`, so a GUI
host does not flash a console. It has no effect on other platforms, and the
default keeps every earlier request unchanged.

## Compatibility

Both changes are additive. The method catalogue appends `ValidateDockerPolicy`
(index 35); `CONTRACT_VERSION` becomes (1, 18); a host requires a module minor
at least 18 to use either.

Acceptance: wire round-trips and defaults, catalogue order, the module answering
the member with the same findings as the run check for safe and unsafe inputs,
and a Windows execution with `hide_window` set.
