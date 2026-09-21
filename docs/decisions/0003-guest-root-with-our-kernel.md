# 0003: root inside the guest, on our kernel and our init

Status: Accepted
Date: 2026-09-21

## Context

A sandbox is a microVM running a customer's code. Somewhere inside it we also run a guest agent: the process that starts commands, captures output, freezes the workload for a pause, and gates its release on resume.

Whether the customer's code runs as root in that VM decides two things at once. It decides whether the runtime is usable — `apt install`, `pip install`, binding port 80, and most install scripts on the internet all need root. And it decides whether the guest agent shares a privilege level with the code it is supposed to supervise.

Refusing root makes the agent safe and the product unusable. Granting root along with a customer-supplied kernel makes the product maximally flexible and the agent indefensible, because a kernel the customer controls can lie about anything inside the VM.

## Decision

**Customer code runs as root inside its own sandbox. The kernel, the init, and the guest agent are ours.**

Customers get full root in userspace: install packages, write anywhere, bind any port, run services. They cannot supply or replace the guest kernel, cannot change the boot path, and cannot load kernel modules.

Concretely, every sandbox boots with:

- a kernel we build, supplied by the supervisor and pinned by digest, with module loading compiled out and kernel lockdown enabled. Firecracker takes the kernel and the root filesystem as separate inputs, so the kernel is not part of the image and is not selectable;
- our init as PID 1, inside the image, starting the guest agent before any customer process exists;
- the guest agent in a `system` cgroup and its own PID namespace, separate from the `workload` cgroup that holds every customer process.

An image that does not carry our init and guest agent at the expected paths is not an allowed image. [Supported configuration](../compatibility.md#how-a-sandbox-boots) owns the boot inputs.

## Consequences

**The runtime is usable.** This is the point. A general-purpose secure runtime whose users cannot install a dependency is not the product described in [product goal](../goal.md).

**A root customer can interfere with our guest agent inside their own sandbox.** Root in the same VM can attempt to kill it, replace it, or feed it false results. The separate namespace and cgroup raise the effort substantially — the agent is not in their process view, and a broad kill sweep does not reach it — but this is a hardening measure, not a boundary. We state that plainly in [threat model](../threat-model.md) rather than implying an in-guest guarantee we do not have.

**What is unaffected:** the VM boundary, the jailer, per-VM cgroup limits, host networking and egress rules, and cross-project isolation. None of those depend on the customer's privilege level inside their own VM. A root customer is no closer to the host, to another project, or to the platform database than an unprivileged one.

**The exposure is self-harm for honest users and a limits problem for hostile ones.** A developer running their own build gains nothing by killing the process that delivers their output. The attack only pays against limits we enforce — deadlines, and eventually billing. Treat a missing or unresponsive guest agent as a failed sandbox, and report it as such, rather than as a sandbox that ran for free.

**Keeping the kernel ours is what makes the resume design possible.** The [freeze boundary](../lifecycle.md#resume) depends on cgroup freezer behaviour and on the agent being invisible to the workload. Both are kernel-enforced, so both evaporate if the customer supplies the kernel.

**The fallback is a mechanism change, not a product change.** If the Phase 0 spikes show the agent cannot be shielded well enough, resume gating moves host-side: the controller checks deadlines while the VM is still paused, before Firecracker resumes it. Coarser, and the public API does not move.

## Alternatives considered

**No root — unprivileged user only.** Rejected: breaks package installation, privileged ports, and most real workloads. The guest agent would be untouchable, protecting a product nobody could use.

**Root plus a customer-supplied kernel.** Rejected: once the customer controls the kernel, nothing the guest reports can be trusted, cgroup and namespace guarantees are gone, and the snapshot compatibility matrix expands to kernels we have never tested. In-guest gating would have to be abandoned entirely.
