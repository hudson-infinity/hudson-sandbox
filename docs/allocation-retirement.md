# Whole-allocation retirement

Status: proposed protocol requirements; no whole-allocation deletion is implemented. This document extends [history reclamation](history-reclamation.md) for the remaining host/guardian lifecycle work in [issue #79](https://github.com/hudson-infinity/hudson-sandbox/issues/79). It does not authorize manual deletion or claim a passing release gate.

## Problem and current ownership

Domain retirement removes eligible command/file records while preserving the allocation tombstone. The [host journal](../crates/sandbox-supervisor/src/host/journal.rs) still permits at most 1,024 allocation records and 16 MiB. Repeated create/destroy cycles therefore eventually exhaust host admission even when every command and file prefix has been reclaimed. Raising the cap postpones the failure.

The retained record is an authority, not just a log. [Normal host admission](../crates/sandbox-supervisor/src/host/mod.rs) uses it to reject changed ownership and delayed Create. [Previous-epoch recovery](../crates/sandbox-supervisor/src/host/previous.rs) and [destruction retirement](../crates/sandbox-supervisor/src/host/released_history.rs) require its exact original owner and independently observe cleanup. Deleting it cannot make a subsequent lookup return a new allocation or a successful absence proof.

Guardian state has an additional entry point. [Prepare and fence-unstarted](../crates/sandbox-supervisor/src/guardian/mod.rs) use the allocation directory and its lifecycle lock. [Launch and namespace initialization](../crates/sandbox-supervisor/src/guardian/process.rs) run separately from the host RPC handler. A host-epoch check alone does not fence a delayed process carrying an old manifest. Removing a locked directory also permits another process to create a different lock inode at the same path.

## Required replacement authority

Before any tombstone can disappear, a durable authority outside its directory must deny its original launch identity. The authority must cover host RPC admission, guardian prepare, launch, namespace initialization and recovery paths. A check followed by an unlocked launch is insufficient: retirement and the final launch decision must serialize across processes. The lock that provides this serialization must survive allocation-directory deletion.

The replacement must stay bounded under sustained reuse, including while an older sandbox remains live. Neither a permanent tombstone per retired allocation nor a single prefix blocked indefinitely by the oldest live allocation meets that requirement. A proposed implementation must explicitly account for its retained active exceptions, retirement intents and acknowledgements in the same capacity model; it cannot move an unbounded list into another file.

Admission identities must be issued by trusted control-plane code and bound to host, project, sandbox, allocation, generation and original epoch. Wall-clock age, a caller-selected UUID, a local timeout and a release flag are not replacement authorities. Restart must retain the fence, reject rollback and fail closed on missing/corrupt authority. Upgraded state must be rejected by binaries that do not enforce the replacement fence.

An epoch-based design must additionally show how admission resumes under sustained same-process operation and how active workloads behave during epoch advancement. An ordered-admission design must synchronize ID issuance and barrier reservation in the database and preserve older active owners explicitly. Neither design is selected or implemented here; these obligations rule out deleting records merely because their original epoch is old.

## Preparation and consumer closure

The database owns preparation under the allocation lock. It freezes the exact original owner and a versioned retirement identity before dispatch. Eligibility requires all of the following:

- Verified allocation release or fencing, with the original release evidence retained in the database.
- Every admitted operation has a known outcome and its original project-lifetime retry key, digest and result remain readable. Unknown outcomes cannot be converted to not-started by deletion.
- All command/file history and their output/source consumers have independently verified retirement. Empty domains need explicit closure; absence of a domain row is not an acknowledgement.
- No pending lifecycle, previous-epoch recovery, archival or history request still requires the host record to prove its result. Late responses cannot reopen eligibility or replace frozen evidence.
- Further work for that allocation is closed under the same admission lock. No request admitted concurrently can fall outside the approved retirement scope.

Host receipt deletion does not delete project-lifetime operation identities. Storage-marker deletion is a separate protocol: a delayed object-store PUT is not fenced by a host epoch or guardian launch fence.

## Durable sequence

The eventual implementation must provide the following ordering, with bounded retained intent and an exact authenticated acknowledgement at each handoff:

1. **Prepare in the database.** Freeze original identity, consumer closure and a retry-stable retirement intent. A new worker claim may change its revision/deadline, not the approved allocation or evidence.
2. **Install the replacement fence.** Serialize against every launch/admission entry point and persist denial before deleting any original record. A failed save leaves the original state intact and prevents further mutation until reconciled.
3. **Verify actual release.** Under allocation ownership and the persistent cross-process serialization, verify guardian exit, cgroup absence and runtime-file cleanup. Check original ownership; do not signal a saved arbitrary PID or treat missing metadata as proof.
4. **Delete owned metadata recoverably.** Persist enough intent to distinguish interrupted authorized deletion from unexplained absence. Remove only the verified allocation's files, reject symlinks/unowned entries, sync changed directories and preserve the replacement authority. Retrying after partial deletion uses this intent, not a newly invented absence receipt.
5. **Acknowledge to the database.** Return exact original identity, retirement identity, current reporting epoch, verified release and completed cleanup. The database rechecks its claim and frozen preparation before persisting completion. Lost replies must be reconciled even when the old host record is gone.
6. **Retire acknowledgement state.** Delete per-allocation protocol metadata only once a bounded replacement authority and database completion make that metadata unnecessary. An old request must then receive an explicit retired response, never a new start or fabricated release result. This final step is required for capacity recovery, not optional housekeeping.

A retired response means that an authority rejects the request. It is not interchangeable with `Released` or `FencedAbsent`: those observations currently prove exact original cleanup. Existing recovery callers must consume retained database completion or reconcile the retirement protocol, rather than translating missing host state into release.

## Acceptance evidence

The implementation is incomplete until these cases are linked to executable tests and controlled real-host evidence:

| Boundary | Required observation |
| --- | --- |
| Repeated reuse | More than 1,024 create/execute/destroy/retire cycles admit successfully with bounded host journal, guardian metadata and protocol receipts; retain an older live sandbox during the run |
| Delayed Create | Original request before intent, after fencing, during deletion and after acknowledgement cannot relaunch or allocate fresh ownership |
| Delayed guardian process | Pause after manifest read, after prepare, before namespace initialization and immediately before launch; resume after retirement and prove no VM or cgroup is recreated |
| Lock identity | A process waiting on the old allocation lock cannot bypass the persistent fence after directory removal/recreation |
| Consumer races | Unexpired output, unretired source, unknown command, unfinished upload and concurrent admission prevent preparation or completion |
| Crash windows | Stop after each durable write, fence installation, unlink, directory sync and acknowledgement; restart reconciles without weakening ownership or losing original outcomes |
| Lost acknowledgements | Repeat identical intent under a new claim and reporting epoch; recover completion after original host metadata is removed |
| Corrupt or missing state | Fail closed on lost authority, malformed intent, wrong owner, changed scope, stale claims, rollback and older binaries |
| Recovery callers | Previous-epoch release and domain-history workers cannot reinterpret retired/missing state as an independent release proof |
| Tenant and generation boundaries | Another project, allocation, generation, host or epoch cannot consume the proof or remove metadata |
| Retained results | Original API retry keys, request digests and known outcomes remain unchanged after reclamation |
| Filesystem and resource state | No owned cgroups, guardian processes or runtime files remain; unrelated files and the live sandbox stay intact |
| Storage authority | Permanent object-store markers remain until their separate old-writer fence has been verified |

Unit/state tests establish ordering and malformed-input behavior. Database tests establish admission/consumer serialization. Controlled Linux/KVM tests establish process and filesystem behavior. Full supported-host isolation and distribution gates remain governed by the [roadmap](roadmap.md); a passing retirement test does not replace them.
