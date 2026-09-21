# Private output storage

Status: shared output metadata, the S3 transport library, independently fenced PostgreSQL publication, and the supervisor archival worker are implemented. Operation status responses expose output progress. Authenticated retained-byte retrieval is connected. A separate [supervisor read-only live-output RPC](supervisor-protocol.md#read-only-live-output) now supplies the guest transport; [authenticated public SSE](api-contract.md#implemented-output-streams) now consumes that transport and prefers verified archived objects. Cleanup remains unfinished. [Issue #46](https://github.com/hudson-infinity/hudson-sandbox/issues/46) tracks that integration and authenticated streaming. Upload success alone is neither execution success nor publication.

## Object identity and integrity

[OutputPlan](../crates/sandbox-protocol/src/output.rs) records version, project, sandbox, operation, allocation, generation, host epoch, guest boot, upload attempt, stdout/stderr name, SHA-256, byte length, seen/truncated statistics, creation, retention expiry, and earliest deletion time. These are private descriptors, not bearer credentials. The reader's expected owner must come from authorized operation/allocation state, separately from the reference. Timestamps passed to the library are trusted service time, never client input.

Object keys are derived solely from typed IDs, generation, epoch, attempt and the stdout/stderr enum. Callers cannot supply a raw path, bucket, URL, or arbitrary output name. The full plan has a versioned JSON representation whose SHA-256 is stored as object metadata; guest boot, retention and truncation changes therefore conflict even if the output bytes did not change. Boot IDs never become path components. `OutputRef` adds the verified ETag and optional storage version. A final `OutputRefs` pair contains both streams, including explicit empty ones, and validates their shared owner/attempt/retention and combined size against the command's admitted limit (at most 10 MiB).

[sandbox-artifacts](../crates/sandbox-artifacts/src/lib.rs) uses `object_store` 0.14.2 with its S3 transport. Uploads use `PutMode::Create` / S3 `If-None-Match: *`. There is no overwrite fallback. An existing object is accepted only after its complete bytes, length, SHA-256 and plan metadata have been verified. A failed acknowledgement leaves an uncertain outcome: reconcile the exact persisted plan against storage, without requiring the original bytes. Only a missing object permits another conditional upload from the same captured bytes. Never mint a new attempt merely because a request timed out. A changed plan at the same key fails; a different attempt uses a different key and cannot overwrite the selected attempt's bytes. These properties rely on a backend that correctly implements [conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html); incompatible backends must fail rather than disable the conditions.

Reads pin ETag and storage version when present, verify metadata, length and the full content digest, and only then return a requested binary range of up to 32 KiB. EOF is the end of captured output; `truncated` still indicates discarded process bytes. Missing objects, expired retention, invalid ranges, integrity failures, and unavailable storage are separate errors. Missing output is never a successful empty result. A forged prefix with corrupt bytes later in the object cannot pass a range read. This deliberately trades repeated full-object reads (bounded by 10 MiB) for simple verification; digest-aware caching or chunk manifests may be added after measuring demand.

Four transfers per process share one admission semaphore across handles and buckets. Excess calls fail immediately instead of queuing caller buffers. Transfers have a 30-second overall timeout; HTTP connection and request timeouts are 5 and 15 seconds. Neither client layer automatically retries. Error messages and Debug output exclude credentials, underlying provider responses and output bytes. The wrapper exposes no listing, public URL, overwrite or deletion method.

## Operator configuration

`S3Config` is operator-only configuration with explicit endpoint, bucket, region and credentials. It does not discover ambient cloud credentials or contact metadata services. It disables HTTP redirects and ambient proxies, uses certificate-verified HTTPS, and uses the workspace's ring TLS provider. An explicit development option permits HTTP only to literal loopback IPs. This does not provision a bucket, enforce bucket IAM, or configure encryption at rest: operators must provision private storage with suitable encryption and least-privilege credentials. The initial bucket-name subset accepts lowercase letters, digits and hyphens.

Both `sandbox-host` and `sandbox-fake-host` accept `--output-config /absolute/path/output.json`. This opt-in file must be a regular file owned by the service UID, with no group/other permissions, at most 64 KiB, and no final symlink. Provision it under trusted service-owned ancestors. Values and parsing errors are redacted. The file is loaded at startup; restart with a fresh host epoch to rotate real-host configuration. Keep the endpoint/bucket mapping stable for retained references; changing storage does not migrate existing objects.

```json
{
  "endpoint": "https://objects.example.test",
  "region": "us-east-1",
  "bucket": "sandbox-output",
  "access_key": "REPLACE_FROM_OPERATOR_SECRET_STORE",
  "secret_key": "REPLACE_FROM_OPERATOR_SECRET_STORE"
}
```

An optional `session_token` supplies temporary credentials. `allow_loopback_http` defaults to false. Never put production credentials into the repository, customer configuration, command arguments or logs. The supervisor needs private get/conditional-put permissions; this worker does not need delete permission.

Enable the controller's `--archive-output` flag with the usual endpoint, host identity and TLS options. Defaults are `--output-retention-seconds 86400` and `--output-cleanup-grace-seconds 3600`. The normal controller runs archival in an independent task; `--once --archive-output` performs one lifecycle tick then one archival tick for diagnostics. The fake requires `--allow-simulated` at the controller and archives explicit empty simulated streams without executing customer argv.

## Database publication

[Migration 0006](../migrations/0006_output_publication.sql) adds output status, an independent claim revision/lease/retry time, a stable upload ticket, a saved pair of plans and retention expiry. Existing confirmed terminal executions become candidates without changing their results, dispatch count or receipts. Legacy executions without a pinned allocation or final receipt do not acquire invented output ownership. Upgrading expects the previously unused `output_refs` field to be empty; unexpected legacy references require investigation rather than automatic replacement.

The [publication store](../crates/sandbox-store/src/output.rs) implements this sequence:

1. A confirmed exit, timeout or cancellation sets `output_status=pending` in the execution-completion transaction. Unknown outcomes and no-start fences do not imply complete output.
2. A publisher claims pending work with its own monotonic revision and database-time lease. This does not reopen the execution operation or consume its dispatch claim. Concurrent workers use row locking with `SKIP LOCKED`.
3. `prepare_output` validates the original dispatch intent, command digest, final guest receipt, allocation, generation, host epoch and guest boot. It persists a ticket containing a single upload-attempt ID and retention policy. Retention starts at command completion, is at most 30 days, and is capped by response expiry; retrying does not reset it. Cleanup grace is at most seven days and is only an eligibility timestamp.
4. Before any S3 upload, `save_output_plans` persists both final digests and sizes under that ticket. Stream statistics must exactly match the final receipt and fit the command's combined cap. A replacement publisher reuses the exact saved ticket and plans. Changing a digest, owner, attempt, truncation or retention is rejected.
5. After verified uploads, `publish_output` atomically selects the stdout/stderr references in `operations.output_refs`. They must match the saved plans. The final database write rechecks the output claim and expiry after lock waits. A stale publisher cannot change the selected references. If the commit acknowledgement is lost, inspect the published references; never re-execute the command to recover output.

All of these transactions preserve execution status, result, attempt count, receipts and resource reservations. They use the pinned historical allocation even after destroy clears the sandbox pointer or a host advances its epoch. Existing-work archival does not require the initiating token to remain valid; customer reads still require current authority. Project deletion prevents further preparation/publication. Simulation evidence requires explicit opt-in at every publication step. Object metadata is not proof of an authorized producer: the store validates references from an authenticated archiver, while the storage reader independently checks object existence and integrity.

`output_for_project` scopes private references by project and rejects inactive projects. It withholds expired references even without a cleanup worker. Public operation/list routes expose only `output_status`, never object keys, ETags, versions or tickets. `none` means no final archive has been established, `pending` means a final receipt awaits archival, `uploading` means a ticket is persisted, `published` means references are selected, and `expired` means retention ended. These are separate from process success. Published references are not a promise that the bytes still exist; byte reads must report storage loss or corruption explicitly.

## Supervisor archival and recovery

The [archiver](../crates/sandbox-controller/src/archive.rs) claims work for 120 seconds and persists the ticket before calling `PrepareOutput`. The supervisor verifies its retained final command receipt, original allocation/generation/boot and producing epoch; persists the independent publication revision; then collects bounded guest chunks and hashes both streams. The controller saves those plans before `ArchiveOutput` can upload. Only metadata crosses either RPC: bytes travel guest → supervisor → private storage. Typed JSON metadata fields are limited to 8 KiB each inside the existing 64 KiB RPC limit; no customer URL, bucket or host path is accepted.

Two supervisor archive slots bound concurrency separately from lifecycle workers. Collection has a 20-second total limit and uses chunks up to 32 KiB, with combined capture at most the admitted 10 MiB. The host briefly acquires lifecycle workers/allocation locks to validate and persist metadata, then releases them before guest/network transfers. Prepare and archive client calls have 30/80-second bounds on a separate connection; lifecycle requests keep their five-second transport bound. The upload phase has a 75-second supervisor deadline. Both host and database recheck revision, claim expiry and retention before final publication. Errors defer the same ticket for a later attempt; neither path dispatches a command.

`ArchiveOutput` first verifies any existing objects against the saved plans. If both exist, no guest is needed. If either is missing, it recaptures output and requires exact digests/statistics before conditionally writing missing objects. There is no host disk spool: destroying a guest before archival may make missing bytes unrecoverable. Missing history stays unavailable and is retried until retention expires; it cannot become an invented empty stream. A persisted archive plan survives supervisor restart. A new serving epoch may reconcile older producing-epoch objects for the same host from its retained journal, but cannot contact or restart the old guest. This does not implement old-epoch database allocation recovery.

## Integration still required

The [retained-output endpoint](api-contract.md#implemented-retained-output-reads) consumes selected references and rechecks credentials after storage and metadata lookups. The [SSE endpoint](api-contract.md#implemented-output-streams) now adds pinned live-guest reads, independent credential rechecks, byte-position cursors and explicit gaps.

`expires_unix_ms` stops reads; `delete_after_unix_ms` is an earliest eligibility timestamp, not a deletion receipt. Cleanup must retain compact operation tombstones and reconcile selected and orphaned attempts without deleting a referenced object early. That worker and its deletion authority are unfinished. Destroy remains independent of publication. The API exposes missing/expired history and reconnect semantics; reclamation of objects and compact deduplication history still needs a safe cleanup worker.

## Evidence and local verification

[PostgreSQL publication tests](../crates/sandbox-store/tests/output.rs) cover simultaneous claims/publications, exact-plan recovery after claim replacement, lost commit acknowledgement, every owner/statistic/retention mismatch, claim expiry during lock waits at preparation and publication, corrupt stored evidence, simulation opt-in, tenant/project restrictions, retention without a cleanup worker, and upgrades over populated v5 data. These use synthetic guest receipts, not VM execution. Existing controller/API tests also confirm that a successful command reports pending output.

[Tests](../crates/sandbox-artifacts/src/tests.rs) cover binary ranges, stderr and empty objects, truncation, bounds, full-capacity output, combined command limits, every owner field, retention, conflicting plans, simultaneous identical writes, acknowledgement-loss reconciliation by discarding the first result, separate attempts, out-of-band corruption/deletion, short/oversized/interrupted response bodies, and process-wide transfer admission. The acknowledgement-loss test does not simulate a host crash during a network write.

[Archive collection tests](../crates/sandbox-supervisor/tests/archive.rs) cover binary chunks, truncation, empty versus missing streams, malformed offsets/identities/completion, deadline bounds and receipt ownership. [Controller/MinIO tests](../crates/sandbox-controller/tests/archive.rs) cover lost post-upload acknowledgements followed by destroy and replacement publication without guest access, unchanged execution receipts, stale/changed plans, missing history, and renewal/destroy during a six-second archive delay over real mTLS. CI invokes these explicitly with PostgreSQL and MinIO.

The explicit artifact MinIO test covers conditional races, identical retry, conflicting bytes, binary and empty output, pinned reads after replacement, missing objects, and removal of only its own randomly named objects. CI starts an ephemeral loopback-only MinIO container at the pinned multi-architecture release digest in [rust-check](../.github/workflows/rust.yml). It requires no privileged runner or VM. These storage tests do not establish public-output or sandbox-isolation readiness.

For the existing local development stack, create the private test bucket once:

```sh
docker compose exec -T minio sh -c 'MC_HOST_hudson="http://${MINIO_ROOT_USER}:${MINIO_ROOT_PASSWORD}@127.0.0.1:9000" mc mb --ignore-existing hudson/hudson-output-test'
cargo test -p sandbox-artifacts
HUDSON_TEST_S3_ENDPOINT=http://127.0.0.1:59000 \
HUDSON_TEST_S3_BUCKET=hudson-output-test \
HUDSON_TEST_S3_ACCESS_KEY=sandbox \
HUDSON_TEST_S3_SECRET_KEY=sandbox-dev-secret \
cargo test -p sandbox-artifacts output_minio -- --ignored
# With the same HUDSON_TEST_S3_* variables and DATABASE_URL set:
cargo test -p sandbox-controller output_minio -- --ignored --test-threads=1
```

These credentials are the repository's synthetic local fixture, not production credentials. The ignored test requires its configuration and fails if storage is unavailable; CI invokes it explicitly rather than treating a skip as evidence.

[Controlled archival evidence](evidence/2026-09-21-aarch64-output-archive.json) records eight passing real-host tests, source/artifact hashes, binary stdout/stderr and empty-stream verification, one execution marker, and object reconciliation after VM destruction and host epoch advancement. This is nested aarch64 development evidence; that recording predates the public read endpoint and does not satisfy supported-release isolation gates.


## API reader configuration and evidence

`sandbox-api serve --output-config /absolute/path/output.json` enables retained-byte reads using the same private configuration-file format above. Use separate read-only credentials with access to the same endpoint, bucket and object namespace as the supervisor. File ownership is checked against the API service UID. Without configuration the route still authenticates and checks ownership, then returns `503` for a published output read. No public object URL or client storage credential is issued.

The API exposes a trusted read-only adapter interface; the production adapter is `ArtifactStore`, which verifies full object integrity and pinned identity. Database references and API selectors remain independent of provider credentials. [API tests](../crates/sandbox-api/tests/outputs.rs) use controlled adapters to pause storage and test revocation, secret replacement, project suspension/deletion, retention, changing references, backend failures, concurrency and timeout behavior. The explicit `cargo test -p sandbox-api output_minio -- --ignored` case uses real HTTPS and MinIO with binary ranges, removal and corruption of only its own random test objects. CI supplies its configuration alongside the existing artifact/controller MinIO tests.

[Public retrieval evidence](evidence/2026-09-21-aarch64-output-read.json) verifies authenticated final stdout/stderr reads from real microVM output, including binary and empty streams and reads after destruction/host epoch advancement. Separate HTTPS/MinIO tests cover wire transport and missing/corrupt objects. Controlled delayed-reader and database-lock tests verify post-read authorization, including revocation during final metadata lookup. Live streaming is described in the [API contract](api-contract.md#implemented-output-streams); the broader release gates remain unfinished.
