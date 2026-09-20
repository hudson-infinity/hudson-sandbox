# Hudson Sandbox

**Give your agent a sandbox.**

Hudson Sandbox is a project from **Hudson Labs** building isolated Linux environments for AI agents. Give an agent a place to run code and work with files, save its running environment when work pauses, and resume it later.

**Status: design stage.** Architecture and contracts are documented; the runtime, CLI, management UI, and installer are not implemented yet. There are no validated isolation or performance guarantees, and no working quickstart to run today.

## What we are building

- **Create and execute:** start a sandbox with explicit resource limits, run commands, and read output.
- **Pause and resume:** save memory and disk, release compute, then continue the same sandbox.
- **Manage:** inspect operations, cancel work, and destroy sandboxes with tracked cleanup.
- **Self-host:** operate the service independently, with Project and Admin access through an API and management UI.

```text
Your agent or harness → Sandbox API → Controller → Firecracker microVM
                              │
                    Status, output, snapshots
```

[Hudson](https://github.com/hudson-infinity/hudson) is the agent harness. Hudson Sandbox supplies the execution environment and works with other harnesses too. Agent workflows and Temporal stay with the caller. Authentication is required everywhere, including local development.

The selected stack is **Rust, Firecracker/Linux KVM, PostgreSQL, and S3-compatible object storage**, with HTTP/JSON APIs. We start with one Linux compute host; Kubernetes deployment and multiple hosts follow a verified lifecycle.

## Follow the build

Start with the [documentation guide](docs/README.md), [architecture](docs/architecture.md), and [roadmap](docs/roadmap.md).

Our first complete milestone is **create → execute → pause → release compute → resume → destroy**, without requiring Hudson, Temporal, or Kubernetes.

## Contribute

Small fixes can start with a pull request. For larger changes, open an issue to agree on scope first. Contributions go through a branch, relevant checks, maintainer review, and a squash merge into `main`.

Read [CONTRIBUTING.md](CONTRIBUTING.md) for commits, PRs, local checks, and releases. Report vulnerabilities through the private channel in [SECURITY.md](SECURITY.md).

## License

An open-source release is intended; license selection is pending. This repository currently has no license granting reuse or redistribution rights.
