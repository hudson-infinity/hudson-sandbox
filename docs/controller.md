# Create, destroy, and allocation maintenance

Status: a single-host create/execute/destroy controller is implemented with PostgreSQL, the authenticated HTTP router, and gRPC/mTLS. The [real Linux supervisor](real-supervisor.md) now has controlled create, readiness, renewal and destroy evidence; the development fake remains available for portable tests. File transfer and automatic old-epoch database recovery remain unfinished; [command cancellation](command-cancellation.md) and output streaming are implemented.

## One operation through the loop

[The controller](../crates/sandbox-controller/src/lib.rs) verifies the configured supervisor's certificate through the [shared transport](supervisor-protocol.md), then checks its reported host ID, epoch, and evidence mode. It refreshes the heartbeat of that existing database host. This cannot create a host, change its epoch or capacity, or promote a draining host to ready. Host rows, epochs, capacity, endpoints, certificates, and allowed image digests are operator-provisioned for this initial integration; authenticated registration and automatic epoch issuance remain unfinished.

Each tick maintains at most one due running allocation, then claims at most one create or destroy with a 30-second operation lease. The preferred operation kind alternates between ticks, falling back to the other kind when empty. Create reserves capacity and examines persisted dispatch history. [Dispatch storage](../crates/sandbox-store/src/dispatch.rs) locks the operation, project, sandbox, and host in the same order as reservation. It constructs ownership from persisted rows, not caller-supplied copies of project or sandbox fields.

If the operation is still provably undispatched, storage rechecks the project credential, deadlines, desired state, host freshness, and the configured image allowlist. It commits the allocation's initial execution lease (at most 30 seconds, capped by the sandbox deadline), increments the dispatch attempt count, and records a create intent before returning the request to send. A controller crash cannot erase that intent.

A previous intent or unknown status selects inspection of the same allocation. It never selects another create request. A reservation without a dispatch intent can continue under a new claim because the database proves no controller following this protocol could have sent its create yet.

The controller currently works with one explicitly configured host. It does not route work across multiple hosts. Capacity shortages, draining/unavailable hosts, or reservations belonging to another host are deferred for five seconds. Work remains durable when the process exits or loses its claim.

## Completion and uncertainty

A successful RPC is only a candidate observation. Storage verifies the complete host/project/sandbox/allocation/operation tuple, generation, supervisor epoch, current claim revision and deadline, original create operation ID, and a supported observed state. The observation timestamp must be within ten seconds of PostgreSQL time. Readiness must still be inside the allocation's execution lease. Final writes check the claim again after lock waits; expiry rolls back all state changes.

A matching readiness observation atomically marks the allocation and sandbox running, completes the create operation, clears lifecycle ownership, and appends the bounded receipt. A matching released observation instead fails the create and records release evidence, retires the sandbox, and frees its reservation. Deadline expiry or missing evidence alone cannot release capacity.

A lost response, absent allocation, or rejected observation records `unknown`, retains capacity and dispatch history, and schedules reconciliation after one second. Repeated inspection does not increment create attempts or append unbounded copies of the same uncertainty. `unknown` has no completion timestamp. A later matching observation can complete it without replaying create.

The crash boundary after intent but before sending remains deliberately unresolved when the supervisor reports absence. The create loop preserves that uncertainty; the destroy path below can take cleanup ownership and resolve it using stop/fencing evidence. It cannot infer that an absent in-memory record proves there was no external action in a different incarnation.

A revoked credential, disallowed image, or unsupported resource size can fail an operation before dispatch. Placement and first-dispatch preparation both check the shared [guest resource envelope](compatibility.md#sandbox), including reservations retained from an earlier policy. A recorded dispatch intent still selects inspection before that policy check, preserving uncertain work. Service-owned rejection frees a reservation only when the locked records prove zero dispatch attempts, no execution lease, and an undispatched phase. It records that proof atomically with failure. Unknown or previously dispatched work cannot use this shortcut.

## Destroy admission and cleanup

`POST /v1/sandboxes/{sandbox_id}/destroy` requires project authentication, an idempotency key, and a JSON object. The only optional input is `correlation_id` (at most 200 bytes); caller-supplied lifecycle or ownership fields are rejected. Admission resolves the key and digest before lifecycle state. Identical retries return the same operation; a changed request conflicts. A new key for an already destroyed sandbox returns a completed no-op without changing its original destruction timestamp.

[Destroy storage](../crates/sandbox-store/src/destroy.rs) serializes on the active operation, project, and sandbox. A live conflicting transition returns `409` with its authorized `operation_id`; the rejected key is not consumed. An unknown create can hand cleanup ownership to destroy: its claim revision increases, its status stays unknown with phase `cleanup_owned_by_destroy`, and create claimers skip it. No in-flight create response can overwrite the new lifecycle owner.

The destroy controller persists stop intent before RPC. After a lost response it first inspects the same allocation. Confirmed release completes directly; ready or plain absent state permits another idempotent stop/fence, with intent recorded before that attempt. At most 100 stop attempts are issued for one operation; beyond that, inspection continues and unconfirmed capacity stays reserved for operator investigation. Receipt history retains the initial stop intent and final release, without appending unbounded copies of retries.

Completion requires a fresh, tuple-matching `released` or `fenced_absent` observation under the current claim, allocation generation, and supervisor epoch. Plain `absent`, an expired lease, and a stop request are insufficient. The explicit `fenced_absent` state means the supervisor has confirmed that no incarnation exists in that epoch and blocked future starts of that allocation; the fake models that fence in memory and reports simulated evidence. A restarted supervisor must have a new epoch and cannot reuse empty memory as old-epoch release proof.

Allocation release, its evidence, the sandbox tombstone, and successful destroy completion commit atomically. If destroy superseded an unknown create, that create ends as failed with `create_outcome_unknown=true` and a link to destroy; this does not invent whether it previously ran. A provably never-allocated create (generation and attempt count both zero) can be retired directly under database locks. Already admitted cleanup continues under service authority after credential revocation.

The fake's lost-stop-reply hook exercises both release of a recorded incarnation and fencing an absent one. The real supervisor must provide durable fencing and actual resource cleanup before it can return either completion state. Fake tests prove neither of those hardware guarantees.

## Allocation maintenance

[Maintenance storage](../crates/sandbox-store/src/leases.rs) uses a separate revision, claim deadline, and next-attempt time on each allocation. Completed create operations stay completed. The [migration](../migrations/0003_allocation_maintenance.sql) preserves existing execution deadlines and makes existing running allocations immediately eligible, without inventing observations or extending their leases.

Newly confirmed creates become due after ten seconds. Each controller tick claims at most one due allocation on its configured host/epoch using `FOR UPDATE SKIP LOCKED`; the running controller uses a ten-second maintenance claim. Database mutation locks follow project → sandbox → host → allocation, with final revision/deadline checks after lock waits. An expired claim cannot record a renewal or admit cleanup. No claim timeout, host timeout, or timestamp alone releases capacity.

Before renewal, storage checks the current allocation/generation/epoch, active transition, desired state, project status, and sandbox deadline. The new deadline is at most thirty seconds from database time and capped by sandbox expiry. `lease_requested_until` and `renewal_pending` commit before RPC; `lease_expires_at` advances only on matching ready evidence from the supervisor. The initial create still records its execution deadline as dispatch intent before readiness. Maintenance observations must match the complete maintenance ownership tuple, be within ten seconds of database time, and report a still-live deadline matching either the persisted request or the previously known lease. Simulation opt-in and provenance apply as in create/destroy.

A lost reply or crash with pending intent selects `InspectLease` before any new extension. If the host kept the old lease, inspection can confirm it and a subsequent claim may renew normally; if the renewal took effect, inspection confirms that deadline. Neither case replays create. A rejected or missing observation leaves the reservation counted, marks the sandbox unknown, and retries after two seconds. A failed health check also marks running observations on that host/epoch unknown without freeing capacity. Confirmed renewals schedule the next check in at most ten seconds, earlier near expiry, with a one-second minimum. Stored maintenance evidence is a bounded latest observation, not an unbounded per-heartbeat history.

A suspended/deleting project, expired sandbox deadline, or deadline shortened below an outstanding execution permit admits an ordinary `destroy` operation with `initiator_kind=service`. Credential rotation alone does not cancel already-admitted execution. A matching `released`/`fenced_absent` maintenance observation also admits service-owned destroy, preserving the observed evidence. The existing destroy path obtains its own stop/release evidence and atomically frees capacity and tombstones the sandbox; prior create outcomes remain historical facts. Plain absence never authorizes release. Destroy admission prevents later renewal commits, and the supervisor stop fence prevents a delayed renewal from reviving the incarnation.

Maintenance runs before one operation each tick, so a continuously nonempty operation queue does not skip renewal. All RPCs retain the five-second transport bound. The current loop processes one host and one maintenance item per tick: size and load-test the host against the thirty-second permit window before deployment; large-host batching and scheduling performance remain unvalidated. These are conservative development timings, not an availability guarantee. A healthy replacement epoch marks prior running observations unknown. Old-epoch allocations stay reserved until independently proven stopped/fenced; reconnecting to an empty restarted fake supplies no such proof.

[Lease integration tests](../crates/sandbox-controller/tests/leases.rs) cover competing claims, lost acknowledgements, crash-before-RPC recovery, stale observations, expiry during lock waits, deadline changes, health-check failure, service cleanup after suspension, credential rotation, and maintenance alongside operation queues. [Fake state tests](../crates/sandbox-fake-host/tests/state.rs) also verify survival beyond an initial lease after renewal, monotonic deadlines, watchdog expiry, and rejection of stale or post-stop renewal.

## Simulated observations remain visible

The controller rejects a simulated supervisor unless `--allow-simulated` is explicitly set. Use this mode only with isolated development data. A successful fake create includes `result.simulated=true` in the operation response. The sandbox response includes `observation_simulated=true`; false denotes real evidence, and omission denotes no confirmed observation source.

[Migration 0002](../migrations/0002_observation_source.sql) adds the nullable source field without assigning a source to existing rows. [The upgrade test](../crates/sandbox-store/tests/upgrade.rs) applies it over populated migration-0001 data and verifies that existing state is preserved. This field describes evidence origin; it does not make a simulated running sandbox executable.

## Running the controller

The binary connects to PostgreSQL, applies migrations, requires mTLS credentials and explicit host/image configuration, then polls every 500 ms. It handles shutdown without erasing pending intent. `--once` performs at most one tick for controlled diagnostics.

```sh
cargo run -p sandbox-controller -- --help
```

Required settings are `DATABASE_URL` (or `--database-url`), `--endpoint` using HTTPS, `--host-id`, `--host-epoch`, `--ca-cert`, `--client-cert`, `--client-key`, and one or more `--image-digest` values. The configured host must already exist at that epoch, and its server certificate must match the derived host DNS identity. The fake also needs its matching epoch and controller certificate fingerprint; see [fake-host setup](supervisor-protocol.md#run-the-fake).

The image allowlist is static process configuration and checked at dispatch. Updating it currently requires restarting the controller. The API independently requires an explicit [admission allowlist](api-contract.md#implemented-image-admission). Operators must keep API, controller, and supervisor policy aligned; disagreement rejects new admission or later dispatch. Removing an image does not invalidate an existing retry handle or stop an already running allocation. Image byte verification, manifest pinning, and host compatibility checks remain necessary work.

Public state remains a timestamped last observation, not a live VM-presence guarantee. The maintenance loop below renews confirmed allocations and reconciles same-epoch watchdog release. Real VM watchdog enforcement and host-restart fencing remain unverified.

## Evidence and remaining work

[Controller integration tests](../crates/sandbox-controller/tests/create.rs) exercise:

- Authenticated HTTP admission → PostgreSQL intent → real gRPC/mTLS → confirmed simulated state visible through the HTTP router.
- Multiple controllers competing for one create, with exactly one applied fake start.
- Lost acknowledgements and crash boundaries before and after intent, with no duplicate starts or premature capacity release.
- Mandatory simulation opt-in; image/credential rejection; reauthorization after reservation.
- Ownership mismatches, stale claims, expired readiness, implausible observation times, and claim expiry during lock waits.
- Confirmed release versus absent evidence and the rollback of incomplete terminal writes.

[Destroy integration tests](../crates/sandbox-controller/tests/destroy.rs) additionally cover concurrent idempotent admission, tenant isolation, live-transition conflicts, unknown-create handoff, delayed starts after fencing, lost stop/fence replies, unconfirmed release, claim expiry during completion, credential revocation after admission, and permanent tombstones.

The [standalone HTTPS API and offline project provisioning](api-server.md) are exercised through real TCP/TLS tests; the controller remains a separate process. These tests close the create control-plane loop but do not satisfy the real execution or isolation gates in [Phase 1](roadmap.md#scope-discipline-for-phase-1). Remaining work includes authenticated host registration, old-epoch database recovery, file transfer, production images, and the full supported-host/adversarial failure gates.

## Real Linux lifecycle integration

The [real supervisor](real-supervisor.md) now implements the same lifecycle RPCs with `simulated=false`. Its controlled PostgreSQL/HTTP-router test exercises authenticated admission, real VM readiness, scheduled renewal and verified destroy/release through this controller. Command execution is integrated below; file transfer and automatic recovery of database allocations from an earlier host epoch remain unfinished. See the real supervisor document for host capacity, restart and evidence limits.

## Command admission and dispatch ownership

The [execution store](../crates/sandbox-store/src/execute.rs), [public execute handler](../crates/sandbox-api/src/execute.rs), controller and supervisor now connect `POST /v1/sandboxes/{id}/execute` to the guest runner. The controller rotates create, destroy and execute work while maintaining allocation leases first. The [real supervisor](real-supervisor.md#command-dispatch-and-reconciliation) owns command dispatch; the fake reports explicit simulation. [Output retrieval](output-storage.md) and [streaming](api-contract.md#implemented-output-streams) are implemented; [Command cancellation](command-cancellation.md) is implemented; file transfer remains unfinished.

[Command input](../crates/sandbox-protocol/src/command.rs) normalizes an argv array, nonsecret environment (default empty), absolute working directory (default `/`), required absolute `deadline_unix_ms`, and combined stdout/stderr `output_limit` (default 1 MiB, maximum 10 MiB). It inherits the guest runner's argument, environment and encoded-size bounds. New deadlines must be in the next six hours and cannot exceed sandbox expiry. These are the input names for the public execute route. Arguments and environment are persisted as operation payload, never copied into dispatch receipts or `Debug` output; this is not a secret transport.

Admission requires a running, leased allocation at the host's current epoch and a live project credential. It serializes on the project and sandbox, uses the project-wide idempotency constraint, and returns existing handles before applying deadline or runtime-state policy. Changed normalized input under the same key conflicts. Defaults and environment key order normalize identically. Another project's sandbox is absent; a destroyed target rejects new work. One unfinished command occupies the sandbox slot, including `unknown` outcomes. Execute does not take `active_transition_operation_id`, so destroy remains admissible while a command is running or uncertain.

[Migration 0005](../migrations/0005_execution_ownership.sql) pins `execution_allocation_id` through a same-sandbox foreign key and enforces one active pinned command per sandbox. The pin survives destroy clearing `current_allocation_id`. Existing generic execute rows keep a null pin and cannot be dispatched; the migration never guesses an allocation or changes their outcome. Legacy unfinished rows also block new admission until reconciled.

Admission also checks retained capacity under the same project/sandbox locks. All admitted commands pinned to this allocation count toward the protocol's 32-command limit; the sum of their full output limits plus the new request must fit 64 MiB. Completed, failed-before-start, expired and compacted commands stay counted. Payload compaction supplies the original limit through its validated command descriptor. Invalid history fails closed. Identical retries resolve before this check; rejected new requests consume no operation or key. This conservative reservation can exceed the host's actual retained bytes but never treats absence of output as proof of reclamation.

[Migration 0011](../migrations/0011_execution_capacity.sql) adds an allocation/history index without changing existing rows. The query reads at most 33 records, does not lock operation rows while holding project locks, and therefore preserves the reconciliation lock order. Compaction replaces payload and descriptor atomically, so admission sees either representation with the same budget. New admission serializes with other admissions and dispatch through project/sandbox locks. First-dispatch preparation applies the same bound to previously accepted work; dispatched/unknown commands bypass admission policy and continue inspection. This protects the supported API/controller path; independently injected host RPCs or corrupted database/host accounting still require operator reconciliation.

First-dispatch preparation locks operation → project → sandbox → host, rechecks claim ownership, authority, target identity, deadlines and runtime lease, and commits one intent with the guest command digest before returning a dispatch action. The expected host/epoch must match before that first action. Undispatched commands whose authority or target changed fail without releasing VM capacity. Once intent exists, later claims inspect or request cancellation under the original allocation, generation, epoch and digest, even after revocation, destroy or a host restart. Missing or changed evidence fails closed. A database dispatch action is not proof that a guest received or ran the command.

[PostgreSQL execution tests](../crates/sandbox-store/tests/execute.rs) cover same-key and distinct-key concurrency, direct uniqueness and foreign-key enforcement, tenant scope, normalized retries after actual deadline expiry, invalid input, authority revocation, destroy during uncertain work, host and claim fences, lock-wait expiry, retained ownership after release, and payload redaction. [Upgrade tests](../crates/sandbox-store/tests/upgrade.rs) preserve old rows and verify that missing allocation ownership cannot dispatch. These tests seed database allocations; they make no VM execution or isolation claim.

[Capacity tests](../crates/sandbox-store/tests/support/execution_capacity.rs) cover exact slot/byte boundaries, concurrent admission at the last slot, retained reservations after compaction, malformed history, tenant scope, retry identity and migration over populated history. Legacy queued work is rejected before dispatch when full; unknown dispatched work remains inspectable even over budget. The [controller tests](../crates/sandbox-controller/tests/execute.rs) run 32 commands through HTTP and real mTLS to the fake, reject the next request without dispatch, then complete destroy. They also prove an output-budget rejection leaves its key reusable by a smaller request. These checks establish admission/reconciliation behavior, not real-VM isolation.


`ExecuteCommand` carries the pinned ownership and normalized guest command; `InspectCommand` carries the same ownership and digest. Lost RPC replies select inspection on the next claim. A host with no retained dispatch for this operation can commit a durable no-start fence and return `not_started`; a late execute then cannot start. Missing guest receipts after a host dispatch remain unknown, including when destroy has removed the guest. A replacement host epoch cannot answer old ownership and does not authorize replay.

Result recording validates the current claim, full ownership tuple, digest, observation time, guest context, deadline and output budget. A known boot identity cannot change across observations. Exit zero succeeds; nonzero exit or signal fails with `command_failed`; timeout fails with `deadline_exceeded`. Active receipts remain running and missing/unknown receipts remain unknown. [Public command cancellation](command-cancellation.md) now drives guest interruption under the target execution claim; acceptance alone never changes a target outcome. A no-start fence fails with `command_not_started`. Completion stores bounded output statistics and exit metadata with `guest_reported=true`; guest output bytes never traverse the controller and guest cleanup never releases an allocation. History retains the initial intent and latest command evidence, at most two entries. The guest is root-controlled, so command reports are workload data, not trusted isolation or external-effect evidence.

[Public execution tests](../crates/sandbox-controller/tests/execute.rs) exercise HTTP admission and polling through real mTLS to the fake, including duplicate and changed retries, lost acknowledgements, no-start fencing, nonzero results, destroy during active execution, credential revocation, forged or cross-boot evidence, and command history pressure leaving capacity for destroy. Real API/VM evidence is recorded by the [host integration tests](../crates/sandbox-supervisor/tests/host.rs).


## Independent output worker

`--archive-output` enables [final-output archival](output-storage.md#supervisor-archival-and-recovery). It runs independently of lifecycle ticks using its own publication claim and mTLS connection, so waiting on storage does not serialize renewal or destruction. The controller persists tickets/plans/references and never receives guest output bytes or object credentials. Execution completion reports pending output first; archival can publish it later, including after destroy if the objects already exist. The API handles retained-byte retrieval independently of the controller; live SSE remains separate work.
