# Roadmap and delivery gates

Status: all milestones planned. The runtime is design only. Contribution tooling and documentation checks exist; there are no deployed services, working runtime setup commands, migrations, or runtime validation results.

The design documents are well ahead of the code, and their central assumptions have never been executed on real hardware. The phase order below starts by testing those assumptions rather than adding to the contracts that rest on them.

## First usable milestone

**Create → execute and transfer files → enforce isolation and limits → destroy**, on one compatible Linux/KVM host with PostgreSQL and object storage. Include failure recovery, confirmed cleanup, and a reproducible CLI installation. Run it without Hudson, Temporal, or Kubernetes. Authentication stays enabled. [Product goal](goal.md) owns the general-purpose scope and the two-sandbox acceptance scenario.

The first milestone requires phases 1–3 below. Pause/resume follows as a separate capability: create → run a script → pause → release resources → resume the same script → destroy. Its existing memory/disk, controlled-resume, and recovery guarantees remain required before it ships.

## Implementation phases

| Phase | Deliverable and exit gate | Status |
| --- | --- | --- |
| 0. Feasibility spikes | Throwaway code on a real Linux/KVM host answering the questions in [feasibility spikes](#feasibility-spikes-phase-0); written findings, kept or discarded design assumptions, and first [performance](performance.md) measurements | Planned |
| 1. Foundation, execution, recovery and isolation | Two gates, built together and passed in order. **Gate 1a — it works:** the [threat model](threat-model.md) turned into enforced behavior, plus the compatibility and privilege contract; Rust protocol/API/controller/supervisor/guest, SQLx storage with versioned migrations for the records shipped operations touch, authenticated setup and project requests; one jailed VM runs commands and long-running processes, transfers files, streams output, enforces resource and network limits, and tears down. **Gate 1b — it does not lie:** failure-injection at create/execute/cancel/destroy boundaries; fenced ownership, honest unknown outcomes, retry deduplication, egress and tenant isolation, cleanup convergence, and the applicable [threat model](threat-model.md#required-validation) test set | Planned |
| 2. Usable distribution | Supported single-host installation, CLI, working workload example, diagnostics, monitoring, client conformance checks, and verified teardown exercised by another developer; basic backup/restore and upgrade procedures for the shipped components | Planned |
| 3. Pause/resume | Snapshot persistence and verified complete memory/disk publication, compute release, compatible restore, guest handshake, original deadlines, and failure-injection at every snapshot/restore boundary; the published format satisfies the [page-addressable constraints](performance.md#constraints-on-designs-we-are-choosing-now) | Planned |
| 4. Management UI and platform packaging | Session migrations, shared Project/Admin policy, UI flows and acceptance checks, and Hudson using ordinary APIs; Kubernetes packaging follows the standalone proof | Planned |
| 5. Multiple hosts and optimization | Compatible cross-host restore, placement, draining, provider autoscaling, and measured cache/snapshot optimizations including differential and lazily loaded snapshots | Planned |

API contracts come first; implement the CLI against working endpoints and add SDKs against the same versioned schemas. A minimal CLI supports the single-host milestone; client packaging and conformance checks belong to the distribution phase, and cover all three SDKs. Existing UI designs remain available for later implementation, which shares the same admission and lifecycle services instead of creating a second control path. Define generic authenticated service connectivity before exposing guest services. Exact work breakdown can be split into issues once each phase has concrete interfaces.

## Feasibility spikes (Phase 0)

Two assumptions carry most of this design's risk, and neither has been executed. The first is that a jailed Firecracker VM on our supported host enforces the resource and connectivity limits the contracts assume. The second is that a guest agent can survive a snapshot outside the frozen customer process groups, reconnect afterwards, and gate the release of those processes; everything in [lifecycle](lifecycle.md#resume) depends on it.

The second group gates Phase 3, not Phase 1, but it belongs here anyway: a negative answer changes what pause/resume can promise, and it is cheaper to learn that before two phases of contracts are built on top of it.

Spike on a real Linux/KVM host, with throwaway code that is not intended to merge, and write the findings down.

| Question | Gates | Why it decides the design |
| --- | --- | --- |
| Do jailer, seccomp, cgroups, and host networking actually enforce the CPU, memory, disk, and egress limits we specify? | Phase 1 | The isolation promise is the product; a limit that is requested but not enforced is not a limit |
| What does create-to-readiness cost on a supported host with a warm image cache? | Phase 1 | Sets whether the [performance](performance.md#proposed-budgets) create budget is reachable |
| Does a frozen customer cgroup stay frozen across a Firecracker snapshot and restore? | Phase 3 | The entire controlled-resume contract assumes it does |
| Can the guest agent reconnect over vsock after restore, given Firecracker's vsock reset? | Phase 3 | Without a reconnect there is no handshake, and without a handshake processes cannot be gated |
| What happens to guest time, timers, and TCP connections across a long pause? | Phase 3 | Deadline enforcement and "credentials are not valid after restore" depend on the answer |
| Can expired or cancelled process groups be terminated before any thaw? | Phase 3 | [Lifecycle](lifecycle.md#deadlines-and-cancellation) requires it |
| What do a pause and a cold cross-host restore actually cost in seconds and bytes? | Phase 3 | Sets whether the [performance](performance.md) resume budgets are reachable |
| How well can the guest agent be shielded from a root customer in the same VM? | Phases 1 and 3 | [Decision 0003](decisions/0003-guest-root-with-our-kernel.md) grants root deliberately. Measure what a hostile root can actually do to the agent: kill sweeps, reaching its control socket, forging a handshake |

The [spike sheet](implementation/phase-0-spikes.md) carries the method for each one. Exit gate: a written findings document per question, with the commands run and the host configuration recorded. A negative answer is a successful spike; it redirects the design before the dependent phase rather than during it. If process-continuous resume proves unreachable on this stack, reopen [alternatives](alternatives.md#revisit-triggers).

## Scope discipline for Phase 1

Execution and recovery were previously separate phases. They are merged because the mechanics that make recovery work — allocation generations, supervisor epochs, claim revisions, receipts before and after every external action — are cheap to build into the first implementation and expensive to retrofit into a working one. Writing failure-injection tests alongside the code that must survive them is also better than bolting them on to code that was never shaped for it.

The merge does not mean one undifferentiated push. The phase has **two gates, passed in order**, so there is still a moment where the system is demonstrably working before it is asked to prove it does not lie.

**Gate 1a — it works.** One authenticated path end to end: an authorized caller creates a sandbox, runs a command and a long-running process in a real VM, transfers a file, reads output, destroys the sandbox, and the allocation's release is confirmed in the database. Resource and connectivity limits are enforced by the host, not requested politely. Stop here and tag it; this is the first thing worth showing anyone.

**Gate 1b — it does not lie.** Every row of the [lifecycle recovery table](lifecycle.md#destroy-and-recovery) that applies to create, execute, cancel and destroy reconciles correctly under injected failure. Stale controller claims, stale supervisor epochs and stale allocation generations are all rejected. Uncertain outcomes are recorded as `unknown` and reconciled rather than guessed. The [threat model's adversarial set](threat-model.md#required-validation) passes for the surfaces that exist. No leaked allocations, disk, or VMs after any of it.

Scope discipline still applies to both. Migrations cover the records those operations touch and nothing further. Snapshot persistence belongs to Phase 3, sessions and browser audit surfaces to Phase 4. What 1a may not do is skip the ownership mechanics on the grounds that 1b will add them — they are in scope from the first migration, and 1b proves they work rather than introducing them.

## Required evidence by delivery gate

| Area | Required evidence | Current evidence |
| --- | --- | --- |
| Feasibility | Written [Phase 0 spike findings](#feasibility-spikes-phase-0) on a real KVM host, before the phase each question gates | Not run |
| Recovery under failure | Every applicable row of the [lifecycle recovery table](lifecycle.md#destroy-and-recovery) reconciled under injected failure, at gate 1b | Not implemented/tested |
| Lifecycle correctness | [Lifecycle acceptance cases](lifecycle.md#acceptance-checks) applicable to shipped operations, including controller/host failure and cleanup; snapshot/restore cases are mandatory at the pause/resume gate | Not implemented/tested |
| API behavior | [Admission, retries, errors, and streaming checks](api-contract.md#acceptance-checks-and-open-decisions) for each shipped endpoint | Not implemented/tested |
| Ownership/storage | SQLx query/schema checks, fresh and supported-upgrade migration tests, and [model constraints and ID/storage checks](data-models.md#acceptance-checks-and-open-decisions) for shipped resources; snapshot checks before pause/resume | Not implemented/tested |
| Authentication | [Auth acceptance](auth-design.md#acceptance-checks) for shipped surfaces, mandatory locally and in deployment; browser/session checks before shipping the UI | Not implemented/tested |
| Host isolation | The adversarial test set in [threat model](threat-model.md#required-validation): guest privilege, filesystem traversal, metadata/control-plane egress, cross-tenant access, and credential/session attacks | Not implemented/tested |
| Distribution and usability | Fresh-host installation, workload/file example, diagnostics, failure recovery, and verified resource reclamation by another developer | Not implemented/tested |
| Performance | [Budget table](performance.md#proposed-budgets) measured on a supported host, with configuration recorded; resume budgets at the pause/resume gate | Not measured |
| Management UI | [UI acceptance](ui-design.md#acceptance-checks) before shipping the dashboard | Not implemented/tested |

Add links to actual test files, CI runs, supported-host evidence, and releases as each gate is demonstrated. A document, successful process start, or passing unit test alone does not establish snapshot or isolation correctness.

## Settled decisions

The pre-implementation decisions were worked through on 2026-09-21 and now live in their owning documents. Summarised here so the delivery picture is readable in one place:

| Area | Settled | Owning document |
| --- | --- | --- |
| Customer privilege | Root in guest userspace; our kernel, init and guest agent | [Decision 0003](decisions/0003-guest-root-with-our-kernel.md) |
| Freeze boundary | In-guest agent, outside the frozen workload cgroup | [Lifecycle](lifecycle.md#resume) |
| Snapshot encryption | One installation-wide key from the operator's KMS, key ID in the manifest | [Threat model](threat-model.md#open-decisions) |
| Host and guest envelope | x86_64, Ubuntu 24.04 host, Debian userland, 4 vCPU / 8 GiB ceiling, services supported | [Supported configuration](compatibility.md) |
| Networking | Deny-by-default egress on CIDR and port, host-side resolver, no ingress in v1, per-sandbox bandwidth cap | [Networking](networking.md) |
| Images | Operator allowlist only; per-project images named as the next capability | [Data models](data-models.md#what-we-keep-inside-these-models) |
| Clients | CLI plus Python, TypeScript and Rust SDKs, generated from one OpenAPI document with a shared conformance suite | [API contract](api-contract.md#sdk-and-cli-behavior) |
| API shape | RFC 9457 problem+json, server-sent events, opaque cursors, single-PUT file upload | [API contract](api-contract.md) |
| Storage | PostgreSQL 16, `sqlx migrate`, MinIO in development and CI, 7-day snapshot retention, tombstones for the project's lifetime | [Data models](data-models.md) |
| Internal protocols | gRPC over mTLS to the supervisor, vsock with length-prefixed protobuf to the guest | [Architecture](architecture.md#selected-stack) |
| Policy numbers | 15-minute default and 6-hour maximum deadline, 1-hour idle, 10 MiB retained output, 25 sandboxes per project | [Lifecycle](lifecycle.md#deadlines-and-cancellation) |
| Observability | OpenTelemetry into Prometheus and Tempo; JSON logs everywhere | [Architecture](architecture.md#selected-stack) |
| License | Apache-2.0 | [Decision 0004](decisions/0004-apache-2-0-license.md) |
| Delivery | Self-host first, hosted possible later; usage derived rather than metered; public 0.1 once Phase 3 passes | [Alternatives](alternatives.md) |

## Still blocking

Two decisions remain, and both are procurement rather than design. Phase 0 cannot start without the first.

| Decision | What it blocks | Owner action |
| --- | --- | --- |
| Hardware for the spikes | Every Phase 0 question, and therefore Phase 1. None of this work runs on macOS | Rent a bare-metal x86_64 host with KVM |
| CI for tests that need a real VM | Whether snapshot and isolation tests run on every pull request or only when someone remembers | Decide once the host exists; a self-hosted runner on it is the obvious answer, with fork pull requests excluded |

## Work breakdown

Phase 0 and Phase 1 are concrete enough to become issues now, and the decisions above remove the remaining excuse for not writing them. Both are broken down in [implementation notes](implementation/README.md): a [spike sheet](implementation/phase-0-spikes.md) and a [task list](implementation/phase-1-tasks.md) covering both gates.

Those notes are deliberately temporary and are deleted as the work lands. This document, the contracts, and the decision records are not — the reasoning outlives the build order.

## Development and operational prerequisites

Begin with one Linux compute host matching [supported configuration](compatibility.md#host): x86_64, Ubuntu 24.04, KVM available. A standalone development setup needs the API/controller, PostgreSQL, object storage, and that host. It must run without the Hudson harness or a Temporal service. Kubernetes is the intended platform deployment, not a requirement for every developer unit test.

Local admin setup creates the installation's first admin credential. The Admin UI/API or authenticated tooling then creates projects and issues project tokens once; contributors and self-hosters use the same authenticated setup contract as Hudson deployments. Keep the raw token in the calling backend's secret configuration and only its hash in PostgreSQL. Setup requires installation-administrator authority; no unprotected public bootstrap endpoint is provided. Local development does not disable authentication. This tooling is planned, not implemented yet.

Use a remote Linux host for real VM tests from macOS. An unrestricted local process is not a substitute for the isolation boundary. Publish reproducible guest image builds with immutable digests and compatibility metadata.

Instrument operations through OpenTelemetry and expose metrics for Prometheus/Grafana. Record queue time, VM readiness, snapshot/upload duration, restore duration broken down by stage, resource usage, lease expiry, uncertain outcomes, and leaked resources. Correlate by project, sandbox, operation, attempt, host, and optional caller correlation ID. Redact secrets and keep terminal output in bounded artifacts.

Drain hosts before maintenance and prevent new placements while draining. In the initial runtime, wait for work to finish or explicitly stop it with accurate outcomes; do not imply saved memory exists. Once pause/resume ships, verify resumable snapshots before a drain that promises state preservation. Preserve explicit failure outcomes for forced termination. Database migrations, API/controller upgrades, host supervisor upgrades, and guest image changes need independent compatibility and rollback plans. Kubernetes restarts do not replace those plans.

Operational documentation should include proven setup commands, secret provisioning, backups/restores, upgrade/rollback procedures, host maintenance, and capacity recovery once those mechanisms exist. Do not publish hypothetical commands as an install guide.

## Deferred scope

Pause/resume, multiple hosts, Kubernetes packaging, enterprise identity, specialized workload experiences, a polished dashboard, and performance optimizations follow the first usable runtime. Live migration, transparent recovery of unsaved memory after host loss, one Kubernetes pod per sandbox, and a custom hypervisor are outside the initial scope. No MCP server is planned for the current scope; see [decision 0002](decisions/0002-no-mcp-server-initially.md) and its revisit trigger. Agent integration uses the API/SDK or the CLI through a harness shell tool. Redis, ClickHouse, and more elaborate scheduling remain deferred. The scoped Project/Admin management UI remains part of later planned delivery; Hudson's agent/task UI remains outside this repository.

Differential snapshots, lazy loading, and warm pools are deferred to Phase 5, but the format constraints that keep them possible apply from Phase 3. [Performance](performance.md#constraints-on-designs-we-are-choosing-now) owns those constraints.

## Documentation to add when supported by implementation

- `development.md`: reproducible contributor build/run/test instructions.
- `self-hosting.md`: actual installation, first Admin credential, TLS, storage, upgrades, and rollback.
- `operations.md`: backup recovery, monitoring, host drains, and incident procedures.

These future guides are intentionally not empty placeholders today. [CONTRIBUTING.md](../CONTRIBUTING.md) defines the PR/release process and documentation checks; [SECURITY.md](../SECURITY.md) provides the private reporting channel; [decisions](decisions/README.md) records significant choices as they are made. The repository is released under Apache-2.0. Version pins, frontend framework, provider integration, and concrete test locations remain open.
