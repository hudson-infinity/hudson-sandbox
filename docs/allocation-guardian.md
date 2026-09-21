# Linux allocation guardian

Status: implemented component, with controlled aarch64 Linux/KVM evidence. The root-only `sandbox-supervisor` CLI stages and owns a real Firecracker allocation. The [real lifecycle supervisor](real-supervisor.md) now connects it to controller dispatch; public execute, files, and network policy remain separate work. [Guest bootstrap and durable boot binding](guest-bootstrap.md) now connect the guest channel to this owner. The supported x86_64 release gates remain open.

The implementation is in [guardian](../crates/sandbox-supervisor/src/guardian/mod.rs), [process ownership and control](../crates/sandbox-supervisor/src/guardian/process.rs), and [the irrevocable deadline](../crates/sandbox-supervisor/src/lease.rs). The existing [supervisor RPC](supervisor-protocol.md) supports both the development fake and the real host adapter. The authenticated [guest protocol](guest-protocol.md) is a separate component to integrate with this owner.

## Ownership and process lifetime

An operator supplies a trusted manifest containing host, project, sandbox, allocation, create-operation, generation and supervisor epoch; absolute artifact paths and SHA-256 digests; private state/cgroup roots; jailer UID/GID; resource bounds; and an initial lease. No customer argv, environment, or shell program becomes a host launch command.

The allocation directory retains an immutable manifest, its digest, a receipt, and a lifecycle lock. Exact retries return the retained incarnation; changed manifests conflict. Preparation copies and hashes the actual staged bytes before marking them usable. A partial preparation is fenced and cleaned up rather than silently restarted.

The single-threaded CLI wrapper creates private mount and PID namespaces and starts a separate guardian as namespace PID 1. The guardian holds the allocation lock until process death. It records launch intent and the cgroup identity before starting the jailer. A delayed second guardian can start only from `prepared`; recovery fences `launch_intent` and later states before cleanup. Recovery never signals an arbitrary saved PID.

The jailer parent may exit successfully while Firecracker is alive. The guardian reads the actual Firecracker child PID from the jailer pidfile, checks process presence, and reaps all adopted children. `running` means this host process was observed, not that the guest booted, authenticated, or can execute commands. It must not be mapped directly to public sandbox readiness.

The retained state progression is:

```text
staging -> prepared -> launch_intent -> running -> stopping
    any recoverable unfinished state -> fenced -> stopped
```

Only `stopped` has `cleanup_confirmed=true`. Cleanup verifies the owned cgroup, kills remaining owned processes if necessary, waits for it to become empty, removes the cgroup and staged runtime files, and durably records completion. It retains the manifest and receipt for reconciliation. An unknown populated cgroup is not killed or reported released: cleanup requires the matching host boot and recorded cgroup inode. These receipts do not themselves release PostgreSQL reservations; the [host adapter and controller](real-supervisor.md) validate ownership and cleanup before committing database release.

## Independent deadlines

The guardian uses host `CLOCK_BOOTTIME` for an allocation deadline, initially derived from a bounded wall-clock lease of at most 300 seconds. A separate watchdog thread checks it independently of control handlers and journal locks. Before launching the VM, the guardian opens the owned `cgroup.kill` endpoint. On expiry or a terminal deadline, the watchdog writes to that retained descriptor before terminating namespace PID 1. The kill request does not acquire journal locks or reopen paths on the state filesystem. Namespace teardown provides a second termination mechanism. Killing the outer supervisor wrapper cannot disable this watchdog; ordinary guardian SIGKILL is separately tested through namespace teardown.

Zero is an irrevocable terminal deadline. Renewal requires a live allocation, a positive revision, and no shortening. Exact revision/deadline retry is accepted; stale or conflicting revisions are rejected. A new revision is persisted before compare-and-swap extends the live deadline, using fresh clocks. Stop or expiry winning that race cannot be overwritten. Losing the response is uncertain and requires inspection, not blind relaunch.

A stop request records stopping and fences the deadline, including when its metadata write fails. A stalled metadata operation may consume the remaining lease, but cannot make the independent watchdog wait on that operation. Termination and cleanup are separate: after losing the outer wrapper, a later explicit reconciliation still has to confirm cgroup and file cleanup. Neither elapsed controller time nor a `running` receipt proves release.

The journal-stall regression freezes a disposable ext4 state filesystem and confirms a guardian writer is in uninterruptible (`D`) sleep. Before the direct cgroup kill, the VM continued executing after expiry. With it, the original vCPU stops and measured CPU usage stops while storage remains frozen; cleanup is confirmed only after thaw and reconciliation. A simultaneous guardian SIGKILL and blocked journal write has not been validated by this test.

This is not a hard real-time guarantee under a starved or suspended host, nor protection against trusted host root disabling the guardian. The watchdog polls every 10 ms when scheduled. End-to-end partition tests, host registration/epoch ownership, and replacement-allocation safety remain release requirements.

## Staging and resource controls

The state root and allocation directory must be private and root-owned. Operators must place them under trusted ancestors on local durable storage. Artifacts are regular files opened without following a final symlink; staging copies bounded bytes, verifies their configured SHA-256, and syncs data and metadata before launch. Digests authenticate against the operator's selection; this component does not provide a signed image registry or choose a trusted kernel for the operator.

The fixed launch supports 1–4 vCPU, 128–8192 MiB guest memory and a 64–65536 MiB rootfs backing file. The host cgroup enforces a CPU quota, memory equal to guest memory plus 128 MiB of VMM allowance, and 64 host tasks. The backing file is allocated to the configured disk size before launch. This bound covers the writable rootfs, not total installation storage or copied kernel/VMM artifacts. Guest filesystem growth within that file and host-wide disk admission remain image/controller responsibilities.

Firecracker runs through the verified jailer with an unprivileged host UID/GID, its own PID namespace, and no NIC. The guardian and watchdog stay outside the VM cgroup. A separate fixed-size read-only bootstrap drive provisions the [guest channel](guest-bootstrap.md); the writable rootfs remains a separate artifact. No NIC in this component is not evidence for the planned resolver, egress allowlist, network namespace or bandwidth enforcement. Guest userspace remains writable; the product's [guest-root contract](decisions/0003-guest-root-with-our-kernel.md) is unchanged.

## Local control and use

Build on Linux with `cargo build -p sandbox-supervisor`. Commands take a root-owned operator manifest via `--manifest /absolute/path/manifest.json`:

| Command | Result |
| --- | --- |
| `prepare` | Verify/stage once, or return the retained receipt |
| `run` | Start an eligible guardian and wait for it; an active exact retry inspects the same owner |
| `inspect` | Query the live guardian |
| `bind-guest` | Authenticate the guest and durably bind its boot identity |
| `renew --revision N --expires-unix-ms T` | Submit a fenced lease renewal |
| `stop` | Fence the live owner and request teardown |
| `reconcile` | Fence and clean a dead owner; refuses while its lifecycle lock is held |

`prepare`, `run`, and `reconcile` print a receipt. Live control commands print a response with `receipt` or `error`; callers must inspect that field, not only the CLI exit status. A lost stop response is not a failed stop, and a successful stop response is not cleanup confirmation. Reconciliation returns a retained stopped receipt after cleanup.

The Unix control socket is private and root-only, with kernel peer-UID verification on both sides. Ancestor-namespace clients can have PID zero as viewed by the guardian, so authentication reads the UID from the raw `ucred` structure rather than treating peer PID as identity; see [Linux Unix socket credentials](https://man7.org/linux/man-pages/man7/unix.7.html). No user namespace is entered. Frames carry a big-endian u32 length and at most 64 KiB of JSON, with at most eight simultaneous handlers. Complete server reads and writes each have a 500 ms budget; client reads/writes each have a two-second budget. Client connect is nonblocking so a full listener backlog fails promptly. Those transport budgets do not cancel a handler already committing a mutation; its outcome remains uncertain if the client leaves.

This is an operator diagnostic interface, not an installer, customer API, or supported hosted service. A prepared manifest is immutable, including the initial lease; renewal is a separate control operation.

## Verification and limits

The [controlled VM tests](../crates/sandbox-supervisor/tests/guardian.rs) require root, the isolated [Linux development VM](linux-development.md), static BusyBox, `mkfs.ext4`, loopback ext4 mounts, `fsfreeze`, Python 3, and the verified aarch64 Firecracker/jailer/kernel artifacts already recorded in that guide. Run only reviewed local source on that dedicated host:

```sh
sudo env HUDSON_GUARDIAN_TEST_VM=1 cargo test -p sandbox-supervisor --test guardian -- --ignored --nocapture --test-threads=1
```

The fixture builds its own minimal writable ext4 image, using either a busy-loop init or the real guest bootstrap init for channel tests. It is not the production Debian/guest-agent image. Failed cleanup retains fixture files instead of deleting potentially live state. The journal-stall test mounts and freezes only a newly created, exclusively owned loopback filesystem, with an independent timed thaw helper and unwind cleanup. Ordinary CI leaves these privileged tests ignored and exercises portable models plus Linux control framing.

The VM tests cover real cgroup limits/backing-file size, active exact retry without another incarnation, rejection of conflicting/stale renewals and wrong ownership, renewal beyond the original deadline, normal stop, supervisor loss followed by independent expiry, guardian SIGKILL without a surviving supervisor, stop/renew concurrency, stalled clients during expiry, staging digest failure, and recovery fencing before delayed launch. Ordinary death/expiry tests inspect cgroup emptiness before explicit recovery. The storage-stall test instead measures original vCPU liveness and cgroup CPU usage before thaw, while requiring cleanup to remain unconfirmed. It then checks stopped state and retry fencing after storage recovers.

See the [journal-stall before/after evidence](evidence/2026-09-21-aarch64-journal-stall.json) and [original guardian validation](evidence/2026-09-21-aarch64-guardian.json) for source hashes, environment and results. This evidence covers component behavior on a nested aarch64 development host. It does not establish adversarial escape resistance, host-wide admission, customer command delivery through the API, guest readiness, production image/certificate lifecycle, network policy, x86_64 compatibility, or a completed Phase 1 gate.
