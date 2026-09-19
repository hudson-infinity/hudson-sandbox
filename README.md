# Hudson Sandbox

The planned isolated execution backend for [Hudson](https://github.com/hudson-infinity/hudson), implemented in Rust with Temporal for durable coordination and Firecracker for Linux microVM isolation.

**Status: design only.** This repository contains implementation documentation; no sandbox service, guest agent, deployment, or security guarantees have been implemented or validated yet.

Hudson Sandbox will create execution environments, run commands, exchange files, enforce resource and network limits, and reclaim resources. Hudson's main runtime will own the agent loop, business permissions, approvals, and credential authority.

Start with [the implementation design](docs/implementation.md) for component boundaries, execution contracts, recovery, security, and phased delivery.

The design keeps Temporal and trusted workers outside customer microVMs. Customer code runs inside Firecracker; the durable execution record survives the loss of that VM.
