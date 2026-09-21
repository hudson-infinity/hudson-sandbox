# Development environment — temporary

Delete this file when `self-hosting.md` and `development.md` exist and describe commands that actually work. See [the deletion protocol](README.md#deletion-protocol).

The service runs customer code in Firecracker microVMs, which needs Linux with KVM. Most of the codebase does not. This file is about which half you are working on, and where each half runs.

## Stage 0 — everything except a real VM, on macOS

The Rust libraries and local dependencies run natively today. The [HTTPS API binary and offline project provisioning](../api-server.md) now run locally; the public client CLI remains incomplete; the [create/destroy controller](../controller.md) now runs against one operator-provisioned host. [The authenticated fake supervisor](../supervisor-protocol.md) now runs on loopback with operator-supplied certificates. Install `protoc` before building (`brew install protobuf` on macOS).

```text
macOS
├── sandbox-api          HTTP, auth, admission
├── sandbox-controller   claims, placement, dispatch
├── sandbox-fake-host    answers the supervisor's gRPC interface from memory
└── docker compose       PostgreSQL 16, MinIO
```

```sh
make up      # PostgreSQL and MinIO, waits for both to be healthy
make check   # fmt, clippy, tests, documentation checks
make down    # stop, keeping data
```

`.env.example` holds the connection strings; copy it to `.env`. Those credentials are for the local stack only.

The stack binds PostgreSQL on 55432 and MinIO on 59000, not their defaults. If you already run PostgreSQL natively, the container binds `::` while `127.0.0.1` stays with your own server, and every connection from the host quietly reaches the wrong database — which surfaces as a missing role rather than a port conflict. Non-default ports remove the ambiguity. `make reset-db` drops the development schema when a migration changes underneath you.

Authentication, admission, claims, reservations, and read handlers have executable tests. The fake provides the shared gRPC create/inspect/stop service over mTLS and always reports simulated evidence. It starts no VM or process. Create and destroy are integrated through the controller; other operation kinds, streaming, the CLI, and real execution remain unfinished.

The fake models bounded resource accounting, stale ownership, lease expiry, duplicate requests, and lost acknowledgements for control-plane tests. Those models do not test host resource enforcement or hardware isolation; those require stage 1 and supported hardware.

## Stage 1 — real Firecracker, still on your Mac

`sandbox-supervisor` and `sandbox-guest` need cgroups, netlink, and vsock. They do not run on macOS at all. They run in a Linux VM on it.

Apple added nested virtualization for M3 and later on macOS 15 and up, so a Linux guest can itself expose `/dev/kvm` and run Firecracker.

```text
macOS                         Lima VM — Ubuntu arm64, nested virt
├── api + controller   ←───→  ├── sandbox-supervisor
├── postgres, minio           ├── firecracker (aarch64)
└── your editor               └── microVM + guest agent
    repo mounted into the VM, so Linux binaries build there
```

Sketch of the Lima configuration — **not yet verified on this machine**, so treat the first run as part of the work:

```yaml
vmType: vz
rosetta:
  enabled: false
nestedVirtualization: true
images:
  - location: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-arm64.img"
    arch: "aarch64"
mounts:
  - location: "~/Desktop/work/hudsonlabs/hudson-sandbox"
    writable: true
```

```sh
brew install lima
limactl start --name=sandbox ./lima-sandbox.yaml
limactl shell sandbox -- ls -l /dev/kvm   # the whole question, answered
```

If `/dev/kvm` is there, install Firecracker inside the VM, build the supervisor from the mounted repo, and point the controller on macOS at it. Then `hudson-sandbox exec` boots a real microVM on your laptop.

Two limits to keep in mind. This is **aarch64**, and the first release targets x86_64 ([supported configuration](../compatibility.md#host)) — logic transfers, architecture-specific behaviour does not. And timing under nested virtualization is not trustworthy, so no number measured here belongs in [performance](../performance.md).

## Stage 2 — rented x86_64 hardware

Needed for three things and nothing else: trustworthy performance numbers, genuine cross-host restore, and a CI runner for VM tests. Bare metal by the hour is enough for the [spikes](phase-0-spikes.md); a monthly box makes sense once the supervisor is real.

Not blocking stages 0 and 1. It is [still listed as blocking](../roadmap.md#still-blocking) because the phase gates cannot pass without it.

## Build the images for both architectures from the start

You will be on aarch64 locally and x86_64 in production for a long time. Parameterize the guest kernel build and the rootfs build by architecture in the first version of those scripts. Hardcoding one and adding the other later turns every image change into two manual jobs, and the two drift.

## What runs where

| Component | macOS | Lima VM | Rented host |
| --- | --- | --- | --- |
| `sandbox-protocol`, `sandbox-store` | yes | yes | yes |
| `sandbox-api`, `sandbox-controller`, `sandbox-cli` | yes | yes | yes |
| `sandbox-fake-host` | yes | yes | yes |
| `sandbox-supervisor` | no — Linux only | yes | yes |
| `sandbox-guest` | no — Linux only | yes, inside the microVM | yes |
| PostgreSQL, MinIO | yes, via compose | yes | yes |
| Trustworthy measurements | no | no | yes |
