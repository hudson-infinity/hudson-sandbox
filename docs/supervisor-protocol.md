# Supervisor protocol and development fake

Status: the versioned gRPC transport and an in-memory fake are implemented. The [create controller](controller.md) now uses this transport. Authenticated host registration, certificate provisioning, and the real Firecracker supervisor remain unfinished. A fake observation never establishes that a VM exists, ran code, enforced limits, or released real resources.

## Contract and identity

[The protobuf source](../proto/supervisor.proto) generates Rust messages, a client, and a server at build time through `tonic-prost-build`. The package is `hudson.supervisor.v1`. Keep wire field numbers stable; do not reuse removed numbers. The protocol currently covers health, create, inspect, and stop. Execute, output, file transfer, and lease renewal still need protocol and implementation work.

Every request uses gRPC over mutual TLS. [Shared transport](../crates/sandbox-supervisor/src/transport.rs) requires a trusted private CA and client certificate. The server interceptor also compares the connected client's leaf-certificate SHA-256 fingerprint against one or two configured controller certificates. Another CA-valid certificate, even with the same subject name, has no authority. The second pin supports an explicitly configured rotation overlap. No raw Project or Admin token is accepted on this interface.

The client verifies the server's CA chain and the exact DNS name `host-<host UUID>.sandbox.internal`, derived from its expected host ID. Endpoint addresses are operator configuration, never customer request fields. The transport has no plaintext or certificate-verification bypass. TLS handshakes and client requests have five-second bounds; metadata messages are limited to 64 KiB in both directions. These are control-message limits, not output-stream limits.

Controller identity authorizes the trusted control-plane service, not arbitrary tenant requests. The controller must still check persisted project authority before dispatch. It must bind the configured endpoint and certificate identity to an authenticated registered host and current epoch; a successful TLS connection alone is not host registration.

The implementation uses [tonic's server TLS configuration](https://docs.rs/tonic/0.14.6/tonic/transport/server/struct.ServerTlsConfig.html) and [protobuf generation](https://docs.rs/tonic-prost-build/0.14.6/tonic_prost_build/). Test certificates are generated ephemerally with rcgen; no test private keys are checked in.

## Ownership and observations

Create, inspect, and stop carry the host, project, sandbox, allocation, operation, allocation generation, supervisor epoch, controller claim revision, and absolute claim deadline. IDs must have their canonical typed UUIDv7 form. Revisions and generations are positive. A request for the wrong host or epoch is rejected.

Claim deadlines must be in the future and at most 300 seconds away when the supervisor handles the request. Allocation lease deadlines have the same bound. Wall clocks must be synchronized; after validation, the fake converts the allocation deadline to a monotonic local timer. Controller claim expiry never extends an allocation lease.

An observation echoes the requested ownership tuple, names the original create operation when known, includes its observation time, and distinguishes `absent`, `ready`, and `released`. The controller must verify the complete tuple and expected evidence before changing PostgreSQL state. `unspecified` is never a usable result. Every fake response sets `simulated=true`, including health. The create controller requires explicit development opt-in before accepting simulated evidence and preserves that distinction in receipts and public state.

`Absent` means this supervisor has no evidence for that allocation. It is not confirmation that another epoch stopped the VM, permission to free its reservation, or permission to repeat an uncertain command. Database intent, authenticated observations, and the [lifecycle reconciliation contract](lifecycle.md#destroy-and-recovery) govern the next action.

## What the fake exercises

[The fake library](../crates/sandbox-fake-host/src/lib.rs) runs no subprocesses. It models the following control-plane behavior:

- A repeated create for the same allocation, operation, image, resources, and original allocation deadline returns the existing result. It neither starts again nor extends the lease. Changed create input conflicts.
- A higher observed claim revision fences older requests for that operation, including when inspection found no allocation. Identity bindings and fences survive simulated release.
- A stop received before create prevents a delayed create from starting that allocation. Stopping a known allocation is idempotent; replaying its old create returns released state.
- A sandbox can have a new generation only after the previous recorded incarnation is released. Changing project, sandbox, or generation under an allocation ID is rejected.
- An explicit digest allowlist, per-sandbox resource bounds, and aggregate CPU/RAM/disk capacity checks control simulated admission. They do not verify image bytes or enforce hardware resources.
- The binary checks allocation leases independently every 100 ms; expired allocations become simulated released records. Calls also check expiry before serving observations. There is no lease-renewal RPC yet.
- A test hook can lose a create acknowledgement after recording the start. Inspection then reports the original single start.

The fake bounds retained allocation/fence records to 10,000 and operation revisions to 64 per allocation. It rejects additional records rather than evicting deduplication or fencing evidence. Restart clears memory: supply a new externally assigned epoch. Do not reuse an epoch to make lost evidence appear authoritative. Production epoch issuance and durable supervisor receipts are still required.

## Run the fake

Install the pinned Rust toolchain and `protoc` (`brew install protobuf` on macOS; `sudo apt-get install protobuf-compiler` on Ubuntu). CI installs the same compiler package on its Ubuntu runner. Generated Rust files stay in Cargo's build output; the `.proto` file is their source of truth.

The binary requires operator-supplied CA/server certificate files, a server private-key file, controller certificate fingerprints, a host ID, an explicit epoch, and allowed image digests:

```sh
cargo run -p sandbox-fake-host -- --help
```

The server certificate must contain the host-specific DNS identity above and permit server authentication; controller certificates must permit client authentication. Keep private keys outside the repository. Compute each controller fingerprint from its DER certificate bytes with SHA-256; a PEM file's text hash is not the fingerprint. Certificate issuance, trusted local provisioning, and rotation tooling remain separate work.

The default listen address is `127.0.0.1:7443`; non-loopback addresses are rejected. There is no unauthenticated development mode. On restart, use a new epoch and reconcile existing reservations instead of assuming the empty fake proved release. Use an isolated development database when the controller integration is available.

## Evidence and remaining gates

[State-machine tests](../crates/sandbox-fake-host/tests/state.rs) cover concurrent duplicates, changed retries, lost acknowledgements, stale claims after absence, stop-before-create, cross-project/allocation identity, generation replacement, limits, deadlines, and monotonic lease expiry. [Loopback TLS tests](../crates/sandbox-fake-host/tests/tls.rs) exercise actual gRPC calls, configured certificate rotation, same-CA unauthorized peers, missing/untrusted client certificates, wrong host identities, plaintext rejection, and server message bounds.

These tests establish the modeled behavior and transport checks only. Further lifecycle integration, real certificate lifecycle, host watchdog fencing under partitions, Firecracker/jailer, image-byte verification, filesystem isolation, resource enforcement, and network policy require their own evidence. [Phase 1](roadmap.md#scope-discipline-for-phase-1) cannot pass on fake-host tests.
