# Threat model

Status: proposed. No implementation exists, so nothing here is validated. This document owns the adversary model, the trust boundaries, and the explicit list of what the service does and does not promise. [Architecture](architecture.md#isolation-and-data-protection) owns the isolation mechanisms, [auth design](auth-design.md) owns access control, and [roadmap](roadmap.md#required-evidence-by-delivery-gate) owns the evidence required before any promise here is claimed.

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
| Malicious code inside a sandbox | Full root inside the guest, chosen kernel-facing syscalls, arbitrary network attempts | Firecracker with jailer, seccomp, per-VM cgroups and namespaces, deny-by-default egress, no host credentials in the guest |
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
4. **Guest agent to customer processes.** The management agent must stay outside the frozen customer process groups and outside customer privilege. [Lifecycle](lifecycle.md#resume) owns this contract; it is the least proven boundary in the design.
5. **Management UI origin to guest content.** Guest output and sandbox ports are never served from the UI origin.
6. **Project to project.** Enforced in the database and on every lookup, not by identifier obscurity.

## What we promise

Subject to the evidence gates in [roadmap](roadmap.md#required-evidence-by-delivery-gate), and unproven until those gates pass:

- Customer code runs only inside a microVM, never as a host process.
- A sandbox cannot reach another sandbox, the platform database, the supervisor control channel, or cloud metadata.
- Guest root does not imply host access, another tenant's data, or orchestration credentials.
- Snapshots are encrypted, integrity-verified, and readable only by their owning project.
- Access decisions are re-evaluated on resume rather than trusted from restored memory.
- Revocation stops new requests and new execution dispatch within a bounded time across replicas.

## What we do not promise

Stating these prevents a reader from assuming a stronger product than we are building.

- **No exactly-once execution.** Arbitrary commands have external side effects. Uncertain outcomes are reported as `unknown`, not retried silently.
- **No resistance to CPU microarchitectural side channels.** We rely on the host's kernel and firmware mitigations and do not claim protection beyond them.
- **No protection against an installation administrator.** Admin access can read customer output; that access is audited, not prevented.
- **No protection of data the customer's own code exfiltrates** through destinations its project policy allows.
- **No recovery of unsaved memory after host loss.** Work since the last published snapshot can be lost, and this is reported rather than concealed.
- **No isolation of a caller's other tools.** Installing the CLI in an agent's environment does not confine that agent's other file or shell access. [Architecture](architecture.md#client-interfaces-and-agent-integration) states this boundary.
- **No availability or performance guarantee.** [Performance](performance.md) holds engineering budgets, not service commitments.
- **No security claim from documentation.** Every item above requires the adversarial tests named below.

## Required validation

These tests gate the first usable runtime and are owned by [roadmap](roadmap.md#required-evidence-by-delivery-gate). [Product goal](goal.md) makes defining this contract the first engineering priority, including the explicit decision about guest root.

1. Guest privilege escalation attempts against the host, jailer, and supervisor socket.
2. Filesystem and archive traversal on every privileged file path, including symlink races.
3. Egress attempts to cloud metadata, the platform database, the supervisor channel, and another tenant, including DNS, IPv6, redirect, and alternate-protocol bypasses.
4. Cross-project access through guessed and known identifiers, on every route and in storage paths.
5. Credential and session attacks: cookie fallback on bearer routes, bearer fallback on UI routes, CSRF, cross-origin login, and revocation propagation across replicas.
6. Guest-controlled output rendered in the management UI without executing.
7. Stale supervisor, stale controller claim, and stale allocation generation attempting to mutate current state.
8. A crafted snapshot or image failing closed at verification rather than restoring.

## Open decisions

Choose snapshot encryption key management, the egress policy configuration model and its per-project granularity, the supported host hardening baseline and its mitigation requirements, log and trace redaction rules, and whether a third-party review precedes the first release. Mechanism details belong to [architecture](architecture.md#isolation-and-data-protection); access-control details belong to [auth design](auth-design.md).
