# Alternatives and why we are building this

Status: positioning record, written 2026-09-20. This document owns the build-versus-adopt argument, the honest cost of building, and the conditions that should make us revisit the decision. [Product goal](goal.md) owns what we are building; this document owns why we are building it rather than adopting it. It does not own any technical contract.

Existing products already run untrusted code for agents. Nothing else in this repository explains why we are not using one, which leaves the largest question about the project's direction unanswered. This document answers it and states what would change the answer.

## Requirements that drive the choice

| Requirement | Why it is not negotiable for us |
| --- | --- |
| Self-hostable by us and by our users | The service must run in an operator's own infrastructure, with their storage and their credentials |
| Direct control of the isolation and data boundary | We answer for the isolation claim, so we must be able to test and change it |
| General-purpose workloads, not an agent-shaped product | [Product goal](goal.md) commits to scripts, applications, build jobs, automation, and services through the same interfaces |
| Pause and resume with live process continuity, later | A running process survives the pause and continues afterward; a fresh container with the same files is a different product. Phase 4, but it shapes the architecture from the start |
| One identity across pause and resume | Callers and operators track a sandbox, not a sequence of unrelated environments |
| Honest uncertain outcomes | An interrupted operation reports `unknown` and reconciles, rather than being retried silently |
| No dependency on our own harness | Hudson is one client. The service must be usable without it |

Self-hosting and owning the isolation boundary decide the question on their own. Process-continuous pause/resume is what keeps the answer from changing later, even though it ships after the first usable runtime.

## Options considered

Vendor capabilities change quickly. Re-verify each row against current product documentation before citing it anywhere outside this repository; the assessments below are ours as of the date above, not vendor statements.

| Option | Why not chosen |
| --- | --- |
| Adopt a hosted sandbox product (for example E2B, Modal, Daytona) | Hosted-first operation conflicts with user self-hosting, and we would be attesting to an isolation and data boundary we cannot inspect or test ourselves |
| Run an existing open-source sandbox runtime as a dependency | Closest option, and the reason [E2B's architecture](https://github.com/e2b-dev/runtime/blob/main/docs/ARCHITECTURE.md#deployment-topology) is an explicit reference here. Our pause/resume, identity, and recovery contracts are stricter than what we would inherit, and we would still own the control plane, auth, and operational surface |
| Containers with a sandboxed runtime such as gVisor or Kata | Mature and simpler to operate, and a real option for the first milestone alone. They do not supply pause/resume with process continuity, so choosing them would mean rebuilding the execution boundary later |
| A VM-per-sandbox on a cloud provider's machine API | Shifts isolation to the provider at the cost of self-hosting, per-sandbox economics, and control of the snapshot format |
| Firecracker directly, with our own control plane | Chosen. Firecracker supplies VM isolation and snapshotting; we own placement, lifecycle, recovery, and the API |

## What building costs us

State the cost plainly so the decision can be judged.

The scope in [roadmap](roadmap.md#implementation-phases) spans a Rust control plane, a host supervisor, a guest agent, snapshot storage, SDKs, a CLI, a management UI, and an installer. That is a large surface for a small team, and several of those components are load-bearing before the first user can self-host anything. The mitigation is sequencing, not optimism: prove the feasibility spikes first, ship the narrowest usable runtime second, and refuse to widen any phase before its exit gate has evidence.

The second cost is the isolation claim. Building our own boundary means we own its failures. [Threat model](threat-model.md) states what that commits us to.

## Revisit triggers

Reopen this decision if any of these becomes true.

| Trigger | Action |
| --- | --- |
| The [Phase 0 spikes](roadmap.md#feasibility-spikes-phase-0) show process-continuous pause/resume is not achievable on our stack within budget | Reconsider whether a container-based option meets the real product need |
| An existing runtime ships self-hostable pause/resume with process continuity under a license we can operate on | Re-evaluate adopting it instead of maintaining ours |
| Users consistently want a hosted service and not a self-hosted one | The self-hosting requirement, which drives this whole decision, would need to change first |
| Agent clients standardize on a protocol our API cannot serve through the CLI or an SDK | Revisit the no-MCP decision in [decision 0002](decisions/0002-no-mcp-server-initially.md) |
| [Performance](performance.md) targets prove unreachable for cross-host resume | Reconsider whether resume must be cross-host at all in the first product |

## Open decisions

Self-hosting comes first, and a hosted offering stays possible rather than planned; usage is therefore derived from allocations and operations rather than metered in the schema, and that must be revisited before anything is billed from it. The project is released under Apache-2.0 ([decision 0004](decisions/0004-apache-2-0-license.md)). The first public release is a 0.1 once the Phase 3 recovery and isolation gate passes.
