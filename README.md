# Hudson Sandbox

**Run untrusted workloads in isolated environments.**

Hudson Sandbox is a project from **Hudson Labs** building a general-purpose secure runtime for untrusted Linux workloads. Run scripts, applications, build jobs, automation, and services with controlled resources, controlled connectivity, and a reliable lifecycle. AI agents are one possible client.

**Status: design stage.** Architecture and contracts are documented; the runtime, SDKs, CLI, management UI, and installer are not implemented yet. There are no validated isolation or performance guarantees, and no working quickstart to run today.

## What we are building

- **Create and execute:** start a sandbox with explicit resource limits, run commands, and read output.
- **Files and networking:** transfer files safely and enforce connectivity policy.
- **Pause and resume, later:** save memory and disk, release compute, then continue the same sandbox.
- **Manage:** inspect operations, cancel work, and destroy sandboxes with tracked cleanup.
- **Self-host:** operate the service independently, with Project and Admin access through an API and CLI, with a management UI to follow.

```text
Your application      → Sandbox API → Controller → Firecracker microVM
                              │
                    Status, output, snapshots
```

[Hudson](https://github.com/hudson-infinity/hudson) is the agent harness. Hudson Sandbox supplies the execution environment and works with other applications and harnesses too. Agent workflows and Temporal stay with the caller. Authentication is required everywhere, including local development.

The selected stack is **Rust, Firecracker/Linux KVM, PostgreSQL with SQLx, and S3-compatible object storage**, with HTTP/JSON APIs. Customers get root inside their own sandbox, on a kernel and init that stay ours. We start with one x86_64 Linux compute host; Kubernetes deployment and multiple hosts follow a verified lifecycle.

## Ways to use it

The **HTTP API** is the foundation. Planned **SDKs** provide convenient language functions, the **CLI** serves people, scripts, and agents with shell access, and the **management UI** serves Project users and Admins. Each uses the API; sandbox execution stays on the server.

```text
Application → SDK ──┐
Agent/human → CLI ──┼──→ Sandbox API → Controller → Firecracker
Browser UI ────────┘
```

Output streaming and file transfers are API capabilities. We are not adding MCP for now; [decision 0002](docs/decisions/0002-no-mcp-server-initially.md) records why and what would change it. See [client interfaces](docs/architecture.md#client-interfaces-and-agent-integration) for the agent flow and [client behavior](docs/api-contract.md#sdk-and-cli-behavior) for retries and results. These interfaces are planned, not available packages or commands yet.

## Follow the build

Start with the [product goal](docs/goal.md), [documentation guide](docs/README.md), [architecture](docs/architecture.md), and [roadmap](docs/roadmap.md). [Supported configuration](docs/compatibility.md) states what a sandbox is and what it may contain, [networking](docs/networking.md) states what it may reach, [threat model](docs/threat-model.md) states what it must withstand, [alternatives](docs/alternatives.md) explains why we build this rather than adopt an existing product, and [performance](docs/performance.md) states the budgets it has to meet.

Our first usable milestone is **create → execute and transfer files → enforce isolation and limits → destroy**, including failure recovery and a reproducible installation on one supported host. Pause/resume follows. Hudson, Temporal, and Kubernetes are not required.

Before that milestone, a set of [feasibility spikes](docs/roadmap.md#feasibility-spikes-phase-0) runs on real Linux/KVM hardware. They establish what the host boundary costs and actually enforces, and they test the assumption the later pause/resume work rests on — a guest agent that survives a snapshot outside the frozen customer processes and gates their release on resume — which has never been executed.

## Contribute

Small fixes can start with a pull request. For larger changes, open an issue to agree on scope first. Contributions go through a branch, relevant checks, review, and a maintainer squash merge into `main`.

Read [CONTRIBUTING.md](CONTRIBUTING.md) for commits, PRs, local checks, and releases. Report vulnerabilities through the private channel in [SECURITY.md](SECURITY.md).

## License

[Apache-2.0](LICENSE). See [decision 0004](docs/decisions/0004-apache-2-0-license.md) for why.
