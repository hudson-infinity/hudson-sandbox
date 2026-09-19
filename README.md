# Hudson Sandbox

The planned sandbox tool for [Hudson](https://github.com/hudson-infinity/hudson), implemented in Rust with Firecracker for Linux microVM isolation. It exposes APIs to create, execute, pause, resume, and destroy sandboxes.

**Status: design only.** This repository contains implementation documentation; no sandbox service, guest agent, deployment, or security guarantees have been implemented or validated yet.

**Hudson is the harness; Hudson Sandbox is a tool it calls.** The harness owns agent behavior, business permissions, approvals, and any Temporal workflows. This repository has no Temporal dependency and can serve other authenticated API clients too.

The selected stack is Rust, HTTP/JSON with OpenAPI, Firecracker and Linux KVM, PostgreSQL, S3-compatible object storage, and OpenTelemetry with Prometheus/Grafana. Kubernetes deploys the API and controllers; dedicated Linux hosts run the microVMs through our supervisor. Individual sandboxes are not Kubernetes pods in the initial design, so sandbox placement and resource accounting remain our controller's responsibility.

Start with [the implementation design](docs/implementation.md) for component boundaries, execution contracts, recovery, security, and phased delivery.

The first complete flow is create → execute → save memory and disk → release compute → resume → destroy. Pause/resume is a core capability; performance optimizations and multi-host scheduling follow a verified single-host implementation.
