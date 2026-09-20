# IDs and resource records

Date: 2026-09-19

Status: initial design for implementation. Examples describe the intended contract; no API, schema migration, or ID library is implemented yet. This extends [the implementation design](implementation.md), which defines the stack and lifecycle. The selected six-table schema is in [data models](data-models.md).

## 1. The rule users should remember

**One sandbox keeps one identity through create, execute, pause, and resume.** A snapshot identifies saved state. An allocation identifies one attempt to run that sandbox as a VM. An operation identifies one requested action.

Destroy permanently retires the sandbox ID. Creating another sandbox always creates a new ID, even when its display name or image is the same. Copying saved state into a different sandbox would be a future explicit fork operation, not ordinary resume.

Hudson is an ordinary client. None of these identities depend on a Hudson run, a Temporal workflow, a Kubernetes pod, an IP address, or a guest process ID.

## 2. ID format

Use a short resource prefix followed by a canonical lowercase, hyphenated UUIDv7:

```text
sbx_01996110-7c00-7000-8000-000000000001
```

This is an illustrative valid-format ID. The prefix helps people distinguish resources in API responses and logs. Generate the UUID once in the trusted service using an established Rust UUID implementation. Store its underlying value in PostgreSQL's `uuid` type; attach or remove the fixed prefix at the API boundary. There is no separate public-to-internal ID mapping table.

UUIDv7 carries a millisecond timestamp and supports time-oriented index locality. It does not establish strict ordering across machines, prove freshness, or act as a secret. Keep explicit timestamps, revisions, and generations for those purposes. Sources: [RFC 9562, UUIDv7](https://www.rfc-editor.org/rfc/rfc9562.html#section-5.7), [PostgreSQL UUID type](https://www.postgresql.org/docs/current/datatype-uuid.html).

The parser accepts the exact resource prefix and canonical UUIDv7 format; reject wrong types, truncated IDs, malformed values, and alternate spellings. Keep database uniqueness constraints and handle the unlikely generation collision before publishing an ID. Do not hand-roll the generator, truncate random bits, or make IDs out of database row counts.

IDs are opaque identifiers, never bearer credentials. They may reveal approximate generation time. Authorization applies to every lookup regardless of whether the identifier is known, guessed, or supplied in a storage path.

## 3. Resource inventory

The notation `<uuidv7>` below represents the full canonical UUID, not a literal API value.

| Record | API format | Lifetime and purpose |
| --- | --- | --- |
| Project | `prj_<uuidv7>` | Stable tenant boundary for ownership, quotas, and authentication |
| Sandbox | `sbx_<uuidv7>` | Stable environment identity across pause/resume; never reused after destroy |
| Operation | `op_<uuidv7>` | One create, execute, pause, resume, destroy, or other mutating request |
| Snapshot | `snp_<uuidv7>` | One immutable, completed save of memory, disk, and VM state; reserved while preparing |
| Allocation | `alc_<uuidv7>` | One reserved VM incarnation on one host; new on initial start, resume, or replacement |
| Host | `hst_<uuidv7>` | Internal registered machine identity; replacement/reprovisioning gets a new ID |

Most clients need only project, sandbox, operation, and snapshot IDs. Images are addressed by authorized immutable digests; outputs are retrieved through their producing operation. Allocation and host identities are internal/admin details. Host addresses are not exposed in ordinary sandbox responses.

Avoid extra resource types initially:

- An executed command is an operation of kind `execute`; do not add a second command/job ID for the same action.
- A controller attempt is `(operation_id, attempt_number)`, with a positive incrementing number. Its receipts live on the operation; it does not need a separate table or globally unique ID.
- A sandbox's generation is an increasing integer, not another UUID. It orders allocation replacements and rejects stale commands.
- An operation's claim revision is a separate increasing integer. It orders controller ownership, including when the allocation has not changed.
- Display names and labels are optional mutable metadata. Names may repeat and are never API lookup keys or authorization inputs.

Provision a local project identity in the sandbox service. Authenticate callers with opaque project API tokens over HTTPS, using the hash and expiry/revocation metadata on that project. User login remains in the calling harness. See [authentication](artitecture.md#simple-project-token-authentication). An optional external reference can associate it with a Hudson workspace or organization without coupling the ID format or requiring the harness to exist. Deleting a project revokes its callers and never permits reuse of its ID.

Client authentication is mandatory in every environment, including localhost and self-hosted installations. Local setup provisions an ordinary project and token. Missing, invalid, expired, or revoked credentials are rejected, with no implicit project identity or authentication-disable option. Token validation uses this installation's stored hashes and never requires calling Hudson.

## 4. What changes during a session

The short names in this table are explanatory aliases, not API IDs.

| Event | Sandbox | Operation | Allocation/generation | Snapshot |
| --- | --- | --- | --- | --- |
| Create | S, newly assigned | Create operation | A, generation 1 | None; boots immutable image I |
| Execute a script | S | Execute operation E | A, generation 1 | None |
| Retry the same HTTP execute request | S | The same E | The same A | None |
| Pause | S | Pause operation P | A is frozen, then released | Snapshot Q is published |
| Resume | S | Resume operation R | B, generation 2 | Restore Q |
| Continue the previously frozen script | S | The same E | B, generation 2 | Q remains immutable |
| Execute a new script | S | A new execute operation | B, generation 2 | No new snapshot automatically |
| Destroy | S, tombstoned | Destroy operation | B is released | Resume disabled; retention policy applies |

An execute operation that spans pause/resume keeps one ID and its original absolute deadline. While paused, retain nonterminal status `running` with progress phase `suspended`. Reattaching it on a new allocation is not a new execution request. Store dispatch/reconnect receipts per allocation, preserving the earlier history rather than rewriting it. If the deadline expires while paused, enforce cancellation before allowing that restored process to continue; restoring memory must not reset its timeout.

A new resume allocation receives a new ID and generation before it can run. If an allocation attempt fails, its generation is not reused. Retrying transport to the same allocation retains its ID; allocating a replacement requires the old incarnation to be stopped or fenced first.

## 5. Client retries and operation IDs

The service generates the operation ID at admission. The client supplies an `Idempotency-Key` to identify a logical mutation before the first response exists.

Require this header for create, execute, pause, resume, destroy, cancellation requests, and committed file mutations. It is an opaque case-sensitive value of 16–128 ASCII characters from letters, digits, `.`, `_`, and `-`. The SDK should generate a fresh random UUIDv4 string per logical mutation, persist it across retries, and keep it distinct from the server's UUIDv7 operation ID. Keys carry no secrets or user text.

Deduplicate on `(project_id, idempotency_key)`, across mutating routes. A key represents exactly one logical request in that project. Store a versioned request digest covering method, canonical resource target, and the validated payload. Object field ordering is normalized; array order and command strings remain significant. Resolve defaults consistently, record effective limits/image digests separately, and preserve the normalization version so an API upgrade cannot reinterpret an old retry.

Admission is transactional:

1. Authenticate and resolve the project; check current access to the target or existing receipt.
2. Look up the operation by its project and retry key. For a new request, the operation insert below enforces the unique key; a concurrent loser rolls back its tentative changes and reads the winning operation.
3. For an existing record, compare its stored request digest before evaluating today's lifecycle state. Identical content returns the existing operation, even if the sandbox has since paused or finished. Different content returns a conflict.
4. For a new record, validate the lifecycle transition and quotas, reserve identities, and insert the operation plus resource changes in the same transaction.
5. Return the operation handle. The controller claims it independently of the HTTP connection.

An admitted asynchronous mutation returns `202 Accepted` with an operation ID and status URL. An identical retry returns the same operation ID and current status, never another dispatch. A digest conflict returns `409 Conflict`; malformed IDs/keys return `400 Bad Request`. Authorization is checked again on receipt retrieval and before dispatch; a retry does not revive revoked authority.

Result bodies may expire, but compact operation tombstones retain the request digest and original operation ID for the project's lifetime. An identical key whose response has expired returns `410 Gone` and the original operation identity, with no execution. Reject key reuse with changed content even after result expiry. On project deletion, revoke access before purging its records; the project ID cannot be re-created. This deliberately trades small metadata retention for predictable retry behavior and must be included in storage planning.

New keys mean new requests. Executing the same script with a new key runs it again. Lifecycle operations may converge without work: pause on an already-paused sandbox, resume on an already-running sandbox, or destroy on an already-destroyed sandbox returns a completed no-op operation for that new key. No new VM or snapshot is created for those no-ops. Conflicting transitions in progress return `409` with the existing operation reference for an authorized caller; the rejected key is not admitted. Exact duplicates are resolved first.

Neither an operation ID nor an idempotency key guarantees exactly-once external effects. If the guest acted but a receipt was lost, record `unknown` and reconcile before deciding whether anything can safely repeat.

## 6. API example

The endpoints and field names below are the proposed first API shape. Full IDs are shown so the examples can be checked for format consistency. A valid project API token resolves the project. The header below is a placeholder, never a usable credential.

```http
POST /v1/sandboxes
Authorization: Bearer <project-api-token>
Idempotency-Key: 80d6bfaa-7245-493b-8d08-2cdb2de9885c
Content-Type: application/json

{
  "image_digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "name": "spreadsheet-analysis",
  "resources": { "vcpu": 2, "memory_mib": 1024, "disk_mib": 4096 },
  "correlation_id": "customer-task-42"
}
```

```json
{
  "sandbox_id": "sbx_01996110-7c00-7000-8000-000000000001",
  "operation_id": "op_01996110-7c00-7000-8000-000000000002",
  "status": "queued",
  "status_url": "/v1/operations/op_01996110-7c00-7000-8000-000000000002"
}
```

Wait for create success, then call execute. An execute request returns an operation ID used for results, output, and cancellation. Proposed routes are:

```text
GET  /v1/sandboxes/{sandbox_id}
POST /v1/sandboxes/{sandbox_id}/execute
POST /v1/sandboxes/{sandbox_id}/pause
POST /v1/sandboxes/{sandbox_id}/resume
POST /v1/sandboxes/{sandbox_id}/destroy
GET  /v1/operations/{operation_id}
POST /v1/operations/{operation_id}/cancel
GET  /v1/snapshots/{snapshot_id}
GET  /v1/operations/{operation_id}/outputs/{output_name}
GET  /v1/operations/{operation_id}/stream
```

The read-only stream authenticates with the same project token from a backend client. It checks current ownership/allocation and forwards output through the API streaming endpoint, bypassing the controller for bytes. Reconnect uses a cursor on the original operation; it does not create an operation or dispatch another command. Reauthorize on connect, at expiry, and at most every 30 seconds; close on failed checks. Stream tokens for direct browser access are deferred.

Cancellation is itself an idempotent operation referencing the target operation; a requested cancel does not change the target to cancelled until confirmed. Pause's completed result includes the published snapshot ID. Ordinary resume resolves the sandbox's current pause snapshot on admission and pins that reference in the operation. It does not accept an arbitrary old snapshot to silently rewind history. A future explicit recovery/fork API must address repeated external effects separately.

## 7. Proposed PostgreSQL records and invariants

The following describes the logical schema, not executable migration SQL. All tenant-owned references include project ownership. Use composite foreign keys such as `(project_id, sandbox_id)` and matching unique constraints, even though resource UUIDs are globally generated. This prevents accidentally linking one project's snapshot or operation to another's sandbox.

The six tables are `projects`, `sandboxes`, `operations`, `hosts`, `allocations`, and `snapshots`. See [data models](data-models.md) for fields and relationships. Retry keys and attempt receipts live on operations; image digests live on sandboxes and create operations; output references live on their producing operations.

Create resolves an operator-allowed, project-authorized image digest to its verified immutable manifest and pins compatibility data in the operation. There is no image catalog table initially. Host identity is internal and not tenant-owned. A database-issued supervisor epoch increases at each supervisor registration after restart, forcing reconciliation before it can renew old ownership. This epoch is separate from the machine's OS boot ID and is not evidence by itself that an old VM has stopped.

Enforce these invariants:

- A sandbox belongs to one project for its lifetime. All operations, snapshots, and output references retain that ownership.
- At most one unreleased allocation exists for a sandbox. Unreachable is not released. Use a database constraint plus transactional reservations, with positive fencing before a replacement can execute.
- `(sandbox_id, generation)` is unique. Allocation generation and operation claim revision never decrease or reset after restore.
- Serialize lifecycle transitions by locking/versioning the sandbox record. A command can remain suspended while a separate pause/resume transition runs; do not mistake a long-running execute operation for the lifecycle lock.
- A snapshot's source allocation and pause operation must belong to the same sandbox. One pause operation reserves one snapshot ID across retries; upload attempts are separate from snapshot identity.
- Publish the verified manifest and update the sandbox's pause reference transactionally. Publication alone does not mean compute is released: `pausing` becomes `paused` only after release evidence is recorded.
- Snapshot contents are immutable once published. Deletion state and retention may change; captured memory/disk and their digests may not.
- Controller attempt outcomes cannot overwrite a terminal receipt using an older claim revision. An unknown outcome can become confirmed only through recorded reconciliation evidence, not blind re-execution.
- IDs remain unique, but their timestamp order is not a synchronization mechanism. Use explicit state revisions and transactional comparisons.

## 8. Object-storage layout

Store raw snapshot components under one snapshot. Outputs, logs, and file exports are referenced by their producing operation; they have no separate public artifact ID initially. The full prefixed IDs below are represented by placeholders for readability.

```text
projects/{project_id}/sandboxes/{sandbox_id}/
  snapshots/{snapshot_id}/uploads/{upload_attempt_number}/
    memory.bin
    disk.img
    vm-state.bin
    manifest.json
  operations/{operation_id}/outputs/{output_name}/{upload_attempt_number}/content
```

Upload attempts get distinct paths. An interrupted or stale uploader must not overwrite the objects selected by a completed publication. Use immutable/conditional writes or pinned object versions, and put the exact keys, sizes, digests, source allocation/generation, format version, and compatibility data in the manifest. PostgreSQL publishes one verified manifest; clients never infer readiness by listing a prefix.

Unpublished upload attempts can be garbage-collected after their leases and retention expire. Published components remain referenced until deletion policy permits removal. The controller must distinguish abandoned uploads from an in-progress publication before deleting bytes.

The prefix is organization, not security. Resolve operation output names and snapshot IDs through authorized metadata, keep buckets private, and issue only short-lived scoped transfer access where needed. Never accept caller-supplied bucket names or arbitrary object keys as trusted references.

## 9. Errors, retention, and implementation checks

Return `404` for inaccessible tenant resources without revealing whether another project owns them. An authorized caller may inspect a destroyed sandbox tombstone, but new execute/resume operations return `410 Gone`. Expired snapshots cannot be resumed. Destroy permanently prevents resume even when policy retains its snapshot bytes for a bounded period.

Keep sandbox, operation, and snapshot metadata long enough to explain ownership, cleanup, and uncertain outcomes. Payload/artifact retention can be shorter than ID/deduplication tombstone retention. Deleting a resource must not turn its ID or retry keys into reusable names. Revoked callers do not gain receipt access just because they know a retry key.

Before implementing the schema and API, turn these cases into contract/integration tests:

1. Every resource parser round-trips a valid ID and rejects another resource's prefix, wrong UUID version, and truncated input.
2. Concurrent identical create requests produce one sandbox and operation; changed payloads under the same key conflict.
3. Execute retries reuse one operation before, during, and after pause/resume; a genuinely new key creates new execution.
4. Pause retries retain one snapshot ID while incomplete uploads cannot become current pause state.
5. Resume keeps the sandbox ID but advances allocation identity/generation; transport retry does not allocate another VM.
6. Stale supervisor epochs, generations, and controller claim revisions cannot mutate current state.
7. Database references and API reads prevent cross-project access, including snapshot and operation output paths.
8. Destroyed IDs and expired-response retry keys never produce a new execution.
9. An execute operation suspended in a snapshot reconnects under its original ID and deadline.
10. Failed/unknown external effects are not automatically repeated because a controller attempt changed.
11. Invalid, expired, or revoked tokens cannot admit requests, read another project, or retain streaming access beyond the 30-second recheck bound; internal host endpoints reject project tokens.
12. Concurrent pause admissions respect disk/upload reservations; expired leases do not free staging bytes that still exist.
13. Restore runs the guest agent while customer processes remain frozen until policy and deadline checks succeed.
14. Local, self-hosted, and Hudson clients all require valid project tokens for API requests and output streams. Authentication storage failures deny access, and missing credentials cannot produce a development or service identity.

The remaining schema work is executable migrations, concrete index/constraint definitions, pagination, retention defaults, and the internal transport implementation. Project-token authentication is selected; its storage and revocation contract are in [data models](data-models.md). This document selects resource identity and retry semantics without adding a Temporal or harness dependency.
