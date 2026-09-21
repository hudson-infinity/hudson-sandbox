# Implementation notes — temporary

**Everything in this directory is disposable.** These are working notes for building what the rest of the documentation already decided. A file here is deleted once its work is done and the evidence is linked from the permanent document that owns the contract.

Nothing in here is authoritative. If a note and a contract document disagree, the contract document is right and the note is stale.

## What persists and what does not

| Persists forever | Deleted as work lands |
| --- | --- |
| [Decisions](../decisions/README.md) — what we chose and why, including rejected options | Task lists and checklists |
| [Architecture](../architecture.md), [supported configuration](../compatibility.md), [networking](../networking.md), [threat model](../threat-model.md) | Spike sheets and their raw measurements |
| The contracts: [lifecycle](../lifecycle.md), [API](../api-contract.md), [data models](../data-models.md), [auth](../auth-design.md), [UI](../ui-design.md) | Notes on how to wire a specific crate |
| [Product goal](../goal.md), [alternatives](../alternatives.md), [performance](../performance.md), [roadmap](../roadmap.md) | Anything phrased as "next, do X" |

The reasoning is the asset. A decision record explains why the system is shaped this way to someone reading it in three years, after every task list here is long gone. Keep writing records for anything hard to reverse, and never delete one — supersede it instead.

## Deletion protocol

When a note's work is finished:

1. Move anything durable into the owning document — a measured number into [performance](../performance.md), a confirmed mechanism into [architecture](../architecture.md), a changed choice into a new [decision record](../decisions/README.md).
2. Link the actual test or migration from that document's acceptance checks.
3. Delete the note in the same pull request that lands the work.

A note surviving past its work is a bug. If half of one is done, delete the done half.

## Current notes

| Note | Covers | Delete when |
| --- | --- | --- |
| [Development environment](dev-env.md) | Where each component runs while there is no hardware | `development.md` and `self-hosting.md` exist and work |
| [Phase 0 spikes](phase-0-spikes.md) | The eight hardware questions | All eight have written findings and the durable ones have moved into the contracts |
| [Phase 1 tasks](phase-1-tasks.md) | The walking skeleton, then proving it does not lie | Both gates pass: it works, and it reports honestly under injected failure |
