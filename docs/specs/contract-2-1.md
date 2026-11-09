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
orphan cleanup. It lists matching containers with `docker ps -a`, so it also
finds containers that were created for the verified path below but never
started (for example after a crash between `create` and `start`), not only
running ones.

Environment values (`DockerRunRequest.env`) are delivered only to the container,
through a private env file passed with `--env-file`. They never appear on any
Docker command line (which any local user can read from the process table) and
are not exported into the Docker client's own environment.

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

## Caller-supplied user and process limit (`docker-run-hardened-v1`)

`DockerRunRequest` also accepts two optional, security-critical fields:

- `user`: a numeric non-root `uid:gid` for `--user`. Names, uid 0 and gid 0
  are refused with `InvalidDockerRun`. Unset keeps the non-root default above.
- `pids_limit`: a `--pids-limit` between 1 and 256. It can tighten the
  256-process cap, never loosen it. Unset keeps 256.

`memory_limit_mb: 0` is also refused with `InvalidDockerRun`, because Docker
reads `-m 0` as "no memory limit".

### Every run is verified

Every `RunDocker` and `RunDockerWithoutWorkspace` call takes the same verified
path, whether or not the request sets `user` or `pids_limit`:

1. **Enforcement probe.** Before anything is created, the module runs
   `docker info --format '{{json .}}'`. A stored `HostConfig` is not proof
   that a kernel controller enforces it, so the run is refused unless
   `PidsLimit` is true, `MemoryLimit` is true (a memory limit always applies),
   and, for a rootless daemon (`SecurityOptions` contains `name=rootless`),
   `CgroupVersion` is `2` with `CgroupDriver` `systemd`. That refusal returns
   the distinct error `ai.tinyhumans.tinybox.Error.DockerLimitsUnenforceable`
   (`tinybox_bus::DOCKER_LIMITS_UNENFORCEABLE`) with a message naming the
   missing control. Nothing was created or started. A probe that fails, cannot
   be read, or overruns the deadline is not a refusal: it fails closed with
   `InvalidDockerRun` ("Docker limits could not be verified", or "... before
   the run deadline" for the deadline case).
2. **Create and read back.** The module `docker create`s the container (same
   flags, unique `tinybox-<uuid>` name), reads the effective memory, process
   limit and user back with `docker inspect`, and removes it without starting
   it if they differ from what was requested (or from the floor defaults for
   what was left unset). That failure is `InvalidDockerRun`.
3. **Start.** Only then does it `docker start -a` the container under the same
   supervision, bounded draining and targeted timeout cleanup as before. When a
   start fails or the deadline passes, the created container is removed by id
   with `docker rm -f`.

The values read back are reported in `DockerRunOutput.observed_limits` for
every run. A run that exceeds its deadline after starting is still an ordinary
result with `timed_out: true` and `killed` evidence, never one of the errors
above.

A module that predates this extension ignores the unknown request fields and
runs the workload with its old behaviour; the host would only learn afterwards,
from a missing `observed_limits`. The contract version cannot tell the two
apart, and this fork does not change upstream's 2.x numbering. Instead the
module advertises an explicit capability:

- `ModuleCapabilities.features` is a list of named fork capabilities. It is
  optional on the wire: older modules omit it and it reads as empty, and a
  module omits it when empty, so the existing `Capabilities` snapshot is
  unchanged.
- The name `docker-run-hardened-v1` (`tinybox_bus::HARDENED_DOCKER_RUN_FEATURE`)
  is listed only by a module whose `RunDocker` and `RunDockerWithoutWorkspace`
  members, on every run: enforce the non-root identity (never uid 0 or gid 0)
  and the 256-process floor; honour an explicit `user` / `pids_limit` only
  within that floor; and run the enforcement probe and the limit read-back
  before start, as described above. This fork's module lists it on every
  platform.
- `ModuleCapabilities.features` is a public-field addition to a public struct.
  Downstream code that builds `ModuleCapabilities` with a struct literal must
  name the new field or end the literal with `..Default::default()`;
  `ModuleCapabilities` implements `Default` (contract version `(0, 0)`, no
  backends, no features) for that purpose.

A host must call `Capabilities` and check
`tinybox_bus::supports_hardened_docker_run(&capabilities)` (exact name, equal
contract major) before it sends a request that sets `user` or `pids_limit`.
`tinybox_bus::docker_run_request_is_supported(&capabilities, &request)` combines
that check with `DockerRunRequest::requests_hardened_limits()`, and is always
true for requests that leave both fields unset. A host whose policy requires
the floor for every run (because an older module would run even a plain
request without it, and without `observed_limits`) uses
`tinybox_bus::docker_run_request_is_supported_with_policy(&capabilities,
&request, HardenedDockerRunPolicy::Always)`, which requires the capability for
every request; `HardenedDockerRunPolicy::WhenRequested` is the
`docker_run_request_is_supported` behaviour. When the check is false the host
must not send the request: it must refuse, or drop the fields only if its own
policy accepts the module defaults. `DockerRunRequest` does not use
`deny_unknown_fields`, because that would break older hosts and modules, so the
capability check is the guard. A host that receives no `observed_limits` must
treat the limits as unverified. A host that receives
`DockerLimitsUnenforceable` must not retry the same run on that daemon
expecting a different result: the daemon itself cannot enforce the limits.

Host policy still decides which configurable network and root-filesystem settings
are allowed. OpenHuman's sandbox policy permits only network `none` and a read-only
root filesystem; its host adapter enforces that narrower policy before calling
TinyBox. Configurable runtime adapters may intentionally use a different policy.
