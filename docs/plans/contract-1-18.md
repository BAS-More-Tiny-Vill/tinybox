# Contract 1.18 implementation order

[Specification](../specs/contract-1-18.md).

1. Add `DockerPolicyRequest` and `DockerPolicyReport` in
   crates/tinybox-bus/src/policy.rs and `hide_window` in shell_execution.rs;
   append `names::VALIDATE_DOCKER_POLICY` to `METHODS`; bump the version to 1.18
   and extend the bus wire tests.
2. Implement `validate_docker_policy` on `BoxService` over
   `tinybox_docker::run_policy_issues` and add it to the module export list.
3. Apply `CREATE_NO_WINDOW` in native_shell on Windows through one helper, with
   tests beside it.
4. Update the module README member table and version history.
