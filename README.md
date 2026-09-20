# Hudson Sandbox

A planned standalone sandbox service for AI applications, implemented in Rust with Firecracker microVM isolation. Its APIs create, execute, pause, resume, and destroy Linux sandboxes. The management UI provides Project access and Admin access.

**Status: design only.** There is no implemented runtime, UI, installer, migration, or executable test suite yet. These documents describe intended behavior, not validated security or performance guarantees.

**Hudson is the harness; Hudson Sandbox is a tool it calls.** The service also supports other authenticated clients and self-hosting without Hudson. User/business workflows and any Temporal dependency stay in the calling harness.

The selected stack is Rust, HTTP/JSON, PostgreSQL, S3-compatible storage, Firecracker/Linux KVM, and OpenTelemetry with Prometheus/Grafana. Start with one compute host and standalone platform services; Kubernetes deployment and multiple hosts follow verified lifecycle behavior. Authentication is mandatory everywhere, including local development.

Start with the [documentation guide](docs/README.md). It links the authoritative architecture, data models, authentication, lifecycle, API, UI, and roadmap documents.

The first complete milestone is **create → execute → save memory and disk → release compute → resume → destroy**. See the [roadmap](docs/roadmap.md) for delivery gates and current evidence.
