# OpenAPI and wire models

Status: the implemented Project API has a checked [OpenAPI 3.1 document](../api/openapi.json) and [generated Rust wire models](../crates/sandbox-protocol/src/api.rs). The API uses those models for JSON requests/responses and SSE payloads. Client transport, SDK packages and the public CLI are not implemented yet. This is interface evidence, not sandbox isolation evidence.

The specification owns exact wire shapes for 14 operations: create, execute, destroy, cancel, sandbox/operation reads and lists, retained-output reads, SSE, binary uploads, and capture/read/release downloads. It contains no planned Admin, browser-session, pause/resume or snapshot endpoints. It is a repository artifact; the service does not expose a new discovery endpoint. Replace its reserved example HTTPS origin with your installation's trusted API URL.

[API contract](api-contract.md) owns admission, authorization, idempotency, retry, retention and streaming semantics. [The OpenAPI standard](https://spec.openapis.org/oas/v3.1.0.html) defines the document format. The contract version matches the workspace's current `0.0.0`; it is not a published release or a claim that the runtime is ready for production.

## Change and check the contract

Use Python 3.10 or newer and the pinned Rust toolchain:

```sh
make api-setup       # isolated .venv-openapi, pinned validation dependencies
make api-generate    # after editing api/openapi.json
make api            # standards validation, generation drift, checker regressions
make check          # additionally runs Rust checks and PostgreSQL/router conformance
```

`make up` supplies PostgreSQL on port 55432. Direct `cargo test` runs the conformance test too; install the validator environment first. `HUDSON_OPENAPI_PYTHON` may point to another Python environment with the pinned [validation dependencies](../scripts/api-requirements.txt). Nothing installs into system Python. The Rust CI job performs this setup and all checks before merge.

Edit the specification, regenerate, change the handler/admission implementation as needed, and update the owning semantic document in the same PR. Generated output is committed for ordinary Cargo builds; Python is needed for regeneration and contract tests, not by the runtime. CI rejects stale output. Rust and internal protobuf dependencies are unchanged.

## Generation boundaries

[The generator](../scripts/generate_api.py) supports this repository's finite model profile: named objects, scalar strings/booleans/integers, arrays, string-keyed maps, local references and explicit nullable unions. It is not a general OpenAPI client generator. Unsupported type composition fails. `x-rust-model` selects a named wire object; the other `x-rust-*` extensions preserve integer widths, omitted/null/default behavior, existing JSON result storage, and redacted command debugging. These extensions do not change the public JSON schema.

Generation emits serde shapes, not authorization or complete input validation. Existing admission code still enforces numeric and aggregate byte limits, resource compatibility, workspace path rules, deadlines, quotas and lifecycle state. For example, JSON Schema's `maxLength` counts characters; `x-max-utf8-bytes` records the additional byte limit. Operation result/error objects retain documented, extensible metadata; they are not evidence that an unresolved operation succeeded. Clients must tolerate additive response fields and status/code values.

Command argument/environment order and defaults affect durable digests. [Golden normalization tests](../crates/sandbox-protocol/tests/api_wire.rs) preserve command serialization order, sorted environment keys, defaults, create/destroy null fields, strict command/destroy/cancel inputs and redacted Debug. List `next_cursor` remains present and nullable; absent optional status fields remain omitted. No digest-version change or database migration is involved.

SSE is described as `text/event-stream` with `x-sse-events` referring to generated JSON payload schemas. This extension documents frames; it does not implement a stream decoder, reconnection or retry policy. Binary endpoints declare their content type and bounded byte-position headers. File capture tokens and output cursors are opaque positions, never credentials. Command completion, output completion and allocation release remain separate facts.

## Evidence and remaining client work

[Router conformance](../crates/sandbox-api/tests/openapi.rs) exercises every declared operation against PostgreSQL. Its storage/guest adapters supply synthetic bytes. The independent [validator](../scripts/check_api.py) checks successful request shapes, response status/media types, required headers, JSON payloads and actual SSE framing; missing successful coverage of a declared operation fails. It also validates real bad-request, unauthorized, cross-project, expired-response and invalid-range responses. The malformed UTF-8 path regression first exposed native plain-text extractor errors on five routes; those routes now return uncached `400 bad_request` problems. A source inventory guard compares literal Axum route/method registrations with the specification and rejects unrecognized registration syntax. This guard is specific to the current registration style; it is not a Rust parser. [Checker regression tests](../scripts/test_api_contract.py) prove schema/header drift is detected. These checks complement the existing authorization, storage and runtime suites; they do not replace them.

The validator is [openapi-spec-validator](https://github.com/python-openapi/openapi-spec-validator), pinned with its transitive Python dependencies. JSON Schema 2020-12 validates actual JSON exchanges. The repository permits local references only; validation does not fetch schema URLs supplied by a contract.

The next client layer must generate the Rust, Python and TypeScript request models/transport from this same specification and run shared conformance cases. It must explicitly handle private credential configuration, stable idempotency keys across restarts, uncertain outcomes, bounded waits, binary output, stream reconnects and file digests. Generated DTOs alone are not an SDK, and no package or installer is published by this change.
