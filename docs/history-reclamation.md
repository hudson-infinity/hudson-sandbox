# Acknowledged history reclamation

Status: guest-local command and file retirement primitives are implemented and tested. No RPC, controller or cleanup worker invokes them. Host journal reclamation, database coordination, reservation refunds, and storage-marker lifecycle remain unfinished under [issue #79](https://github.com/hudson-infinity/hudson-sandbox/issues/79). Public runtime capacity limits are unchanged. This document owns the retirement barrier and the prerequisites for enabling it across the platform.

## Why deletion needs an admission barrier

A retained receipt prevents a delayed request from looking like a new operation. Removing the receipt without retaining a fence can execute a command or publish an upload again. An elapsed retention deadline, a successful object DELETE, or a cancelled local request does not provide that fence.

The [barrier type](../crates/sandbox-protocol/src/history.rs) closes an inclusive prefix of operation IDs for one allocation, generation, guest boot and domain (`commands` or `files`). IDs are compared as opaque ordered values. Their UUID timestamps are not an expiry clock and are not assumed to match admission order. A retained barrier rejects every later admission at or below `through`, including IDs for which no receipt remains. One monotonic value replaces the discarded per-operation admission records.

A caller must have durably retained the original outcomes and retired all consumers that still need guest bytes **before** asking the guest to remove history. The current internal library methods cannot verify those external facts and grant no cleanup authority. Do not connect them to automatic cleanup or a customer endpoint before the coordination below is implemented.

## Guest implementation

[`Runner::retire_history`](../crates/sandbox-guest/src/runner.rs) and [`Transfers::retire_history`](../crates/sandbox-guest/src/files/mod.rs) follow this sequence under their existing ownership locks:

1. Validate the barrier version, domain and exact allocation/generation/boot. A healthy engine reconciles a repeated or lower request by returning its current barrier without regressing it.
2. Check the entire covered prefix. Commands must have a known terminal outcome, confirmed process-tree cleanup and no active covered command or remaining covered cgroup. Files must be committed or aborted. Active, staging, commit-intent, unknown and unconfirmed-cleanup work blocks advancement. Newer commands can remain active outside the prefix.
3. Replace and sync `context.json` with the complete versioned binding and barrier. This happens before any deletion. A separate optional watermark file would be unsafe to lose while retaining an apparently legacy context.
4. Unlink covered internal receipts, output/exit files, staged data and temporary metadata, then sync the state directory. Scanning remains bounded by the existing retained-state envelope. Basenames must be recognized internal operation names; deletion never follows symlinks or recursively deletes directories. Published upload destinations are untouched.
5. Remove the same records from in-memory accounting only after deletion/sync succeeds. The successful return acknowledges guest-local completion. `history_barrier` reports the admission floor alone and is **not** a deletion acknowledgement.

Any uncertain barrier write or deletion poisons the current engine for new work until recovery. Covered commands/uploads cannot be admitted even if some old receipt remains in memory. Reads may already hold a receipt or open file snapshot; they do not gain new mutation authority. Removing directory entries is not physical erasure of open file handles or backups. Guest filesystem quotas remain independent of declared output/upload reservations.

On reopen, load and validate the binding first, finish pruning its closed prefix, then load and charge only the remaining receipts. This handles a crash after the barrier but before deletion, a partially removed prefix, and a lost completion acknowledgement. Recovery never needs the removed receipt to decide whether a covered request may run. A malformed binding or unexpected remaining cgroup fails closed. Unknown outcomes beyond the barrier keep their normal recovery behavior and remain charged.

Existing context files load unchanged. Only an explicit retirement upgrades the binding. Older binaries reject the upgraded context shape, so downgrading cannot silently forget the barrier. Removing/replacing the binding or restoring an older backup is unsupported. Guest root can tamper with guest state; the host must retain its own independent barrier before it discards its records. A guest acknowledgement is not proof of VM destruction, host resource release, physical erasure or sandbox isolation.

## Required platform coordination

These steps are the remaining delivery contract, not implemented behavior:

| Layer | Required behavior before automatic retirement |
| --- | --- |
| Database preparation | Serialize preparation with admission under the same allocation lock. Reserve a monotonic per-domain barrier only when every admitted operation in its prefix has a known terminal outcome and all necessary output/source retirement has been verified. Preserve original identity, owner, key, request digest/version and outcome for the project's lifetime. Do not skip an unresolved operation merely because later operations are eligible. |
| New admission | Mint every new operation above the reserved barrier under that lock, including after clock rollback or concurrent frontend admission. Ordinary UUID generation alone is insufficient. Exact retries resolve their retained original identities before any new admission. |
| Host intent | Validate the exact approved owner, domain and prefix against retained command/file/archive evidence under the allocation gate. Durably install its own admission barrier before contacting the guest. Every delayed begin/execute/write/commit/cancel/archive path must respect it; old inspection must report retired history rather than inventing not-started evidence. |
| Guest acknowledgement | Call the authenticated guest primitive for the original bound boot. Reconcile a lost reply through the retained barrier. Validate the response scope and sufficient floor; an untrusted guest must not enlarge the prefix approved by the database/host or refund unrelated work. |
| Host completion | Discard eligible host records and release their reserved journal headroom only after the required guest acknowledgement. A separate path may use verified allocation destruction and exact ownership; a stopped flag, expiry or missing guest alone is insufficient. Keep the host barrier. |
| Database completion | Validate the retained retirement attempt and authenticated acknowledgement under the current claim, then mark only that approved prefix reclaimed. Admission accounting excludes it once, without reopening operations or freeing old idempotency keys. Cancellation, stale workers, corruption and lost acknowledgements retain enough intent to reconcile. |
| Later lifecycle | Snapshot/restore, previous-epoch recovery and whole-allocation journal retirement must preserve the admission barriers or establish a stronger independent fence. Never restore an old guest/context as authority for new work. |

Object-store markers have a different problem: an already issued conditional PUT may complete after a local timeout. Guest barriers do not fence that storage request. The [output](output-storage.md#storage-retirement) and [file-source](file-transfer.md#source-cleanup-worker) markers remain retained until a separate namespace/authority protocol proves old writers cannot recreate payloads. No marker deletion is added here.

## Evidence

The [recorded development evidence](evidence/2026-09-22-guest-history-barriers.json) contains matching source and Linux binary hashes, test totals and limitations.

The [protocol tests](../crates/sandbox-protocol/src/history.rs) cover binding compatibility, scope/domain validation, inclusive ordering and fail-closed downgrade. The unprivileged Linux [file tests](../crates/sandbox-guest/tests/files.rs) cover full descriptor/byte reservations, repeated retirement, admission after recovery, unchanged published files, unresolved prefixes, pre-barrier write failure, partial deletion, failed unlink, malformed bindings and symlink safety.

The opt-in [guest-runner tests](../crates/sandbox-guest/tests/linux_runner.rs) run real commands in a dedicated Linux VM with cgroup v2. They cover future-deadline replay rejection after receipt/output removal, process-tree cleanup, exhausted output reservations, newer active work, unknown outcomes, wrong ownership, failed barrier persistence and partial-deletion recovery. Crash windows are represented by controlled on-disk states and failed filesystem operations; this is not a power-loss/filesystem certification test.

Run the ordinary affected tests on Linux, and run the privileged tests only inside the dedicated development VM:

```sh
cargo test -p sandbox-protocol -p sandbox-guest
sudo env HUDSON_GUEST_TEST_VM=1 cargo test -p sandbox-guest --test linux_runner -- --ignored --test-threads=1
```

Hosted PR runners compile but do not execute the privileged suite. These checks establish guest-local behavior. They do not establish host/database reclamation, automatic reservation refunds, supported x86_64 release readiness or hostile-workload isolation.
