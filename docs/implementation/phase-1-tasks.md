# Phase 1 task list — temporary

Delete this file once both gates below have passed. See [the deletion protocol](README.md#deletion-protocol).

Scope is fixed by [roadmap](../roadmap.md#scope-discipline-for-phase-1). Nothing here adds to it. Snapshots, the UI, sessions, audit surfaces, admin routes and multiple hosts are later phases; resisting them is most of the discipline.

Execution and recovery are one phase because the ownership mechanics are cheap to build in and expensive to retrofit. They are still **two gates, passed in order** — 1a is a real stopping point, and skipping it produces a system that has never been demonstrated working before it is asked to survive failure.

## Groundwork

- [x] Cargo workspace with the crates from the [proposed layout](../architecture.md#proposed-code-layout). Only the ones this phase needs.
- [x] Pin the Rust toolchain. `rustfmt` and `clippy` configured, clippy warnings denied in CI.
- [x] CI jobs: fmt, clippy, tests including the schema tests against a PostgreSQL service, alongside the existing docs check.
- [x] Local stack: PostgreSQL 16 and MinIO via compose. Seeded admin credential follows the auth work.
- [ ] A fake supervisor implementing the real gRPC interface, so the control plane is testable before any hardware exists.
- [ ] Self-hosted runner for VM tests, once a host exists. Fork pull requests never run on it.

---

# Gate 1a — it works

## Storage

- [x] First migration for `projects`, `sandboxes`, `operations`, `allocations`, `hosts` — the five records this phase touches. Snapshots wait for Phase 3.
- [x] The constraints from [data models](../data-models.md#database-rules): `UNIQUE (project_id, idempotency_key)`, `UNIQUE (sandbox_id, generation)`, one unreleased allocation per sandbox, composite keys keeping sandbox-local links in the same sandbox.
- [x] Typed UUIDv7 IDs with prefixes attached at the API boundary.
- [x] Fresh-install migration test. Upgrade tests arrive with the second migration, which is the first one that can break an existing database.

## API

- [ ] OpenAPI document for this phase's routes only. Generate types from it.
- [x] Project bearer token authentication, hashed storage, constant-time comparison.
- [x] Transactional admission with idempotency keys and request digests, per [API contract](../api-contract.md#retries-and-admission).
- [x] `problem+json` errors with the machine-readable code list. Codes are added as routes need them.
- [ ] Create, get sandbox, get operation (done); execute, outputs, file PUT, destroy.
- [ ] SSE output stream with sequence cursors and resume.

## Controller

- [ ] Integrate the implemented claim storage into a controller scheduling loop; claim semantics and evidence now live in [lifecycle](../lifecycle.md#operations-and-controller-ownership).
- [ ] Capacity check and allocation reservation against one host.
- [ ] gRPC client over mTLS to the supervisor.
- [ ] Persist intent before every external action and confirmed evidence after it. This is gate 1a work even though gate 1b is what proves it.

## Supervisor

- [ ] gRPC server over mTLS, private interface only, verifies the controller's certificate identity rather than merely a valid certificate.
- [ ] Per-host certificate issuance and a small internal CA.
- [ ] Jailer, per-VM cgroups and namespaces, Firecracker boot from our kernel plus an allowlisted rootfs.
- [ ] Resource limits enforced by the host, not requested politely.
- [ ] Network namespace per sandbox, nftables rules from [networking](../networking.md), host-side resolver that refuses unapproved names.
- [ ] Supervisor epoch issued on registration; stale controllers rejected.
- [ ] Lease watchdog that stops VMs when the host is partitioned.

## Guest

- [ ] Guest image: Debian slim, our init, our guest agent, `system` and `workload` cgroups, agent in its own PID namespace.
- [ ] Guest kernel build: modules off, lockdown on, pinned and digest-published.
- [ ] vsock protocol with length-prefixed protobuf, shared `.proto` files with the supervisor.
- [ ] Spawn, process-tree tracking, bounded output capture, exit reporting.
- [ ] File write into the workspace with path and size validation.

## Passing 1a

- [ ] A caller with a project token runs a command in a real microVM and reads its output.
- [ ] A long-running process outlives the request that started it.
- [ ] A file transfers in and is readable from inside the sandbox.
- [ ] Destroy confirms allocation release in the database.
- [ ] Limits and egress rules hold against the attempts from [spike question 1](phase-0-spikes.md#1-do-the-limits-actually-hold--gates-phase-1).
- [ ] Tag it. This is the first thing worth showing anyone.

---

# Gate 1b — it does not lie

Everything here is about what the system reports when something breaks. Nothing new is introduced; 1b proves the mechanics 1a already built.

## Failure injection

Work the [lifecycle recovery table](../lifecycle.md#destroy-and-recovery) row by row. For each, interrupt at the boundary and confirm the recorded outcome matches what actually happened.

- [ ] Create reserved, readiness unknown — kill the controller between dispatch and confirmation.
- [ ] Execute dispatched, result missing — kill the supervisor mid-command.
- [ ] Destroy stopped the VM, cleanup incomplete — kill during teardown.
- [ ] Lost acknowledgement on every one of the above: the response dies, the work did not.
- [ ] Host partitioned: the lease watchdog stops VMs, and the controller does not start a replacement before that is confirmed.

## Ownership and fencing

- [ ] A stale controller claim cannot mutate current state.
- [ ] A stale supervisor epoch is rejected after a supervisor restart.
- [ ] A stale allocation generation cannot release or command a current VM.
- [ ] A replacement allocation is refused until the previous incarnation is confirmed stopped or fenced.

## Honest outcomes

- [ ] An unprovable execution result is recorded as `unknown`, never as success or failure.
- [ ] Reconciliation resolves `unknown` from receipts and host observation, and records how it resolved.
- [ ] A command is never blindly re-dispatched to recover a lost response.
- [ ] Concurrent identical idempotency keys admit exactly one operation; changed payloads under the same key conflict.
- [ ] A guest agent that disappears mid-execution fails the sandbox and reports it, per [lifecycle](../lifecycle.md#resume).

## Isolation under attack

- [ ] The [networking acceptance checks](../networking.md#acceptance-checks) pass in full.
- [ ] The applicable rows of the [threat model's required validation](../threat-model.md#required-validation) pass for the surfaces that exist — guest privilege, filesystem traversal, egress, cross-project access, hostile root against the guest agent.

## Cleanup convergence

- [ ] No leaked allocations, disk reservations, network namespaces, or VMs after any injected failure.
- [ ] Capacity accounting returns to its true value once cleanup completes.

## Passing 1b

- [ ] Every applicable recovery-table row reconciles correctly under injected failure.
- [ ] Every acceptance check this phase covers is linked from its owning document.
- [ ] Delete this file.
