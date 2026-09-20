# Decision records

Status: active. This directory holds short records of significant choices, each with its status and, where applicable, what superseded it. The [documentation guide](../README.md) asks for these; this index is where they live.

A record belongs here when a choice is hard to reverse, when a reader would otherwise have to reconstruct the reasoning from a contract document, or when we expect to be asked "why not the other thing" later. Ordinary contract detail stays in the owning document.

## Records

| Record | Status | Subject |
| --- | --- | --- |
| [0001: standalone service with no Temporal dependency](0001-standalone-service-no-temporal.md) | Accepted | Whether sandbox lifecycle uses a durable workflow engine |
| [0002: no MCP server in the initial scope](0002-no-mcp-server-initially.md) | Accepted | How agents reach the service |

## Writing one

Copy the shape below. Keep it to one page. Record the decision as it was actually made, including the option not taken.

```text
# NNNN: short title

Status: Proposed | Accepted | Superseded by NNNN
Date: YYYY-MM-DD

## Context
What forced a choice, and what constrained it.

## Decision
What we chose, stated plainly.

## Consequences
What this makes easy, what it makes hard, and what it commits us to.

## Alternatives considered
Each rejected option and the reason it lost.
```

Number records sequentially. Do not edit an accepted record to reflect a new decision: add a new record and mark the old one superseded, so the history stays readable. A superseded record keeps its original text.
