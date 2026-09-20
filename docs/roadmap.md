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

## Required evidence by delivery gate

| Area | Required evidence | Current evidence |
| --- | --- | --- |
| Lifecycle correctness | [Lifecycle acceptance cases](lifecycle.md#acceptance-checks) applicable to shipped operations, including controller/host failure and cleanup; snapshot/restore cases are mandatory at the pause/resume gate | Not implemented/tested |
| API behavior | [Admission, retries, errors, and streaming checks](api-contract.md#acceptance-checks-and-open-decisions) for each shipped endpoint | Not implemented/tested |
| Ownership/storage | SQLx query/schema checks, fresh and supported-upgrade migration tests, and [model constraints and ID/storage checks](data-models.md#acceptance-checks-and-open-decisions) for shipped resources; snapshot checks before pause/resume | Not implemented/tested |
| Authentication | [Auth acceptance](auth-design.md#acceptance-checks) for shipped surfaces, mandatory locally and in deployment; browser/session checks before shipping the UI | Not implemented/tested |
| Host isolation | Adversarial tests for guest privilege, filesystem traversal, metadata/control-plane egress, and cross-tenant access | Not implemented/tested |
| Distribution and usability | Fresh-host installation, workload/file example, diagnostics, failure recovery, and verified resource reclamation by another developer | Not implemented/tested |
| Management UI | [UI acceptance](ui-design.md#acceptance-checks) before shipping the dashboard | Not implemented/tested |

Add links to actual test files, CI runs, supported-host evidence, and releases as each gate is demonstrated. A document, successful process start, or passing unit test alone does not establish snapshot or isolation correctness.

## Development and operational prerequisites

Begin with one Linux compute host exposing KVM and one supported architecture. A standalone development setup needs the API/controller, PostgreSQL, object storage, and that host. It must run without the Hudson harness or a Temporal service. Kubernetes is the intended platform deployment, not a requirement for every developer unit test.

Local admin setup creates the installation's first admin credential. The Admin UI/API or authenticated tooling then creates projects and issues project tokens once; contributors and self-hosters use the same authenticated setup contract as Hudson deployments. Keep the raw token in the calling backend's secret configuration and only its hash in PostgreSQL. Setup requires installation-administrator authority; no unprotected public bootstrap endpoint is provided. Local development does not disable authentication. This tooling is planned, not implemented yet.

Use a remote Linux host for real VM tests from macOS. An unrestricted local process is not a substitute for the isolation boundary. Publish reproducible guest image builds with immutable digests and compatibility metadata.

Instrument operations through OpenTelemetry and expose metrics for Prometheus/Grafana. Record queue time, VM readiness, snapshot/upload duration, restore duration, resource usage, lease expiry, uncertain outcomes, and leaked resources. Correlate by project, sandbox, operation, attempt, host, and optional caller correlation ID. Redact secrets and keep terminal output in bounded artifacts.

Drain hosts before maintenance and prevent new placements while draining. In the initial runtime, wait for work to finish or explicitly stop it with accurate outcomes; do not imply saved memory exists. Once pause/resume ships, verify resumable snapshots before a drain that promises state preservation. Preserve explicit failure outcomes for forced termination. Database migrations, API/controller upgrades, host supervisor upgrades, and guest image changes need independent compatibility and rollback plans. Kubernetes restarts do not replace those plans.

Operational documentation should include proven setup commands, secret provisioning, backups/restores, upgrade/rollback procedures, host maintenance, and capacity recovery once those mechanisms exist. Do not publish hypothetical commands as an install guide.

## Deferred scope

Pause/resume, multiple hosts, Kubernetes packaging, enterprise identity, specialized workload experiences, a polished dashboard, and performance optimizations follow the first usable runtime. Live migration, transparent recovery of unsaved memory after host loss, one Kubernetes pod per sandbox, and a custom hypervisor are outside the initial scope. No MCP server is planned for the current scope; agent integration uses the API/SDK or the CLI through a harness shell tool. Redis, ClickHouse, and more elaborate scheduling remain deferred. The scoped Project/Admin management UI remains part of later planned delivery; Hudson's agent/task UI remains outside this repository.

## Documentation to add when supported by implementation

- `development.md`: reproducible contributor build/run/test instructions.
- `self-hosting.md`: actual installation, first Admin credential, TLS, storage, upgrades, and rollback.
- `operations.md`: backup recovery, monitoring, host drains, and incident procedures.
- `decisions/`: short records for significant future choices, with status and supersession links.

These future guides are intentionally not empty placeholders today. [CONTRIBUTING.md](../CONTRIBUTING.md) now defines the PR/release process and documentation checks; [SECURITY.md](../SECURITY.md) provides the private reporting channel. License selection remains an owner decision; the repository currently has no license file. Version pins, frontend framework, provider integration, and concrete test locations remain open.
