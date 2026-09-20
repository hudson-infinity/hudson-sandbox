# Hudson Sandbox

A planned general-purpose secure runtime for untrusted Linux workloads, implemented in Rust with Firecracker microVM isolation. Its APIs manage isolated environments, processes, files, networking, and resource limits. AI applications are one possible client. Pause/resume and a Project/Admin management UI are planned subsequent capabilities.

**Status: design only.** There is no implemented runtime, UI, installer, migration, or executable test suite yet. These documents describe intended behavior, not validated security or performance guarantees.

**Hudson is the harness; Hudson Sandbox is a tool it calls.** The service also supports other authenticated clients and self-hosting without Hudson. User/business workflows and any Temporal dependency stay in the calling harness.

The selected stack is Rust, HTTP/JSON, PostgreSQL, S3-compatible storage, Firecracker/Linux KVM, and OpenTelemetry with Prometheus/Grafana. Start with one compute host and standalone platform services; Kubernetes deployment and multiple hosts follow verified lifecycle behavior. Authentication is mandatory everywhere, including local development.

Start with the [product goal](docs/goal.md) and [documentation guide](docs/README.md). They explain the scope and link the authoritative architecture, data models, authentication, lifecycle, API, UI, and roadmap documents.

The first usable milestone is **create → execute and transfer files → enforce isolation and limits → destroy**, including failure recovery and a reproducible installation on one supported host. Pause/resume follows. See the [roadmap](docs/roadmap.md) for delivery gates and current evidence.
