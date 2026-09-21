# Private output storage

Status: the shared output metadata and S3 transport library are implemented and tested. They are not yet connected to the supervisor, database publication, or public API. [Issue #46](https://github.com/hudson-infinity/hudson-sandbox/issues/46) tracks that integration and authenticated streaming. Upload success alone is neither execution success nor publication.

## Object identity and integrity

[OutputPlan](../crates/sandbox-protocol/src/output.rs) records version, project, sandbox, operation, allocation, generation, host epoch, guest boot, upload attempt, stdout/stderr name, SHA-256, byte length, seen/truncated statistics, creation, retention expiry, and earliest deletion time. These are private descriptors, not bearer credentials. The reader's expected owner must come from authorized operation/allocation state, separately from the reference. Timestamps passed to the library are trusted service time, never client input.

Object keys are derived solely from typed IDs, generation, epoch, attempt and the stdout/stderr enum. Callers cannot supply a raw path, bucket, URL, or arbitrary output name. The full plan has a versioned JSON representation whose SHA-256 is stored as object metadata; guest boot, retention and truncation changes therefore conflict even if the output bytes did not change. Boot IDs never become path components. `OutputRef` adds the verified ETag and optional storage version. A final `OutputRefs` pair contains both streams, including explicit empty ones, and validates their shared owner/attempt/retention and combined size against the command's admitted limit (at most 10 MiB).

[sandbox-artifacts](../crates/sandbox-artifacts/src/lib.rs) uses `object_store` 0.14.2 with its S3 transport. Uploads use `PutMode::Create` / S3 `If-None-Match: *`. There is no overwrite fallback. An existing object is accepted only after its complete bytes, length, SHA-256 and plan metadata have been verified. A failed acknowledgement leaves an uncertain outcome: retry the exact persisted plan and bytes. Never mint a new attempt merely because a request timed out. A changed plan at the same key fails; a different attempt uses a different key and cannot overwrite the selected attempt's bytes. These properties rely on a backend that correctly implements [conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html); incompatible backends must fail rather than disable the conditions.

Reads pin ETag and storage version when present, verify metadata, length and the full content digest, and only then return a requested binary range of up to 32 KiB. EOF is the end of captured output; `truncated` still indicates discarded process bytes. Missing objects, expired retention, invalid ranges, integrity failures, and unavailable storage are separate errors. Missing output is never a successful empty result. A forged prefix with corrupt bytes later in the object cannot pass a range read. This deliberately trades repeated full-object reads (bounded by 10 MiB) for simple verification; digest-aware caching or chunk manifests may be added after measuring demand.

Four transfers per process share one admission semaphore across handles and buckets. Excess calls fail immediately instead of queuing caller buffers. Transfers have a 30-second overall timeout; HTTP connection and request timeouts are 5 and 15 seconds. Neither client layer automatically retries. Error messages and Debug output exclude credentials, underlying provider responses and output bytes. The wrapper exposes no listing, public URL, overwrite or deletion method.

## Configuration and remaining publication work

`S3Config` is operator-only configuration with explicit endpoint, bucket, region and credentials. It does not discover ambient cloud credentials or contact metadata services. It disables HTTP redirects and ambient proxies, uses certificate-verified HTTPS, and uses the workspace's ring TLS provider. An explicit development option permits HTTP only to literal loopback IPs. This does not provision a bucket, enforce bucket IAM, or configure encryption at rest: operators must provision private storage with suitable encryption and least-privilege credentials. The initial bucket-name subset accepts lowercase letters, digits and hyphens.

The integration still must persist a plan before sending bytes and publish the chosen `OutputRefs` under a database ownership fence. Object metadata is integrity evidence, not proof that the producer is authorized. The database must compare owner, guest receipt statistics and command limit, and reject stale or competing publishers. Completed execution and output availability remain separate states. The controller carries only metadata; output bytes travel between the guest, supervisor, object storage and authorized reader.

`expires_unix_ms` stops reads; `delete_after_unix_ms` is an earliest eligibility timestamp, not a deletion receipt. Cleanup must retain compact operation tombstones and reconcile selected and orphaned attempts without deleting a referenced object early. That worker and its deletion authority are unfinished. Destroy must remain possible when output publication fails, with explicit output loss rather than a false complete stream. Public auth checks, credential revalidation during streams, cursor/resumption semantics and guest-to-storage recovery remain integration work; this library does not claim those guarantees.

## Evidence and local verification

[Tests](../crates/sandbox-artifacts/src/tests.rs) cover binary ranges, stderr and empty objects, truncation, bounds, full-capacity output, combined command limits, every owner field, retention, conflicting plans, simultaneous identical writes, acknowledgement-loss reconciliation by discarding the first result, separate attempts, out-of-band corruption/deletion, short/oversized/interrupted response bodies, and process-wide transfer admission. The acknowledgement-loss test does not simulate a host crash during a network write.

The explicit MinIO test covers conditional races, identical retry, conflicting bytes, binary and empty output, pinned reads after replacement, missing objects, and removal of only its own randomly named objects. CI starts an ephemeral loopback-only MinIO container at the pinned multi-architecture release digest in [rust-check](../.github/workflows/rust.yml). It requires no privileged runner or VM. These storage tests do not establish public-output or sandbox-isolation readiness.

For the existing local development stack, create the private test bucket once:

```sh
docker compose exec -T minio sh -c 'MC_HOST_hudson="http://${MINIO_ROOT_USER}:${MINIO_ROOT_PASSWORD}@127.0.0.1:9000" mc mb --ignore-existing hudson/hudson-output-test'
cargo test -p sandbox-artifacts
HUDSON_TEST_S3_ENDPOINT=http://127.0.0.1:59000 \
HUDSON_TEST_S3_BUCKET=hudson-output-test \
HUDSON_TEST_S3_ACCESS_KEY=sandbox \
HUDSON_TEST_S3_SECRET_KEY=sandbox-dev-secret \
cargo test -p sandbox-artifacts output_minio -- --ignored
```

These credentials are the repository's synthetic local fixture, not production credentials. The ignored test requires its configuration and fails if storage is unavailable; CI invokes it explicitly rather than treating a skip as evidence.
