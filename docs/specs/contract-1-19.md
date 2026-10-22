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

- Names are resolved outside the box, once, and pinned. The box gets those
  addresses (or a proxy that connects only to them) and no resolver of its own,
  so a later DNS answer cannot widen the list.
- Cloud metadata endpoints (`169.254.169.254`, `169.254.170.2`,
  `100.100.100.200`, `fd00:ec2::254`, `metadata.google.internal`) are refused
  when the spec is validated, and a backend must also block them when a name
  resolves to one.
- A backend that cannot enforce the list refuses the box with
  `ConstraintNotEnforced { constraint: Network, enforcement: Unsupported }`.
  It never widens the list to `Egress`. A backend that enforces only part of it
  (for example a Seatbelt profile without host matching, or a proxy fallback)
  must report the network constraint `BestEffort`, never `Enforced`.
- `NetworkPolicy::Open` shares the host network and is the highest-risk setting
  (`NetworkPolicy::is_highest_risk`).

## Status per backend

No shipped backend enforces `Allowlist` yet: `passthrough`, `docker`,
`namespace` and `microvm` all refuse it. Enforcement (netns + nftables or an
egress proxy, Seatbelt or proxy, AppContainer or WFP) is tracked under
tinybox#27 phase 3.

## Deprecated

`tinybox_core::shell::{classify, scan, env_guard}` is command policy and now
lives in `tinysecurity-policy`. It is deprecated in 0.1.34 and removed in the
release after. `shell::analysis` (structure facts for the `AnalyzeShell` and
`AnalyzeCommand` members) remains.
