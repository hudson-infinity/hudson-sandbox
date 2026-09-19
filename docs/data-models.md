# Data models

Status: proposed initial PostgreSQL design. No migrations or runtime models are implemented yet.

Start with **six tables**. A project owns sandboxes; operations request actions; allocations reserve compute on hosts; snapshots preserve sandbox state. Hudson is a client of this service, and Temporal stays in the harness.

This is the selected model, replacing the earlier ten-table proposal. See [architecture and data flow](artitecture.md) for how these records connect to the running services, [implementation](implementation.md) for runtime details, and [identities and retries](identity-and-resources.md) for the API contract.

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

### 1. projects — ownership and limits

API ID: `prj_<uuidv7>`.

| Fields | Purpose |
| --- | --- |
| `id`, `name`, `status` | Stable owner identity and lifecycle |
| `limits` | CPU, memory, disk, snapshot storage, sandbox count, and pending-operation quotas |
| `api_tokens` | Small bounded token metadata collection: key ID, SHA-256 hash, creation/expiry/revocation times; at most two active tokens for rotation |
| `development_only` | Operator-controlled marker, false by default; permits selection as the sole project in an isolated local-development database |
| `external_reference` (optional) | Mapping to a caller's organization or workspace |

Use project-scoped opaque API tokens, provisioned by operator tooling. Each has a random 256-bit secret and a nonsecret project/key locator; the locator only selects the record to verify and never authorizes access. Hash the entire token, compare the stored digest in constant time, and check expiry/revocation and project status. Return the raw token only at issuance; persist no plaintext tokens. Rotation adds a new key before revoking the old one. Missing or removed keys fail authentication. Never reuse a key ID.

Token metadata lives on the project initially, preserving six tables. User login, memberships, and business permissions remain in Hudson; there is no sandbox user/role model. Operator tooling manages credentials outside the customer sandbox API. Internal service credentials are separate and stored in deployment secret configuration. Project names may repeat and IDs are never reused. See [architecture](artitecture.md#simple-project-token-authentication) for the request and streaming rules.

The optional [local-development mode](artitecture.md#local-development-without-api-tokens) creates one marked project with empty token metadata in an isolated development database. Local requests receive that project's context, not administrative access to every project. The marker and selected project cannot be changed through customer requests. The development project still has normal limits, lifecycle state, and ownership references.

### 2. sandboxes — the persistent environment

API ID: `sbx_<uuidv7>`.

| Fields | Purpose |
| --- | --- |
| `id`, `project_id`, `name`, `labels` | Identity and ownership |
| `image_digest`, `image_compatibility`, `resources` | Verified immutable starting image and requested CPU, RAM, and disk limits |
| `desired_state`, `observed_state`, `observed_at`, `state_revision` | Intent, last confirmed state, observation freshness, and concurrent-update protection |
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
| `initiator_kind`, `initiator_key_id` (nullable) | Server-assigned `project_token`, `local_development`, or `service` identity; a token initiator requires its key ID, never raw credentials |
| `idempotency_key`, `request_digest`, `digest_version` | Deduplicate the caller's mutation and reject changed payloads under the same key |
| `payload`, `input_refs`, `target_operation_id` (nullable) | Validated request, pinned image/snapshot inputs, and cancellation target |
| `status`, `phase`, `result`, `error`, `output_refs` | Progress, bounded results, and stored-output metadata |
| `attempt_count`, `attempt_receipts`, `receipt_history_ref` (nullable) | Dispatch, acknowledgement, reconnect, and completion evidence, including allocation and claim revision |
| `claim_revision`, `lease_expires_at`, `next_retry_at`, `deadline` | Controller ownership and bounded scheduling/retries |
| `response_expires_at`, `completed_at` (nullable) | Result retention and completion time |

Use `UNIQUE (project_id, idempotency_key)` directly on this table. Admission inserts the operation and related resource changes in one transaction. A repeated key with identical content returns the same operation; a changed request returns a conflict.

Check the initiating key before starting new customer execution. Revocation does not erase receipts or prevent required reconciliation, stop, and cleanup. Internal maintenance operations use service authority, not customer tokens. Record lifecycle phases and confirmation receipts, including guest freeze, manifest publication, restore handshake, and customer-process release. Streaming cursors are scoped to operation/output and survive allocation changes; replay gaps must be explicit. Output chunks do not become individual database rows.

For `local_development` operations, dispatch requires the standalone disabled-auth mode and the selected active development project. An absent key ID never implies service authority. If token mode is restored, pending development operations remain blocked; service reconciliation may still stop and clean up existing resources. Customer payloads cannot set any initiator fields.

Bound retries and receipt metadata. Preserve earlier allocation receipts when reconnecting after resume. If history exceeds the inline bound, publish an immutable history object and persist its reference before removing inline entries; do not discard unresolved execution evidence. No separate attempt table is required initially.

After result expiry, retain a compact operation tombstone containing its identity, ownership, retry key, request digest/version, and outcome for the project's lifetime. An expired identical retry returns `410` with the original operation identity and never reruns the command. Large payloads and outputs may expire independently. Active or unknown operations keep the evidence needed for reconciliation.

### 4. hosts — Linux compute machines

Internal ID: `hst_<uuidv7>`.

| Fields | Purpose |
| --- | --- |
| `id`, `status`, `last_seen_at` | Registered machine identity, readiness/draining, and health observation |
| `cpu_capacity`, `memory_capacity_mib`, `disk_capacity_mib`, `compatibility` | Schedulable resources after OS/cache headroom and supported VM/image configuration |
| `max_snapshot_uploads` | Maximum concurrent snapshot uploads/staging admissions on this host |
| `supervisor_epoch` | Increasing registration epoch that rejects stale supervisor messages |

Hosts are infrastructure records shared across projects. Calculate reservations from unreleased allocations under transactional capacity checks. A heartbeat timeout makes a host uncertain; it does not prove its VMs stopped.

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
| `published_at`, `expires_at`, `deleted_at` (nullable) | Publication and retention lifecycle |

PostgreSQL stores metadata; object storage holds the large files. One pause operation reserves one snapshot ID across retries. A published manifest is immutable, and an incomplete upload cannot become resumable state.

Reserve worst-case staging capacity and a host upload slot before freezing the guest. Serialize upload attempts per snapshot initially; a new attempt cannot overwrite or replace the previous reservation until its uploader is stopped and leftover bytes are accounted for. Release the upload slot after confirmed upload completion/termination; release staging bytes only after confirmed file deletion or host storage retirement. Expired leases alone release neither. No additional reservation table is required.

## What we keep inside these models

| Deferred table | Initial representation |
| --- | --- |
| `request_keys` | Retry key and digest fields on `operations` |
| `operation_attempts` | Bounded attempt receipts and history references on `operations` |
| `image_versions` | Verified image digest and compatibility pinned on the sandbox/create operation; an operator-configured image allowlist controls permitted inputs |
| `artifacts` | Output references on `operations`, containing object key/version, digest, size, retention, and cleanup state |

There are no public image or artifact IDs initially. Retrieve outputs through their authorized operation. Output names identify entries within that operation, not arbitrary bucket paths. Separate catalogs or artifact management can be introduced when needed.

## Database rules

- Tenant-owned references carry `project_id`. Composite foreign keys must also ensure sandbox-local links point to the same sandbox: current allocation/snapshot, active transition, cancellation target, and snapshot source/pause operation.
- Enforce `UNIQUE (sandbox_id, generation)` and a partial unique constraint on `allocations(sandbox_id)` where `released_at IS NULL`.
- Enforce `UNIQUE (pause_operation_id)` on snapshots. Verify the referenced operation is a pause for the same sandbox before publication.
- Serialize lifecycle transitions with row locks/state revisions. An execute operation can stay suspended while a separate pause/resume operation runs.
- Publish the verified snapshot manifest and current snapshot pointer transactionally. Mark the sandbox paused only after allocation release is confirmed.
- A generation or expired database lease alone does not stop a VM. Require confirmed shutdown, infrastructure fencing, or the validated host lease watchdog before replacement execution.
- Perform reservation and project/host quota checks transactionally. Local disk use includes unreleased allocation disk plus unreleased snapshot staging; image cache/OS headroom is excluded from schedulable capacity. Snapshot upload slots are bounded separately. Unknown reservations stay counted until safely released, and the supervisor checks actual free disk before writes.
- Expiring outputs must not remove retry protection or unresolved receipts. Project deletion revokes access, drains resources, and completes cleanup before purging records; it never permits ID reuse.

## Example session

```text
Project: Hudson development
└── Sandbox S: spreadsheet analysis
    ├── Create operation → allocation A on host 1
    ├── Execute operation E → runs a script
    ├── Pause operation P → publishes snapshot Q, releases A
    └── Resume operation R → restores Q into allocation B
                             continues E under its original deadline
```

Sandbox S stays the same. Allocation B gets a new ID and generation. An HTTP retry reuses its original operation, and resume never submits the frozen script as a new execution.
