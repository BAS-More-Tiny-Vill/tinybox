//! Serialized requests and results for the `TinyBox` module.
//!
//! This crate has no bus connection, executor, native backend, or policy.
//! A caller applies its own authorization before sending these requests.
//!
//! ```
//! use tinybox_bus::{ProcessRef, ResourceId};
//! let process = ProcessRef {
//!     resource: ResourceId("caller-reservation-1".into()),
//!     process: ResourceId("caller-process-1".into()),
//! };
//! assert_eq!(process.resource.0, "caller-reservation-1");
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod grants;
pub use grants::{LocalGrantConfig, LocalGrants, ProtectedPaths, ResolveLocalGrantsRequest};

mod jail;
pub use jail::{JailEnforcement, NativeJailConfig, NativeJailRequest, NativeJailResourceRequest};

mod shell_execution;
mod shell_selection;
pub use shell_execution::{RunNativeShellRequest, RunNativeShellResult, ShellExecRequest};
pub use shell_selection::{SelectShellRequest, ShellFlavor, ShellInvocation};

mod stdio;
#[cfg(feature = "storage")]
pub mod storage;
pub use stdio::{
    MAX_STDIO_BUFFER_BYTES, MAX_STDIO_PROCESSES, MAX_STDIO_READ_BYTES, MAX_STDIO_WAIT_MS,
    MAX_STDIO_WRITE_BYTES, STDIO_CLOSED, StdioChunk, StdioExit, StdioReadRequest,
    StdioStartRequest, StdioStatus, StdioWriteRequest,
};

mod policy;
pub use policy::{DockerPolicyReport, DockerPolicyRequest};

mod event;
pub use event::{DenialKind, EgressAllowlist, LimitKind, ProcessEvent, SandboxEvent};

mod files;
pub use files::{
    AbortFileWriteRequest, BeginFileReadRequest, BeginFileWriteRequest, FileChunk, FileReadInfo,
    FileWriteInfo, FileWriteProgress, FinishFileReadRequest, FinishFileWriteRequest,
    ReadFileChunkRequest, WriteFileChunkRequest,
};

/// Wire vocabulary version, independent of the release workflow's package version.
///
/// 1.0 denotes the original discovery-only surface. 1.1 adds resource
/// reservations, owned lifecycle/operations, capabilities and terminal shutdown.
/// 1.2 adds host selection, container networking/ports, and owned forwards.
/// 1.3 adds native jail discovery.
/// 1.4 added legacy shell facts for host execution policy, removed in 2.0. 1.5 adds bounded
/// file transfer for mounted workspaces. 1.6 adds bounded live native output.
/// 1.7 adds explicitly approved native directory-jail execution. 1.8 adds a
/// bounded, module-owned one-shot Docker operation. 1.9 adds bounded Docker
/// daemon status and label-scoped cleanup operations. 1.10 adds a bounded
/// Docker command that does not mount a host workspace. 1.11 adds local grant
/// discovery. 1.12 adds platform shell selection. 1.13 adds supervised native
/// shell execution on local passthrough resources.
/// 1.14 adds native shell execution with owned scratch, timeout and optional jail.
/// 1.15 adds `runtime_path` and `python_utf8` to shell selection.
/// 1.16 adds the [`names`] member constants; `METHODS` is derived from them.
/// 1.17 makes `RunDocker` and `RunDockerWithoutWorkspace` refuse host-network
/// runs and dangerous mounts.
/// 1.18 adds the standalone `ValidateDockerPolicy` member, so a host can ask the
/// module for the same verdict the Docker run members enforce without starting
/// a container, and `hide_window` on [`ShellExecRequest`] (Windows console
/// suppression for a native shell child; ignored elsewhere).
/// 1.19 adds `NetworkPolicy::Allowlist` and the observational
/// [`SandboxEvent`] vocabulary (`Started`, `Denied`, `LimitHit`, `Exited`).
/// 1.20 adds `deny_read` to [`NativeJailConfig`].
/// 1.21 adds the interactive piped-stdio members `StartStdio`, `WriteStdio`,
/// `ReadStdio`, `CloseStdin`, `KillStdio` and `StdioStatus`, which run one
/// program inside a mandatory native jail and stream its stdin, stdout and
/// stderr over the bus. 1.22 adds optional hard resource limits to
/// [`NativeJailConfig`].
/// 2.0 removes TinyBox-owned command policy classification and the
/// `AnalyzeShell.hidden_execution` policy signal. Generic shell structure facts
/// and transport helpers remain.
/// 2.1 adds separate resource-aware native jail request members without
/// changing the 2.0 Rust request shapes. 2.2 adds process-scoped lifecycle
/// events for module-owned executions and timeout observations. 2.3 adds
/// module-owned native-shell orchestration through completion or yield, and
/// the `openshell` sandbox backend for `Create`.
///
/// Fork extension carried inside 2.3: the optional [`DockerRunOutput`]
/// `killed` and `duration_ms` evidence fields from this fork's Docker
/// one-shot extension (docs/specs/contract-2-1.md; not upstream's 2.1). They
/// are additive, default to `None` when a module omits them, and no contract
/// version identifies them. The same holds for the optional
/// [`DockerRunRequest`] `user` and `pids_limit` fields and
/// [`DockerRunOutput`] `observed_limits` (DEF-32), except that those are
/// security-critical, so a module advertises that it enforces them (and the
/// floor, probe and read-back on every run) with the explicit
/// [`HARDENED_DOCKER_RUN_FEATURE`] capability in
/// [`ModuleCapabilities::features`]; see [`supports_hardened_docker_run`].
/// The [`DOCKER_LIMITS_UNENFORCEABLE`] error name rides inside 2.3 the same way.
pub const CONTRACT_VERSION: (u32, u32) = (2, 3);

/// Minimum contract version required for resource-aware native jail members.
pub const NATIVE_JAIL_RESOURCES_VERSION: (u32, u32) = (2, 1);

/// Whether a module explicitly advertises support for the resource-aware
/// native jail members. Hosts must check this before sending a 2.1 request.
#[must_use]
pub fn supports_native_jail_resources(module: (u32, u32)) -> bool {
    module.0 == NATIVE_JAIL_RESOURCES_VERSION.0 && module.1 >= NATIVE_JAIL_RESOURCES_VERSION.1
}

/// Capability a module lists in [`ModuleCapabilities::features`] when, on
/// EVERY `RunDocker` and `RunDockerWithoutWorkspace` call (whether or not the
/// request sets `user` or `pids_limit`), it:
///
/// - enforces the floor: a numeric non-root identity (never uid 0 or gid 0;
///   the workspace owner, else `65534:65534`, when `user` is unset) and a
///   process cap of at most 256;
/// - honours an explicit [`DockerRunRequest`] `user` / `pids_limit` only
///   within that floor (a pids limit can tighten the cap, never loosen it);
/// - probes the daemon (`docker info`) before anything is created and refuses
///   the run with [`DOCKER_LIMITS_UNENFORCEABLE`] when it reports it cannot
///   enforce the process cap or the memory limit;
/// - creates the container, reads its effective limits back with
///   `docker inspect` before start, refuses to start one whose limits are not
///   in force, and reports them in [`DockerRunOutput::observed_limits`].
///
/// Fork extension carried inside contract 2.3; no contract version identifies
/// it. A module without it (for example unmodified upstream 2.3) ignores the
/// unknown request fields and runs with its old behaviour.
pub const HARDENED_DOCKER_RUN_FEATURE: &str = "docker-run-hardened-v1";

/// Whether a module explicitly advertises [`HARDENED_DOCKER_RUN_FEATURE`]
/// (exact name, equal contract major), that is, whether it enforces the
/// non-root / 256-process floor, the enforcement probe and the limit
/// read-back on every Docker run.
///
/// Hosts must check this before sending a [`DockerRunRequest`] that sets
/// `user` or `pids_limit`, and a host whose policy requires the floor must
/// check it before sending any Docker run request; the contract version
/// cannot detect the extension. See [`docker_run_request_is_supported_with_policy`].
#[must_use]
pub fn supports_hardened_docker_run(capabilities: &ModuleCapabilities) -> bool {
    capabilities.contract_version.0 == CONTRACT_VERSION.0
        && capabilities
            .features
            .iter()
            .any(|feature| feature == HARDENED_DOCKER_RUN_FEATURE)
}

/// When a host requires [`HARDENED_DOCKER_RUN_FEATURE`] before sending a
/// [`DockerRunRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HardenedDockerRunPolicy {
    /// Only for requests that set `user` or `pids_limit`; plain requests may
    /// go to any module (an older one runs them with its own defaults). This
    /// is what [`docker_run_request_is_supported`] checks.
    #[default]
    WhenRequested,
    /// For every request: the host relies on the non-root / 256-process floor
    /// and the limit read-back, so it never sends a Docker run to a module
    /// that does not advertise the capability.
    Always,
}

/// Whether `request` may be sent to a module with `capabilities`: always for
/// requests that leave `user` and `pids_limit` unset, otherwise only when the
/// module advertises [`HARDENED_DOCKER_RUN_FEATURE`]. Equivalent to
/// [`docker_run_request_is_supported_with_policy`] with
/// [`HardenedDockerRunPolicy::WhenRequested`]; a host that requires the floor
/// for every run uses [`HardenedDockerRunPolicy::Always`] instead.
#[must_use]
pub fn docker_run_request_is_supported(
    capabilities: &ModuleCapabilities,
    request: &DockerRunRequest,
) -> bool {
    docker_run_request_is_supported_with_policy(
        capabilities,
        request,
        HardenedDockerRunPolicy::WhenRequested,
    )
}

/// Whether `request` may be sent to a module with `capabilities` under the
/// host's `policy`. With [`HardenedDockerRunPolicy::Always`] the capability is
/// required for plain requests too, because a module without it would run
/// them without the enforced floor and without `observed_limits`.
#[must_use]
pub fn docker_run_request_is_supported_with_policy(
    capabilities: &ModuleCapabilities,
    request: &DockerRunRequest,
    policy: HardenedDockerRunPolicy,
) -> bool {
    let required = match policy {
        HardenedDockerRunPolicy::WhenRequested => request.requests_hardened_limits(),
        HardenedDockerRunPolicy::Always => true,
    };
    !required || supports_hardened_docker_run(capabilities)
}

/// Whether this host vocabulary can bind to a module's advertised version.
/// The major must match and the module's minor must include every host member.
#[must_use]
pub fn is_compatible(module: (u32, u32)) -> bool {
    module.0 == CONTRACT_VERSION.0 && module.1 >= CONTRACT_VERSION.1
}

/// Maximum reservation identifier length in bytes.
pub const MAX_ID_BYTES: usize = 128;
/// Maximum additional read or write grants for one native jail.
pub const MAX_NATIVE_JAIL_GRANTS: usize = 64;
/// Maximum live resources in a module instance.
pub const MAX_ACTIVE_RESOURCES: usize = 64;
/// Maximum live processes or queued startups in one resource.
pub const MAX_PROCESSES_PER_RESOURCE: usize = 64;
/// Maximum live forwarding tunnels per resource.
pub const MAX_FORWARDS_PER_RESOURCE: usize = 64;
/// Maximum unused reservations, reclaimed when consumed, closed, or expired.
pub const MAX_RESERVATIONS: usize = 4096;
/// Idle lifetime of an unused reservation, in seconds.
pub const RESERVATION_TTL_SECS: u64 = 60;
/// Maximum combined stdout and stderr bytes collected by the module host.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Maximum bytes returned or accepted by one file-transfer chunk.
pub const MAX_FILE_CHUNK_BYTES: usize = 64 * 1024;
/// Maximum total bytes accepted by one file write.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Maximum read and write handles held by one resource.
pub const MAX_FILE_TRANSFERS_PER_RESOURCE: usize = 8;
/// Maximum UTF-8 path length accepted for a workspace file.
pub const MAX_FILE_PATH_BYTES: usize = 4096;
/// Stable error name for module admission capacity exhaustion.
pub const RESOURCE_LIMIT: &str = "ai.tinyhumans.tinybox.Error.ResourceLimit";

/// Well-known module interface.
pub const INTERFACE: &str = "ai.tinyhumans.tinybox.Box";
/// Module object path.
pub const OBJECT_PATH: &str = "/ai/tinyhumans/tinybox/Box";
/// Named bus members. Hosts call members through these constants, never by
/// position in [`METHODS`] or by string literal.
pub mod names {
    /// Wire name of the `Describe` member.
    pub const DESCRIBE: &str = "Describe";
    /// Wire name of the `Create` member.
    pub const CREATE: &str = "Create";
    /// Wire name of the `Exec` member.
    pub const EXEC: &str = "Exec";
    /// Wire name of the `Inspect` member.
    pub const INSPECT: &str = "Inspect";
    /// Wire name of the `Close` member.
    pub const CLOSE: &str = "Close";
    /// Wire name of the `Spawn` member.
    pub const SPAWN: &str = "Spawn";
    /// Wire name of the `IsRunning` member.
    pub const IS_RUNNING: &str = "IsRunning";
    /// Wire name of the `Cancel` member.
    pub const CANCEL: &str = "Cancel";
    /// Wire name of the `AnalyzeShell` member.
    pub const ANALYZE_SHELL: &str = "AnalyzeShell";
    /// Wire name of the `Reserve` member.
    pub const RESERVE: &str = "Reserve";
    /// Wire name of the `Capabilities` member.
    pub const CAPABILITIES: &str = "Capabilities";
    /// Wire name of the `Shutdown` member.
    pub const SHUTDOWN: &str = "Shutdown";
    /// Wire name of the `Forward` member.
    pub const FORWARD: &str = "Forward";
    /// Wire name of the `CloseForward` member.
    pub const CLOSE_FORWARD: &str = "CloseForward";
    /// Wire name of the `JailStatus` member.
    pub const JAIL_STATUS: &str = "JailStatus";
    /// Wire name of the `BeginFileRead` member.
    pub const BEGIN_FILE_READ: &str = "BeginFileRead";
    /// Wire name of the `ReadFileChunk` member.
    pub const READ_FILE_CHUNK: &str = "ReadFileChunk";
    /// Wire name of the `FinishFileRead` member.
    pub const FINISH_FILE_READ: &str = "FinishFileRead";
    /// Wire name of the `BeginFileWrite` member.
    pub const BEGIN_FILE_WRITE: &str = "BeginFileWrite";
    /// Wire name of the `WriteFileChunk` member.
    pub const WRITE_FILE_CHUNK: &str = "WriteFileChunk";
    /// Wire name of the `FinishFileWrite` member.
    pub const FINISH_FILE_WRITE: &str = "FinishFileWrite";
    /// Wire name of the `AbortFileWrite` member.
    pub const ABORT_FILE_WRITE: &str = "AbortFileWrite";
    /// Wire name of the `StartExec` member.
    pub const START_EXEC: &str = "StartExec";
    /// Wire name of the `ReadOutput` member.
    pub const READ_OUTPUT: &str = "ReadOutput";
    /// Wire name of the `ReleaseOutput` member.
    pub const RELEASE_OUTPUT: &str = "ReleaseOutput";
    /// Wire name of the `StartJailedExec` member.
    pub const START_JAILED_EXEC: &str = "StartJailedExec";
    /// Wire name of the `RunDocker` member.
    pub const RUN_DOCKER: &str = "RunDocker";
    /// Wire name of the `DockerStatus` member.
    pub const DOCKER_STATUS: &str = "DockerStatus";
    /// Wire name of the `CleanupDocker` member.
    pub const CLEANUP_DOCKER: &str = "CleanupDocker";
    /// Wire name of the `RunDockerWithoutWorkspace` member.
    pub const RUN_DOCKER_WITHOUT_WORKSPACE: &str = "RunDockerWithoutWorkspace";
    /// Wire name of the `ResolveLocalGrants` member.
    pub const RESOLVE_LOCAL_GRANTS: &str = "ResolveLocalGrants";
    /// Wire name of the `SelectShell` member.
    pub const SELECT_SHELL: &str = "SelectShell";
    /// Wire name of the `StartShellExec` member.
    pub const START_SHELL_EXEC: &str = "StartShellExec";
    /// Wire name of the `StartNativeShellExec` member.
    pub const START_NATIVE_SHELL_EXEC: &str = "StartNativeShellExec";
    /// Wire name of the module-owned native-shell lifecycle member.
    pub const RUN_NATIVE_SHELL: &str = "RunNativeShell";
    /// Wire name of the `ValidateDockerPolicy` member.
    pub const VALIDATE_DOCKER_POLICY: &str = "ValidateDockerPolicy";
    /// Wire name of the `StartStdio` member.
    pub const START_STDIO: &str = "StartStdio";
    /// Wire name of the `WriteStdio` member.
    pub const WRITE_STDIO: &str = "WriteStdio";
    /// Wire name of the `ReadStdio` member.
    pub const READ_STDIO: &str = "ReadStdio";
    /// Wire name of the `CloseStdin` member.
    pub const CLOSE_STDIN: &str = "CloseStdin";
    /// Wire name of the `KillStdio` member.
    pub const KILL_STDIO: &str = "KillStdio";
    /// Wire name of the `StdioStatus` member.
    pub const STDIO_STATUS: &str = "StdioStatus";
    /// Wire name of the 2.1 resource-aware jailed exec member.
    pub const START_JAILED_EXEC_WITH_RESOURCES: &str = "StartJailedExecWithResources";
    /// Wire name of the process-scoped sandbox lifecycle signal.
    pub const PROCESS_EVENT: &str = "ProcessEvent";
}

/// Supported signals in this contract.
pub const SIGNALS: &[&str] = &[names::PROCESS_EVENT];

/// Supported methods in this contract, including the original discovery
/// operation. Derived from [`names`]; the order is stable within this major.
pub const METHODS: &[&str] = &[
    names::DESCRIBE,
    names::CREATE,
    names::EXEC,
    names::INSPECT,
    names::CLOSE,
    names::SPAWN,
    names::IS_RUNNING,
    names::CANCEL,
    names::ANALYZE_SHELL,
    names::RESERVE,
    names::CAPABILITIES,
    names::SHUTDOWN,
    names::FORWARD,
    names::CLOSE_FORWARD,
    names::JAIL_STATUS,
    names::BEGIN_FILE_READ,
    names::READ_FILE_CHUNK,
    names::FINISH_FILE_READ,
    names::BEGIN_FILE_WRITE,
    names::WRITE_FILE_CHUNK,
    names::FINISH_FILE_WRITE,
    names::ABORT_FILE_WRITE,
    names::START_EXEC,
    names::READ_OUTPUT,
    names::RELEASE_OUTPUT,
    names::START_JAILED_EXEC,
    names::RUN_DOCKER,
    names::DOCKER_STATUS,
    names::CLEANUP_DOCKER,
    names::RUN_DOCKER_WITHOUT_WORKSPACE,
    names::RESOLVE_LOCAL_GRANTS,
    names::SELECT_SHELL,
    names::START_SHELL_EXEC,
    names::START_NATIVE_SHELL_EXEC,
    names::VALIDATE_DOCKER_POLICY,
    names::START_STDIO,
    names::WRITE_STDIO,
    names::READ_STDIO,
    names::CLOSE_STDIN,
    names::KILL_STDIO,
    names::STDIO_STATUS,
    names::START_JAILED_EXEC_WITH_RESOURCES,
    names::RUN_NATIVE_SHELL,
];

/// Facts about the detected native directory-jail backend.
/// Hosts apply their own execution policy to these facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JailStatus {
    /// Stable backend identifier.
    pub backend: String,
    /// Whether this build can apply the detected backend.
    pub available: bool,
    /// Declared process isolation: none, process, kernel, or hardware.
    pub isolation: String,
    /// Whether process isolation and filesystem enforcement meet the security floor.
    pub suitable_for_untrusted_code: bool,
    /// Filesystem constraint: unsupported, best effort, or enforced.
    pub filesystem: String,
    /// Network constraint: unsupported, best effort, or enforced.
    pub network: String,
    /// Subprocess constraint: unsupported, best effort, or enforced.
    pub subprocess: String,
}

/// Stable error name for an unknown or closed module resource.
pub const UNKNOWN_RESOURCE: &str = "ai.tinyhumans.tinybox.Error.UnknownResource";
/// Stable error name for a process not issued for the selected resource.
pub const UNKNOWN_PROCESS: &str = "ai.tinyhumans.tinybox.Error.UnknownProcess";
/// Stable error name for a backend the module cannot construct.
pub const UNSUPPORTED_BACKEND: &str = "ai.tinyhumans.tinybox.Error.UnsupportedBackend";
/// Stable error name for an operation without acknowledged native ownership.
pub const UNSUPPORTED_OPERATION: &str = "ai.tinyhumans.tinybox.Error.UnsupportedOperation";
/// Stable error name for an identifier already reserved by this module instance.
pub const DUPLICATE_ID: &str = "ai.tinyhumans.tinybox.Error.DuplicateId";
/// Stable error name for an empty reservation identifier.
pub const INVALID_ID: &str = "ai.tinyhumans.tinybox.Error.InvalidId";
/// Stable error name when combined output exceeds the collection budget.
pub const OUTPUT_LIMIT: &str = "ai.tinyhumans.tinybox.Error.OutputLimit";
/// Stable error name when Close cancels collected execution.
pub const EXEC_CANCELLED: &str = "ai.tinyhumans.tinybox.Error.ExecCancelled";
/// Stable error name for a native backend operation failure.
pub const BACKEND_ERROR: &str = "ai.tinyhumans.tinybox.Error.Backend";
/// Stable error name for an invalid workspace-relative file path.
pub const INVALID_FILE_PATH: &str = "ai.tinyhumans.tinybox.Error.InvalidFilePath";
/// Stable error name for a missing or mismatched file-transfer handle.
pub const UNKNOWN_FILE_TRANSFER: &str = "ai.tinyhumans.tinybox.Error.UnknownFileTransfer";
/// Stable error name for a file-transfer size or handle limit.
pub const FILE_LIMIT: &str = "ai.tinyhumans.tinybox.Error.FileLimit";
/// Stable error name for an invalid file-transfer offset or chunk.
pub const INVALID_FILE_CHUNK: &str = "ai.tinyhumans.tinybox.Error.InvalidFileChunk";
/// Stable error name when safe workspace file access is unavailable.
pub const FILE_UNSUPPORTED: &str = "ai.tinyhumans.tinybox.Error.FileUnsupported";
/// Stable error name for a workspace file operation failure.
pub const FILE_ERROR: &str = "ai.tinyhumans.tinybox.Error.FileOperation";
/// Stable error name for an invalid Docker run request.
pub const INVALID_DOCKER_RUN: &str = "ai.tinyhumans.tinybox.Error.InvalidDockerRun";
/// Stable error name for invalid Docker lifecycle requests.
pub const INVALID_DOCKER_CONTROL: &str = "ai.tinyhumans.tinybox.Error.InvalidDockerControl";
/// Stable error name for failed Docker lifecycle operations.
pub const DOCKER_CONTROL_FAILED: &str = "ai.tinyhumans.tinybox.Error.DockerControlFailed";
/// Stable error name when a Docker run is refused because the daemon reports
/// (through the `docker info` enforcement probe) that it cannot enforce the
/// process cap or memory limit. Nothing was created or started. Distinct from
/// [`INVALID_DOCKER_RUN`], which also covers probes, creates or read-backs that
/// failed or exceeded the deadline. Fork extension carried inside contract
/// 2.3; see [`HARDENED_DOCKER_RUN_FEATURE`].
pub const DOCKER_LIMITS_UNENFORCEABLE: &str =
    "ai.tinyhumans.tinybox.Error.DockerLimitsUnenforceable";

/// Opaque module-minted reservation identifier bound by one module instance.
/// IDs use ASCII letters, digits, hyphens, or underscores, up to [`MAX_ID_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourceId(pub String);

/// Allocate a single-use startup reservation without starting native work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReserveRequest {
    /// Reserve a resource for Create.
    Resource,
    /// Reserve a process for Spawn, bound to the selected resource.
    Process(ResourceId),
    /// Reserve a forward associated with a resource.
    Forward(ResourceId),
    /// Reserve a workspace file reader associated with a resource.
    FileRead(ResourceId),
    /// Reserve a staged workspace file writer associated with a resource.
    FileWrite(ResourceId),
}

/// Backend operations with acknowledged native ownership in this module build.
///
/// `features` is a public-field addition in this fork: downstream code that
/// builds this struct with a literal must name the new field or end the
/// literal with `..Default::default()`. [`Default`] is the empty description:
/// contract version `(0, 0)` (compatible with nothing), no backends and no
/// features, so a defaulted field never claims a capability.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModuleCapabilities {
    /// Wire vocabulary served by the artifact, independent of package version.
    pub contract_version: (u32, u32),
    /// Backends accepted by Create; no fallback is performed.
    pub create_backends: Vec<String>,
    /// Backends accepted by bounded collected Exec.
    pub exec_backends: Vec<String>,
    /// Backends accepted by supervised Spawn/Cancel.
    pub spawn_backends: Vec<String>,
    /// Named fork capabilities this module build enforces, such as
    /// [`HARDENED_DOCKER_RUN_FEATURE`]. Older modules omit it, which reads as
    /// empty; it is omitted from the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

/// A workspace supplied to a backend; unsupported sources are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Workspace {
    /// Existing directory on the selected host.
    Directory(String),
    /// OCI image reference.
    Image(String),
}

/// Which host executes the selected sandbox and its commands.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HostConfig {
    /// Run on the module's machine.
    #[default]
    Local,
    /// Reach a machine using the host's OpenSSH client and configuration.
    Ssh(SshHostConfig),
}

/// Explicit OpenSSH target and host-key policy for a remote host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHostConfig {
    /// Host name or `user@host`, passed as one OpenSSH destination argument.
    pub destination: String,
    /// Optional SSH port override.
    #[serde(default)]
    pub port: Option<u16>,
    /// Optional private-key path on the module host.
    #[serde(default)]
    pub identity: Option<String>,
    /// Optional known-hosts path on the module host.
    #[serde(default)]
    pub known_hosts: Option<String>,
    /// Whether to trust an unknown host key on first connection.
    #[serde(default)]
    pub accept_new_host_key: bool,
}

/// Network access policy applied by a sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NetworkPolicy {
    /// Disable all network access.
    #[default]
    Denied,
    /// Permit outbound connections without publishing inbound ports.
    Egress,
    /// Permit outbound connections only to the named hosts and ports. Added in
    /// contract 1.19; a backend that cannot enforce it refuses the request.
    Allowlist(EgressAllowlist),
    /// Permit unrestricted network access.
    Open,
}

/// CPU, memory, process, and disk limits requested for a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// CPU allowance in thousandths of a core.
    pub cpu_millis: u32,
    /// Maximum resident memory in bytes.
    pub memory_bytes: u64,
    /// Maximum number of processes and threads.
    pub pids_max: u32,
    /// Maximum writable filesystem size in bytes.
    pub disk_bytes: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpu_millis: 2_000,
            memory_bytes: 2 * 1024 * 1024 * 1024,
            pids_max: 512,
            disk_bytes: 8 * 1024 * 1024 * 1024,
        }
    }
}

/// A guest port published by the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMapping {
    /// Port inside the sandbox.
    pub guest: u16,
    /// Requested host port, or `None` to let the sandbox choose one.
    #[serde(default)]
    pub host: Option<u16>,
}

/// A guest port and the effective host port assigned by the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedPort {
    /// Port inside the sandbox.
    pub guest: u16,
    /// Effective port on the selected host.
    pub host: u16,
}

/// Allocate a local-host sandbox; the backend is always explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRequest {
    /// Module-minted Resource reservation, known before native startup.
    pub resource: ResourceId,
    /// Requested backend, such as `docker`, `namespace`, or `passthrough`.
    pub backend: String,
    /// Host on which the backend operates; defaults to the module machine.
    #[serde(default)]
    pub host: HostConfig,
    /// Workspace to materialize.
    pub workspace: Workspace,
    /// Network policy enforced by the sandbox.
    #[serde(default)]
    pub network: NetworkPolicy,
    /// Resource limits enforced by the sandbox.
    #[serde(default)]
    pub resources: ResourceLimits,
    /// Guest ports published on the selected host.
    #[serde(default)]
    pub ports: Vec<PortMapping>,
    /// Variables inherited by commands.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Run one Docker-isolated shell command and collect bounded output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DockerRunRequest {
    /// Host workspace mounted read-write at `/workspace` for `RunDocker`; must
    /// be empty for `RunDockerWithoutWorkspace`.
    pub workspace: String,
    /// Container image; `None` uses the existing one-shot default.
    #[serde(default)]
    pub image: Option<String>,
    /// Shell command executed with `sh -c`, or `sh -lc` when `login_shell` is true.
    pub command: String,
    /// Start the POSIX shell as a login shell (`sh -lc`) instead of `sh -c`.
    #[serde(default)]
    pub login_shell: bool,
    /// Docker network mode; `None` uses the existing isolated default.
    #[serde(default)]
    pub network: Option<String>,
    /// Memory limit in MiB, or `None` for the existing one-shot default.
    /// `0` (which Docker reads as unlimited) is refused with `InvalidDockerRun`.
    #[serde(default)]
    pub memory_limit_mb: Option<u64>,
    /// CPU quota in cores, or `None` for the existing one-shot default.
    #[serde(default)]
    pub cpus: Option<f64>,
    /// Read-only container root filesystem; `None` preserves the existing
    /// read-only default.
    #[serde(default)]
    pub read_only_rootfs: Option<bool>,
    /// Additional read-only host paths mounted at the same path in the box.
    #[serde(default)]
    pub read_only_mounts: Vec<String>,
    /// Additional Linux capabilities to drop.
    #[serde(default)]
    pub extra_cap_drops: Vec<String>,
    /// Environment variables passed to the command. Their values are
    /// delivered to the container only; they never appear on a Docker
    /// command line.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional Docker label for the owned container.
    #[serde(default)]
    pub label: Option<String>,
    /// Maximum command runtime in milliseconds.
    #[serde(default = "default_docker_run_timeout_ms")]
    pub timeout_ms: u64,
    /// Numeric non-root `uid:gid` the command runs as (`--user`); uid 0,
    /// gid 0 and names are refused. `None` keeps the module's non-root default
    /// (the workspace owner, else `65534:65534`). A module advertising
    /// [`HARDENED_DOCKER_RUN_FEATURE`] probes enforcement and reads the
    /// effective limits back before EVERY run, whether or not this is set,
    /// refuses to start one whose limits are not in force, and reports them in
    /// [`DockerRunOutput::observed_limits`]. Only send it to such a module: an
    /// older module ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Maximum number of processes in the container (`--pids-limit`); must be
    /// between 1 and 256, so it can tighten but never loosen the module's
    /// mandatory 256-process cap. `None` keeps 256. See `user` for the
    /// probe and read-back every run gets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids_limit: Option<u64>,
}

impl DockerRunRequest {
    /// Whether this request sets `user` or `pids_limit`, which only a module
    /// advertising [`HARDENED_DOCKER_RUN_FEATURE`] enforces. Check
    /// [`docker_run_request_is_supported`] (or
    /// [`docker_run_request_is_supported_with_policy`] when the host requires
    /// the floor for every run) before sending it.
    #[must_use]
    pub fn requests_hardened_limits(&self) -> bool {
        self.user.is_some() || self.pids_limit.is_some()
    }
}

/// Container limits the module read back with `docker inspect` before the
/// command started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerObservedLimits {
    /// `HostConfig.Memory`, in bytes.
    pub memory_bytes: u64,
    /// `HostConfig.PidsLimit`; `None` when unset or unlimited.
    pub pids_limit: Option<i64>,
    /// `Config.User`; empty when the image default applies.
    pub user: String,
}

/// Bounded result from one Docker command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerRunOutput {
    /// Process exit code; `-1` denotes a signal or timeout.
    pub exit_code: i32,
    /// Bounded, lossily decoded stdout.
    pub stdout: String,
    /// Bounded, lossily decoded stderr.
    pub stderr: String,
    /// Whether the command exceeded its deadline; this alone does not prove a stop.
    pub timed_out: bool,
    /// The effective limits read back before the command started (the
    /// request's values, or the module defaults for the ones it left unset).
    /// A module advertising [`HARDENED_DOCKER_RUN_FEATURE`] reports them for
    /// every run, with or without `user` / `pids_limit`. `None` means the
    /// module did not verify them (an older module), so a host must treat the
    /// limits as unverified. Fork extension carried inside contract 2.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_limits: Option<DockerObservedLimits>,
    /// Whether Docker confirmed targeted timeout cleanup. Older modules omit it.
    /// Fork extension carried inside contract 2.3; optional and additive.
    #[serde(default)]
    pub killed: Option<bool>,
    /// Observed elapsed milliseconds, including bounded cleanup; never a requested limit.
    /// Fork extension carried inside contract 2.3; optional and additive.
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// Check whether the module's Docker daemon responds within a bounded interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerStatusRequest {
    /// Maximum probe duration in milliseconds.
    pub timeout_ms: u64,
}

/// Result of a bounded Docker daemon probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerStatus {
    /// Whether Docker responded successfully before the deadline.
    pub available: bool,
}

/// Stop module-owned containers matching a caller-selected Docker label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupDockerRequest {
    /// Docker label filter in `key=value` form.
    pub label: String,
    /// Maximum total list-and-stop duration in milliseconds.
    pub timeout_ms: u64,
}

/// Result of a completed labeled-container cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupDockerOutput {
    /// Number of matching container identifiers successfully submitted to Docker kill.
    pub containers_stopped: u32,
}

/// Default Docker lifecycle operation timeout, in milliseconds.
pub const DEFAULT_DOCKER_CONTROL_TIMEOUT_MS: u64 = 10_000;
/// Maximum Docker lifecycle operation timeout, in milliseconds.
pub const MAX_DOCKER_CONTROL_TIMEOUT_MS: u64 = 60_000;

/// Maximum accepted one-shot Docker runtime, in milliseconds.
pub const MAX_DOCKER_RUN_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;
/// Default Docker command runtime, in milliseconds.
pub const DEFAULT_DOCKER_RUN_TIMEOUT_MS: u64 = 30_000;

fn default_docker_run_timeout_ms() -> u64 {
    DEFAULT_DOCKER_RUN_TIMEOUT_MS
}

impl Default for CreateRequest {
    fn default() -> Self {
        Self {
            resource: ResourceId(String::new()),
            backend: String::new(),
            host: HostConfig::default(),
            workspace: Workspace::Directory(String::new()),
            network: NetworkPolicy::default(),
            resources: ResourceLimits::default(),
            ports: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

/// A command inside an existing resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Opaque box identifier.
    pub resource: ResourceId,
    /// Unshelled program and arguments.
    pub argv: Vec<String>,
    /// Optional working directory on the host or inside the sandbox.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Environment overrides.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Bytes for standard input.
    #[serde(default)]
    pub stdin: Option<Vec<u8>>,
}

/// Collected command output; a nonzero exit code is a successful transport reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutput {
    /// Process exit status.
    pub exit_code: i32,
    /// Unmodified standard output.
    pub stdout: Vec<u8>,
    /// Unmodified standard error.
    pub stderr: Vec<u8>,
}

/// Current resource state, without native handles or backend implementation types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceInfo {
    /// Opaque identifier reserved by this module instance.
    pub resource: ResourceId,
    /// Explicit backend selected at creation.
    pub backend: String,
    /// Backend-reported lifecycle state.
    pub state: String,
    /// Effective host ports assigned to requested guest ports.
    #[serde(default)]
    pub published_ports: Vec<PublishedPort>,
}

/// Open a gateway to a published guest port using a caller-known reservation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardRequest {
    /// Resource whose host published the guest port.
    pub resource: ResourceId,
    /// Module-minted forward reservation.
    pub forward: ResourceId,
    /// Published guest port to reach.
    pub guest_port: u16,
}

/// An open gateway endpoint; the module owns its tunnel until `CloseForward`,
/// resource Close, or Shutdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardInfo {
    /// Resource whose host published the guest port.
    pub resource: ResourceId,
    /// Caller-known opaque forwarding handle.
    pub forward: ResourceId,
    /// Local address accepting connections through this gateway.
    pub local_address: String,
}

/// Close an opaque forward previously opened for a resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseForwardRequest {
    /// Resource owning the forward.
    pub resource: ResourceId,
    /// Opaque forward handle returned by Reserve.
    pub forward: ResourceId,
}

/// Start a detached process under a caller-known reservation identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// Module-minted Process(resource) reservation, known before native startup.
    pub process: ResourceId,
    /// Command inside the selected resource.
    pub command: ExecRequest,
}

/// A module-owned detached process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRef {
    /// Resource in which the process runs.
    pub resource: ResourceId,
    /// Opaque caller-known process reservation identifier.
    pub process: ResourceId,
}

/// Generic structural shell facts. These do not classify a command, grant
/// permission, or choose an access tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAnalysis {
    /// Live shell segments after quoted heredoc bodies are removed.
    pub segments: Vec<String>,
    /// Whether an unquoted redirect occurs outside heredoc data.
    pub redirection: bool,
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;

mod output;
pub use output::{ExecutionFailure, ExecutionState, OutputBatch, OutputChunk, OutputStream};

/// Invalid native shell execution inputs.
pub const INVALID_SHELL_EXEC: &str = "ai.tinyhumans.tinybox.Error.InvalidShellExec";
/// Invalid interactive stdio program, argument, or environment.
pub const INVALID_STDIO: &str = "ai.tinyhumans.tinybox.Error.InvalidStdio";
