# API contract

Status: partially implemented. Create, execute, destroy, sandbox/operation status, project-scoped sandbox/operation lists, retained-output reads, and bearer-authenticated SSE have handlers and tests; other routes and OpenAPI remain unfinished. This document owns client admission, idempotency, response/error behavior, cancellation requests, and output transport. When introduced, a versioned OpenAPI specification will own exact wire schemas; this document will retain semantic explanations and link to it.

## API surfaces

HTTP/JSON is the public interface. Backend automation uses project bearer tokens for `/v1`, admin credentials for `/admin/v1`, and the same-origin UI uses sessions for `/ui-api`. The [auth design](auth-design.md#api-and-browser-boundaries) owns accepted credential types, Project/Admin permissions, sessions, and CSRF policy. Authentication is required in all environments.

Admin sandbox mutations explicitly select a target project and call the same admission/lifecycle services. They do not impersonate a Project credential or bypass quota/state checks. Public resource IDs follow [data models](data-models.md#id-format-and-identity).

## SDK and CLI behavior

[Architecture](architecture.md#client-interfaces-and-agent-integration) defines the interface boundaries. SDKs and the CLI call the HTTP API; they do not contact PostgreSQL, host control sockets, or Firecracker. Their source packages, installation commands, and exact public signatures are not implemented yet.

The first release ships the CLI and three SDKs — Python, TypeScript, and Rust. Models and the request layer are generated from the same OpenAPI document for all three; only the retry, wait, and stream-reconnect behavior below is written by hand. The Rust SDK is the client crate the CLI already depends on, published rather than written twice. One conformance suite, defined as data, runs against all three in CI so they stay genuinely equivalent rather than nominally equivalent, and all three carry the same version as the API they target.

- **Configuration and auth:** resolve a configured API URL and the appropriate project/admin credential. Use trusted credential configuration outside model tool arguments and avoid raw tokens in command-line flags, prompts, logs, or output. Ordinary agent sandbox work uses Project access. Browser session behavior remains separate and is defined in [auth design](auth-design.md).
- **Requests and retries:** preserve the same idempotency key and payload for retries of one logical mutation. Expose a way for a caller to retain/reuse that key across separate CLI invocations or process restarts; a fresh invocation must not silently retry uncertain work with a new key. The server's [admission rules](#retries-and-admission) remain authoritative.
- **Long operations:** return the admitted operation ID promptly. Provide explicit status/wait and cancellation actions; an optional wait follows the same operation without resubmission. Distinguish request acceptance from execution success. A client wait timeout or disconnection does not cancel the server operation.
- **Results:** provide readable CLI output for people and a structured JSON mode for scripts/agents. Keep diagnostics separate from structured stdout. Preserve API error categories and distinguish command exit, pending work, cancellation, and unknown outcomes; exact CLI exit codes are still to be specified.
- **Output and files:** return bounded output with truncation/cursor information and let callers request additional retained output. Retained output is capped at 10 MiB per operation; beyond that, callers get truncation markers and the operation's stored artifacts. Streaming reconnects use the existing operation. Explicit file transfers use the API's ownership/path/size checks; a local file path is not automatically available in the remote guest.

For example, this is a proposed mapping, not a working command:

```text
hudson-sandbox pause <sandbox-id>
    → POST /v1/sandboxes/{sandbox_id}/pause
    → Authorization: Bearer <configured-project-token>
    → Idempotency-Key: <key-for-this-logical-request>
    ← 202 Accepted with operation ID and status URL
```

The CLI displays the operation handle or waits when explicitly requested. It reports the sandbox paused only when the server confirms snapshot publication and compute release. It does not take snapshots itself. SDK pause methods use the same contract; initial SDK languages and exact method signatures remain open.

## Operation contracts

| Operation | Contract |
| --- | --- |
| Create | Accept an authorized immutable image digest and limits; pin verified template compatibility at admission; identical retries return the original operation. Only operator-allowlisted digests are accepted, so a project cannot supply its own image today, and limits are bounded by the [supported configuration](compatibility.md#sandbox) ceiling of 4 vCPU and 8 GiB |
| Execute | Accept executable, argument array, working directory, nonsecret environment, deadline, and output bounds; return an operation handle |
| Pause | Save guest memory and matching disk state, publish the snapshot, release compute, and report completion only after those stages are confirmed |
| Resume | Restore a completed snapshot into one authorized allocation and report ready after guest communication is reestablished |
| Destroy | Revoke sandbox access, stop execution, and reclaim resources; repeated requests are safe |
| Inspect sandbox/operation | Return desired state, observed state, generation, progress, freshness, and known result references |
| Cancel operation | Request interruption; report confirmed cancellation only after actual stopping or a safe transition boundary |
| Import/export files | Validate ownership, paths, sizes, and digests; use staging or bounded streams |

Asynchronous acceptance is not completion. Inspect the operation until the relevant [lifecycle completion evidence](lifecycle.md#states-and-transition-rules) exists. Polling and stream reconnection never submit a new command. Requests may carry an optional external `correlation_id`; it is not ownership or an idempotency key.

## Retries and admission

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

Result bodies may expire, while the request digest/version and original operation ID must remain for the project's lifetime. [Operation response retention](operation-retention.md) now implements opt-in deadline assignment and `410 response_expired` with the original `operation_id` for terminal status reads and identical create/execute/destroy retries. Changed content or an incompatible stored digest version still conflicts. Operation lists keep outcome/identity metadata, mark `response_expired: true`, and omit expired result/error bodies. Opt-in payload compaction now removes eligible request/result/error bodies while retaining identity, outcomes and recovery receipts; further history archival remains unfinished. On project deletion, revoke access before purging its records; the project ID cannot be re-created. Account for retained retry metadata in storage planning.

New keys mean new requests. Executing the same script with a new key runs it again. Lifecycle operations may converge without work: pause on an already-paused sandbox, resume on an already-running sandbox, or destroy on an already-destroyed sandbox returns a completed no-op operation for that new key. No new VM or snapshot is created for those no-ops. Conflicting transitions in progress return `409` with the existing operation reference for an authorized caller; the rejected key is not admitted. Exact duplicates are resolved first.

Neither an operation ID nor an idempotency key guarantees exactly-once external effects. If the guest acted but a receipt was lost, record `unknown` and reconcile before deciding whether anything can safely repeat.

Admin project/token/quota/host mutations use the separate admin admission-receipt contract in [auth design](auth-design.md#storage-and-audit). Login/logout are session operations governed by auth rules, not sandbox execution operations.

## Example and resource routes

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

New create requests must fit the shared [guest resource envelope](compatibility.md#sandbox): 1–4 vCPU, 128–8192 MiB memory, and 64–65536 MiB writable disk. Unsupported sizes return `400 bad_request` without creating a sandbox or operation or consuming the idempotency key. Tightening a minimum does not erase an already admitted identical retry handle; changed input under that key still conflicts. These size bounds are separate from project quotas, host overhead, and image compatibility.

Wait for create success, then call execute. An execute request returns an operation ID for polling results. Output and cancellation routes are still planned. The combined implemented and proposed route list is:

```text
GET  /v1/sandboxes
GET  /v1/sandboxes/{sandbox_id}
POST /v1/sandboxes/{sandbox_id}/execute
POST /v1/sandboxes/{sandbox_id}/pause
POST /v1/sandboxes/{sandbox_id}/resume
POST /v1/sandboxes/{sandbox_id}/destroy
GET  /v1/operations
GET  /v1/operations/{operation_id}
POST /v1/operations/{operation_id}/cancel
GET  /v1/snapshots/{snapshot_id}
GET  /v1/operations/{operation_id}/outputs/{output_name}
GET  /v1/operations/{operation_id}/stream
```

Live output is delivered as server-sent events. Each event carries a monotonic sequence number, and a client reconnects by presenting the last sequence it saw; the server resumes from there or signals an explicit gap when that history has expired. SSE was chosen over a WebSocket because the traffic is one-directional and resumption is part of the format rather than something each client reimplements.

The read-only stream authenticates with the same project token from a backend client. It checks current ownership/allocation and forwards output through the API streaming endpoint, bypassing the controller for bytes. Reconnect uses a cursor on the original operation; it does not create an operation or dispatch another command. Apply the connection/expiry/revocation checks from [auth design](auth-design.md#live-output-and-revocation). The same-origin management UI uses a validated Project/Admin session with Origin, ownership, and expiry checks. Dedicated tokens for third-party browser streams remain deferred.

Cancellation is itself an idempotent operation referencing the target operation; a requested cancel does not change the target to cancelled until confirmed. Pause's completed result includes the published snapshot ID. Ordinary resume resolves the sandbox's current pause snapshot on admission and pins that reference in the operation. It does not accept an arbitrary old snapshot to silently rewind history. A future explicit recovery/fork API must address repeated external effects separately.

The example image digest is illustrative, not an available image. List endpoints paginate with an opaque cursor: a response carries `next_cursor`, and the client passes it back unchanged. The cursor's contents are not part of the contract, which keeps sort order and index strategy changeable without a version bump. Offset-and-page pagination is deliberately not offered, because concurrent creates make it skip and duplicate rows.

File import is a single `PUT` carrying the whole body, bounded by a published size cap and requiring an idempotency key. Resumable multipart staging is deferred to a separate route so adding it later is additive rather than a breaking change to this one.

Session/admin wire schemas and exact response objects remain OpenAPI design work; this route list is not a working endpoint inventory.

## Implemented execute admission and results

`POST /v1/sandboxes/{sandbox_id}/execute` requires the project bearer token and `Idempotency-Key`. The JSON shape is:

```json
{
  "argv": ["/bin/busybox", "sh", "-c", "echo hello"],
  "env": {},
  "cwd": "/",
  "deadline_unix_ms": 1790000000000,
  "output_limit": 1048576
}
```

The timestamp is illustrative: supply a future absolute Unix-millisecond deadline, at most six hours away and no later than sandbox expiry. `argv[0]` is the executable; a shell is used only when explicitly supplied as in this example. The sandbox image determines which executables exist. Environment defaults to empty, cwd to `/`, and combined captured stdout/stderr to 1 MiB (maximum 10 MiB). Argument, environment and encoded request bounds follow the [guest runner](guest-runner.md). Unknown fields or invalid bounds are `400`; oversized JSON bodies are `413`. Environment is persisted configuration and must not carry secrets.

Admission returns `202` with the same operation/status-handle shape as create. Identical normalized retries retain that handle after command deadlines or destruction; a terminal operation with expired response retention returns `410 response_expired` and its original identity. Changed input returns `409`. Inaccessible sandboxes return `404`, destroyed targets return `410`, and a sandbox without a current running lease returns `409`. One active or unknown command is allowed per sandbox. Conflicts return the owning operation ID; destroy remains available while a command runs or is unresolved.

Poll `GET /v1/operations/{operation_id}`. Confirmed exits provide `result.exit_code` or `result.signal`, `result.stdout` and `result.stderr` statistics (`seen`, `stored`, `truncated`), `result.simulated`, and `result.guest_reported=true`. Exit zero is `succeeded`; nonzero or signal is `failed` with `command_failed`; a deadline termination is `failed` with `deadline_exceeded`. Missing execution evidence is `unknown`, never a guessed exit. A durable host fence proving the command never started fails with `command_not_started`. HTTP request completion, disconnection or client timeout does not cancel admitted work.

Execute operation status and list responses also include `output_status`: `none`, `pending`, `uploading`, `published`, or `expired`. This is independent of the process outcome. A confirmed exit initially reports `pending`; the opt-in independent archival worker can subsequently publish verified private references. Private object references are never returned in these status bodies. [Output storage](output-storage.md#database-publication) defines publication and expiry.

Output bytes are not returned by these routes. Retained output is available through the separate endpoint below; file transfer and cancellation remain unfinished; live output uses the SSE endpoint below. The host retains at most 32 command records and reserves at most 64 MiB of combined output limits per allocation without eviction; repeated work eventually requires a new sandbox. Exceeding journal capacity after dispatch intent can leave the command unknown; destroy remains available. This is a development limit, not a complete retention service. See [command ownership](controller.md#command-admission-and-dispatch-ownership) for recovery details.

## Output, files, and reconnects

Live output follows guest → supervisor → authorized streaming endpoint → client. Use operation/output sequence cursors, bounded buffers, and backpressure. A reconnect requests the same operation's retained history; signal an explicit gap when history expired. Neither disconnect nor gap causes cancellation or re-execution. For planned pause/resume, pause ends the stream and resume needs an explicit history transition under the original operation ID. Current stream cursors bind one producing allocation/boot; cross-allocation history transitions are not implemented.

Stored output is retrieved by authorized operation and output name. The service resolves exact object references; clients cannot supply trusted bucket names or arbitrary keys. File mutations validate paths, sizes, ownership, and digests, stage incomplete uploads, and require an idempotency key at commit. The host never expands customer shell strings. Bound aggregate output, file sizes, and active streams. Final result references and byte-retention expiry are distinct from execution success.

The implemented SSE wire format is specified below. Authentication and periodic rechecks remain authoritative in [auth design](auth-design.md#live-output-and-revocation).

## Versioning and deprecation

The `/v1` and `/admin/v1` prefixes are a compatibility promise, and the promise needs stating before anything ships against it.

Within a major version, only additive changes are allowed: new optional request fields, new response fields, new routes, and new enum members in fields documented as extensible. Clients must ignore unknown response fields and must not depend on field order or on an exhaustive enum. Removing a field, narrowing a type, making an optional field required, changing a default, or changing an existing status code is a breaking change and needs a new prefix.

Two versions carry their own compatibility rules and are not the HTTP version. `digest_version` covers idempotency-key request normalization, so an API upgrade cannot reinterpret an old retry; [admission](#retries-and-admission) owns it. `manifest_version` covers the snapshot format, so a later snapshot layout does not invalidate published snapshots; [data models](data-models.md#6-snapshots--saved-sandbox-state) owns it. Both may change while `/v1` stays stable.

Before 1.0, releases are `0.x` and breaking changes are possible between them, but each one must be called out in release notes with an upgrade path. That is a smaller promise than `/v1` stability and should not be described as the same thing.

When a deprecation eventually happens, publish the replacement first, keep the old surface working through a stated window, and announce the removal in release notes. The window length, any deprecation response headers, and whether server and client versions are checked at handshake are open decisions; pick them before the first release, not after callers depend on the current behavior.

## Errors and retention

Error responses use RFC 9457 `application/problem+json`. The standard members carry the human-readable parts, and a stable machine-readable code travels in an extension member so clients branch on the code rather than on status alone or on prose. The exact code list ships with the OpenAPI document.

| Condition | HTTP behavior |
| --- | --- |
| Malformed ID/key/payload | `400` |
| Invalid or expired authentication/session | `401` |
| Valid identity with wrong access level | `403` |
| Missing or inaccessible project resource | Nonrevealing `404` |
| Changed payload under a retry key, or conflicting lifecycle transition | `409` |
| Expired response retained only as a tombstone, or prohibited execution/resume after destruction | `410` |
| Rate/admission limit exceeded | `429`; include useful retry guidance where applicable |
| Capacity unavailable and request not admitted, or required backend unavailable | `503`; never imply an operation exists unless admission committed |

Capacity policy may queue an admitted request within a deadline or reject it; never acknowledge unpersisted work. A request timeout does not prove whether admission/execution happened, so retry with the same key. Never expose another project's identifiers or raw internal errors.

Return `404` for inaccessible tenant resources without revealing whether another project owns them. An authorized caller may inspect a destroyed sandbox tombstone, but new execute/resume operations return `410 Gone`. Expired snapshots cannot be resumed. Destroy permanently prevents resume even when policy retains its snapshot bytes for a bounded period.

Keep sandbox, operation, and snapshot metadata long enough to explain ownership, cleanup, and uncertain outcomes. Payload/artifact retention can be shorter than ID/deduplication tombstone retention. Deleting a resource must not turn its ID or retry keys into reusable names. Revoked callers do not gain receipt access just because they know a retry key.

## Acceptance checks and open decisions

[Create admission tests](../crates/sandbox-api/tests/create.rs), [status route tests](../crates/sandbox-api/tests/reads.rs), and [destroy recovery tests](../crates/sandbox-controller/tests/destroy.rs) exercise implemented behavior. [Response retention tests](../crates/sandbox-api/tests/retention.rs) now cover expired reads, retries and collection entries; [SSE tests](../crates/sandbox-api/tests/streams.rs) cover gaps and reconnects. Receipt-history archival, host/guest journal reclamation and the remaining operation surfaces still need implementation and validation. Cross-project and Admin-scope checks follow [auth acceptance](auth-design.md#acceptance-checks). Recovery from uncertain execution follows [lifecycle acceptance](lifecycle.md#acceptance-checks).

Client acceptance must also cover equivalent API/SDK/CLI outcomes, key reuse across client restarts, no resubmission after a wait timeout, structured output without credential leakage, output truncation/reconnects, and explicit file transfer. These checks require implemented clients and are not available today.

Before implementation, define SDK distribution per registry, CLI syntax, credential configuration and exit codes, the OpenAPI schemas themselves, list filtering, the machine-readable error code list, the file size cap, and the deprecation window in [versioning](#versioning-and-deprecation). Projects cannot register their own guest images in the first release; that capability, and what it adds to this surface, follows the operator allowlist described in [data models](data-models.md#what-we-keep-inside-these-models). Examples remain proposals until validated against those schemas.

## Observation source

Sandbox status responses optionally include `observation_simulated`: true for confirmed development fake-host observations, false for real supervisor evidence, and absent before an observation source is confirmed. Successful fake create operations also return `result.simulated=true`. The [create controller](controller.md#simulated-observations-remain-visible) requires explicit simulation opt-in; these responses do not establish VM execution or isolation.

## Implemented destroy admission

The [destroy controller contract](controller.md#destroy-admission-and-cleanup) owns implemented stop and release evidence. `POST /v1/sandboxes/{sandbox_id}/destroy` accepts `{}` or an optional `correlation_id` up to 200 bytes, requires the ordinary project token and idempotency key, and returns `202` with an operation handle. Exact retries resolve first. A live transition returns `409` with its authorized `operation_id`; an unknown create can transfer cleanup ownership. A new key on a tombstone returns an already succeeded no-op operation. Completion retains the original sandbox identity permanently.

## Implemented image admission

The API router requires an explicit immutable `ImageAllowlist` in `AppState`. The [shared configuration type](../crates/sandbox-protocol/src/images.rs) accepts 1–256 distinct canonical `sha256:` digests with exactly 64 lowercase hexadecimal characters. Empty, duplicate, uppercase, malformed, and oversized configurations are rejected; there is no allow-all default. The [HTTPS server](api-server.md) loads this policy from required `--image-digest` arguments. Embedded callers supply the same type directly.

For a new create, [admission storage](../crates/sandbox-store/src/admission.rs) checks membership before inserting sandbox or operation rows. An unapproved image returns HTTP `403`, `application/problem+json`, code `image_not_allowed`, with `Cache-Control: no-store`. No retry key or capacity is consumed. Malformed or noncanonical digests return `400`.

Authentication and request validation still apply to every retry. For a valid request, storage resolves an existing project-wide idempotency key before consulting the current image allowlist: an identical retry returns its original handles and current status even after image removal; different content returns `409`. A fresh key for a removed image is rejected. A policy update takes effect when the operator replaces the router state; it is not a customer API operation. Configure every API replica consistently during a rollout, since each uses its own immutable policy snapshot.

The controller and supervisor retain independent checks before starting an allocation. Align their configuration with the API; acceptance never promises execution. Allowlist membership is operator authorization, not verification of artifact bytes, required init/agent components, or host compatibility. Image production, byte verification, and compatibility-manifest pinning at admission remain required before the runtime can claim the [supported image contract](compatibility.md#how-a-sandbox-boots).

[HTTP admission tests](../crates/sandbox-api/tests/create.rs) cover policy removal, retry preservation, no-side-effect denial, concurrent admission, authentication, and project isolation.


## Implemented collection reads

The [list handlers](../crates/sandbox-api/src/lists.rs) implement authenticated `GET /v1/sandboxes` and `GET /v1/operations`. Both accept optional `limit` (1–100, default 50) and `cursor`. The operations list also accepts an optional canonical `sandbox_id`; a missing or inaccessible sandbox filter returns the same `404`. Other filters are not implemented and unknown/duplicate query fields return `400`.

Each response contains an `items` array and `next_cursor`; the cursor is `null` on the last or empty page. Items use the same fields as individual sandbox/operation reads. Destroyed sandbox tombstones, unknown operation outcomes, and service-owned cleanup remain discoverable. Simulation provenance and observation timestamps keep their existing meaning. Collection reads do not admit work or change lifecycle state, and every response carries `Cache-Control: no-store`.

Pagination orders by immutable `(created_at, id)` descending, retaining microsecond timestamp precision and using ID to break ties. The database fetches at most `limit + 1` rows. A continuation selects strictly older keys; an insertion ahead of the current boundary does not shift older pages. This is a live view across requests, not a transaction snapshot: status fields can change, and newly committed rows behind a boundary can appear. Start a new traversal to see newer resources. Page size may change between requests.

Cursors are opaque, versioned positions scoped to the authenticated project, collection, and operation filter. Pass each `next_cursor` back unchanged with the same filter. Malformed, oversized (over 2,048 bytes), wrong-version, or wrong-scope cursors return `400` with `problem+json`. Their private encoding is not a client contract. A cursor is not a credential: every page authenticates again and [all list queries](../crates/sandbox-store/src/lists.rs) independently bind the owning project in SQL. Editing a cursor may change a position but cannot grant access to another project.

[Migration 0004](../migrations/0004_collection_indexes.sql) adds indexes matching project-scoped ordering and the operations sandbox filter. It changes no resource rows. Ordinary index creation can block writes while the indexes build; apply it through the coordinated migration process and plan a maintenance window for an existing large installation. An [upgrade test](../crates/sandbox-store/tests/collection_upgrade.rs) verifies index definitions and preservation of existing rows, while [HTTP tests](../crates/sandbox-api/tests/lists.rs) cover ties, inserts between pages, bounds, revocation, cursor scope/tampering, project isolation, and unchanged resource state. Large-dataset query latency remains to be measured.

## Implemented HTTPS transport

The [API server guide](api-server.md#transport-contract) owns TLS configuration, listener limits, startup/shutdown, and runnable local setup. The existing create/execute/destroy JSON routes normalize malformed JSON to `400 bad_request` and oversized bodies to `413 payload_too_large`, both as uncached problems. These replace Axum's raw JSON extractor errors; streaming routes remain unfinished.


## Implemented retained-output reads

`GET /v1/operations/{operation_id}/outputs/{output_name}` returns archived final bytes for `stdout` or `stderr`. It requires a current project bearer token and a project-owned execute operation with published references. The API resolves private object identity from validated database evidence; clients cannot supply buckets, keys, URLs, host paths or upload attempts. Non-execute, missing and cross-project operations return the same `404 not_found`. Reading output never executes or cancels a command.

The optional query fields are `offset` (nonnegative byte offset, default 0) and `limit` (1–32768 bytes, default 32768). Unknown/duplicate fields, malformed values and the HTTP `Range` header return `400 bad_request`; this endpoint uses its explicit offset/limit contract. An offset beyond the captured size returns `416 output_range_invalid`. Offset equal to size returns `200` with an empty body and EOF, after storage verification. A missing object cannot take that path.

Successful responses are `200 application/octet-stream`, `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, and an attachment named `stdout.bin` or `stderr.bin`. Binary data is unchanged, including NUL and invalid UTF-8. These response headers describe the requested stream:

| Header | Meaning |
| --- | --- |
| `X-Output-Offset`, `X-Output-Next-Offset` | Requested offset and next byte position; use the latter to fetch the next retained chunk |
| `X-Output-Size`, `X-Output-Seen` | Captured byte length and guest-reported total bytes observed |
| `X-Output-EOF` | Whether this chunk reaches the end of captured bytes |
| `X-Output-Truncated` | Whether the guest discarded output beyond its admitted capture budget; EOF does not erase truncation |
| `X-Output-Simulated` | Whether the producing receipt came from the development fake |

These offsets address immutable final stdout or stderr bytes; they are not live SSE sequence cursors. Object keys, provider ETags/versions and private references are never returned. A command's success is independent of output availability.

| HTTP / problem code | Meaning |
| --- | --- |
| `409 output_not_ready` | Final output is not published, including running, unknown, no-start and pending archival states; inspect the existing operation rather than resubmitting it |
| `410 output_expired` | Output or operation response retention expired |
| `410 output_missing` | Selected retained object is missing; do not interpret it as an empty stream |
| `502 output_corrupt` | Stored bytes or metadata failed integrity verification |
| `503 unavailable` | Backend failure, unavailable configuration, capacity limit, deadline, or inconsistent ownership/reference evidence |
| `401 unauthenticated` | Credential missing, invalid, expired, removed/replaced/revoked, or project access disabled |

The API admits at most four output reads per process without queuing buffers. Each storage read has a 25-second deadline inside the ordinary 30-second request limit. The [storage adapter](output-storage.md) verifies the complete bounded object before returning a range; repeated small reads therefore reread the object, a documented initial efficiency limit. After storage access, the API reloads ownership, selected references and retention, then revalidates the same credential hash and project as its final awaited check. It checks the effective retention deadline again immediately before forming the response. Database or credential-check failure releases no bytes. Revocation is enforced at these checks; already-delivered bytes cannot be withdrawn.

The separate SSE endpoint below serves live output and resumable archived history. [Router tests](../crates/sandbox-api/tests/outputs.rs) exercise concurrent limits, deadline cancellation, binary ranges, tenant boundaries, simulated provenance, and revocation/retention changes during slow reads. A lock-contention regression test revokes a token during the final metadata lookup; returning bytes is forbidden. [HTTPS/MinIO tests](../crates/sandbox-api/tests/server.rs) exercise actual binary responses and missing/corrupt storage.

## Implemented output streams

`GET /v1/operations/{operation_id}/stream` requires a current Project bearer token and returns `text/event-stream` with `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, and `X-Accel-Buffering: no`. This is the backend bearer interface; browser session authentication, Admin routes and the management UI remain unfinished. Never put a token in a URL or cursor. Clients may use fetch/HTTP streaming with an Authorization header; native browser EventSource does not supply that header automatically.

The service resolves the original execution allocation, intent digest and observed guest boot from tenant-scoped database evidence. It never uses a customer-provided host, path, allocation or object key, and does not follow a sandbox's newer current allocation. Missing/cross-project/non-execute resources share `404`. A dispatch without an observed boot returns `409 output_not_ready`; expired retention returns `410 output_expired`; known missing guest history or a changed cursor binding returns `410 output_missing`. An unavailable backend/configuration or exhausted stream capacity returns `503`. A running command does not need final publication before streaming. Published, validated artifacts take precedence over live reads and remain readable after VM destruction or epoch advancement.

Frames use these event names:

| Event | Meaning |
| --- | --- |
| `output` | One stdout/stderr chunk, including an explicit empty final chunk where applicable |
| `end` | Both captured streams reached their confirmed final ends; not a claim that the command succeeded |
| `gap` | Retained history expired or is missing; never a successful empty stream |
| `error` | A backend, range or integrity failure; inspect its stable `code` and reconnect only to the same operation |

Each `output` frame has a JSON object with `stream` (`stdout` or `stderr`), `offset`, `next_offset`, `data_base64`, `at_end`, `complete`, `seen`, `stored`, `truncated`, `simulated`, and `guest_reported=true`. `data_base64` is standard padded Base64 of up to 32 KiB of arbitrary bytes; it is not UTF-8 text. Decode it before use and treat it as untrusted data. Offsets are ordered within each stream; stdout/stderr interleaving is unspecified. `at_end=true, complete=false` means only that the current captured prefix ended. No `end` event is produced until both streams are final. Empty active polls produce no output event. A ten-second SSE comment heartbeat carries no cursor or payload.

`end` carries `reason=complete`, final stdout/stderr statistics, `simulated`, and `guest_reported=true`. Execution status/exit remains available from the operation endpoint; streaming never updates it. `gap` and `error` carry only a stable `code`, omit an event ID, and close the connection. Authorization failure closes without disclosing more output or resource availability. Codes include `output_missing`, `output_expired`, `output_corrupt`, `output_range_invalid`, and `unavailable`. Missing history and transport failure never trigger command re-execution or cancellation.

`output` and `end` carry an opaque event ID representing the positions **after** that event. Reconnect with the last fully processed event ID in `Last-Event-ID`, or with the `cursor` query parameter if the client cannot set that header. Do not supply both. Duplicate/unknown query fields, duplicate ID headers, HTTP Range, malformed/oversized cursors (over 2,048 bytes), wrong project/operation/version or positions exceeding the 10 MiB combined cap return `400`. A cursor is not a credential: each connection reauthenticates. Its encoding is private, versioned Base64url JSON with both byte offsets and a hash of the producing ownership; it contains no host endpoint, private object reference or secret. Changing an offset can select another authorized position but cannot select another tenant or execution. Without a cursor, reading starts at offset zero in both streams. Empty final chunks and `end` can share an ID because IDs represent byte positions, not unique event numbers; process completion events even when their ID repeats.

Streams permit four concurrent sessions per API router instance. Each has one queued frame plus one read/frame being prepared; reads and pushes obey backpressure. A blocked queue closes after five seconds and discards queued frames; dropping the response aborts outstanding read work and releases its stream slot. A connection's stream task has a 90-second lifetime; the HTTPS server also retains its 120-second connection bound. Reconnect using the last processed cursor when a lifetime or connection limit closes the stream. A disconnect before `end` is not completion.

Each live backend read has a ten-second bound around connection plus RPC; archived reads retain the 25-second bound and full-object verification. Metadata and credential checks each have a five-second deadline. Before queuing data, the service reloads the selected source and retention, then checks the credential as its final awaited database operation. A publication that wins during a live read causes those bytes to be discarded and the same cursor retried against the selected archive. Both stream ends are rechecked when moving to archived data. Frames are also rejected at body consumption if their known effective retention has expired, including while a consumer was not polling.

An independent authorization watchdog waits five seconds between checks and gives each check at most five seconds, including while guest/storage reads or a consumer are stalled. Revocation, expiry, project suspension or failed authorization checks stop the producer and suppress queued bytes. This satisfies the thirty-second revocation-check contract without requiring client activity. Bytes already handed to the HTTP transport or client cannot be withdrawn. Operation/output retention is independently enforced; output does not extend credentials, leases or command deadlines.

[Stream tests](../crates/sandbox-api/tests/streams.rs) exercise binary reconnects, cross-tenant and cursor boundaries, forged replies, bounded readers/queues, publication races, and revocation during slow reads and final metadata lock waits. A regression first demonstrated queued bytes reaching a consumer after retention expired; the consumption-time check prevents it. [HTTPS/MinIO tests](../crates/sandbox-api/tests/server.rs) verify real SSE framing and explicit missing-object gaps. [Controlled microVM evidence](evidence/2026-09-21-aarch64-sse.json) covers live binary bytes before completion, reconnects without repeated execution, archival delivery after destruction and epoch advancement, and the limits of that evidence.

The current CLI configures one live host endpoint; fleet registration/routing and automatic certificate rotation are not implemented. Archived streaming repeats full-object verification per bounded chunk, as the retained-range endpoint does; this has a bounded memory footprint but can amplify storage reads for large outputs. No throughput/fleet-load claim is made. Safe retention/orphan cleanup, cross-allocation pause/resume history, browser sessions, file transfer, public cancellation and supported-host security release gates remain unfinished.
