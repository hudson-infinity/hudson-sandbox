# 0002: no MCP server in the initial scope

Status: Accepted
Date: 2026-09-20 (recording a choice made earlier in this repository's design)

## Context

Agent clients increasingly expect to reach tools through MCP. Shipping an MCP server would make the service reachable by chat clients with no custom tool integration.

It would also add a protocol adapter to maintain alongside the HTTP API, the SDKs, and the CLI, before any of those exist.

## Decision

No MCP server in the current scope. Agent integration uses the HTTP API, an SDK, or the CLI through a harness shell tool, as described in [architecture](../architecture.md#client-interfaces-and-agent-integration).

## Consequences

One public boundary to specify, version, and secure. A chat client with no custom-tool or shell integration cannot use the service.

This is a sequencing decision, not a judgement that MCP is unnecessary. It should be revisited deliberately rather than by drift; [alternatives](../alternatives.md#revisit-triggers) carries the trigger.

## Alternatives considered

**Ship MCP alongside the first API.** Rejected: it doubles the client surface before the API's own shape has been validated against a working runtime.

**Ship MCP instead of the CLI.** Rejected: humans, scripts, and agents with shell access all need the CLI, and the CLI is also how we test the API by hand.
