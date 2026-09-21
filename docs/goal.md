# Product goal

Status: selected product direction; design only. No runtime or security guarantees have been implemented or verified. This document owns product scope and priorities; the [roadmap](roadmap.md) owns delivery gates and evidence, [threat model](threat-model.md) owns the security contract this scope requires, [performance](performance.md) owns the budgets it must meet, and [alternatives](alternatives.md) owns why we build it rather than adopt it.

## What we are building

**Hudson Sandbox is a general-purpose secure runtime for untrusted workloads.**

The product promise is: run untrusted code in isolated environments, with controlled resources, controlled connectivity, and a reliable lifecycle.

Applications use an authenticated API to create environments, run processes, transfer files, observe results, and reclaim resources. Workloads may include scripts, applications, build jobs, automation, and long-running services. AI agents are one possible client; no agent framework, programming language, business workflow, or Hudson harness is required.

“Run anything” means any workload within a published compatibility envelope. That envelope is now written down in [supported configuration](compatibility.md): x86_64, an Ubuntu 24.04 host, our pinned guest kernel, a Debian userland, sandboxes up to 4 vCPU and 8 GiB, and root for the customer inside their own VM. Broader compatibility must not silently weaken isolation.

## Core capabilities

| Primitive | Responsibility |
| --- | --- |
| Create | Start an isolated environment from an authorized immutable image with explicit limits |
| Execute | Run finite commands and long-running processes with arguments, environment, working directory, deadlines, and cancellation |
| Files | Safely transfer files and maintain the sandbox's private writable filesystem during its lifetime |
| Network | Deny-by-default egress against an allowlist of address ranges and ports, a host-side resolver, and no inbound path in the first release ([networking](networking.md)) |
| Lifecycle | Inspect state, terminate processes, destroy the environment, and confirm resource reclamation |
| Observe | Stream bounded output and report process results, resource usage, and infrastructure failures |

Pause/resume is a subsequent capability. It must preserve the full memory/disk and recovery contracts in [lifecycle](lifecycle.md) before it is offered. Initial writable files are not a promise of recovery after host loss.

Customization initially comes through supported images and configuration. Specialized runtimes, templates, integrations, and organization features can build on these primitives later.

## Immediate engineering priorities

1. **Define the security and workload contract.** [Threat model](threat-model.md) states the trusted components, attacker capabilities, and tenant boundaries; turning it into enforced behavior is Phase 1 work. Guest root is settled: customers are root in their own sandbox, on our kernel and our init ([decision 0003](decisions/0003-guest-root-with-our-kernel.md)). The kernel stays ours precisely because a customer-controlled one could not be assumed to protect the guest agent or its process-freeze boundary.
2. **Build the execution boundary.** Use Firecracker/jailer with isolated storage and networking. Enforce resource and connectivity limits outside customer control; protect host services, platform credentials, cloud metadata, and other sandboxes.
3. **Provide a small, useful API.** Mandatory authentication, durable operation handles, execution, files, output, cancellation, and destruction come first. Define generic service connectivity without coupling it to an application framework.
4. **Make failure and cleanup correct.** Preserve ownership across crashes and lost acknowledgements. Never blindly repeat an uncertain command or free a reservation without evidence. Bound resource exhaustion and account for cleanup that is still pending.
5. **Make installation reproducible.** Supply one supported installation path, a CLI, a working example, diagnostics, and verified teardown. Another developer should be able to install and use the runtime without the author's assistance.

Detailed component boundaries, authentication, and operation semantics remain in [architecture](architecture.md), [authentication](auth-design.md), and [API contract](api-contract.md).

## Capabilities and their validation

The product is the execution environment and its controls. Tests provide evidence that those capabilities behave as specified; testing is not a separate substitute for building them.

| Validation area | Questions it answers |
| --- | --- |
| Isolation | Can customer code reach protected infrastructure, access another sandbox, bypass limits, or interfere with host control? |
| Reliability | What happens after a process crash, client disconnect, controller/supervisor restart, or lost acknowledgement? Are outcomes and cleanup accurate? |
| Usability | Can another person install the system, run a workload, transfer files, retrieve results, and destroy resources? |

Passing isolation tests establishes evidence within the tested threat model and configuration. It does not prove that escape is impossible. Release claims must distinguish intended, implemented, and verified behavior.

The first end-to-end acceptance scenario is:

> On a fresh supported host, create two sandboxes. Run workloads and transfer files. Verify they cannot access each other or protected infrastructure. Exhaust one sandbox's configured limits while the other remains usable within documented bounds. Restart platform components, inspect the outcomes, then destroy both and confirm resource reclamation.

This scenario complements the detailed acceptance checks; it does not replace adversarial testing or the failure cases for each shipped capability.

## Scope after the foundation

Pause/resume, multiple hosts, Kubernetes packaging, enterprise identity, specialized experiences, a polished dashboard, and performance optimizations follow the first usable runtime. Existing designs for them remain future contracts, not prerequisites for proving basic execution.

The runtime should be usable by small teams and larger organizations through the same core interfaces. Managed hosting, private deployment, and organization-specific customization can be selected later. The current focus is isolation, resource enforcement, lifecycle correctness, and reproducible installation.
