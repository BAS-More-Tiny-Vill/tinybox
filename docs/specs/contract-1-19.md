# Contract 1.19: network allowlists and sandbox events

Part of tinyhumansai/openhuman#7328 (tinybox#27).

Contract 1.19 is a vocabulary bump. It appends no bus member, so `METHODS` is
unchanged. A host that sends the new vocabulary requires a module minor of at
least 19, which `tinybox_bus::is_compatible` already enforces.

## Added

- `NetworkPolicy::Allowlist(EgressAllowlist { hosts, ports })`. The enum is no
  longer `Copy`.
- `SandboxEvent { Started, Denied { kind, target }, LimitHit { limit },
  Exited { status } }`, with `DenialKind` and `LimitKind`. Events are purely
  observational; the sandbox never decides what is allowed.

## Allowlist semantics

- Names are resolved before filter installation, once, and the resulting
  addresses are pinned into the firewall rules. A later DNS answer cannot widen
  the set of destinations the box can reach.
- Cloud metadata endpoints (`169.254.169.254`, `169.254.170.2`,
  `100.100.100.200`, `fd00:ec2::254`, `metadata.google.internal`) are refused
  when the spec is validated, and a backend must also block them when a name
  resolves to one. DNS names are matched case-insensitively with an optional
  terminal root dot, so `Metadata.Google.Internal.` is refused as well.
- Legacy numeric IPv4 spellings that represent a listed metadata address are
  refused too, including `0251.0376.0251.0376` and `0xa9fea9fe` for
  `169.254.169.254`.
- Explicit loopback entries are refused during validation: IPv4/IPv6 loopback
  literals (including IPv4-mapped IPv6 and scoped forms such as `::1%1`),
  `localhost` and `.localhost` names, and legacy numeric IPv4 spellings
  accepted by common resolvers, such as `127.1`, `2130706433`, `0x7f000001`,
  and `0177.0.0.1`. This check applies to entries as written; an enforcing
  backend must also block resolved loopback addresses. It does not by itself
  provide DNS-rebinding protection.
- A backend that cannot enforce the list refuses the box with
  `ConstraintNotEnforced { constraint: Network, enforcement: Unsupported }`.
  It never widens the list to `Egress`. A backend that enforces only part of it
  (for example a Seatbelt profile without host matching, or a proxy fallback)
  must report the network constraint `BestEffort`, never `Enforced`.
- `NetworkPolicy::Open` shares the host network and is the highest-risk setting
  (`NetworkPolicy::is_highest_risk`).

## Status per backend

`docker` can enforce `Allowlist` only when explicitly configured with
`NativeLinuxDockerEgressController` and a local, rootful Linux Docker Engine
whose firewall chains can be verified. It resolves names before creating the
per-box bridge, installs default-drop IPv4 and IPv6 filters before `docker run`,
and permits only pinned destination IP/port pairs. It writes the pinned
hostname/address pairs into the container's `/etc/hosts` and configures its
only DNS upstream as container-local `127.0.0.1`, so Docker's embedded resolver
cannot forward arbitrary queries to host DNS. Unlisted names therefore fail to
resolve unless the workload provides a resolver in its own loopback namespace.
Docker documents `--dns` as a per-container resolver setting, and its [embedded
DNS security advisory](https://github.com/moby/moby/security/advisories/GHSA-mq39-4gv4-mvpx)
explains that a custom upstream forces external lookups to originate from the
container network namespace.
New traffic from that bridge to the Docker host is dropped, except IPv6
Neighbor Solicitation for bridge-gateway address resolution. Existing
`ESTABLISHED` or `RELATED` traffic is allowed so host-initiated connections,
including published-port requests, can receive replies. The ordinary Docker
sandbox continues to refuse allowlists unless this controller is installed.

Docker Desktop, Windows, remote or unknown Hosts, rootless daemons, and daemons
without the required firewall support refuse `Allowlist` as Unsupported.
`passthrough`, `namespace` and `microvm` continue to refuse it. The workload
cannot fall back to host DNS.

## Deprecated

`tinybox_core::shell::{classify, scan, env_guard}` is command policy and now
lives in `tinysecurity-policy`. It is deprecated in 0.1.34 and removed in the
release after. `shell::analysis` (structure facts for the `AnalyzeShell` and
`AnalyzeCommand` members) remains.
