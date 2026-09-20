# Hudson Sandbox

The planned sandbox tool for [Hudson](https://github.com/hudson-infinity/hudson), implemented in Rust with Firecracker for Linux microVM isolation. It exposes APIs to create, execute, pause, resume, and destroy sandboxes.

**Status: design only.** This repository contains implementation documentation; no sandbox service, guest agent, deployment, or security guarantees have been implemented or validated yet.

**Hudson is the harness; Hudson Sandbox is a tool it calls.** The harness owns agent behavior, business permissions, approvals, and any Temporal workflows. This repository has no Temporal dependency and can serve other authenticated API clients too.

The selected stack is Rust, HTTP/JSON with OpenAPI, Firecracker and Linux KVM, PostgreSQL, S3-compatible object storage, and OpenTelemetry with Prometheus/Grafana. Start with standalone API/controller processes and one compute host. Later, Kubernetes deploys the API and controllers; dedicated Linux hosts run the microVMs through our supervisor. Individual sandboxes are not Kubernetes pods in the initial design, so sandbox placement and resource accounting remain our controller's responsibility.

Authentication uses project-scoped opaque API tokens over HTTPS, with hashed storage, rotation, and revocation. User login stays in Hudson. Live output uses an authenticated streaming endpoint; the controller persists operations and results.

Authentication is required everywhere, including local development and self-hosting. Project access manages one project; Admin access manages the installation. Backend clients use the appropriate bearer credential, and the management UI exchanges a validated credential for a short-lived session. There is no option to disable authentication.

See [authentication and UI access](docs/auth-design.md) for permissions, login, token/session validation, and administrative audit.

Start with [architecture and data flow](docs/artitecture.md) for the system diagram and create, execute, pause, and resume examples.

See [the implementation design](docs/implementation.md) for component boundaries, execution contracts, recovery, security, and phased delivery.

See [data models](docs/data-models.md) for the six sandbox resource models and their relationships: projects, sandboxes, operations, hosts, allocations, and snapshots.

See [IDs and resource records](docs/identity-and-resources.md) for sandbox identity, API retry keys, snapshots, allocations, and the proposed database relationships.

The first complete flow is create → execute → save memory and disk → release compute → resume → destroy. Pause/resume is a core capability; performance optimizations and multi-host scheduling follow a verified single-host implementation.
