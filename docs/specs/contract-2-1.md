# Fork extension (carried inside contract 2.3): supervised Docker one-shot execution

This document describes a fork-only extension to the Docker one-shot members.
It is not upstream's contract 2.1, which adds the resource-aware native jail
members (`NATIVE_JAIL_RESOURCES_VERSION`, see the module README). The merged
module advertises contract 2.3, and these guarantees ride inside it as additive,
optional result fields. No contract version number identifies them: an
unmodified upstream module at 2.1, 2.2 or 2.3 passes the same version check
without providing any of the guarantees below. The file name is kept for link
stability only.

`RunDocker` and `RunDockerWithoutWorkspace` keep their request shapes. Their
one-shot containers always drop all capabilities, prohibit privilege escalation,
and cap the process count at 256. Workspace-backed runs use the workspace owner's
non-root UID and GID when available; root-owned, missing, and unmounted workspaces
use UID/GID 65534. Neither path inherits UID 0.

Every invocation has an independent UUID container name. The module supervises
execution through the deadline even if a caller drops its wait. On timeout it
asks Docker to kill that exact container, then retries a failed kill with
`docker rm -f` for the same name. Each control command has a five-second deadline.
Cleanup never sweeps a shared label, so a failed job cannot stop another agent's
healthy job. The existing label-based cleanup operation remains available for
orphan cleanup.

The module continuously drains stdout and stderr while retaining at most one
MiB per stream, plus UTF-8 boundary handling and a truncation notice. This bounds
host capture memory as well as the returned result.

`DockerRunOutput` adds two optional fields:

- `killed`: absent for ordinary completion, true when timed-out workload cleanup
  was confirmed, false when both targeted cleanup attempts failed.
- `duration_ms`: observed elapsed time including bounded cleanup, never the
  requested time limit.

`timed_out` means a deadline expired; it alone cannot prove that anything stopped.
Hosts must not gate on the contract version to obtain these guarantees, because
the version cannot detect the extension. Instead, a host must treat absent or
null `killed` on a timed-out result as unknown cleanup, never as a confirmed
stop, and must treat `killed == false` as a failed stop. Hosts must retain
failed or unknown cleanup evidence in their error messages. Older wire results,
and results from modules without this extension, deserialize with absent
evidence and therefore read as unknown.

Host policy still decides which configurable network and root-filesystem settings
are allowed. OpenHuman's sandbox policy permits only network `none` and a read-only
root filesystem; its host adapter enforces that narrower policy before calling
TinyBox. Configurable runtime adapters may intentionally use a different policy.
