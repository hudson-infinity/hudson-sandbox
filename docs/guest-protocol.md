# Authenticated guest protocol

Status: the guest listener and supervisor client perform real command, receipt and output calls over Firecracker vsock. [Recorded aarch64 evidence](evidence/2026-09-21-aarch64-vsock.json) covers that boundary. Public execute admission, controller dispatch, the real lifecycle supervisor, independent host watchdog and file transfer remain unfinished. Guest reports do not prove host isolation or allocation release.

## Transport and peer identity

[The shared protobuf](../proto/guest.proto) defines `hudson.guest.v1`. Each connection carries one request and one response, each prefixed by a four-byte big-endian message length. Version 1 supports hello, execute, inspect, cancel and bounded output reads. Keep field numbers stable; file transfer and pause/resume messages are not implemented.

The guest uses [tokio-vsock](https://docs.rs/tokio-vsock/0.7.2/tokio_vsock/) and accepts only host CID 2. The supervisor connects to an absolute, operator-configured Unix socket created by Firecracker, rejects a symlink/non-socket endpoint, and performs the [Firecracker CONNECT handshake](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/vsock.md#host-initiated-connections). The acknowledgement names the assigned host-side port, which need not equal the requested guest port. Customer input must never select this Unix path or the TLS configuration; its parent directory must be controlled by the operator.

Both ends then require TLS 1.3, mutual certificate verification against the configured private CA, an exact SHA-256 peer leaf-certificate pin, and ALPN `hudson-guest/1`. The host additionally verifies the server name `allocation-<allocation UUID>.sandbox.internal`. The guest pins the authorized supervisor's client certificate; a different CA-valid client is insufficient. There is no plaintext mode, CA-only identity bypass, early data, or automatic certificate rotation. [Shared TLS code](../crates/sandbox-protocol/src/guest_wire.rs) uses Rustls's [required client verifier](https://docs.rs/rustls/0.23.45/rustls/server/struct.WebPkiClientVerifier.html).

Provision a guest certificate/key scoped to its allocation, the CA certificate and the supervisor's public certificate pin inside the guest. Keep supervisor private keys, CA signing keys, platform database credentials and Project/Admin tokens outside it. Guest root may steal its own agent key or falsify guest reports; this channel does not make that guest honest. Certificate issuance, durable boot registration, renewal and the protected guest-agent image remain integration work. A reused allocation certificate or unprotected host socket directory is not a supported provisioning shortcut.

## Identity and request flow

Every request carries protocol version, a fresh typed request ID, allocation ID, positive generation and boot ID. The response must echo the request ID and matching context. Missing/unspecified actions, wrong allocation/generation/boot context and malformed frames are rejected. Protobuf unknown fields retain normal protobuf forward-compatibility semantics; explicit protocol versions and known action values still govern dispatch.

A hello request may omit the boot ID to discover the current one. [The host client](../crates/sandbox-supervisor/src/guest.rs) returns that context without changing its existing binding. Commands require a client explicitly bound to the discovered boot. The future supervisor must persist and reconcile that binding under its allocation ownership before dispatch; discovery alone is not authorization to start a replacement VM or repeat uncertain work.

The [guest listener](../crates/sandbox-guest/src/server.rs) dispatches authenticated calls into the [command runner](guest-runner.md). Execute still commits launch intent before launching. Inspect returns the existing receipt, cancellation persists the request before acknowledgement, and disconnecting does not cancel admitted execution. A sent frame does not prove admission: connection failure can happen before execution, after admission or after a side effect. The client never automatically retries a command.

The initial wire errors are `invalid`, `not_found`, `rejected` and `uncertain`, without arbitrary error strings or command contents. Execute/cancel runner failures conservatively return `uncertain`, including currently undifferentiated admission conflicts/capacity failures; the caller must inspect the original operation and compare its digest. A not-found guest receipt is not proof that another boot or a missing journal never ran it. More specific public API error mapping remains part of execute admission work.

Host response validation checks correlation, context, known result type, receipt version/state, output counters, digest length, exit-code/signal shape, and cleanup consistency. Execute additionally compares the request digest, deadline and output reservation. Output responses must match operation, stream, offset, next offset and requested length. These checks bound and validate an untrusted report; they do not establish that the reported side effect or cleanup truly happened. Only host-owned observations can release a VM allocation.

## Bounds and shutdown

| Resource | Bound |
| --- | --- |
| Guest connections | 32, including stalled TLS handshakes; excess connections close |
| TLS handshake | Five seconds |
| Frame read/write | Five seconds per complete frame, including a slowly delivered header/body |
| Whole connection | 15 seconds, including host CONNECT/TLS/request/response |
| Encoded protobuf | 1–128 KiB; length checked before allocating its body |
| Output read | 1–32 KiB per call, within the runner's retained reservation |
| Guest request | The stricter argv/environment/request limits in [guest runner](guest-runner.md#bounds-and-retained-state) still apply |

The listener reaps completed connection tasks and holds its semaphore permit for the whole handler. It does not queue unbounded connection work. Dropping a connection handler after its deadline cannot undo already committed execution intent. Blocking guest storage/kernel failures still require an independent host watchdog; these async deadlines cannot substitute for it.

Output requests choose only a typed operation ID, stdout/stderr, offset and length. They cannot supply a filesystem path. The runner opens without following a final symlink, rejects nonregular/oversized files, and bounds reads. `at_end` describes the current retained file; `complete` is false while execution is active or its result is unknown. Readers must not confuse a current empty read with final output. Both data and guest counters remain untrusted, and guest output may contain secrets. Public output storage, authorization and streaming are not implemented by this protocol.

On SIGINT/SIGTERM, the guest server stops accepting work and asks the runner to cancel its active command, waiting up to ten seconds for confirmed cleanup. An unconfirmed shutdown returns failure and retains state for recovery. Host-side watchdog enforcement remains required for process death, hung kernel I/O and malicious guest root.

## Development entry point

Inside an operator-prepared Linux guest, use `sandbox-guest serve --help`. Supply the allocation ID/generation, private state directory, dedicated cgroup subtree, CA/certificate/key files and exact supervisor certificate pin. Port 52 is the default. The existing single-request diagnostic invocation remains available. No TCP listener or unauthenticated testing switch is exposed by the binary.

The production caller is the supervisor library's `GuestClient`, constructed from operator-owned endpoint/TLS configuration and an explicitly bound allocation context. It exposes hello, execute, inspect, cancel and output calls. The controller-to-supervisor [gRPC service](supervisor-protocol.md) still lacks execution dispatch; this client must be integrated under durable host allocation ownership rather than exposed directly to customers.

## Evidence and remaining gates

[Portable wire/TLS tests](../crates/sandbox-protocol/tests/guest_wire.rs) cover framing round trips, zero/oversized/truncated/invalid frames, read/write deadlines, wrong CA, wrong exact peer, wrong allocation name, plaintext/stalled TLS and redacted command/output Debug values. [Host client tests](../crates/sandbox-supervisor/tests/guest_client.rs) reject malformed CONNECT replies and forged correlation, identity, receipt and output fields. These run in normal CI without VM privileges.

Nine [opt-in Linux tests](../crates/sandbox-guest/tests/linux_runner.rs) passed, including authenticated host-client calls into real process execution, hello binding, duplicate/conflicting execute, inspect/cancel/output, stale context, disconnect after admission, output symlink/offset rejection and orderly active-command shutdown. The broader existing process/recovery cases remain covered. The Unix-socket test fixture models Firecracker's routing acknowledgement; it is not a VM isolation test.

The separate [real microVM experiment](evidence/2026-09-21-aarch64-vsock.json) uses actual guest AF_VSOCK and the host Firecracker Unix socket, with ephemeral mutually authenticated TLS identities. It observes exit code 9, guest UID 0, output delivery, timeout, cancellation, exact retry without repeating a side effect and changed-payload rejection. It also holds 32 incomplete TLS handshakes, observes rejection of an additional call and recovery of connection capacity after closing them. The guest server then shuts down and the actual Firecracker child exits; its host cgroup and disposable rootfs are removed.

That experiment uses the [development kernel/Firecracker artifacts](evidence/2026-09-21-aarch64-boot.json) and a one-off BusyBox image fixture. It is not a production image builder, supported x86_64 test or automated release gate. Hostile guest-root impersonation, resource/network attacks, host partitions, real supervisor recovery and public API-to-VM execution remain unverified. The [Phase 1 gates](roadmap.md#scope-discipline-for-phase-1) and [threat-model validation](threat-model.md#required-validation) remain open.
