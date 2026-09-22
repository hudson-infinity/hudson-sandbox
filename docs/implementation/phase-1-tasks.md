# Phase 1 task list — temporary

Delete this file once both gates below have passed. See [the deletion protocol](README.md#deletion-protocol). Completed implementation has moved to the owning contracts linked below; the remaining checkboxes concern missing implementation or the complete release acceptance, not a claim that no component tests exist.

Scope is fixed by [roadmap](../roadmap.md#scope-discipline-for-phase-1). Snapshots, the UI, sessions, audit surfaces, admin routes and multiple hosts remain later phases. Execution and recovery are built together, with two gates passed in order.

## Implemented groundwork

The [development guide](dev-env.md) covers the pinned workspace, local PostgreSQL/MinIO stack, hosted checks and native control-plane workflow. [Data models](../data-models.md) own the implemented SQLx migrations, constraints and typed IDs. The [API contract](../api-contract.md), [HTTPS/setup guide](../api-server.md), [OpenAPI contract](../openapi.md), [CLI](../client-cli.md) and [language clients](../language-clients.md) cover authentication, offline provisioning, admission, collections, commands, cancellation, files, output and resumable SSE.

The [controller](../controller.md), [real supervisor](../real-supervisor.md), [guardian](../allocation-guardian.md), [bootstrap](../guest-bootstrap.md), [guest protocol](../guest-protocol.md) and [runner](../guest-runner.md) own implemented dispatch, ownership, watchdog, execution and recovery behavior. [History reclamation](../history-reclamation.md) owns opt-in live/destruction retirement and quota recovery. The [development evidence](../evidence/2026-09-22-released-history-accounting.json) includes real command execution, confirmed database release, host restart and retained outcomes. This is nested-aarch64 evidence, not passage of the supported x86_64 gates.

## Groundwork

- [ ] Provide supported x86_64 KVM capacity and a reviewed privileged CI workflow. Never run untrusted fork code on that host.
- [ ] Record complete supported-host evidence and link it from the owning contracts; keep development fixtures distinct from production images and release artifacts.

# Gate 1a — it works

## Remaining implementation and integration

- [ ] Add authenticated host registration, externally issued epochs and verified image/host compatibility to the operator-provisioned path. Preserve the existing stale-epoch and ownership fences.
- [ ] Supply production Debian images with our init/agent and a reproducible pinned guest kernel build: modules disabled and lockdown enabled. Existing BusyBox development fixtures do not satisfy this image contract.
- [ ] Implement the per-sandbox network namespace, host-side resolver, nftables CIDR/port policy and bandwidth controls from [networking](../networking.md). A development VM with no NIC is not this policy engine.
- [ ] Complete supported-installation private RPC binding, per-host certificate issuance/rotation and internal CA operations. Pinned mTLS authentication already exists; certificate operations and installation policy remain work.
- [ ] Complete whole-allocation host journal/guardian metadata lifecycle and permanent object-store marker authority fencing. Do not delete replay fences based on time, local cancellation or a release flag. Domain history retirement alone does not finish [issue #79](https://github.com/hudson-infinity/hudson-sandbox/issues/79).

## Passing 1a

- [ ] On a fresh supported host, reproduce authenticated create, command execution, a process that outlives its initiating request, file upload/download, live/retained output, cancellation, destroy and confirmed database release. These paths have development evidence; fresh supported-host acceptance remains open.
- [ ] Exercise host-enforced CPU, memory, PID and writable-disk limits under hostile load, and all egress checks from [spike question 1](phase-0-spikes.md#1-do-the-limits-actually-hold--gates-phase-1).
- [ ] Demonstrate the [two-sandbox product scenario](../goal.md#capabilities-and-their-validation), including one sandbox exhausting limits while the other remains usable within the documented bounds.
- [ ] Collect the gate evidence for maintainer review before any release/tag under [the contribution process](../../CONTRIBUTING.md#releases). A passing component suite is not this gate.

# Gate 1b — it does not lie

The mechanisms below have unit, database, simulated-host and controlled real-host tests. The remaining task is the complete applicable [recovery table](../lifecycle.md#destroy-and-recovery) and adversarial matrix on the supported configuration, with evidence for each boundary.

## Failure injection

- [ ] Create reserved, readiness unknown: kill the controller between dispatch and confirmation.
- [ ] Execute dispatched, result missing: kill the supervisor mid-command and preserve an honest unknown outcome.
- [ ] Destroy stopped the VM, cleanup incomplete: kill during teardown and reconcile actual resource release.
- [ ] Lose acknowledgements at every applicable create/execute/cancel/destroy boundary; verify the original work is reconciled without replay.
- [ ] Partition the host: prove the independent watchdog stops workloads and the controller cannot start a replacement until release/fencing is confirmed.
- [ ] Cover simultaneous host/process/storage failures beyond the individual development regressions; do not infer combined-fault behavior from isolated tests.

## Ownership and fencing

- [ ] Reject stale controller claims, supervisor epochs and allocation generations throughout the supported-host matrix.
- [ ] Refuse replacement allocation until the previous incarnation is confirmed stopped or fenced.
- [ ] Verify concurrent identical idempotency keys retain exactly one operation and changed payloads conflict across failure/recovery boundaries.
- [ ] Preserve known outcomes and record unresolved execution as `unknown`; never blindly redispatch to recover a missing response.
- [ ] Resolve `unknown` only from valid original receipts/host observations and retain how it resolved.
- [ ] Validate guest-agent loss and the resulting sandbox/operation reports under the [lifecycle contract](../lifecycle.md), including when guest root interferes with agent state.

## Isolation under attack

- [ ] Pass the complete [networking acceptance set](../networking.md#acceptance-checks).
- [ ] Pass the applicable [threat-model validation](../threat-model.md#required-validation): guest privilege and hardening, filesystem traversal, egress, cross-project access, credential boundaries and hostile root against the agent. Browser/session checks belong to the later UI gate.

## Cleanup convergence

- [ ] Confirm no leaked allocations, disk reservations, network namespaces or VMs after each injected failure.
- [ ] Verify capacity accounting returns only after the corresponding release/consumer-retirement evidence, including repeated crashes and lost acknowledgements.
- [ ] Demonstrate sustained reuse without exhausting retained host allocation metadata or silently removing storage replay fences.

## Passing 1b

- [ ] Every applicable recovery-table row reconciles correctly under injected failure.
- [ ] Every acceptance check this phase covers is linked from its owning document with its actual test configuration and limitations.
- [ ] Delete this file after moving durable findings to the owning contracts.
