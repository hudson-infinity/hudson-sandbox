# Threat model

Status: selected security contract, with partial mechanisms and development evidence. The complete adversarial validation set and supported-host release gates have not passed. Implemented controls and their limits are linked from the [roadmap](roadmap.md#required-evidence-by-delivery-gate). This document owns the adversary model, the trust boundaries, and the explicit list of what the service does and does not promise. [Architecture](architecture.md#isolation-and-data-protection) owns the isolation mechanisms, [auth design](auth-design.md) owns access control, and [roadmap](roadmap.md#required-evidence-by-delivery-gate) owns the evidence required before any promise here is claimed.

The security requirements were previously spread across several documents. This one states what we are defending, against whom, and what we are deliberately not defending, so a reader can judge the mechanisms against a stated goal.

## What we are protecting

| Asset | Why it matters |
| --- | --- |
| Customer code, files, and process memory inside a sandbox | The workload itself, and the contents of every snapshot |
| Snapshots in object storage | Full memory images; may contain credentials the customer's own code handled |
| Project and admin credentials | Grant execution and installation control |
| The host and its supervisor | Compromise reaches every sandbox on that machine |
| PostgreSQL metadata and object storage | Ownership, receipts, audit history, and all stored bytes |
| Other tenants' sandboxes on shared infrastructure | The isolation promise itself |
| The management UI origin | An authenticated surface adjacent to untrusted guest output |

## Adversaries

| Adversary | Assumed capability | Primary defenses |
| --- | --- | --- |
| Malicious code inside a sandbox | Root in guest userspace by design ([decision 0003](decisions/0003-guest-root-with-our-kernel.md)), chosen syscalls, arbitrary network attempts | Firecracker with jailer, seccomp, per-VM cgroups and namespaces, our kernel with modules off and lockdown on, deny-by-default egress, no host credentials in the guest |
| A root customer attacking our guest agent inside their own sandbox | Same privilege level as the agent; may kill, replace, or impersonate it | Agent in a separate PID namespace and `system` cgroup, control socket unreachable from the workload namespace, host-side detection. Hardening, not a boundary — see the non-promises below |
| A tenant attacking another tenant | Same as above, plus knowledge of ID formats and timing | Per-project ownership checks on every lookup, separate VMs, host-enforced network isolation, nonrevealing `404` responses |
| A stolen project token | Full API authority for one project | Hashed storage, expiry, rotation, revocation, per-project scoping, no admin route acceptance |
| A stolen admin credential | Installation control | Separate validator and namespace, audited mutations, hashes only, revocation across replicas |
| An attacker in the user's browser context | CSRF, cross-origin requests, guest output rendered as markup | Same-origin sessions, `SameSite=Strict` `__Host-` cookie, CSRF token plus Origin checks, guest output rendered as inert text, restrictive CSP |
| A malicious or corrupted snapshot or image | Crafted bytes presented for restore | Digest verification, immutable published manifests, compatibility checks, restore with egress blocked and customer processes frozen |
| A partitioned or compromised host supervisor | Stale or forged control messages | Supervisor epochs, allocation generations, claim revisions, lease watchdogs, fencing before replacement |
| Supply chain | Malicious dependency, kernel, or guest image | Pinned toolchain and Firecracker release, reproducible guest image builds with immutable digests |

## Trust boundaries

1. **Client to API.** Every request authenticated; no unauthenticated path exists in any environment, including local development.
2. **API and controller to supervisor.** Separate service credentials. Supervisor endpoints reject project and admin credentials.
3. **Supervisor to guest.** The guest is untrusted. Guest messages never grant host authority, and the guest holds no PostgreSQL or object-storage credentials.
4. **Guest agent to customer processes.** The agent stays outside the frozen customer process groups. It does not stay outside customer *privilege* — the customer is root in the same VM — so this is the weakest line in the design and the one most in need of the Phase 0 spikes. [Lifecycle](lifecycle.md#resume) owns the contract; [architecture](architecture.md#privilege-layers-inside-a-sandbox) owns the layering.
5. **Management UI origin to guest content.** Guest output and sandbox ports are never served from the UI origin.
6. **Project to project.** Enforced in the database and on every lookup, not by identifier obscurity.

## What we promise

Subject to the evidence gates in [roadmap](roadmap.md#required-evidence-by-delivery-gate), and unproven until those gates pass:

- Customer code runs only inside a microVM, never as a host process.
- A sandbox cannot reach another sandbox, the platform database, the supervisor control channel, or cloud metadata.
- Guest root does not imply host access, another tenant's data, or orchestration credentials. Root inside a sandbox is expected and reaches nothing outside it.
- No inbound connection reaches a sandbox, and outbound traffic is confined to an allowlist of address ranges and ports enforced on the host ([networking](networking.md)).
- Snapshots are encrypted, integrity-verified, and readable only by their owning project.
- Access decisions are re-evaluated on resume rather than trusted from restored memory.
- Revocation stops new requests and new execution dispatch within a bounded time across replicas.

## What we do not promise

Stating these prevents a reader from assuming a stronger product than we are building.

- **No exactly-once execution.** Arbitrary commands have external side effects. Uncertain outcomes are reported as `unknown`, not retried silently.
- **No resistance to CPU microarchitectural side channels.** We rely on the host's kernel and firmware mitigations and do not claim protection beyond them.
- **No in-guest protection against a hostile root customer.** A customer who is root in their own sandbox can attempt to kill, replace, or impersonate our guest agent. We make this expensive and detectable, not impossible. The consequence is bounded: they can distort our view of *their own* sandbox — its output, its exit codes, whether a deadline was honoured — and nothing beyond it. A sandbox whose agent is missing or unresponsive is failed and reported, never treated as a sandbox that ran for free.
- **No protection against an installation administrator.** Admin access can read customer output; that access is audited, not prevented.
- **No protection of data the customer's own code exfiltrates** through destinations its project policy allows. Egress rules control where traffic goes, never what it carries, and allowed traffic is not inspected.
- **No recovery of unsaved memory after host loss.** Work since the last published snapshot can be lost, and this is reported rather than concealed.
- **No isolation of a caller's other tools.** Installing the CLI in an agent's environment does not confine that agent's other file or shell access. [Architecture](architecture.md#client-interfaces-and-agent-integration) states this boundary.
- **No availability or performance guarantee.** [Performance](performance.md) holds engineering budgets, not service commitments.
- **No security claim from documentation.** Every item above requires the adversarial tests named below.

## Required validation

These tests gate the first usable runtime and are owned by [roadmap](roadmap.md#required-evidence-by-delivery-gate). [Product goal](goal.md) makes defining this contract the first engineering priority, including the explicit decision about guest root.

1. Guest privilege escalation attempts against the host, jailer, and supervisor socket.
2. Filesystem and archive traversal on every privileged file path, including symlink races.
3. The full [networking acceptance set](networking.md#acceptance-checks): egress to metadata, platform services, the supervisor channel and other tenants; DNS exfiltration through unapproved names; IPv6, redirect and alternate-protocol bypasses; and bandwidth saturation.
4. A hostile root customer against the guest agent: a broad kill sweep from the workload namespace, attempts to reach its control socket, attempts to load a kernel module, and a forged handshake on resume. Each must fail closed, and a missing agent must fail the sandbox.
5. Cross-project access through guessed and known identifiers, on every route and in storage paths.
6. Credential and session attacks: cookie fallback on bearer routes, bearer fallback on UI routes, CSRF, cross-origin login, and revocation propagation across replicas.
7. Guest-controlled output rendered in the management UI without executing.
8. Stale supervisor, stale controller claim, and stale allocation generation attempting to mutate current state.
9. A crafted snapshot or image failing closed at verification rather than restoring.

## Open decisions

Snapshots are encrypted under one installation-wide key held in the operator's KMS, with the key identifier recorded in the manifest so per-project keys can follow without breaking published snapshots. A third-party review is planned before 1.0, after the first public release; until it happens, no document here may describe the isolation as externally validated. Still open: the supported host hardening baseline and its mitigation requirements, log and trace redaction rules, and per-project egress granularity. Mechanism details belong to [architecture](architecture.md#isolation-and-data-protection); access-control details belong to [auth design](auth-design.md).
