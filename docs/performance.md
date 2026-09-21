# Performance and resource budgets

Status: proposed targets. Nothing here is measured; no implementation exists to measure. This document owns latency and size budgets, the constraints those budgets place on designs we have already selected, and how results must be reported. [Product goal](goal.md) owns scope; [lifecycle](lifecycle.md) owns correctness of the same operations; [roadmap](roadmap.md) owns when each target must be demonstrated.

## Why targets exist before code

A secure runtime that is correct but slow is not a usable runtime. Create latency decides whether short workloads are worth running at all, and resume latency decides whether pause/resume is worth shipping when it arrives in Phase 4. Neither number is visible in a correctness document.

Writing targets now makes two things explicit: which numbers decide whether the design works, and which deferred optimizations the current format choices must not block. The create and execute budgets apply to the first usable milestone; the snapshot budgets apply at the pause/resume gate but constrain the format from the moment it is designed.

These numbers are opening positions for the first spike, not commitments to callers. Replace each target with a measured value and a recorded host configuration as evidence arrives, or change the target and say why.

## Proposed budgets

Targets assume the host in [supported configuration](compatibility.md#host), a 2 vCPU / 1 GiB sandbox as the reference size, and a warm host image cache. The largest supported sandbox is 4 vCPU / 8 GiB, so the memory-dependent rows scale to eight times the reference figure at the ceiling.

| Path | Proposed target | Current evidence |
| --- | --- | --- |
| Create: admission to guest readiness | Under 2 s | Not measured |
| Execute: dispatch to first output byte on a running sandbox | Under 250 ms | Not measured |
| Pause: freeze to published snapshot and confirmed compute release | Under 10 s at 1 GiB memory | Not measured |
| Resume: admission to released customer processes, same host, cached snapshot | Under 3 s at 1 GiB memory | Not measured |
| Resume: admission to released customer processes, different host, cold fetch | Under 15 s at 1 GiB memory | Not measured |
| Snapshot size published per GiB of sandbox memory | Under 1.1 GiB before compression | Not measured |
| Streaming: reconnect to first replayed byte | Under 500 ms | Not measured |

Scale each memory-dependent target with configured memory rather than treating the 1 GiB figure as a fixed ceiling. Record the host CPU model, disk class, and object-storage endpoint alongside every result; a number without its host configuration is not evidence.

## The dominant cost is snapshot bytes

A pause writes the sandbox's full memory to object storage, and a cross-host resume reads it back before any customer process runs. At 1 GB/s of usable throughput, a 4 GiB sandbox spends roughly four seconds moving bytes in each direction with everything else perfect. Real throughput to an S3-compatible endpoint is frequently lower.

[Lifecycle](lifecycle.md#pause) selects full snapshots as the initial format, and [roadmap](roadmap.md#implementation-phases) defers differential snapshots, lazy loading, and warm pools to Phase 6. That sequencing is reasonable. The risk is not the deferral itself: it is publishing a snapshot format and object layout that makes the deferred work impossible without breaking every existing snapshot.

## Constraints on designs we are choosing now

These apply during the first implementation even though the optimizations they enable are deferred.

- **Page-addressable memory objects.** The manifest must describe the memory component's chunk or page layout, chunk size, and offsets so a restore can fetch ranges instead of the whole object. [Data models](data-models.md#6-snapshots--saved-sandbox-state) owns the manifest fields.
- **Ranged reads over whole-object reads.** Restore must be able to issue ranged requests against published components. Do not build a restore path whose only mode is downloading one object to local disk.
- **A versioned manifest.** `manifest_version` must allow a later differential or lazily loaded format to coexist with published full snapshots rather than invalidating them.
- **Room for a host-local snapshot cache.** Host capacity accounting must be able to represent cached snapshot bytes as distinct from allocation disk and snapshot staging, so a cache can be added without reworking reservations. [Architecture](architecture.md#deployment-and-placement-boundary) owns capacity accounting.
- **Restore timing recorded by stage.** Instrument fetch, disk restore, memory load, guest handshake, and process release separately. A single total hides which stage needs the optimization.

Meeting these constraints costs little now. Retrofitting them after snapshots exist in customer storage is expensive.

## How results are reported

Report p50 and p95 over at least twenty runs, cold and warm separately, with the host configuration, sandbox size, snapshot size, Firecracker version, and guest image digest. State whether the object storage was local or remote to the host.

A single successful run is not a measurement. A measurement on a developer laptop is not evidence for a KVM host target. Do not publish a target as achieved without the run data behind it.

## Acceptance checks

No benchmarks exist yet. Implement measurement for:

1. Each row of the budget table, on a supported host, with recorded configuration.
2. Restore timing broken down by stage, so the dominant cost is identifiable rather than assumed.
3. Snapshot size against configured memory, including any compression actually used.
4. Resume from a cold host with no cached snapshot bytes, which is the case the targets exist to protect.
5. Behavior at the largest supported sandbox size, not only the 1 GiB reference.

## Open decisions

Choose compression, chunk size, whether a host-local snapshot cache ships before multiple hosts, and the throughput assumption used for capacity planning. The memory ceiling is settled at 8 GiB in [supported configuration](compatibility.md#sandbox). Decide whether any target becomes a published service expectation or stays an internal engineering budget. See [roadmap](roadmap.md#implementation-phases) for when these must be demonstrated.
