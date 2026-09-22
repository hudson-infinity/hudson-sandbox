# Whole-allocation retirement

Status: database preparation, host fencing and recoverable guardian metadata deletion are implemented components. Controller handoff, database completion and bounded forgetting remain incomplete. This document extends [history reclamation](history-reclamation.md) for the remaining host/guardian lifecycle work in [issue #79](https://github.com/hudson-infinity/hudson-sandbox/issues/79). It does not authorize manual deletion or claim a passing release gate.

## Problem and current ownership

Domain retirement removes eligible command/file records while preserving the allocation tombstone. The [host journal](../crates/sandbox-supervisor/src/host/journal.rs) still permits at most 1,024 allocation records and 16 MiB. Repeated create/destroy cycles therefore eventually exhaust host admission even when every command and file prefix has been reclaimed. Raising the cap postpones the failure.

The retained record is an authority, not just a log. [Normal host admission](../crates/sandbox-supervisor/src/host/mod.rs) uses it to reject changed ownership and delayed Create. [Previous-epoch recovery](../crates/sandbox-supervisor/src/host/previous.rs) and [destruction retirement](../crates/sandbox-supervisor/src/host/released_history.rs) require its exact original owner and independently observe cleanup. Deleting it cannot make a subsequent lookup return a new allocation or a successful absence proof.

Guardian state has an additional entry point. [Prepare and fence-unstarted](../crates/sandbox-supervisor/src/guardian/mod.rs) use the allocation directory and its lifecycle lock. [Launch and namespace initialization](../crates/sandbox-supervisor/src/guardian/process.rs) run separately from the host RPC handler. A host-epoch check alone does not fence a delayed process carrying an old manifest. Removing a locked directory also permits another process to create a different lock inode at the same path.

## Implemented guardian component

The Linux [launch authority](../crates/sandbox-supervisor/src/launch_authority.rs) now persists the bounded permit ledger and current epoch outside allocation directories. A stable cross-process lock serializes authority updates with guardian prepare, launch, namespace initialization, guest binding and renewal. Fenced or stale permits deny these actions; inspection and stop remain available. Fencing does not stop a running VM or prove physical cleanup.

Activation is explicit and restricted to a fresh, private root-owned allocation root. Hosts provisioned with `launch_permits_required: true` use [authenticated permit registration](allocation-authority.md#fresh-host-registration-and-launch), durable checkpoints and permit-bearing manifests. Legacy hosts retain legacy manifests; legacy migration and automatic metadata deletion are not integrated. Quiesce older binaries before activation. An activated root rejects legacy manifests; missing, corrupt, oversized, linked or mismatched authority fails closed. Independently retained epoch/frontier bounds detect rollback below those checkpoints, not arbitrary coherent filesystem rollback.

The root retains `launch.lock`, `launch.required` and `launch.json`; atomic updates use one `launch.next` staging file. The activation record binds the stable lock inode. An interrupted staging file cannot turn the root into legacy mode or replace a missing authoritative state. A failed fence may leave the previous valid grant active: callers must retain original metadata until a successful durable acknowledgement. `complete` and `forget` trust their caller to verify physical cleanup and database completion; neither performs deletion or establishes those proofs.

Controlled development-VM validation is recorded in [guardian authority evidence](implementation/guardian-authority-evidence.md). This component does not complete the whole-allocation retirement protocol or authorize production tombstone deletion.

## Required replacement authority

Before any tombstone can disappear, a durable authority outside its directory must deny its original launch identity. The authority must cover host RPC admission, guardian prepare, launch, namespace initialization and recovery paths. A check followed by an unlocked launch is insufficient: retirement and the final launch decision must serialize across processes. The lock that provides this serialization must survive allocation-directory deletion.

The replacement must stay bounded under sustained reuse, including while an older sandbox remains live. Neither a permanent tombstone per retired allocation nor a single prefix blocked indefinitely by the oldest live allocation meets that requirement. A proposed implementation must explicitly account for its retained active exceptions, retirement intents and acknowledgements in the same capacity model; it cannot move an unbounded list into another file.

Admission identities must be issued by trusted control-plane code and bound to host, project, sandbox, allocation, generation and original epoch. Wall-clock age, a caller-selected UUID, a local timeout and a release flag are not replacement authorities. Restart must retain the fence, reject rollback and fail closed on missing/corrupt authority. Upgraded state must be rejected by binaries that do not enforce the replacement fence.

An epoch-based design must additionally show how admission resumes under sustained same-process operation and how active workloads behave during epoch advancement. An ordered-admission design must synchronize ID issuance and barrier reservation in the database and preserve older active owners explicitly. The implementation uses ordered permits and retains active owners explicitly. End-to-end bounded retirement is still incomplete; records cannot be deleted merely because their original epoch is old.

## Preparation and consumer closure

The database owns preparation under the allocation lock. It freezes the exact original owner and a versioned retirement identity before dispatch. Eligibility requires all of the following:

- Verified allocation release or fencing, with the original release evidence retained in the database.
- Every admitted operation has a known outcome and its original project-lifetime retry key, digest and result remain readable. Unknown outcomes cannot be converted to not-started by deletion.
- All command/file history and their output/source consumers have independently verified retirement. Empty domains need explicit closure; absence of a domain row is not an acknowledgement.
- No pending lifecycle, previous-epoch recovery, archival or history request still requires the host record to prove its result. Late responses cannot reopen eligibility or replace frozen evidence.
- Further work for that allocation is closed under the same admission lock. No request admitted concurrently can fall outside the approved retirement scope.

Host receipt deletion does not delete project-lifetime operation identities. Storage-marker deletion is a separate protocol: a delayed object-store PUT is not fenced by a host epoch or guardian launch fence.

## Frozen request vocabulary

The shared [allocation retirement types](../crates/sandbox-protocol/src/allocation_retirement.rs) define a bounded version-1 intent and a separate renewable request envelope. The database preparation path below now persists this intent. The host fencing and metadata-retirement RPCs consume the same immutable intent.

The immutable intent binds a retry-stable retirement ID to the complete allocation permit, explicit command and file closures, a lowercase SHA-256 digest of retained release evidence, and whether the evidence is simulated. Each domain is explicitly `empty` or `retired` through an operation ID. These are proposed scopes requiring independent verification; an empty scope never substitutes for host acknowledgement. The release digest identifies evidence and does not prove cleanup.

The envelope carries reporting epoch, claim revision and expiry. Validation compares the entire intent against independently retained scope and checks the receiver's epoch, minimum retained revision and clock. A newer claim cannot change ownership, closures or simulation status. The database must separately require its exact current claim and deadline when accepting completion. Parsing alone does not establish those facts or authenticate a caller.

Both decoders reject payloads over 8 KiB, unknown fields, duplicate fields, unsupported versions and invalid identities. The intent digest uses its typed JSON encoding; it is a stable scope identifier, not a signature. Tests cover scope changes for every immutable field, renewal across reporting epochs, stale/expired claims, missing domains and ambiguous or oversized encodings.

## Database preparation component

Migration 0019 adds `allocation_retirements`, retaining one immutable scope and renewable claim per allocation. `Store::prepare_allocation_retirement` locks the allocation using the same lock as command/file admission. It requires an exact issued permit acknowledged by a permit-enabled host at the reporting epoch, a released allocation, original terminal create/release operations and the retained release receipt. A pre-dispatch rejection is accepted only for the original create with zero dispatch attempts and retained rejection evidence. This is a scheduling prerequisite; the host must still independently verify actual cleanup.

Preparation rejects unfinished lifecycle work, unknown command/file outcomes, active maintenance/history claims, output claims and unfinished output/source retirement. Lifecycle operations are conservatively scoped to the sandbox because the current schema does not pin create/destroy rows to allocations. Existing history validators check original outcomes and consumer evidence in batches of 32; preparation requires verified completion through each domain's actual final admitted operation. An empty domain is explicitly frozen empty, awaiting independent host verification. An expired live-history request may be superseded only when a verified completion covers its reserved prefix.

A held preparation claim returns no work. After expiry, preparation revalidates the original scope and keeps the same retirement identity while incrementing its claim revision; a changed owner, release receipt or domain cannot replace the stored intent. All leases use database time. Allowing simulation marks the frozen intent simulated even if a particular input receipt is physical, so a later retry cannot silently upgrade a simulation-enabled preparation.

The retained freeze rejects further command/file IDs under the allocation lock and stops new released-history claims. Original project-lifetime operation keys, digests and retained outcomes are untouched. Database rows remain retained; this component neither refunds host receipt capacity nor deletes anything. It does not inspect host-local download pins or physical files: those remain mandatory host-side completion checks. There is no periodic preparation worker, database deletion acknowledgement or forgetting integration yet. The host fencing RPC below is callable but has no automatic controller handoff yet.

Database tests use synthetic release/history receipts, not isolation evidence. They exercise concurrent claims, retry stability across epochs, changed evidence, explicit empty scopes, pending outcomes and consumers, malformed completion, simulation policy, and admission rejection after freezing. Existing store migration and history suites remain required alongside these tests.

## Host fencing component

The authenticated `FenceAllocation` supervisor RPC accepts the bounded retirement request and returns its exact request with an observation time only after durable root launch denial. It is unavailable on legacy hosts and the fake host. Simulated intents, wrong owners, stale epochs/frontiers and changed scopes are rejected. A stopped original owner may come from an earlier epoch; a registered unused permit may have no host receipt yet. Admitting that retirement receipt does not claim physical absence.

The host checks its independently retained permit frontier under the persistent shared root lock, verifies the exact permit and closed command/file prefixes, then durably saves the full request under the per-allocation gate. Pending history, remaining command/file/archive records, changed release-evidence digests and changed claims under the same revision cannot pass. Subsequent generic mutation, archive/history and previous-epoch recovery requests receive retirement-in-progress rejection rather than a new release result. Live/file reader admission now takes a shared per-allocation pin under the journal mutex that checks ownership and stopped state. A pin stays held through the final host ownership check, including detached file work. After persisting intent and installing launch denial, fencing takes the exclusive pin without waiting and rejects outstanding readers or pending/unreleased capture tickets. Unknown capture outcomes retain their monotonic 60-second ticket lifetime; acknowledged releases do not block this handoff. Retry reconciles after readers finish or tickets expire. This drains host consumers only: guest process and filesystem cleanup remain independently required before deletion.

Only after persisting scope does the host release the shared lock and install the exclusive root fence. A failed fence can leave the earlier root grant active, but retains the host intent and returns no successful acknowledgement. Retrying the identical scope reconciles that window; a newer database claim can renew revision/epoch without changing scope. Restart retains the intent and rejects older binaries through the journal's strict unknown-field decoding.

This RPC neither verifies physical cleanup nor removes metadata, marks authority complete, forgets an owner, or frees host capacity. Guardian receipts and manifests remain available to existing restart validation. The metadata-retirement RPC below persists recoverable intent before unlinking and makes restart validation distinguish authorized partial deletion from unexplained missing files. The [host fencing evidence](evidence/2026-09-22-host-retirement-fence.json) records controlled validation separately from the unfinished deletion and release gates.

## Host metadata deletion component

The authenticated `RetireAllocationMetadata` RPC consumes the same bounded claim and reconciles `FenceAllocation` first. It rechecks the exact current claim under the allocation gate, closes host readers, and acquires the persistent root authority lock exclusively. It requires the exact fenced permit and retirement ID; active, forgotten, simulated, foreign and changed scopes cannot authorize deletion. The acknowledgement echoes the exact request and observation time. No controller dispatches this RPC automatically yet.

For a staged guardian, the host independently verifies the original manifest and stopped cleanup receipt, absence of the owned cgroup, and a free original lifecycle lock. Only `receipt.json`, `manifest.json` and `lifecycle.lock` may remain. Runtime files, unknown entries, symlinks, hard links, nonprivate files and changed ownership prevent deletion. The host records directory/file identities, bounded file hashes, the original cleanup receipt and the immutable intent digest in its journal before the first unlink. For an unused registered permit, it requires absence of both the allocation directory and cgroup; it retains an explicit unused-owner plan and does not fabricate an ordinary release receipt.

Removal unlinks only those verified files, syncing the allocation directory after each step and the root after removing the empty directory. The root authority files remain. Guardian cleanup now shares the persistent authority gate; a fenced cleanup can use existing metadata but cannot recreate a missing directory or lifecycle lock. A delayed wrapper carrying the old manifest therefore cannot repopulate a retired directory.

Restart verifies the retained plan against the frozen scope, independently retained registration frontier and exact root fence. It permits missing inventory entries only with that saved deletion plan, rejects changed surviving files, and uses the retained validated cleanup receipt for original command/file boot-history checks. A saved completion requires directory absence. An interrupted prepared record can finish deletion or reconcile a lost completion save; missing original files without a plan still fail startup. Old binaries reject the new strict journal field.

This RPC retains its original host record, manifest, history floors and completion plan. The root entry stays fenced; it is not marked complete or forgotten. Database completion, controller recovery callers and bounded removal of these protocol records remain necessary before the 1,024-record admission limit can recover. Host metadata retirement alone is not a release or distribution gate.

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

Reader closure validation is recorded separately in [reader-drain evidence](evidence/2026-09-22-retirement-reader-drain.json), including the initial parallel privileged test failure and the serial rerun.

[Metadata-retirement evidence](evidence/2026-09-22-allocation-metadata-retirement.json) records partial-unlink recovery, retained-history restart checks and controlled host validation, including their limits.
