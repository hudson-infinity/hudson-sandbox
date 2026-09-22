# Bounded allocation authority model

Status: implemented state model, not connected to runtime admission or deletion. [The protocol module](../crates/sandbox-protocol/src/allocation_authority.rs) and [its tests](../crates/sandbox-protocol/src/allocation_authority_tests.rs) explore the replacement authority required for whole-allocation reclamation in [issue #79](https://github.com/hudson-infinity/hudson-sandbox/issues/79). Existing host/guardian tombstones remain mandatory.

## Serial registration and retained owners

A trusted database issuer assigns a positive host-local serial to each immutable allocation identity: host, project, sandbox, allocation, create operation, generation and original supervisor epoch. This is an internal ordering value, not a timestamp, bearer credential or customer-controlled ID. Serial values are bounded by the signed 64-bit database range and never wrap.

The model registers contiguous batches of at most 32 permits, starting immediately after its durable high-water mark. Registration retains each owner explicitly before Create can run; registered launches may then arrive in any order. A missing batch, mixed old/new batch, duplicate owner, duplicate create operation, duplicate sandbox generation or capacity overflow fails without changing state. A wholly retained exact retry does not reactivate fenced entries.

The authority retains at most 1,024 entries, including outstanding retirement/completion records. After a completed entry is forgotten, its serial stays below the high-water mark and cannot be registered again. Older live allocations remain explicit exceptions, so they do not pin newer retired entries. No permanent record per deleted allocation is needed in this model.

The trusted issuer must never assign another serial to an already issued allocation/create identity, including after its host entry is forgotten. Host state cannot prove that global uniqueness after deleting the identity; the project-lifetime database record must enforce it. This remains an integration requirement, not behavior supplied by the model.

## Retirement stages

| State | Launch allowed by model | Required external evidence before next transition |
| --- | --- | --- |
| Active | Yes, only for the exact registered permit | Database preparation freezes owner and all consumer closure before fencing |
| Fenced | No | Persistent cross-process launch denial, verified original-owner release, completed and synced metadata deletion |
| Complete | No | Durable database acknowledgement of the exact retirement identity |
| Forgotten | No; lookup returns `Closed` | No new admission or release inference is permitted for that serial |

A retirement ID stays fixed across retries. Another ID cannot replace it, completion cannot skip fencing, and forgetting cannot skip completion. A late fence retry cannot downgrade a completed record. All three retained stages consume capacity; capacity returns only when the completed record is forgotten. There is no unfence method.

`Closed` means denied by serial ordering. It does not establish that the supplied owner ever existed, that a VM stopped, or that an object was deleted. After forgetting, even a different owner presented at the same serial receives denial; the original owner is deliberately no longer available as evidence. Callers must use their retained database results and cannot translate this response into `Released` or `FencedAbsent`.

## Persistence boundary and integration still required

The model is deliberately in memory. Transitions must be written durably before acknowledgement or effects; mutating the value does not write a journal, hold a guardian lock or verify cleanup. Serialization validates version, host, serial bounds/order, unique retained identities, UUID variants, state shape and a 1 MiB input limit. Unknown fields and duplicate fields fail closed. A caller supplies an independently retained minimum high-water mark when loading. That check can detect a lower frontier, but cannot detect every self-consistent rollback or prove that a serialized completion is true.

Runtime adoption still requires all of the following:

- Transactional host-local serial issuance under a database lock, with no unrepresented gaps after rollback or cancellation. A PostgreSQL sequence alone is insufficient because it can leave gaps. Every issued serial must reach registered/fenced authority before later registration can pass it.
- Ordered, authenticated registration and exact acknowledgement recovery before dispatching Create. An interrupted batch cannot silently discard an admitted operation; registration state must reconcile against original database identities.
- A durable host authority and persistent cross-process lock shared by admission, guardian prepare, launch and namespace initialization. A check followed by launch outside that serialization is unsafe.
- Versioned manifests and host journal integration, fail-closed downgrade, rollback protection, missing-authority handling and a migration that retains every legacy owner until its replacement authority is committed.
- Database consumer closure, verified physical release, recoverable owned-file deletion, exact completion acknowledgements and acknowledgement retirement. The state model accepts trusted transition calls; it does not validate external observations.
- Independent storage-writer fencing before deleting permanent object-store markers. This serial authority only concerns allocation admission and launch.

Do not use a missing authority file as permission to call `Authority::new`, delete a directory after an in-memory `fence`, or treat successful decoding as cleanup evidence. No runtime path currently does any of those things through this module.

## Validation scope

The model suite runs 4,096 newer allocation registrations/retirements while the oldest permit remains active, serializing and reloading at retirement boundaries. It also fills all 1,024 slots, confirms that fenced/completed records remain charged, frees an interior slot and admits newer work without reopening it. Other tests exercise out-of-order launches after registration, changed identities/intents, registration atomicity, serial exhaustion, malformed storage, duplicate fields and rollback below an independent frontier.

These are deterministic state-machine tests on the Mac, not 4,096 microVM lifecycles or crash/power-loss tests. No supported-host security claim follows from them. Full database/host/guardian integration and controlled real-host evidence remain necessary before any tombstone deletion can be enabled.
