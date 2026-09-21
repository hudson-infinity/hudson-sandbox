# Sandbox networking

Status: selected design; no implementation. This document owns egress policy, name resolution, ingress, and bandwidth limits for sandboxes. [Architecture](architecture.md#isolation-and-data-protection) owns where enforcement sits in the system; [threat model](threat-model.md) owns what these rules are defending against.

Networking was previously described only as "deny-by-default egress" spread across other documents, with no grammar, no resolver design, and no position on inbound connections. This document is that position.

## Enforcement point

All of it is enforced on the **host**, by the supervisor, outside the sandbox. Nothing inside the guest participates in its own policy. A sandbox has one virtual interface on a per-sandbox network namespace; nftables rules attached to that namespace decide what leaves it.

Guest root does not change this. [Decision 0003](decisions/0003-guest-root-with-our-kernel.md) gives customers root inside their own VM, which reaches nothing in this document.

## Egress

**Deny by default. The allowlist is IP ranges and ports, written as CIDR.**

```text
allow  151.101.0.0/16      tcp/443     # Fastly — PyPI, crates.io
allow  140.82.112.0/20     tcp/443     # GitHub
allow  151.101.0.0/16      tcp/80      # Debian mirrors
deny   169.254.0.0/16                  # cloud metadata — never allowlistable
deny   10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
deny   0.0.0.0/0                       # everything else
```

Names never appear in a rule. A rule is an address range and a port, so nothing the guest believes about DNS can widen it. This is the whole reason for choosing IP grammar over domain grammar: domain rules require trusting a resolution step, and that step is attackable.

Four destinations are permanently denied and cannot be added to any allowlist: the cloud metadata address, the platform's database and object storage, the supervisor's control channel, and every other sandbox's address range. The operator's allowlist is applied after those denials, never over them.

Ship a default allowlist covering the common package registries, built from the IP ranges those services publish. Operators extend it. Those ranges move, so the refresh mechanism is a real piece of work, not a config file written once — see the open decisions.

IPv6 is disabled on the sandbox interface in the first release. A half-enforced second address family is a bypass, and enabling it later is additive.

## Name resolution

**The guest gets one resolver: ours, on the host. It answers only for approved names and refuses everything else.**

The guest's `resolv.conf` points at the host resolver, and outbound port 53 to anywhere else is denied. So the guest cannot reach a public resolver, and cannot run its own.

The resolver holds the same approved list the egress allowlist covers. A query for an approved name is resolved upstream, the answer is pinned into the sandbox's allow set for its TTL, and returned. A query for anything else is refused and never forwarded.

Refusing rather than forwarding is the point. A forwarding resolver is an exfiltration channel: a guest asks for `<stolen-secret>.attacker.example`, the resolver dutifully walks the delegation chain, and the attacker's nameserver receives the secret without a single outbound connection appearing in the firewall log. Our resolver never forwards a name it does not already allow, so the query dies on the host.

Pinning the answer is what keeps the egress rules honest as CDN addresses rotate: the rule set follows the resolver rather than a static file.

## Ingress

**Nothing reaches a port inside a sandbox in the first release.**

Sandboxes make outbound connections. They receive none. There is no proxy, no per-port URL, and no published hostname.

This is a real limitation and worth stating plainly: a customer can run a web server inside a sandbox, and nobody — including them — can open it. Long-running services are supported in the sense that a process may outlive the request that started it, not in the sense that it is reachable.

Ingress is deferred rather than rejected. It needs its own authentication model, its own threat analysis for guest-controlled content on a routable hostname, and a decision about whether that content shares any origin with the management UI. Revisit when a user has a concrete need; the answer is likely a host proxy issuing one authenticated URL per exposed port, never a bare public hostname.

## Bandwidth

Each sandbox gets a configured bandwidth cap, enforced by the host on its interface. Without one, a single sandbox can saturate the host uplink and degrade every other tenant on that machine, which is a cross-tenant effect even though no data crosses between them.

The cap is part of the sandbox's resource reservation alongside CPU, memory, and disk. Accounting for aggregate host bandwidth during placement is deferred.

## What this does not prevent

Stated so the threat model stays honest.

A sandbox can send data to any destination the operator allowed. If GitHub is reachable, a customer's code can push a repository containing whatever it likes. Egress policy controls *where* traffic goes, never what it carries. Preventing a customer from exfiltrating their own data is not a goal — [threat model](threat-model.md#what-we-do-not-promise) says so directly.

Traffic to allowed destinations is also not inspected. There is no TLS interception, and none is planned.

## Acceptance checks

No implementation or tests exist yet. Implement tests for:

1. A sandbox cannot reach the metadata address, the platform database, object storage, the supervisor channel, or another sandbox — by address, by hostname, and after the guest edits its own `resolv.conf` or `/etc/hosts`.
2. A guest cannot reach any resolver except the host's, on any port.
3. A query for an unapproved name is refused and never leaves the host, including queries whose labels encode data.
4. An approved name resolves, gets pinned, and the connection to the resolved address is permitted — while a connection to a different address for the same name is denied.
5. Egress denial holds over IPv6, over redirects, and over alternate protocols, including while the sandbox is restoring from a snapshot.
6. The bandwidth cap holds under a deliberate saturation attempt, and other sandboxes on that host remain usable.
7. Policy is reapplied on resume before any customer process is released, per [lifecycle](lifecycle.md#resume).

## Open decisions

How the default allowlist is refreshed as published CDN ranges change, and whether that refresh is automatic. Concrete bandwidth cap values. Whether projects can hold their own allowlist entries or only the installation can. DNS cache and TTL handling for the pinned answers. Whether a future ingress design belongs in this document or its own.
