# Supported configuration

Status: selected release envelope, not yet validated as a complete supported configuration. [Nested-aarch64 development evidence](linux-development.md#verified-boot-and-its-limits) exists; it does not certify the x86_64 envelope below. This document owns what a supported installation looks like and what a sandbox may contain. [Product goal](goal.md) requires this envelope to be published; [roadmap](roadmap.md#feasibility-spikes-phase-0) owns when each row is confirmed on a real host.

"Run anything" means anything inside the envelope below. Widening it later is cheap. Narrowing it after a release is not, so the first release claims one configuration and says so.

## Host

| Item | First release | Why this one |
| --- | --- | --- |
| Architecture | x86_64 | Best-tested Firecracker path, cheapest bare metal. Snapshots are not portable across architectures, so this also fixes what a restore can target |
| Operating system | Ubuntu 24.04 LTS, stock kernel 6.8 | Reproducible on rented hardware and in CI with no kernel build; supported until 2029 |
| Virtualization | KVM, `/dev/kvm` present | Bare metal, or a cloud instance with nested virtualization actually enabled |
| cgroups | v2 | The freezer semantics the pause design depends on |

arm64 release support is deferred, not rejected. The nested-aarch64 environment is currently a development/test configuration. It doubles the test matrix and the image build pipeline, and there is no second architecture's worth of demand yet.

A host outside this envelope is not refused by the software — we simply make no claim about it, and the isolation and snapshot evidence does not transfer.

## Sandbox

| Item | First release |
| --- | --- |
| Guest kernel | Ours, built and pinned by digest. Module loading compiled out, kernel lockdown enabled |
| Init | Ours, PID 1, starts the guest agent before any customer process exists |
| Userland | Debian slim — glibc, `apt`, the packages customers expect to find |
| Customer privilege | Root in userspace ([decision 0003](decisions/0003-guest-root-with-our-kernel.md)) |
| Smallest sandbox | 1 vCPU, 128 MiB memory, 64 MiB writable disk |
| Largest sandbox | 4 vCPU, 8 GiB memory, 64 GiB writable disk |
| Long-running processes | Supported — a process may outlive the request that started it |
| Inbound connections | None ([networking](networking.md#ingress)) |

Debian rather than Alpine because musl breaks a meaningful share of prebuilt binaries — Python wheels, Node native modules, vendor binaries — and the failure lands on the customer as a confusing build error rather than on us.

The 8 GiB ceiling is a snapshot decision as much as a scheduling one: memory size sets pause duration, snapshot bytes, and cross-host restore time. [Performance](performance.md#proposed-budgets) holds those budgets.

## How a sandbox boots

Firecracker takes a kernel and a root filesystem as separate inputs. They are not one artifact, and the split matters for [decision 0003](decisions/0003-guest-root-with-our-kernel.md):

```text
supervisor supplies:
  vmlinux             ← our kernel, pinned digest, never customer-supplied
  rootfs.ext4         ← the image: Debian slim + our init + our guest agent
  writable overlay    ← per sandbox, discarded on destroy
```

The image allowlist in [data models](data-models.md#what-we-keep-inside-these-models) governs the root filesystem. The kernel is not part of it and is not selectable. An allowed image must carry our init and our guest agent at the expected paths, or it cannot be admitted — the lifecycle contract has no meaning without them.

## Publishing and changing the envelope

Publish the envelope with each release: architecture, host kernel, guest kernel, Firecracker version, guest agent version, and image digests. A snapshot records the same values in its manifest, and a restore refuses if the target host cannot satisfy them.

Adding a supported configuration is a minor release. Removing one is a breaking change and belongs in release notes with an upgrade path.

## Acceptance checks

The selected supported configuration has not passed the complete acceptance set. Confirm:

1. A clean Ubuntu 24.04 host with `/dev/kvm` runs the full create, execute, destroy path with no manual kernel work.
2. The guest kernel refuses module loading and lockdown is active, verified from inside a sandbox as root.
3. An image missing our init or guest agent is refused at admission rather than booted.
4. A 4 vCPU / 8 GiB sandbox stays inside the [performance budgets](performance.md#proposed-budgets), including snapshot size.
5. A restore onto a host outside the recorded envelope fails explicitly rather than producing a running sandbox.

## Open decisions

The exact guest kernel version and configuration, pending the [Phase 0 spikes](roadmap.md#feasibility-spikes-phase-0). The production Firecracker release to qualify and pin; development experiments already record exact versions/digests. Whether a second image variant with a preinstalled language toolchain ships at launch. When arm64 enters the envelope.
