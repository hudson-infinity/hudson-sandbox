# 0001: standalone service with no Temporal dependency

Status: Accepted
Date: 2026-09-20 (recording a choice made earlier in this repository's design)

## Context

Hudson uses Temporal for durable agent tasks. The sandbox service needs durable asynchronous control of its own: operations outlive HTTP requests, controllers restart, hosts fail mid-transition, and every transition needs recoverable evidence. The obvious move was to reuse Temporal here too.

Doing so would make Temporal a hard dependency for anyone self-hosting the sandbox service, including a contributor running a single-host development setup.

## Decision

This repository has no Temporal dependency. Durable intent lives in PostgreSQL, and focused reconciliation loops drive lifecycle transitions, as specified in [lifecycle](../lifecycle.md#operations-and-controller-ownership).

Hudson may wrap sandbox API calls in Temporal Activities in its own repository. Those calls use the same operation IDs, status APIs, and cancellation contracts as any other client.

## Consequences

Self-hosting needs PostgreSQL, object storage, and one Linux host, and nothing else. The service is usable by callers that have never heard of Hudson.

In exchange we write our own claim, lease, retry, and reconciliation logic. The correctness burden is real and is why the recovery table and failure-injection gates in [lifecycle](../lifecycle.md#destroy-and-recovery) exist.

## Alternatives considered

**Temporal for sandbox lifecycle.** Rejected: it makes a workflow engine a prerequisite for every deployment and every developer, for control flow that a handful of bounded reconciliation loops can express.

**A second lighter workflow framework.** Rejected: it adds a dependency and an abstraction without removing the need to model allocation generations, supervisor epochs, and claim revisions ourselves.
