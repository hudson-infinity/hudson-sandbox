# Private output storage

Status: shared output metadata, the S3 transport library, independently fenced PostgreSQL publication, and the supervisor archival worker are implemented. Operation status responses expose output progress. Authenticated retained-byte retrieval is connected. A separate [supervisor read-only live-output RPC](supervisor-protocol.md#read-only-live-output) now supplies the guest transport; [authenticated public SSE](api-contract.md#implemented-output-streams) now consumes that transport and prefers verified archived objects. An opt-in standalone cleanup worker connects durable inventory and storage retirement to fenced database completion receipts. History compaction remains unfinished. [Issue #46](https://github.com/hudson-infinity/hudson-sandbox/issues/46) tracks the remaining output work. Upload success alone is neither execution success nor publication.

## Object identity and integrity

[OutputPlan](../crates/sandbox-protocol/src/output.rs) records version, project, sandbox, operation, allocation, generation, host epoch, guest boot, upload attempt, stdout/stderr name, SHA-256, byte length, seen/truncated statistics, creation, retention expiry, and earliest deletion time. These are private descriptors, not bearer credentials. The reader's expected owner must come from authorized operation/allocation state, separately from the reference. Timestamps passed to the library are trusted service time, never client input.

Object keys are derived solely from typed IDs, generation, epoch, attempt and the stdout/stderr enum. Callers cannot supply a raw path, bucket, URL, or arbitrary output name. The full plan has a versioned JSON representation whose SHA-256 is stored as object metadata; guest boot, retention and truncation changes therefore conflict even if the output bytes did not change. Boot IDs never become path components. `OutputRef` adds the verified ETag and optional storage version. A final `OutputRefs` pair contains both streams, including explicit empty ones, and validates their shared owner/attempt/retention and combined size against the command's admitted limit (at most 10 MiB).

[sandbox-artifacts](../crates/sandbox-artifacts/src/lib.rs) uses `object_store` 0.14.2 with its S3 transport. Uploads use `PutMode::Create` / S3 `If-None-Match: *`. There is no overwrite fallback. An existing object is accepted only after its complete bytes, length, SHA-256 and plan metadata have been verified. A failed acknowledgement leaves an uncertain outcome: reconcile the exact persisted plan against storage, without requiring the original bytes. Only a missing object permits another conditional upload from the same captured bytes. Never mint a new attempt merely because a request timed out. A changed plan at the same key fails; a different attempt uses a different key and cannot overwrite the selected attempt's bytes. These properties rely on a backend that correctly implements [conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html); incompatible backends must fail rather than disable the conditions.

Reads pin ETag and storage version when present, verify metadata, length and the full content digest, and only then return a requested binary range of up to 32 KiB. EOF is the end of captured output; `truncated` still indicates discarded process bytes. Missing objects, expired retention, invalid ranges, integrity failures, and unavailable storage are separate errors. Missing output is never a successful empty result. A forged prefix with corrupt bytes later in the object cannot pass a range read. This deliberately trades repeated full-object reads (bounded by 10 MiB) for simple verification; digest-aware caching or chunk manifests may be added after measuring demand.

Four transfers per process share one admission semaphore across handles and buckets. Excess calls fail immediately instead of queuing caller buffers. Transfers have a 30-second overall timeout; HTTP connection and request timeouts are 5 and 15 seconds. Neither client layer automatically retries. Error messages and Debug output exclude credentials, underlying provider responses and output bytes. The ordinary `ArtifactStore` wrapper exposes no listing, public URL, overwrite or deletion method. A separate operator-only `ArtifactRetirer` handles expired output as described below.

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

## Cleanup inventory

[Migration 0007](../migrations/0007_output_cleanup.sql) adds private `output_cleanup` rows keyed by operation. The [store interface](../crates/sandbox-store/src/output_cleanup.rs) implements discovery, claims, preparation, deferral and completion. A separate opt-in worker schedules cleanup; the controller does not start it and there is no customer cleanup endpoint. Migration 0007 creates an empty inventory and leaves existing operation records unchanged. [Migration 0008](../migrations/0008_output_cleanup_completion.sql) adds paired `completed_at` and `receipt` fields while preserving pending inventory.

1. `enqueue_expired_output` discovers at most 100 expired tickets per call. A unique operation key makes concurrent discovery and retries idempotent. Both published references and unfinished uploads are candidates, including suspended or deleting projects. Pending executions without a ticket have no authorized artifact keys to inventory.
2. `claim_output_cleanup` gives one worker an independent revision and a database-time lease of at most 300 seconds. Workers use `SKIP LOCKED`; corrupt records can be deferred for up to one hour without blocking other operations.
3. `prepare_output_cleanup` locks the operation and inventory, reconstructs the original command, final receipt and pinned allocation, and validates the saved ticket, stream plans and any selected references. It atomically records this exact manifest and sets `output_status=expired`, clears publisher retry/lease fields, and fences the publication revision. The execution outcome, dispatch claim, receipts, allocation and resource reservations remain unchanged.
4. The ticket's `delete_after_unix_ms` becomes the earliest cleanup eligibility. Before then, preparation releases its claim and returns `Waiting`; claims skip the row until that time. Once eligible, it returns `Ready` with the private manifest. A replacement worker must recover the same manifest; later changes to its owner, plans, references or retention fail validation. Final writes recheck the claim after lock waits and roll back together if it expired.

Plans without selected references identify possible orphan objects from interrupted publication. A ticket without plans records that no upload was authorized. Neither state proves object absence. Current publication permits only one stable upload attempt per operation; arbitrary bucket objects and legacy attempts outside that protocol are not discovered by this inventory. There is no bucket listing or customer-controlled object path.

`Ready` means eligible metadata, not deleted bytes. After storage verification, `complete_output_cleanup` validates both stream receipts against the frozen plans and any selected references, rechecks the live claim after lock waits, and atomically stores completion while clearing lease/retry state. It accepts `NoUploadsAuthorized` only when neither plans nor references exist. Completed rows are never claimed again. Retrying the same receipt under the same completed revision reconciles a lost database acknowledgement; changed receipts or replaced revisions fail. Simulation requires explicit opt-in. The database publication fence alone cannot stop an already-issued storage request from finishing.

## Storage retirement

`S3Config::build_retirer` constructs a separate [ArtifactRetirer](../crates/sandbox-artifacts/src/retirement.rs). Its `retire` method accepts the frozen plan, an independently trusted owner, the selected reference when one exists, and service time at or after `delete_after_unix_ms`. It returns a private [OutputRetirement](../crates/sandbox-protocol/src/output.rs) receipt. There is no raw key, bucket, endpoint, version selector or customer cleanup route.

1. Verify the current object's complete bytes and plan metadata, including the pinned reference when published. For an unpublished attempt, recover the reference from the verified object. A missing key is handled explicitly.
2. Replace verified expired data using `If-Match`, or seal a missing key using `If-None-Match: *`. The replacement is a small JSON marker containing the plan digest and previous reference, with a digest of its own body in metadata. Preserve it at the original key: it stops old create-only uploads from recreating output. A delete marker alone would reopen that key under [S3 conditional-write semantics](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html).
3. Read back and validate the marker. For unversioned storage, conditional replacement has removed the payload. For versioned storage, verify the exact previous version's bytes again, delete only that version ID, and verify its absence. [Deleting a specific version](https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObject.html) removes those bytes; deleting the current key would leave old versions and reopen uploads. A replaced mutable `null` version in a suspended bucket needs no deletion. An ambiguous change of versioning mode fails instead of authorizing current-key deletion.
4. Recheck the same current marker before returning a receipt. A successful DELETE response alone is insufficient. A missing old version can reconcile a lost acknowledgement, while a changed/corrupt version or unavailable backend remains an error. Retrying preserves the same marker and previous reference; it never reuploads bytes, selects another attempt, executes a command or deletes the marker.

Markers are bounded to 8 KiB and contain metadata, never captured output. Even an originally empty stream retains a marker. Cleanup shares the process-wide four-transfer admission limit and 30-second deadline; credential files, explicit endpoints, TLS, redacted errors and disabled redirects/proxies/retries follow the normal storage configuration. Exact-version DELETE uses the pinned `object_store` SigV4 authorizer and an encoded version query, with no new signing implementation.

This protocol requires a private namespace with only managed create-only uploaders and the retirement worker. Operators must preserve markers, keep bucket versioning stable, and exclude this namespace from lifecycle rules or external writers that remove/overwrite current objects. It inventories managed attempts, not arbitrary historical versions created outside this protocol. A cleanup credential needs GET, conditional PUT and exact-version deletion permissions; readers and uploaders keep separate credentials without deletion authority. Bucket IAM and lifecycle policies are operator responsibilities, not configured by this library. Denied deletion, object locks and incompatible providers leave cleanup incomplete.

The primitive reclaims known payloads while retaining compact storage markers. It does not compact database or guest/host journals, reclaim their reservations, or prove every backend/versioning combination is supported. Current integration evidence covers the pinned MinIO release in versioned and unversioned modes, not a live AWS deployment.

## Cleanup worker

The [sandbox-cleanup executable](../crates/sandbox-cleanup/src/main.rs) runs independently of the controller and supervisor. Set `DATABASE_URL` in the service environment and provide a private storage configuration file using the format above, with a separate cleanup credential authorized for retirement:

```sh
cargo run -p sandbox-cleanup -- --output-config /absolute/path/cleanup-storage.json
# One bounded discovery/cleanup tick, with nonzero exit on failure:
cargo run -p sandbox-cleanup -- --output-config /absolute/path/cleanup-storage.json --once
```

Startup applies the repository migrations. The file must satisfy the same ownership and permission checks as other output configurations. The worker needs no host endpoint, supervisor certificate, guest access or command-dispatch capability. `--allow-simulated` is an explicit development-only option and defaults to false.

Each [worker tick](../crates/sandbox-cleanup/src/lib.rs) discovers at most 100 expired attempts, claims one for 120 seconds, prepares its manifest, retires stdout and stderr sequentially, and persists their paired receipts. Database calls have five-second bounds and claimed processing has a 90-second bound; each storage retirement retains its 30-second bound. Failures defer that attempt with exponential delay based on claim revision, starting at five seconds and capped at one hour. The loop waits 500 milliseconds between ticks; multiple processes coordinate through database claims. Logs identify the operation without output bytes, credentials or storage descriptors.

If stdout retirement succeeds and stderr fails, database completion stays unset. A replacement worker recovers stdout's existing marker and finishes stderr without needing the guest or re-executing the command. Process death or cancellation can leave an uncertain storage/commit outcome; markers, claim expiry and idempotent completion support reconciliation. A prepared ticket with no authorized uploads completes without contacting storage. Grace-period work remains waiting without storage effects.

Completion records retirement of the exact archived attempt. It does not certify physical erasure across provider replicas/backups, reclaim guest/host journals or reservations, or establish VM cleanup or sandbox release readiness. The public API continues to report output as expired and never exposes private cleanup receipts.

## Remaining history work

The [retained-output endpoint](api-contract.md#implemented-retained-output-reads) consumes selected references and rechecks credentials after storage and metadata lookups. The [SSE endpoint](api-contract.md#implemented-output-streams) now adds pinned live-guest reads, independent credential rechecks, byte-position cursors and explicit gaps.

`expires_unix_ms` stops reads; `delete_after_unix_ms` is an earliest eligibility timestamp, not a deletion receipt. Even after retirement completion, the cleanup inventory retains the complete execution and publication history. Compacting that history into operation tombstones remains unfinished: retain retry keys, request digests, outcomes and reconciliation evidence, and never make an old command runnable again. Destroy remains independent of publication and retirement. The API exposes missing/expired history and reconnect semantics.

## Evidence and local verification

[PostgreSQL publication tests](../crates/sandbox-store/tests/output.rs) cover simultaneous claims/publications, exact-plan recovery after claim replacement, lost commit acknowledgement, every owner/statistic/retention mismatch, claim expiry during lock waits at preparation and publication, corrupt stored evidence, simulation opt-in, tenant/project restrictions, retention without a cleanup worker, and upgrades over populated v5 data. These use synthetic guest receipts, not VM execution. Existing controller/API tests also confirm that a successful command reports pending output.

[Cleanup inventory tests](../crates/sandbox-store/tests/support/output_cleanup.rs) cover concurrent discovery/claims, stale-worker rejection, recovery of exact manifests, orphan and absent plans, grace periods, corruption deferral, early-cleanup rejection, lease expiry at both operation and inventory lock waits, ownership after destroy/host restart/project deletion, and an upgrade over populated v6 data. They use real PostgreSQL with synthetic receipts and do not exercise storage deletion.

[Completion and worker tests](../crates/sandbox-store/tests/support/cleanup_completion.rs) cover paired-receipt validation, immutable manifests, stale claims, lock-wait expiry, idempotent completion, simulation opt-in, grace periods, schema constraints and a populated v7 upgrade. Two explicit PostgreSQL/MinIO cases recover a failure between stream retirements and retire orphan/missing objects after allocation, epoch and project changes, while preserving execution evidence. They retain markers in the dedicated versioned fixture bucket; use disposable test storage and remove that owned fixture after testing. The [executable test](../crates/sandbox-cleanup/tests/cli.rs) verifies `--once` against an isolated database, private-file enforcement and credential redaction without needing storage for an empty queue.

[Retirement tests](../crates/sandbox-artifacts/src/retirement/tests.rs) exercise binary/empty output, owner and retention gates, missing/orphan objects, bounded capacity, corrupt markers, and concurrent cleanup/late uploads. Three explicit MinIO cases additionally cover real version deletion, interrupted cleanup, lost and false delete acknowledgements, corrupted old-version bytes, cancelled workers, concurrent recovery and retained markers blocking stale uploads. CI creates a dedicated versioned fixture bucket and runs these with the existing artifact MinIO checks. Fixture teardown removes only the tests' randomly scoped keys/versions; production retirement never removes its marker.

[Tests](../crates/sandbox-artifacts/src/tests.rs) cover binary ranges, stderr and empty objects, truncation, bounds, full-capacity output, combined command limits, every owner field, retention, conflicting plans, simultaneous identical writes, acknowledgement-loss reconciliation by discarding the first result, separate attempts, out-of-band corruption/deletion, short/oversized/interrupted response bodies, and process-wide transfer admission. The acknowledgement-loss test does not simulate a host crash during a network write.

[Archive collection tests](../crates/sandbox-supervisor/tests/archive.rs) cover binary chunks, truncation, empty versus missing streams, malformed offsets/identities/completion, deadline bounds and receipt ownership. [Controller/MinIO tests](../crates/sandbox-controller/tests/archive.rs) cover lost post-upload acknowledgements followed by destroy and replacement publication without guest access, unchanged execution receipts, stale/changed plans, missing history, and renewal/destroy during a six-second archive delay over real mTLS. CI invokes these explicitly with PostgreSQL and MinIO.

The explicit artifact MinIO test covers conditional races, identical retry, conflicting bytes, binary and empty output, pinned reads after replacement, missing objects, and removal of only its own randomly named objects. CI starts an ephemeral loopback-only MinIO container at the pinned multi-architecture release digest in [rust-check](../.github/workflows/rust.yml). It requires no privileged runner or VM. These storage tests do not establish public-output or sandbox-isolation readiness.

For the existing local development stack, provision the private test buckets (enable versioning only on the dedicated versioned fixture):

```sh
docker compose exec -T minio sh -c 'MC_HOST_hudson="http://${MINIO_ROOT_USER}:${MINIO_ROOT_PASSWORD}@127.0.0.1:9000" mc mb --ignore-existing hudson/hudson-output-test'
docker compose exec -T minio sh -c 'MC_HOST_hudson="http://${MINIO_ROOT_USER}:${MINIO_ROOT_PASSWORD}@127.0.0.1:9000" mc mb --ignore-existing hudson/hudson-output-version-test'
docker compose exec -T minio sh -c 'MC_HOST_hudson="http://${MINIO_ROOT_USER}:${MINIO_ROOT_PASSWORD}@127.0.0.1:9000" mc version enable hudson/hudson-output-version-test'
cargo test -p sandbox-artifacts
HUDSON_TEST_S3_ENDPOINT=http://127.0.0.1:59000 \
HUDSON_TEST_S3_BUCKET=hudson-output-test \
HUDSON_TEST_S3_VERSIONED_BUCKET=hudson-output-version-test \
HUDSON_TEST_S3_ACCESS_KEY=sandbox \
HUDSON_TEST_S3_SECRET_KEY=sandbox-dev-secret \
cargo test -p sandbox-artifacts -p sandbox-controller -p sandbox-api -p sandbox-store \
  output_minio -- --ignored --test-threads=1
```

Set `DATABASE_URL` to the local test PostgreSQL instance for that combined invocation. These credentials are the repository's synthetic local fixture, not production credentials. The ignored tests require their configuration and fail if storage is unavailable; CI invokes them explicitly rather than treating a skip as evidence. Keep the explicit package list: workspace-wide ignored tests also include supervisor cases that require controlled Linux/KVM infrastructure.

[Controlled archival evidence](evidence/2026-09-21-aarch64-output-archive.json) records eight passing real-host tests, source/artifact hashes, binary stdout/stderr and empty-stream verification, one execution marker, and object reconciliation after VM destruction and host epoch advancement. This is nested aarch64 development evidence; that recording predates the public read endpoint and does not satisfy supported-release isolation gates.


## API reader configuration and evidence

`sandbox-api serve --output-config /absolute/path/output.json` enables retained-byte reads using the same private configuration-file format above. Use separate read-only credentials with access to the same endpoint, bucket and object namespace as the supervisor. File ownership is checked against the API service UID. Without configuration the route still authenticates and checks ownership, then returns `503` for a published output read. No public object URL or client storage credential is issued.

The API exposes a trusted read-only adapter interface; the production adapter is `ArtifactStore`, which verifies full object integrity and pinned identity. Database references and API selectors remain independent of provider credentials. [API tests](../crates/sandbox-api/tests/outputs.rs) use controlled adapters to pause storage and test revocation, secret replacement, project suspension/deletion, retention, changing references, backend failures, concurrency and timeout behavior. The explicit `cargo test -p sandbox-api output_minio -- --ignored` case uses real HTTPS and MinIO with binary ranges, removal and corruption of only its own random test objects. CI supplies its configuration alongside the existing artifact/controller MinIO tests.

[Public retrieval evidence](evidence/2026-09-21-aarch64-output-read.json) verifies authenticated final stdout/stderr reads from real microVM output, including binary and empty streams and reads after destruction/host epoch advancement. Separate HTTPS/MinIO tests cover wire transport and missing/corrupt objects. Controlled delayed-reader and database-lock tests verify post-read authorization, including revocation during final metadata lookup. Live streaming is described in the [API contract](api-contract.md#implemented-output-streams); the broader release gates remain unfinished.
