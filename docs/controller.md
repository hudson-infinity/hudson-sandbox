# Create and destroy controller

Status: a single-host create/destroy controller is implemented and tested with PostgreSQL, the authenticated HTTP router, and real loopback gRPC/mTLS to the fake supervisor. It does not run customer code. Execute, files, outputs, allocation lease renewal, and expired-allocation reconciliation still need implementation.

## One operation through the loop

[The controller](../crates/sandbox-controller/src/lib.rs) verifies the configured supervisor's certificate through the [shared transport](supervisor-protocol.md), then checks its reported host ID, epoch, and evidence mode. It refreshes the heartbeat of that existing database host. This cannot create a host, change its epoch or capacity, or promote a draining host to ready. Host rows, epochs, capacity, endpoints, certificates, and allowed image digests are operator-provisioned for this initial integration; authenticated registration and automatic epoch issuance remain unfinished.

Each tick prioritizes one eligible destroy, otherwise one create, with a 30-second controller lease. Create reserves capacity and examines persisted dispatch history. [Dispatch storage](../crates/sandbox-store/src/dispatch.rs) locks the operation, project, sandbox, and host in the same order as reservation. It constructs ownership from persisted rows, not caller-supplied copies of project or sandbox fields.

If the operation is still provably undispatched, storage rechecks the project credential, deadlines, desired state, host freshness, and the configured image allowlist. It commits the allocation's initial 30-second execution lease, increments the dispatch attempt count, and records a create intent before returning the request to send. A controller crash cannot erase that intent.

A previous intent or unknown status selects inspection of the same allocation. It never selects another create request. A reservation without a dispatch intent can continue under a new claim because the database proves no controller following this protocol could have sent its create yet.

The controller currently works with one explicitly configured host. It does not route work across multiple hosts. Capacity shortages, draining/unavailable hosts, or reservations belonging to another host are deferred for five seconds. Work remains durable when the process exits or loses its claim.

## Completion and uncertainty

A successful RPC is only a candidate observation. Storage verifies the complete host/project/sandbox/allocation/operation tuple, generation, supervisor epoch, current claim revision and deadline, original create operation ID, and a supported observed state. The observation timestamp must be within ten seconds of PostgreSQL time. Readiness must still be inside the allocation's execution lease. Final writes check the claim again after lock waits; expiry rolls back all state changes.

A matching readiness observation atomically marks the allocation and sandbox running, completes the create operation, clears lifecycle ownership, and appends the bounded receipt. A matching released observation instead fails the create and records release evidence, retires the sandbox, and frees its reservation. Deadline expiry or missing evidence alone cannot release capacity.

A lost response, absent allocation, or rejected observation records `unknown`, retains capacity and dispatch history, and schedules reconciliation after one second. Repeated inspection does not increment create attempts or append unbounded copies of the same uncertainty. `unknown` has no completion timestamp. A later matching observation can complete it without replaying create.

The crash boundary after intent but before sending remains deliberately unresolved when the supervisor reports absence. The create loop preserves that uncertainty; the destroy path below can take cleanup ownership and resolve it using stop/fencing evidence. It cannot infer that an absent in-memory record proves there was no external action in a different incarnation.

A revoked credential or disallowed image can fail an operation before dispatch. Service-owned rejection frees a reservation only when the locked records prove zero dispatch attempts, no execution lease, and an undispatched phase. It records that proof atomically with failure. Unknown or previously dispatched work cannot use this shortcut.

## Destroy admission and cleanup

`POST /v1/sandboxes/{sandbox_id}/destroy` requires project authentication, an idempotency key, and a JSON object. The only optional input is `correlation_id` (at most 200 bytes); caller-supplied lifecycle or ownership fields are rejected. Admission resolves the key and digest before lifecycle state. Identical retries return the same operation; a changed request conflicts. A new key for an already destroyed sandbox returns a completed no-op without changing its original destruction timestamp.

[Destroy storage](../crates/sandbox-store/src/destroy.rs) serializes on the active operation, project, and sandbox. A live conflicting transition returns `409` with its authorized `operation_id`; the rejected key is not consumed. An unknown create can hand cleanup ownership to destroy: its claim revision increases, its status stays unknown with phase `cleanup_owned_by_destroy`, and create claimers skip it. No in-flight create response can overwrite the new lifecycle owner.

The destroy controller persists stop intent before RPC. After a lost response it first inspects the same allocation. Confirmed release completes directly; ready or plain absent state permits another idempotent stop/fence, with intent recorded before that attempt. At most 100 stop attempts are issued for one operation; beyond that, inspection continues and unconfirmed capacity stays reserved for operator investigation. Receipt history retains the initial stop intent and final release, without appending unbounded copies of retries.

Completion requires a fresh, tuple-matching `released` or `fenced_absent` observation under the current claim, allocation generation, and supervisor epoch. Plain `absent`, an expired lease, and a stop request are insufficient. The explicit `fenced_absent` state means the supervisor has confirmed that no incarnation exists in that epoch and blocked future starts of that allocation; the fake models that fence in memory and reports simulated evidence. A restarted supervisor must have a new epoch and cannot reuse empty memory as old-epoch release proof.

Allocation release, its evidence, the sandbox tombstone, and successful destroy completion commit atomically. If destroy superseded an unknown create, that create ends as failed with `create_outcome_unknown=true` and a link to destroy; this does not invent whether it previously ran. A provably never-allocated create (generation and attempt count both zero) can be retired directly under database locks. Already admitted cleanup continues under service authority after credential revocation.

The fake's lost-stop-reply hook exercises both release of a recorded incarnation and fencing an absent one. The real supervisor must provide durable fencing and actual resource cleanup before it can return either completion state. Fake tests prove neither of those hardware guarantees.

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

A successful create does not renew the allocation's 30-second lease. The fake's independent watchdog expires it; this controller does not yet reconcile leases of already completed creates. Public state is a timestamped last observation, not a live VM-presence guarantee. Add renewal and cleanup before treating the binary as a usable long-running runtime.

## Evidence and remaining work

[Controller integration tests](../crates/sandbox-controller/tests/create.rs) exercise:

- Authenticated HTTP admission → PostgreSQL intent → real gRPC/mTLS → confirmed simulated state visible through the HTTP router.
- Multiple controllers competing for one create, with exactly one applied fake start.
- Lost acknowledgements and crash boundaries before and after intent, with no duplicate starts or premature capacity release.
- Mandatory simulation opt-in; image/credential rejection; reauthorization after reservation.
- Ownership mismatches, stale claims, expired readiness, implausible observation times, and claim expiry during lock waits.
- Confirmed release versus absent evidence and the rollback of incomplete terminal writes.

[Destroy integration tests](../crates/sandbox-controller/tests/destroy.rs) additionally cover concurrent idempotent admission, tenant isolation, live-transition conflicts, unknown-create handoff, delayed starts after fencing, lost stop/fence replies, unconfirmed release, claim expiry during completion, credential revocation after admission, and permanent tombstones.

The HTTP router is exercised in-process; the standalone HTTPS API server and local bootstrap tooling remain unfinished. These tests close the create control-plane loop but do not satisfy the real execution or isolation gates in [Phase 1](roadmap.md#scope-discipline-for-phase-1). Remaining work includes authenticated host registration, lease renewal/recovery, list and execute endpoints, files/output, image verification, and Linux/KVM execution and failure injection.
