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

Authentication, admission, claims, reservations, and read handlers have executable tests. The fake provides the shared gRPC create/inspect/stop service over mTLS and always reports simulated evidence. It starts no VM or process. Create, execute and destroy are integrated through the controller, with real execution evidence from the [Linux supervisor](../real-supervisor.md). Public cancellation, files/output, streaming and the CLI remain unfinished.

The fake models bounded resource accounting, stale ownership, lease expiry, duplicate requests, and lost acknowledgements for control-plane tests. Those models do not test host resource enforcement or hardware isolation; those require stage 1 and supported hardware.

## Stage 1 — real Firecracker on a nested Linux host

A dedicated aarch64 Linux/KVM host and a real Firecracker/jailer boot have now been verified on an M4 Pro. The [Linux development guide](../linux-development.md) owns the pinned Lima configuration, setup commands, observed process/cgroup evidence, and the limits of that experiment.

The guest runner and real supervisor integration remain unfinished. This local environment lets those components be developed and tested without mounting the Mac filesystem. It does not validate the supported x86_64 host, hostile workload isolation, or production performance. Keep those release gates separate from the aarch64 development evidence.

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
