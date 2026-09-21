# Data models, IDs, and storage

Status: partially implemented; the initial PostgreSQL schema and storage operations have tests. This document owns fields, relationships, identity formats, database constraints, and object-storage references. [Auth design](auth-design.md) owns credential/session enforcement; [API contract](api-contract.md) owns retry behavior; [lifecycle](lifecycle.md) owns transitions and completion evidence.

Implementation evidence: [the initial migration](../migrations/0001_initial.sql) creates projects, hosts, sandboxes, operations, and allocations. [Schema tests](../crates/sandbox-store/tests/schema.rs), [claims](../crates/sandbox-store/tests/claims.rs), and [single-host reservation tests](../crates/sandbox-store/tests/placement.rs) exercise PostgreSQL constraints and concurrency. The [source-field upgrade](../migrations/0002_observation_source.sql) and [upgrade test](../crates/sandbox-store/tests/upgrade.rs) preserve observation provenance; [create completion](controller.md) records evidence transactionally. Snapshot and UI security schemas remain planned; database reservations are not proof that a VM exists or is isolated.

There are **six sandbox resource tables** plus **two supporting UI security tables**, not a user/role directory. The schemas below define the target design; the migration implements the current subset.

## ID format and identity

**One sandbox keeps one identity through create, execute, pause, and resume.** A snapshot identifies saved state. An allocation identifies one attempt to run that sandbox as a VM. An operation identifies one requested action.

Destroy permanently retires the sandbox ID. Creating another sandbox always creates a new ID, even when its display name or image is the same. Copying saved state into a different sandbox would be a future explicit fork operation, not ordinary resume.

Hudson is an ordinary client. None of these identities depend on a Hudson run, a Temporal workflow, a Kubernetes pod, an IP address, or a guest process ID.

Use a short resource prefix followed by a canonical lowercase, hyphenated UUIDv7:

```text
sbx_01996110-7c00-7000-8000-000000000001
```

This is an illustrative valid-format ID. The prefix helps people distinguish resources in API responses and logs. Generate the UUID once in the trusted service using an established Rust UUID implementation. Store its underlying value in PostgreSQL's `uuid` type; attach or remove the fixed prefix at the API boundary. There is no separate public-to-internal ID mapping table.

UUIDv7 carries a millisecond timestamp and supports time-oriented index locality. It does not establish strict ordering across machines, prove freshness, or act as a secret. Keep explicit timestamps, revisions, and generations for those purposes. Sources: [RFC 9562, UUIDv7](https://www.rfc-editor.org/rfc/rfc9562.html#section-5.7), [PostgreSQL UUID type](https://www.postgresql.org/docs/current/datatype-uuid.html).

The parser accepts the exact resource prefix and canonical UUIDv7 format; reject wrong types, truncated IDs, malformed values, and alternate spellings. Keep database uniqueness constraints and handle the unlikely generation collision before publishing an ID. Do not hand-roll the generator, truncate random bits, or make IDs out of database row counts.

IDs are opaque identifiers, never bearer credentials. They may reveal approximate generation time. Authorization applies to every lookup regardless of whether the identifier is known, guessed, or supplied in a storage path.

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

Provision project IDs independently of any harness. An optional external reference maps a Hudson workspace or another caller's organization; project deletion revokes access and never permits ID reuse. Local setup still requires authentication, as defined in [auth design](auth-design.md).

## Relationships

```mermaid
erDiagram
    PROJECTS ||--o{ SANDBOXES : owns
    SANDBOXES ||--o{ OPERATIONS : receives
    SANDBOXES ||--o{ ALLOCATIONS : uses_over_time
    HOSTS ||--o{ ALLOCATIONS : hosts
    SANDBOXES ||--o{ SNAPSHOTS : saves
    ALLOCATIONS ||--o{ SNAPSHOTS : captured_from
    OPERATIONS ||--o| SNAPSHOTS : pause_produces
```

The diagram shows historical relationships. A sandbox can have many allocations over its lifetime, but at most one can remain unreleased. A pause operation produces at most one snapshot; a no-op pause produces none.

## The six models

IDs use PostgreSQL `uuid`, with a typed UUIDv7 prefix added in the API. All tables have `created_at` and `updated_at` timestamps. Times use `timestamptz`; counters use `bigint`; bounded structured metadata uses `jsonb`. The fields below describe the logical schema, not executable SQL.

PostgreSQL 16 is the minimum supported version. Migrations are versioned SQL applied by `sqlx migrate`, which is already in the stack; no second migration tool is introduced. Both a fresh install and a supported upgrade path are tested.

### 1. projects — ownership and limits

API ID: `prj_<uuidv7>`.

| Fields | Purpose |
| --- | --- |
| `id`, `name`, `status` | Stable owner identity and lifecycle |
| `limits` | CPU, memory, disk, snapshot storage, sandbox count, and pending-operation quotas. Initial defaults: 25 concurrent sandboxes per project, each bounded by the 4 vCPU / 8 GiB [supported ceiling](compatibility.md#sandbox) |
| `api_tokens` | Small bounded token metadata collection: key ID, SHA-256 hash, creation/expiry/revocation times; at most two active tokens for rotation |
| `external_reference` (optional) | Mapping to a caller's organization or workspace |

Project token metadata is stored on the project; the credential/session validation and rotation rules are owned by [auth design](auth-design.md#project-credentials). The Admin API/UI and local admin tooling manage project tokens; installation Admin credentials are maintained separately in controlled deployment configuration. Display names are not unique lookup keys. A project with no active tokens grants no Project access.

### 2. sandboxes — the persistent environment

API ID: `sbx_<uuidv7>`.

| Fields | Purpose |
| --- | --- |
| `id`, `project_id`, `name`, `labels` | Identity and ownership |
| `image_digest`, `image_compatibility`, `resources` | Verified immutable starting image and requested CPU, RAM, and disk limits |
| `desired_state`, `observed_state`, `observed_at`, `observation_simulated`, `state_revision` | Intent, last confirmed state, observation freshness/source, and concurrent-update protection |
| `generation`, `current_allocation_id` (nullable) | Latest allocation generation and current compute reservation |
| `current_snapshot_id` (nullable) | Published pause snapshot used for ordinary resume |
| `active_transition_operation_id` (nullable) | Serializes create/pause/resume/destroy transitions |
| `expires_at`, `destroyed_at` (nullable) | Sandbox lifetime and permanent destruction tombstone |

The API derives its visible status from the observed state and pending transition. The sandbox keeps its ID across pause/resume. Destroy permanently prevents further execution or resume.

### 3. operations — actions, retries, and results

API ID: `op_<uuidv7>`.

| Fields | Purpose |
| --- | --- |
| `id`, `project_id`, `sandbox_id`, `kind` | One admitted create, execute, pause, resume, destroy, cancel, or file mutation |
| `initiator_kind`, `initiator_key_id`, `initiator_session_id` (nullable where inapplicable) | Server-assigned `project`, `admin`, or `service` authority and audit references; no raw credentials or credential hashes |
| `idempotency_key`, `request_digest`, `digest_version` | Deduplicate the caller's mutation and reject changed payloads under the same key |
| `payload`, `input_refs`, `target_operation_id` (nullable) | Validated request, pinned image/snapshot inputs, and cancellation target |
| `status`, `phase`, `result`, `error`, `output_refs` | Progress, bounded results, and stored-output metadata |
| `attempt_count`, `attempt_receipts`, `receipt_history_ref` (nullable) | Dispatch, acknowledgement, reconnect, and completion evidence, including allocation and claim revision |
| `claim_revision`, `lease_expires_at`, `next_retry_at`, `deadline` | Controller ownership and bounded scheduling/retries |
| `response_expires_at`, `completed_at` (nullable) | Result retention and completion time |

Enforce `UNIQUE (project_id, idempotency_key)` on this table; [API admission](api-contract.md#retries-and-admission) defines matching, conflicts, and tombstone behavior. Every client-admitted operation records its initiating project/admin credential ID and optional UI session reference. Only authenticated internal maintenance may omit the credential ID. Null fields never create service authority, and client payloads cannot assign initiator identity. Admin actions on a sandbox retain that sandbox's project ownership.

Persist phase and confirmed execution evidence without treating desired state as observed state. [Lifecycle](lifecycle.md) owns phase ordering, claim/lease behavior, and safe continuation; [auth design](auth-design.md#storage-and-audit) owns reauthorization and audit semantics.

Bound retries and receipt metadata. Preserve earlier allocation receipts when reconnecting after resume. If history exceeds the inline bound, publish an immutable history object and persist its reference before removing inline entries; do not discard unresolved execution evidence. No separate attempt table is required initially.

Keep compact operation tombstones with identity, ownership, retry key, request digest/version, and outcome for the project's lifetime; detailed payload/output retention may be shorter. Active/unknown operations retain reconciliation evidence. See [API retention behavior](api-contract.md#errors-and-retention) for how clients observe expiry.

### 4. hosts — Linux compute machines

Internal ID: `hst_<uuidv7>`.

| Fields | Purpose |
| --- | --- |
| `id`, `status`, `last_seen_at` | Registered machine identity, readiness/draining, and health observation |
| `cpu_capacity`, `memory_capacity_mib`, `disk_capacity_mib`, `compatibility` | Schedulable resources after OS/cache headroom and supported VM/image configuration |
| `max_snapshot_uploads` | Maximum concurrent snapshot uploads/staging admissions on this host |
| `supervisor_epoch` | Increasing registration epoch that rejects stale supervisor messages |

Hosts are infrastructure records shared across projects. Calculate reservations from unreleased allocations under transactional capacity checks. A heartbeat timeout makes a host uncertain; it does not prove its VMs stopped. The database issues a new monotonically increasing supervisor epoch on registration after restart. It is separate from an OS boot ID and requires reconciliation before existing ownership is renewed.

### 5. allocations — compute reservations

Internal ID: `alc_<uuidv7>`.

| Fields | Purpose |
| --- | --- |
| `id`, `project_id`, `sandbox_id`, `host_id` | Places one sandbox incarnation on one host |
| `generation`, `supervisor_epoch` | Rejects commands from old VM incarnations or supervisors |
| `vcpu`, `memory_mib`, `disk_mib`, `status`, `lease_expires_at` | Reserved CPU, RAM, writable/restore disk and ownership lifetime |
| `release_evidence`, `released_at` (nullable) | Confirmed termination/fencing and release of the reservation |

Pause releases the allocation only after durable snapshot publication, confirmed VM shutdown, and cleanup of its reserved local resources. Snapshot staging is a separate reservation on the snapshot until its own cleanup completes. Resume creates a new allocation ID and increasing generation. A failed allocation consumes its generation; it cannot be reused.

### 6. snapshots — saved sandbox state

API ID: `snp_<uuidv7>`.

| Fields | Purpose |
| --- | --- |
| `id`, `project_id`, `sandbox_id` | Saved-state identity and ownership |
| `source_allocation_id`, `pause_operation_id` | Which VM and pause request produced the state |
| `status`, `upload_attempt_number` | Preparation, publication, and cleanup progress |
| `staging_host_id`, `staging_reserved_mib`, `staging_released_at` (nullable), `upload_slot_held`, `upload_lease_expires_at` | Temporary disk and upload-slot reservations, retained until cleanup/termination evidence |
| `manifest_key`, `manifest_version`, `manifest_digest`, `compatibility` | Verified immutable references to matching memory, disk, and VM-state objects |
| `chunk_layout` | Chunk or page size and offsets for the memory component, so a restore can fetch ranges instead of whole objects |
| `published_at`, `expires_at`, `deleted_at` (nullable) | Publication and retention lifecycle; snapshots expire 7 days after publication by default, operator-configurable |
| `encryption_key_id` | Which key the components are encrypted under, so per-project keys can replace the installation key without invalidating published snapshots |

PostgreSQL stores metadata; object storage holds the large files. One pause operation reserves one snapshot ID across retries. A published manifest is immutable, and an incomplete upload cannot become resumable state.

`chunk_layout` and `manifest_version` exist for a deferred feature on purpose. Differential snapshots and lazy loading are Phase 5 work, but a published format that only supports whole-object reads would have to break every existing snapshot to get there. Recording the layout and versioning the manifest from the first implementation costs little and keeps that path open. [Performance](performance.md#constraints-on-designs-we-are-choosing-now) owns the reasoning.

Reserve worst-case staging capacity and a host upload slot before freezing the guest. Serialize upload attempts per snapshot initially; a new attempt cannot overwrite or replace the previous reservation until its uploader is stopped and leftover bytes are accounted for. Release the upload slot after confirmed upload completion/termination; release staging bytes only after confirmed file deletion or host storage retirement. Expired leases alone release neither. No additional reservation table is required.

## What we keep inside these models

| Deferred table | Initial representation |
| --- | --- |
| `request_keys` | Retry key and digest fields on `operations` |
| `operation_attempts` | Bounded attempt receipts and history references on `operations` |
| `image_versions` | Verified image digest and compatibility pinned on the sandbox/create operation; an operator-configured image allowlist controls permitted inputs. An allowed image must carry our init and guest agent, and the guest kernel is supplied separately by the supervisor rather than being part of the image ([supported configuration](compatibility.md#how-a-sandbox-boots)) |
| `usage_records` | Derived from allocations and operations rather than sampled; revisit before any billing, per-project reporting, or hosted offering depends on it |
| `artifacts` | Output references on `operations`, containing object key/version, digest, size, retention, and cleanup state |

There are no public image or artifact IDs initially. Retrieve outputs through their authorized operation. Output names identify entries within that operation, not arbitrary bucket paths. Separate catalogs or artifact management can be introduced when needed.

Both deferrals are now deliberate rather than unexamined. The first release ships operator-allowlisted images only, with per-project images named as the next capability so the limitation is visible rather than discovered; customers install their own dependencies at runtime, which is what guest root and the egress allowlist are for. Usage is derived from allocations and operations rather than sampled, which is adequate while nothing is billed from it and must be revisited before anything is.

## Supporting UI security records

These two tables are additional to the six resource models. The authoritative session lifetimes, credential bindings, audit atomicity, and admin mutation retry rules are in [auth design](auth-design.md#storage-and-audit).

| Record | Main fields |
| --- | --- |
| `ui_sessions` | ID, unique session hash, principal kind (`project` or `admin`), credential ID/config revision, nullable project ID (required for Project), CSRF verifier, created/last-activity/absolute-expiry/revoked timestamps |
| `audit_events` | ID, time, principal kind/credential ID, session ID if applicable, action, target project/resource, request ID, safe change summary, outcome, resulting operation/reference; mutation idempotency key and request digest where applicable |

A Project session requires a project ID; an Admin session is installation-scoped. Session secrets and CSRF verifiers are security metadata, never plaintext project/admin credentials. Admin credential IDs/config revisions refer to administrator-controlled deployment configuration; project credential IDs refer to the owning project's token metadata. Validate those references at use, not only at session creation.

Use a unique session hash. Administrative admission receipts require the unique admin-credential/idempotency-key contract specified by auth design, separate from sandbox operations' project/key uniqueness. Detailed audit expiry must not remove compact deduplication receipts. Schema/index definitions remain migration work.

## Database access and migrations

**Use PostgreSQL with SQLx and explicit parameterized SQL; no ORM.** Keep database queries, row mapping, and transaction boundaries together in the proposed `sandbox-store` crate. API handlers and lifecycle controllers call focused storage functions rather than scattering SQL across services. PostgreSQL remains the source of durable metadata; SQLx is the Rust access library.

Bind customer-supplied values as query parameters instead of interpolating them into SQL strings. Any dynamic identifiers or sort expressions must come from trusted, fixed choices. Map results into Rust types and keep ownership checks, operation admission, and resource reservations within the transactions required below. SQLx does not supply tenant authorization or correct locking automatically.

Use SQLx connection pooling and explicit transactions. Its query macros can check queries against the database schema at build time; use them where practical, with a test schema or prepared offline metadata. Keep that metadata consistent with migrations in CI. Dynamic queries still need runtime validation, and compile-time checks do not replace PostgreSQL integration tests for concurrent claims, constraints, or isolation.

Maintain versioned SQL migration files in the repository. Review schema changes alongside the storage code that uses them; do not edit migrations already applied to a released installation. Validate a fresh database and upgrades from supported schema versions before release. Keep migration execution an explicit deployment step with one coordinated runner, rather than letting every API/controller replica change the schema independently. Exact migration paths, tooling commands, and upgrade/rollback procedures remain implementation work.

No SQLx dependency, migration files, or database access code exists yet. Pin the library version and select runtime/TLS features during implementation. The library's capabilities are described in the [official SQLx documentation](https://github.com/transact-rs/sqlx).

## Database rules

- Tenant-owned references carry `project_id`. Composite foreign keys must also ensure sandbox-local links point to the same sandbox: current allocation/snapshot, active transition, cancellation target, and snapshot source/pause operation.
- Enforce `UNIQUE (sandbox_id, generation)` and a partial unique constraint on `allocations(sandbox_id)` where `released_at IS NULL`.
- Enforce `UNIQUE (pause_operation_id)` on snapshots. Verify the referenced operation is a pause for the same sandbox before publication.
- Serialize lifecycle transitions with row locks/state revisions. An execute operation can stay suspended while a separate pause/resume operation runs.
- Publish the verified snapshot manifest and current snapshot pointer transactionally. Mark the sandbox paused only after allocation release is confirmed.
- A generation or expired database lease alone does not stop a VM. Require confirmed shutdown, infrastructure fencing, or the validated host lease watchdog before replacement execution.
- Perform reservation and project/host quota checks transactionally. Local disk use includes unreleased allocation disk plus unreleased snapshot staging; image cache/OS headroom is excluded from schedulable capacity. Snapshot upload slots are bounded separately. Unknown reservations stay counted until safely released, and the supervisor checks actual free disk before writes.
- Expiring outputs must not remove retry protection or unresolved receipts. Project deletion revokes access, drains resources, and completes cleanup before purging records; it never permits ID reuse.

Snapshot contents are immutable after publication; retention/deletion metadata may change. Controller receipts cannot overwrite confirmed outcomes under stale ownership. IDs and UUID timestamps never substitute for explicit revisions and transactional comparisons. All references to source allocations and producing operations must remain in the same project and sandbox.

## Implemented single-host reservation

[Reservation storage](../crates/sandbox-store/src/placement.rs) locks the current operation, project, sandbox, and selected host in that order. It counts all unreleased allocations, including uncertain allocations and allocations with expired leases. Two controllers cannot overbook a host or a project through this path. A lease expiring during lock acquisition rolls back the entire reservation.

Initial project allocation quota keys are `sandboxes`, `vcpu`, `memory_mib`, and `disk_mib`. Omitted keys default to 25, 100, 204800, and 1638400 respectively: 25 sandboxes at the current per-sandbox ceiling. Zero denies new reservations; negative or malformed values fail closed. These are allocation limits, not admission limits on queued sandboxes or pending operations; those admission checks remain to be implemented.

Fresh reservations require the persisted project credential to remain active, unexpired, and unrevoked, with an active project and unexpired operation/sandbox deadlines. The selected host must be ready, have the expected positive supervisor epoch, and have an observation within 30 seconds. Host registration, heartbeat authentication, and image/host compatibility validation remain caller prerequisites for the future controller transport; this storage primitive does not authenticate a host or verify an image.

Reservation publishes the allocation pointer, generation, and a `reserved` receipt atomically. It never reports guest readiness or increments dispatch attempts. Existing allocations return their identity for reconciliation instead of creating replacements, even when new execution is no longer authorized. A successful reservation does not authorize later dispatch without a fresh check, and no release operation is implemented here.

## Object-storage layout

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

Upload attempts get distinct paths. An interrupted or stale uploader must not overwrite the objects selected by a completed publication. Use immutable/conditional writes or pinned object versions, and put the exact keys, sizes, digests, source allocation/generation, format version, chunk layout, and compatibility data in the manifest. The layout entry is what lets a restore issue ranged reads against `memory.bin` instead of downloading it whole. PostgreSQL publishes one verified manifest; clients never infer readiness by listing a prefix.

Unpublished upload attempts can be garbage-collected after their leases and retention expire. Published components remain referenced until deletion policy permits removal. The controller must distinguish abandoned uploads from an in-progress publication before deleting bytes.

The prefix is organization, not security. Resolve operation output names and snapshot IDs through authorized metadata, keep buckets private, and issue only short-lived scoped transfer access where needed. Never accept caller-supplied bucket names or arbitrary object keys as trusted references.

Image digests resolve to verified, allowed immutable manifests with compatibility data pinned at create admission. No mutable tag, caller-controlled object key, or image catalog table is required initially. Snapshot publication records source allocation/generation and matching bytes; local staging reservations remain accounted for until cleanup is confirmed even if the allocation is released.

## Acceptance checks and open decisions

The initial schema, admission, claims, and reservation tests linked above cover a subset of these checks. Remaining coverage must verify typed UUIDv7 collision handling, supervisor epoch registration, replacement generations, unique pause snapshots, snapshot staging/upload accounting, and immutable verified snapshot/object references. Test that stale upload attempts cannot overwrite a publication or cause live bytes to be garbage-collected. No hardware isolation is established by storage tests.

UI session/credential constraints and audit admission must satisfy [auth acceptance](auth-design.md#acceptance-checks). Use [lifecycle](lifecycle.md) for release evidence and [API contract](api-contract.md) for retry response behavior. The session's stable-ID example lives in [lifecycle](lifecycle.md#identity-through-a-sandbox-session).

Open work: SQLx version/features and query-check setup, fresh and upgrade database tests, executable SQL types/enums, indexes/composite constraints, migration ordering, and storage cleanup tests. Development and CI run PostgreSQL 16 against MinIO for object storage. Link actual migrations and tests here once implemented.
