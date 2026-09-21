# Phase 1 task list — temporary

Delete this file once an authenticated caller can create a sandbox, run a command and a long-running process in a real VM, transfer a file, read output, destroy the sandbox, and see the allocation's release confirmed in the database. See [the deletion protocol](README.md#deletion-protocol).

Scope is fixed by [roadmap](../roadmap.md#scope-discipline-for-phase-1). Nothing here adds to it. Snapshots, the UI, sessions, audit surfaces, admin routes and multiple hosts are later phases; resisting them is most of the discipline.

## Repository groundwork

- [ ] Cargo workspace with the crates from the [proposed layout](../architecture.md#proposed-code-layout). Only the ones Phase 1 needs.
- [ ] Pin the Rust toolchain. `rustfmt` and `clippy` configured, clippy warnings denied in CI.
- [ ] CI jobs: fmt, clippy, unit tests, migration test, alongside the existing docs check.
- [ ] Local stack: PostgreSQL 16 and MinIO via compose, plus a seeded admin credential.
- [ ] Self-hosted runner for VM tests, once the host exists. Fork pull requests never run on it.

## Storage

- [ ] First migration for `projects`, `sandboxes`, `operations`, `allocations`, `hosts` — the five records Phase 1 touches. Snapshots wait for Phase 4.
- [ ] The constraints from [data models](../data-models.md#database-rules): `UNIQUE (project_id, idempotency_key)`, `UNIQUE (sandbox_id, generation)`, one unreleased allocation per sandbox, composite keys keeping sandbox-local links in the same sandbox.
- [ ] Typed UUIDv7 IDs with prefixes attached at the API boundary.
- [ ] Fresh-install and upgrade migration tests.

## API

- [ ] OpenAPI document for the Phase 1 routes only. Generate types from it.
- [ ] Project bearer token authentication, hashed storage, constant-time comparison.
- [ ] Transactional admission with idempotency keys and request digests, per [API contract](../api-contract.md#retries-and-admission).
- [ ] `problem+json` errors with the machine-readable code list.
- [ ] Create, execute, get sandbox, get operation, outputs, file PUT, destroy.
- [ ] SSE output stream with sequence cursors and resume.

## Controller

- [ ] Claim operations with bounded leases and monotonic claim revisions.
- [ ] Capacity check and allocation reservation against one host.
- [ ] gRPC client over mTLS to the supervisor.
- [ ] Reconciliation on restart: read receipts before continuing, never infer from desired state.

## Supervisor

- [ ] gRPC server over mTLS, private interface only, verifies the controller's certificate identity.
- [ ] Per-host certificate issuance and a small internal CA.
- [ ] Jailer, per-VM cgroups and namespaces, Firecracker boot from our kernel plus an allowlisted rootfs.
- [ ] Resource limits enforced by the host, not requested politely.
- [ ] Network namespace per sandbox, nftables rules from [networking](../networking.md), host-side resolver that refuses unapproved names.
- [ ] Supervisor epoch on registration; reject stale controllers.
- [ ] Lease watchdog that stops VMs when the host is partitioned.

## Guest

- [ ] Guest image: Debian slim, our init, our guest agent, `system` and `workload` cgroups, agent in its own PID namespace.
- [ ] Guest kernel build: modules off, lockdown on, pinned and digest-published.
- [ ] vsock protocol with length-prefixed protobuf, shared `.proto` files with the supervisor.
- [ ] Spawn, process-tree tracking, bounded output capture, exit reporting.
- [ ] File write into the workspace with path and size validation.

## The gate

- [ ] A caller with a project token runs a command in a real microVM and reads its output.
- [ ] A long-running process outlives the request that started it.
- [ ] Destroy confirms allocation release in the database.
- [ ] Limits and egress rules hold against the adversarial attempts from [spike question 1](phase-0-spikes.md#1-do-the-limits-actually-hold--gates-phase-1).
- [ ] Every acceptance check this phase covers is linked from its owning document.
- [ ] Delete this file.
