# Roadmap and delivery gates

Status: all milestones planned. The runtime is design only. Contribution tooling and documentation checks exist; there are no deployed services, working runtime setup commands, migrations, or runtime validation results.

## First usable milestone

**Create → execute and transfer files → enforce isolation and limits → destroy**, on one compatible Linux/KVM host with PostgreSQL and object storage. Include failure recovery, confirmed cleanup, and a reproducible CLI installation. Run it without Hudson, Temporal, or Kubernetes. Authentication stays enabled. [Product goal](goal.md) owns the general-purpose scope and the two-sandbox acceptance scenario.

The first milestone requires phases 1–3 below. Pause/resume follows as a separate capability: create → run a script → pause → release resources → resume the same script → destroy. Its existing memory/disk, controlled-resume, and recovery guarantees remain required before it ships.

## Implementation phases

| Phase | Deliverable and exit gate | Status |
| --- | --- | --- |
| 1. Foundation and single-host execution | Defined threat model, compatibility and privilege contract; Rust protocol/API/controller/supervisor/guest, SQLx storage access with versioned SQL migrations needed by shipped operations and admin audit persistence, authenticated setup and project requests; one jailed VM runs commands and long-running processes, transfers files, streams output, enforces resource/network limits, and tears down | Planned |
| 2. Recovery and isolation | Failure-injection at create/execute/cancel/destroy boundaries; fenced ownership, honest unknown outcomes, retry deduplication, egress/tenant isolation, and cleanup convergence | Planned |
| 3. Usable distribution | Supported single-host installation, CLI, working workload example, diagnostics, monitoring, client conformance checks, and verified teardown exercised by another developer; basic backup/restore and upgrade procedures for the shipped components | Planned |
| 4. Pause/resume | Snapshot persistence and verified complete memory/disk publication, compute release, compatible restore, guest handshake, original deadlines, and failure-injection at every snapshot/restore boundary | Planned |
| 5. Management UI and platform packaging | Session migrations, shared Project/Admin policy, UI flows and acceptance checks, and Hudson using ordinary APIs; Kubernetes packaging follows the standalone proof | Planned |
| 6. Multiple hosts and optimization | Compatible cross-host restore, placement, draining, provider autoscaling, and measured cache/snapshot optimizations | Planned |

API contracts come first; implement the CLI against working endpoints and add SDKs against the same versioned schemas. A minimal CLI supports the single-host milestone; client packaging and conformance checks belong to the distribution phase. SDK languages remain undecided. Existing UI designs remain available for later implementation, which shares the same admission and lifecycle services instead of creating a second control path. Define generic authenticated service connectivity before exposing guest services. Exact work breakdown can be split into issues once each phase has concrete interfaces.
