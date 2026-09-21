# Allocation bootstrap and guest boot binding

Status: implemented under the real [allocation guardian](allocation-guardian.md), with controlled aarch64 microVM tests. A reusable rootfs containing our init now boots an allocation-specific authenticated guest agent. The guardian can bind its boot ID and supply a host command client. The real controller/RPC driver, public execute and files/output routes, production Debian images, networking and supported x86_64 release gates remain unfinished.

## Reusable image, separate identity

The operator still selects a kernel and immutable rootfs by verified digest. The guardian copies the rootfs into private writable allocation storage. It does not edit the selected image to insert credentials or mount a guest filesystem on the host.

Preparation issues a new channel identity for the allocation: a private CA and separate guest/server and host/client certificates. The CA signing key is used in memory and is never persisted. Its two leaf certificates have distinct key pairs and the required TLS roles. The guest server name is `allocation-<UUID>.sandbox.internal`; both endpoints pin the exact permitted peer certificate using the existing [guest TLS contract](guest-protocol.md#transport-and-peer-identity).

The root-only `run/identity.json` stores the host-side identity, including the guest bootstrap and host client key. The guest receives only the CA certificate, its own server certificate/key, the host's public certificate fingerprint, allocation ID, generation, format version and validity deadline. No host client key, CA signing key, Project token, database credential or controller credential enters its device. Debug formatting redacts credential fields.

A separate 128 KiB read-only virtio block device carries this bootstrap. Its wire format is eight-byte magic `HDSBOOT1`, a big-endian u32 body length, up to 64 KiB of strictly parsed JSON, and zero padding. Individual credential fields are bounded to 16 KiB. The fixed resource overhead is additional to the writable rootfs reservation. It is not a filesystem and requires no host mount. The guardian verifies the complete device bytes against its retained identity before launch; the guest reads the bounded body from `/dev/vdb`.

The identity content digest and expiration are committed into the allocation receipt before the prepared state. Repeated preparation preserves the identity; missing, corrupt or changed bootstrap state rejects launch and is fenced by recovery. A crash during preparation cannot silently generate replacement credentials and resume the old incarnation. Destroy removes the runtime directory containing both the host identity and guest bootstrap; retained receipts contain only identity metadata, never keys. Removal is not a secure-erasure guarantee for underlying storage. Older prepared allocations without bootstrap material are rejected at launch and require reconciliation; existing stopped receipts remain readable.

## Validity and leases

[Identity issuance](../crates/sandbox-supervisor/src/identity.rs) uses the pinned rcgen dependency with [explicit certificate validity](https://docs.rs/rcgen/0.14.10/rcgen/struct.CertificateParams.html), a one-minute not-before allowance and a 24-hour not-after boundary. That initial lifetime is a component constraint, not automatic credential rotation or a new customer duration entitlement. It is independent of the shorter renewable allocation lease.

Initial and renewed allocation leases must end strictly before the retained identity deadline. Host client construction and guest startup reject expired identity metadata; TLS independently validates certificate time on each connection. The guest service also stops when its initial validity timer elapses. The host guardian's independent lease watchdog remains authoritative if guest time or guest control is compromised. Clock synchronization remains an operator responsibility. Extending an allocation beyond its issued identity lifetime requires future renewal/rotation work; changing the initial manifest or regenerating keys is not a valid retry.

## Guest init

[The guest boot implementation](../crates/sandbox-guest/src/boot.rs) is selected when `sandbox-guest` is the initial PID 1 with no arguments, or by its explicit guest-only `boot` command. An image places that binary at `/init`, with its runtime libraries and a writable userland.

Init mounts guest proc, sysfs, devtmpfs and cgroup v2, creates `system` and `workloads` cgroups, and enables the process controller. A single-threaded helper joins `system`, enters a separate PID/mount namespace, and starts the agent there as namespace PID 1. The agent remounts proc for its namespace, reads the bootstrap block device, initializes the existing runner, and serves authenticated vsock port 52. Customer commands remain root, execute in their own process namespace, and retain writable userspace. No host command argv comes from the bootstrap.

This structure is guest hardening, not a new trust boundary against guest root. Root may steal its own channel key, alter guest-visible cgroups or state, and falsify reports. Kernel module/lockdown requirements, stronger guest hardening, production image packaging and adversarial testing remain open. The host watchdog, resource enforcement and peer ownership remain necessary. Agent exit is not an automatic workload restart or replay; the host must reconcile its loss.

## Binding before commands

`bind-guest` is a new root-only guardian control action. It requires a live allocation, loads the retained identity, and performs authenticated hello. The guardian persists the returned boot ID before acknowledging the binding. A later binding must match the existing boot; it cannot silently accept another boot and repeat uncertain work. Concurrent stop/expiry prevents a successful live binding, and uncertain binding persistence fences the allocation.

The handshake happens outside the receipt mutex, so it does not block lease/stop processing. It retains the guest protocol's 15-second connection budget and the guardian's eight-handler limit. The local control client's two-second response budget can expire first: inspect the receipt to reconcile that uncertainty. A timed-out call does not undo a committed binding.

`Manifest::guest_client()` requires a fresh live-guardian observation, an existing durable boot binding and unexpired retained credentials. It returns the existing authenticated `GuestClient` bound to that allocation/generation/boot, with no command retries. This is a host component interface; it does not implement public authorization, operation admission, controller revision fencing or execution persistence.

Long jail paths can exceed Unix socket pathname limits. On Linux, the client opens the operator-owned jail directory without following a final symlink and keeps the descriptor alive. It reaches the socket through a short `/proc/self/fd/<fd>/vsock.sock` path. Customers do not select this directory or descriptor. The final endpoint must still be a socket; the same TLS identity and context checks apply.

A receipt's `running` state remains host process presence. A stored guest boot ID proves a previous authenticated binding, not continuing guest health, honest output or VM cleanup. The future real supervisor driver must distinguish those observations and handle agent loss. Only confirmed host cleanup can support allocation release.

## Validation

[Protocol tests](../crates/sandbox-protocol/src/bootstrap.rs) exercise bounded decoding, version/magic rejection, truncation, expired bootstrap and redacted diagnostics. [Identity tests](../crates/sandbox-supervisor/src/identity.rs) establish a real TLS channel, reject another allocation's credentials, and reject expired endpoint construction. Existing guest-wire and host-client tests still enforce peer pins, boot/context correlation and no automatic retry.

The controlled [guardian suite](../crates/sandbox-supervisor/tests/guardian.rs) uses the locally built release guest binary, its libraries and BusyBox in an ext4 fixture. Build the guest on the isolated Linux development host before running the reviewed root-only tests:

```sh
cargo build --release -p sandbox-guest
sudo env HUDSON_GUARDIAN_TEST_VM=1 cargo test -p sandbox-supervisor --test guardian -- --ignored --nocapture --test-threads=1
```

The fixture invokes `ldd` only on the reviewed, locally built guest binary and `mkfs.ext4 -d` on a fresh fixture directory; it never mounts a guest image on the host. This is not a distributable Debian image builder or a production installer.

The new real-VM cases confirm unbound command-client rejection, durable/repeated boot binding, root execution and output, a command exit code, exact retry without a repeated write, changed-command rejection, same-size bootstrap corruption fencing, read-only bootstrap enforcement against a guest-root write, and two allocations using one unchanged base image with different credentials/boot IDs. A credential from the first allocation cannot authenticate to the second; stopping one leaves the other running. The existing lease expiry, supervisor/guardian death, stop/renew race and cleanup tests remain applicable.

The [recorded evidence](evidence/2026-09-21-aarch64-bootstrap.json) identifies the tested sources, guest binary and host artifacts. It establishes component behavior on the nested aarch64 development host, not x86_64 compatibility, adversarial isolation, API-to-VM execution, network policy, production images or completed Phase 1 release gates.
