# Roadmap and delivery gates

Status: all milestones planned. Only documentation exists; there are no deployed services, working setup commands, migrations, or executable validation results.

## First complete milestone

**Create → run a script → pause → release resources → resume the same script → destroy**, on one compatible Linux/KVM host with PostgreSQL and object storage. Run it without Hudson, Temporal, or Kubernetes. Authentication stays enabled.

The phases below organize implementation, not optional shortcuts around the complete milestone's recovery and isolation requirements.

## Implementation phases

| Phase | Deliverable and exit gate | Status |
| --- | --- | --- |
| 1. Foundation and single-host execution | Rust protocol/API/controller/supervisor/guest, six resource-model migrations and admin audit persistence, authenticated setup and project requests; one jailed VM executes commands, streams output, enforces CPU/RAM/disk limits, and tears down | Planned |
| 2. Pause/resume | Verified complete memory/disk snapshot, durable publication, compute release, compatible restore, guest handshake, and original deadlines | Planned |
| 3. Recovery and isolation | Failure-injection at every lifecycle boundary; fenced ownership, honest unknown outcomes, retry deduplication, egress/tenant isolation, and cleanup convergence | Planned |
| 4. Management UI and delivery | Session migrations, shared Project/Admin policy, UI flows and acceptance checks, self-hosting setup, deployment/monitoring, and Hudson using ordinary APIs; Kubernetes packaging follows the standalone proof | Planned |
| 5. Multiple hosts and optimization | Compatible cross-host restore, placement, draining, provider autoscaling, and measured cache/snapshot optimizations | Planned |

UI design starts now. Implementation shares the same admission and lifecycle services instead of creating a second control path. Exact work breakdown can be split into issues once each phase has concrete interfaces.

## Required evidence before first usable runtime

| Area | Required evidence | Current evidence |
| --- | --- | --- |
| Lifecycle correctness | Every [lifecycle acceptance case](lifecycle.md#acceptance-checks), including controller/host failure and cleanup | Not implemented/tested |
| API behavior | [Admission, retries, errors, and streaming checks](api-contract.md#acceptance-checks-and-open-decisions) | Not implemented/tested |
| Ownership/storage | [Model constraints and ID/storage checks](data-models.md#acceptance-checks-and-open-decisions) | Not implemented/tested |
| Authentication | [Auth acceptance](auth-design.md#acceptance-checks), mandatory locally and in deployment | Not implemented/tested |
| Host isolation | Adversarial tests for guest privilege, filesystem traversal, metadata/control-plane egress, and cross-tenant access | Not implemented/tested |
| Management UI | [UI acceptance](ui-design.md#acceptance-checks) before shipping the dashboard | Not implemented/tested |

Add links to actual test files, CI runs, supported-host evidence, and releases as each gate is demonstrated. A document, successful process start, or passing unit test alone does not establish snapshot or isolation correctness.

## Development and operational prerequisites

Begin with one Linux compute host exposing KVM and one supported architecture. A standalone development setup needs the API/controller, PostgreSQL, object storage, and that host. It must run without the Hudson harness or a Temporal service. Kubernetes is the intended platform deployment, not a requirement for every developer unit test.

Local admin setup creates the installation's first admin credential. The Admin UI/API or authenticated tooling then creates projects and issues project tokens once; contributors and self-hosters use the same authenticated setup contract as Hudson deployments. Keep the raw token in the calling backend's secret configuration and only its hash in PostgreSQL. Setup requires installation-administrator authority; no unprotected public bootstrap endpoint is provided. Local development does not disable authentication. This tooling is planned, not implemented yet.

Use a remote Linux host for real VM tests from macOS. An unrestricted local process is not a substitute for the isolation boundary. Publish reproducible guest image builds with immutable digests and compatibility metadata.

Instrument operations through OpenTelemetry and expose metrics for Prometheus/Grafana. Record queue time, VM readiness, snapshot/upload duration, restore duration, resource usage, lease expiry, uncertain outcomes, and leaked resources. Correlate by project, sandbox, operation, attempt, host, and optional caller correlation ID. Redact secrets and keep terminal output in bounded artifacts.

Drain hosts before maintenance and prevent new placements while draining. Verify resumable snapshots before removing hosts that hold running workloads; preserve explicit failure outcomes for forced termination. Database migrations, API/controller upgrades, host supervisor upgrades, and guest image changes need independent compatibility and rollback plans. Kubernetes restarts do not replace those plans.

Operational documentation should include proven setup commands, secret provisioning, backups/restores, upgrade/rollback procedures, host maintenance, and capacity recovery once those mechanisms exist. Do not publish hypothetical commands as an install guide.

## Deferred scope

Live migration, transparent recovery of unsaved memory after host loss, one Kubernetes pod per sandbox, and a custom hypervisor are outside the initial scope. Redis, ClickHouse, and more elaborate scheduling remain deferred. The scoped Project/Admin management UI is part of the planned delivery; Hudson's agent/task UI remains outside this repository.

## Documentation to add when supported by implementation

- `development.md`: reproducible contributor build/run/test instructions.
- `self-hosting.md`: actual installation, first Admin credential, TLS, storage, upgrades, and rollback.
- `operations.md`: backup recovery, monitoring, host drains, and incident procedures.
- Root `CONTRIBUTING.md` and `SECURITY.md`: contribution checks and a verified private reporting channel.
- `decisions/`: short records for significant future choices, with status and supersession links.

These files are intentionally not empty placeholders today. Release/license selection and the security-reporting contact remain owner decisions; the repository currently has no license file. Version pins, frontend framework, provider integration, and concrete test locations remain open.
