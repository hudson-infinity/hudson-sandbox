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
| 1. Foundation and single-host execution | The [threat model](threat-model.md) turned into enforced behavior, plus the compatibility and privilege contract; Rust protocol/API/controller/supervisor/guest, SQLx storage access with versioned SQL migrations needed by shipped operations and admin audit persistence, authenticated setup and project requests; one jailed VM runs commands and long-running processes, transfers files, streams output, enforces resource/network limits, and tears down | Planned |
| 2. Recovery and isolation | Failure-injection at create/execute/cancel/destroy boundaries; fenced ownership, honest unknown outcomes, retry deduplication, egress/tenant isolation, cleanup convergence, and the applicable [threat model](threat-model.md#required-validation) test set | Planned |
| 3. Usable distribution | Supported single-host installation, CLI, working workload example, diagnostics, monitoring, client conformance checks, and verified teardown exercised by another developer; basic backup/restore and upgrade procedures for the shipped components | Planned |
| 4. Pause/resume | Snapshot persistence and verified complete memory/disk publication, compute release, compatible restore, guest handshake, original deadlines, and failure-injection at every snapshot/restore boundary; the published format satisfies the [page-addressable constraints](performance.md#constraints-on-designs-we-are-choosing-now) | Planned |
| 5. Management UI and platform packaging | Session migrations, shared Project/Admin policy, UI flows and acceptance checks, and Hudson using ordinary APIs; Kubernetes packaging follows the standalone proof | Planned |
| 6. Multiple hosts and optimization | Compatible cross-host restore, placement, draining, provider autoscaling, and measured cache/snapshot optimizations including differential and lazily loaded snapshots | Planned |

API contracts come first; implement the CLI against working endpoints and add SDKs against the same versioned schemas. A minimal CLI supports the single-host milestone; client packaging and conformance checks belong to the distribution phase. SDK languages remain undecided. Existing UI designs remain available for later implementation, which shares the same admission and lifecycle services instead of creating a second control path. Define generic authenticated service connectivity before exposing guest services. Exact work breakdown can be split into issues once each phase has concrete interfaces.

## Feasibility spikes (Phase 0)

Two assumptions carry most of this design's risk, and neither has been executed. The first is that a jailed Firecracker VM on our supported host enforces the resource and connectivity limits the contracts assume. The second is that a guest agent can survive a snapshot outside the frozen customer process groups, reconnect afterwards, and gate the release of those processes; everything in [lifecycle](lifecycle.md#resume) depends on it.

The second group gates Phase 4, not Phase 1, but it belongs here anyway: a negative answer changes what pause/resume can promise, and it is cheaper to learn that before three phases of contracts are built on top of it.

Spike on a real Linux/KVM host, with throwaway code that is not intended to merge, and write the findings down.

| Question | Gates | Why it decides the design |
| --- | --- | --- |
| Do jailer, seccomp, cgroups, and host networking actually enforce the CPU, memory, disk, and egress limits we specify? | Phase 1 | The isolation promise is the product; a limit that is requested but not enforced is not a limit |
| What does create-to-readiness cost on a supported host with a warm image cache? | Phase 1 | Sets whether the [performance](performance.md#proposed-budgets) create budget is reachable |
| Does a frozen customer cgroup stay frozen across a Firecracker snapshot and restore? | Phase 4 | The entire controlled-resume contract assumes it does |
| Can the guest agent reconnect over vsock after restore, given Firecracker's vsock reset? | Phase 4 | Without a reconnect there is no handshake, and without a handshake processes cannot be gated |
| What happens to guest time, timers, and TCP connections across a long pause? | Phase 4 | Deadline enforcement and "credentials are not valid after restore" depend on the answer |
| Can expired or cancelled process groups be terminated before any thaw? | Phase 4 | [Lifecycle](lifecycle.md#deadlines-and-cancellation) requires it |
| What do a pause and a cold cross-host restore actually cost in seconds and bytes? | Phase 4 | Sets whether the [performance](performance.md) resume budgets are reachable |
| Does the guest image keep customer privilege away from the management agent? | Phases 1 and 4 | A resumable image that cannot enforce this must be rejected, and [product goal](goal.md) makes the guest-root decision explicit |

Exit gate: a written findings document per question, with the commands run and the host configuration recorded. A negative answer is a successful spike; it redirects the design before the dependent phase rather than during it. If process-continuous resume proves unreachable on this stack, reopen [alternatives](alternatives.md#revisit-triggers).

## Scope discipline for Phase 1

Phase 1 is the largest phase and the easiest to let grow. The exit gate is a single authenticated path working end to end: an authorized caller creates a sandbox, runs a command and a long-running process in a real VM, transfers a file, reads output, destroys the sandbox, and the allocation's release is confirmed in the database. Resource and connectivity limits are enforced by the host, not requested politely.

Migrations cover the records those operations touch and nothing further. Snapshot persistence belongs to Phase 4, sessions and browser audit surfaces to Phase 5. Deferring a table is not deferring correctness: allocation generations, supervisor epochs, and claim revisions are cheap to build in now and expensive to retrofit, so they stay in scope from the first migration.

## Required evidence by delivery gate

| Area | Required evidence | Current evidence |
| --- | --- | --- |
| Feasibility | Written [Phase 0 spike findings](#feasibility-spikes-phase-0) on a real KVM host, before the phase each question gates | Not run |
| Lifecycle correctness | [Lifecycle acceptance cases](lifecycle.md#acceptance-checks) applicable to shipped operations, including controller/host failure and cleanup; snapshot/restore cases are mandatory at the pause/resume gate | Not implemented/tested |
| API behavior | [Admission, retries, errors, and streaming checks](api-contract.md#acceptance-checks-and-open-decisions) for each shipped endpoint | Not implemented/tested |
| Ownership/storage | SQLx query/schema checks, fresh and supported-upgrade migration tests, and [model constraints and ID/storage checks](data-models.md#acceptance-checks-and-open-decisions) for shipped resources; snapshot checks before pause/resume | Not implemented/tested |
| Authentication | [Auth acceptance](auth-design.md#acceptance-checks) for shipped surfaces, mandatory locally and in deployment; browser/session checks before shipping the UI | Not implemented/tested |
| Host isolation | The adversarial test set in [threat model](threat-model.md#required-validation): guest privilege, filesystem traversal, metadata/control-plane egress, cross-tenant access, and credential/session attacks | Not implemented/tested |
| Distribution and usability | Fresh-host installation, workload/file example, diagnostics, failure recovery, and verified resource reclamation by another developer | Not implemented/tested |
| Performance | [Budget table](performance.md#proposed-budgets) measured on a supported host, with configuration recorded; resume budgets at the pause/resume gate | Not measured |
| Management UI | [UI acceptance](ui-design.md#acceptance-checks) before shipping the dashboard | Not implemented/tested |

Add links to actual test files, CI runs, supported-host evidence, and releases as each gate is demonstrated. A document, successful process start, or passing unit test alone does not establish snapshot or isolation correctness.

## Blocking non-engineering decisions

These are owner decisions. Each one blocks work that is otherwise ready to start, and none of them is resolved by writing more design.

| Decision | What it blocks | Owner action |
| --- | --- | --- |
| License selection | Substantial outside contribution, and any reuse of this repository. [CONTRIBUTING](../CONTRIBUTING.md) currently asks contributors to resolve licensing individually, which is not a workable ask | Choose a license and add the file |
| Hosted offering, or self-hosting only | Whether usage metering belongs in the data model before it has customers in it | Decide, then record it in [alternatives](alternatives.md) |
| Custom guest images | Whether users can bring their own dependencies. Today only an operator-configured digest allowlist is designed, which is unlikely to be enough for the general-purpose workloads in [product goal](goal.md) | Decide whether per-project images ship before or after the first release |
| Supported host baseline | Which kernel, CPU, and Firecracker versions the first release claims | Pick one configuration before Phase 1 hardware work |

## Work breakdown

Phase 0 and Phase 1 are concrete enough to become issues now; waiting for every interface to be settled is what produced a documentation-only repository. Open one issue per spike question and one per Phase 1 scope item, and keep later phases as planning documents until their predecessor's gate passes.

## Development and operational prerequisites

Begin with one Linux compute host exposing KVM and one supported architecture. A standalone development setup needs the API/controller, PostgreSQL, object storage, and that host. It must run without the Hudson harness or a Temporal service. Kubernetes is the intended platform deployment, not a requirement for every developer unit test.

Local admin setup creates the installation's first admin credential. The Admin UI/API or authenticated tooling then creates projects and issues project tokens once; contributors and self-hosters use the same authenticated setup contract as Hudson deployments. Keep the raw token in the calling backend's secret configuration and only its hash in PostgreSQL. Setup requires installation-administrator authority; no unprotected public bootstrap endpoint is provided. Local development does not disable authentication. This tooling is planned, not implemented yet.

Use a remote Linux host for real VM tests from macOS. An unrestricted local process is not a substitute for the isolation boundary. Publish reproducible guest image builds with immutable digests and compatibility metadata.

Instrument operations through OpenTelemetry and expose metrics for Prometheus/Grafana. Record queue time, VM readiness, snapshot/upload duration, restore duration broken down by stage, resource usage, lease expiry, uncertain outcomes, and leaked resources. Correlate by project, sandbox, operation, attempt, host, and optional caller correlation ID. Redact secrets and keep terminal output in bounded artifacts.

Drain hosts before maintenance and prevent new placements while draining. In the initial runtime, wait for work to finish or explicitly stop it with accurate outcomes; do not imply saved memory exists. Once pause/resume ships, verify resumable snapshots before a drain that promises state preservation. Preserve explicit failure outcomes for forced termination. Database migrations, API/controller upgrades, host supervisor upgrades, and guest image changes need independent compatibility and rollback plans. Kubernetes restarts do not replace those plans.

Operational documentation should include proven setup commands, secret provisioning, backups/restores, upgrade/rollback procedures, host maintenance, and capacity recovery once those mechanisms exist. Do not publish hypothetical commands as an install guide.

## Deferred scope

Pause/resume, multiple hosts, Kubernetes packaging, enterprise identity, specialized workload experiences, a polished dashboard, and performance optimizations follow the first usable runtime. Live migration, transparent recovery of unsaved memory after host loss, one Kubernetes pod per sandbox, and a custom hypervisor are outside the initial scope. No MCP server is planned for the current scope; see [decision 0002](decisions/0002-no-mcp-server-initially.md) and its revisit trigger. Agent integration uses the API/SDK or the CLI through a harness shell tool. Redis, ClickHouse, and more elaborate scheduling remain deferred. The scoped Project/Admin management UI remains part of later planned delivery; Hudson's agent/task UI remains outside this repository.

Differential snapshots, lazy loading, and warm pools are deferred to Phase 6, but the format constraints that keep them possible apply from Phase 4. [Performance](performance.md#constraints-on-designs-we-are-choosing-now) owns those constraints.

## Documentation to add when supported by implementation

- `development.md`: reproducible contributor build/run/test instructions.
- `self-hosting.md`: actual installation, first Admin credential, TLS, storage, upgrades, and rollback.
- `operations.md`: backup recovery, monitoring, host drains, and incident procedures.

These future guides are intentionally not empty placeholders today. [CONTRIBUTING.md](../CONTRIBUTING.md) defines the PR/release process and documentation checks; [SECURITY.md](../SECURITY.md) provides the private reporting channel; [decisions](decisions/README.md) records significant choices as they are made. License selection remains an owner decision and is listed above as blocking; the repository currently has no license file. Version pins, frontend framework, provider integration, and concrete test locations remain open.
