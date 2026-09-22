# Acknowledged history reclamation

Status: guest and host command/file retirement are implemented with authenticated internal RPCs. No controller or cleanup worker invokes them automatically. Database coordination, public reservation refunds, retirement after allocation destruction, and storage-marker lifecycle remain unfinished under [issue #79](https://github.com/hudson-infinity/hudson-sandbox/issues/79). Public runtime capacity limits are unchanged. This document owns the retirement barrier and the prerequisites for enabling it across the platform.

## Why deletion needs an admission barrier

A retained receipt prevents a delayed request from looking like a new operation. Removing the receipt without retaining a fence can execute a command or publish an upload again. An elapsed retention deadline, a successful object DELETE, or a cancelled local request does not provide that fence.

The [barrier type](../crates/sandbox-protocol/src/history.rs) closes an inclusive prefix of operation IDs for one allocation, generation, guest boot and domain (`commands` or `files`). IDs are compared as opaque ordered values. Their UUID timestamps are not an expiry clock and are not assumed to match admission order. A retained barrier rejects every later admission at or below `through`, including IDs for which no receipt remains. One monotonic value replaces the discarded per-operation admission records.

A caller must have durably retained the original outcomes and retired all consumers that still need guest bytes **before** asking the guest to remove history. The controller-only `RetireHistory` RPC asserts that these prerequisites have been durably satisfied. The host independently verifies allocation ownership, the original guest boot, and known terminal outcomes; it does not query the database or prove output/source retirement. No automatic caller or customer endpoint is enabled before the database coordination below is implemented.

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

## Host and authenticated transport

The controller-pinned supervisor service exposes `RetireHistory(HistoryRequest)`. Its ownership uses an independent per-domain claim revision and bounded deadline. It accepts a versioned guest barrier, never a host path or storage credential. The API/output/file-reader identities cannot call this mutation. The simulator explicitly rejects retirement because it cannot supply durable evidence.

Under the allocation gate, the host validates the exact project/sandbox/allocation/generation/host epoch and bound guest boot. Covered commands must be known terminal with confirmed cleanup, or have durable not-started fences. Covered uploads must be committed, aborted or durably not-started; `Unknown` is not sufficient. Newer work outside the prefix stays retained. A pending retirement must complete before its prefix can advance. Reusing a revision with a changed barrier or sending an older revision fails. A newer claim may reconcile an equal/lower prefix to the current barrier without regression.

The host persists the retirement intent in its existing journal before calling the guest over pinned mTLS/vsock. Both pending and completed barriers reject old command execution/inspection/cancellation, upload begin/write/inspection/commit/abort, archive setup/completion and live-output access. Missing discarded history never becomes new not-started evidence. An archive/read already in flight can fail; a late storage writer still requires the separate object-store retirement marker to fence its effects.

The guest returns a synced-deletion acknowledgement. The client validates response correlation and the **exact** approved barrier, including boot/domain/prefix. A guest cannot enlarge the prefix used for host reclamation. Timeout, lost response, malformed acknowledgement or expired claim retains the host floor and charged records. Repeating retirement reconciles guest completion; a floor-only getter is never enough.

Only after a valid acknowledgement does one durable host journal replacement remove the covered domain's records, associated command archives, and those operations' revision entries. The barrier remains. A failed journal write poisons the host until restart. Completed barriers survive journal reload; older binaries reject the added fields. The host checks journal byte headroom before installing a new barrier and rejects exhaustion before contacting the guest. This recovers operation slots and their declared budgets, but does not guarantee recovery from an already byte-full legacy journal.

New or unfinished retirement currently requires the original live guest in the current host epoch. A completed retry returns retained proof without requiring a live guest, including after same-epoch destruction. Restart advances the epoch and stops old ownership; pending retirement remains fenced and charged. A verified-destruction path is still required to reclaim that history without the original live guest. Database/public accounting is unchanged even when an internal retirement succeeds. Do not delete host journals, restore older snapshots, or discard storage markers as a substitute for that protocol.

## Required platform coordination

The table records the complete coordination contract. Host intent, guest acknowledgement and live-guest host completion are implemented above; database and later-lifecycle integration remain required:

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

The [host retirement evidence](evidence/2026-09-22-host-history-retirement.json) records source/binary hashes, authenticated microVM tests, failure corrections, excluded cases and the limits of these claims.

The [recorded development evidence](evidence/2026-09-22-guest-history-barriers.json) contains matching source and Linux binary hashes, test totals and limitations.

The [protocol tests](../crates/sandbox-protocol/src/history.rs) cover binding compatibility, scope/domain validation, inclusive ordering and fail-closed downgrade. The unprivileged Linux [file tests](../crates/sandbox-guest/tests/files.rs) cover full descriptor/byte reservations, repeated retirement, admission after recovery, unchanged published files, unresolved prefixes, pre-barrier write failure, partial deletion, failed unlink, malformed bindings and symlink safety.

The opt-in [guest-runner tests](../crates/sandbox-guest/tests/linux_runner.rs) run real commands in a dedicated Linux VM with cgroup v2. They cover future-deadline replay rejection after receipt/output removal, process-tree cleanup, exhausted output reservations, newer active work, unknown outcomes, wrong ownership, failed barrier persistence and partial-deletion recovery. Crash windows are represented by controlled on-disk states and failed filesystem operations; this is not a power-loss/filesystem certification test.

Run the ordinary affected tests on Linux, and run the privileged tests only inside the dedicated development VM:

```sh
cargo test -p sandbox-protocol -p sandbox-guest
sudo env HUDSON_GUEST_TEST_VM=1 cargo test -p sandbox-guest --test linux_runner -- --ignored --test-threads=1
```

The supervisor client tests reject altered acknowledgement scope/prefix and lost responses. Host state tests cover durable intent, stale claims, unknown outcomes, domain separation, retained capacity, failed journal writes and completion. Opt-in supervisor microVM tests exercise the authenticated RPC, full command/file slot recovery, delayed requests, same-epoch retries after destruction, host restart, malformed retained completion, staging refusal, failed host persistence, and an enlarged guest barrier. Direct guest calls model an acknowledgement lost before host completion; this is not packet-loss injection at every network boundary.

Hosted PR runners compile but do not execute the privileged suite. These checks establish guest and host behavior in the tested development configuration. They do not establish database reclamation, automatic public reservation refunds, supported x86_64 release readiness or hostile-workload isolation.
