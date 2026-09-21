# Operation response retention

Status: terminal response expiry and opt-in policy assignment are implemented. Physical payload/history compaction and host/guest journal reclamation remain unfinished. Response expiry changes access to the result body; it does not change execution outcome, retry identity, output-retirement evidence or resource ownership.

## Operator policy

The independent [cleanup worker](output-storage.md#cleanup-worker) accepts an optional installation-wide policy:

```sh
cargo run -p sandbox-cleanup -- \
  --output-config /absolute/path/cleanup-storage.json \
  --response-retention-seconds 604800
```

This example retains terminal responses for seven days after completion. There is no default policy: omitting the option leaves unassigned deadlines unchanged. The allowed range is one second through 365 days. Configure all workers consistently. Existing deadlines continue to apply if the option is later removed, and changing it only affects operations without a deadline.

Each tick assigns deadlines to at most 100 operations that have a confirmed `succeeded`, `failed` or `cancelled` status, a completion timestamp, and no `response_expires_at`. The deadline is `completed_at + policy`, not the time the worker discovers the row. **Enabling the option also applies to existing completed operations and can expire old responses immediately.** Queued, running and unknown work retain their unresolved evidence and are excluded from assignment.

The [store transaction](../crates/sandbox-store/src/retention.rs) uses row locks with `SKIP LOCKED` for concurrent workers. It never changes an assigned deadline, execution state, payload, receipt or retry key. Policy assignment has the worker's five-second database bound. It can progress without storage requests when no output attempt needs retirement. `--once` performs one bounded tick, including policy assignment if configured.

[Migration 0009](../migrations/0009_response_retention.sql) adds an index for pending policy assignment. Upgrading alone does not expire responses or alter existing operation records.

## Client behavior

For a terminal operation at or after its assigned deadline:

- `GET /v1/operations/{id}` returns `410 response_expired` with the original `operation_id` and no result/error body.
- An identical create, execute or destroy retry returns the same `410` and original identity. A changed request or incompatible stored digest version still returns `409`; expiry does not free the key or cause another dispatch.
- Operation lists retain identity, kind, status, phase and timestamps, add `response_expired: true`, and omit result/error bodies. Expired entries remain in pagination. The original outcome is preserved; `expired` is not a new execution status.
- Output reads and streams honor their existing response-retention bound and return `output_expired`. Retention does not extend an output ticket or grant new storage authority.

Current authentication and tenant ownership still apply. Missing and cross-project IDs return the same `404`; revoked credentials do not gain access to expired identities. All responses remain `Cache-Control: no-store`. Before expiry, existing response shapes are preserved and the optional `response_expired` field is absent.

Individual and list queries suppress expired result/error bodies using database time. The API checks expiry again before forming the body, retaining the database's expired decision even if its own clock is behind. Retry expiry is evaluated at database retry resolution, after digest comparison and before considering today's sandbox lifecycle. Active and unknown operations remain inspectable even if an out-of-band edit gave them an elapsed response deadline; ordinary policy assignment never does that.

## Retained evidence and remaining work

Expiry is not physical deletion. The database still retains payloads, result/error fields, dispatch and completion receipts, ownership, request digest/version and idempotency keys. Output plans and retirement receipts also remain available for reconciliation. This preserves the current recovery contract while a separate compaction protocol is developed.

Already-issued output plans keep their original immutable retention/grace timestamps. A newly assigned shorter response deadline stops public reads and publication but does not rewrite those plans or authorize early storage deletion. [Output retirement](output-storage.md#storage-retirement) completes independently at the original attempt's eligibility time.

Safe compaction must retain operation identity, project ownership, retry digest/version and outcome for the project's lifetime, preserve unresolved evidence, and keep completed cleanup retries reconcilable. Host and guest journals still have their documented fixed limits; this worker does not reclaim those reservations or make previously executed work runnable again.

## Evidence

[PostgreSQL tests](../crates/sandbox-store/tests/retention.rs) cover bounded batches, concurrent assignment and locked rows, completion-based deadlines, unchanged evidence, unresolved-work exclusions, immutable assigned deadlines, opt-in worker behavior and an upgrade over populated v8 data. [Router tests](../crates/sandbox-api/tests/retention.rs) exercise terminal reads, all three implemented retry routes, conflicts, tenant boundaries, revocation, pagination, unresolved operations and expiry during a database lock wait. The [executable test](../crates/sandbox-cleanup/tests/cli.rs) covers default-disabled behavior, invalid policies and actual assignment through `--once`. These are database/API tests with synthetic lifecycle evidence, not VM isolation tests.
