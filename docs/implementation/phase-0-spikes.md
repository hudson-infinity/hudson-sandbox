# Phase 0 spike sheet — temporary

Delete this file once every question has written findings and the durable results have moved into [performance](../performance.md), [architecture](../architecture.md), or a [decision record](../decisions/README.md). See [the deletion protocol](README.md#deletion-protocol).

The unanswered questions below still gate the supported release. [Nested-aarch64 boot](../linux-development.md#verified-boot-and-its-limits), guardian and guest tests provide development findings; they do not establish the selected x86_64 limits, hostile-workload or performance claims. Record the answers and preserve useful regression tests.

## Before anything

- [ ] Provide an x86_64 KVM host matching [supported configuration](../compatibility.md#host) for supported-envelope acceptance. Native control-plane work and nested-aarch64 runtime development already run from macOS.
- [ ] Record the exact CPU model, RAM, disk class, kernel version, and Firecracker release in this file. Every measurement below is meaningless without them.
- [ ] Boot one Firecracker microVM by hand, through the jailer, from a Debian rootfs.

## The questions

Each one needs a written answer, the commands that produced it, and a verdict: does the design survive as written.

### 1. Do the limits actually hold? — gates Phase 1

- [ ] CPU cap: run a busy loop with more threads than vCPUs, confirm the cgroup ceiling holds.
- [ ] Memory cap: allocate past the limit, confirm the kernel kills inside the VM and the host is unaffected.
- [ ] Disk cap: fill the writable layer, confirm it stops at the reservation and not at the host's free space.
- [ ] Egress: from inside, attempt the metadata address, a host-only address, and an unapproved public address. All three must fail.

### 2. What does boot cost? — gates Phase 1

- [ ] Time create-to-guest-ready, warm image cache, twenty runs. Record p50 and p95 against the [create budget](../performance.md#proposed-budgets).
- [ ] Same, cold cache.

### 3. Does a frozen cgroup stay frozen across snapshot and restore? — gates Phase 3

The single most important question in this sheet. The entire controlled-resume contract assumes yes.

- [ ] Start a process that appends a timestamp to a file every 100 ms.
- [ ] Freeze the workload cgroup. Snapshot. Restore on the same host.
- [ ] Inspect the file: is there a gap, and did anything get written between restore and the explicit thaw?
- [ ] Repeat with the restore on a different host.

### 4. Can the guest agent reconnect over vsock after restore? — gates Phase 3

- [ ] Confirm the vsock connection resets on restore, as Firecracker documents.
- [ ] Re-establish it from inside the restored guest and complete a handshake.
- [ ] Measure how long the reconnect takes; it sits on the critical path of every resume.

### 5. What happens to guest time, timers and TCP across a long pause? — gates Phase 3

- [ ] Pause for an hour. On restore, check `CLOCK_REALTIME` and `CLOCK_MONOTONIC` inside the guest.
- [ ] Check whether a sleeping timer fires immediately, late, or correctly.
- [ ] Check an open TCP connection: does it error, hang, or silently misbehave.
- [ ] Decide what the guest agent must do about each before releasing customer processes.

### 6. Can expired process groups be killed before any thaw? — gates Phase 3

- [ ] With the workload still frozen, kill a process group from the guest agent. Confirm it dies without ever being scheduled.
- [ ] Confirm the kill is observable to the agent, so the outcome can be recorded honestly.

### 7. What do pause and cold cross-host restore actually cost? — gates Phase 3

- [ ] Pause a 1 GiB sandbox: time the freeze, the capture, the upload, and the confirmed release separately.
- [ ] Record the published snapshot size against the [size budget](../performance.md#proposed-budgets).
- [ ] Restore on a second host with no cached bytes. Time the fetch, disk restore, memory load, handshake, and release separately — a single total hides which stage needs the optimization.
- [ ] Repeat at 8 GiB, the [supported ceiling](../compatibility.md#sandbox).

### 8. How well can the guest agent be shielded from a root customer? — gates Phases 1 and 3

[Decision 0003](../decisions/0003-guest-root-with-our-kernel.md) grants root deliberately and calls this hardening rather than a boundary. Measure how much hardening there actually is.

- [ ] As root in the workload namespace: attempt a broad kill sweep. Does it reach the agent?
- [ ] Attempt to see the agent's processes, reach its control socket, and write to its cgroup.
- [ ] Attempt to load a kernel module and to disable lockdown.
- [ ] Attempt to forge the resume handshake from the workload side.
- [ ] Record which attacks succeed. Each success either gets a mitigation or gets written into the [threat model's non-promises](../threat-model.md#what-we-do-not-promise).

## Exit

- [ ] Eight written answers, with commands and host configuration.
- [ ] Measurements moved into [performance](../performance.md), replacing the proposed targets.
- [ ] Any design change written as a new decision record, not as an edit to this file.
- [ ] If process-continuous resume proved unreachable, reopen [alternatives](../alternatives.md#revisit-triggers) before starting Phase 3.
- [ ] Delete this file.
