# Documentation guide

The repository is currently **design only**. These documents record selected direction and proposed contracts; no runtime, migrations, UI, installer, or runtime tests are implemented. Contribution tooling and documentation checks are available. Read this index to find the authoritative home for each topic.

## Reading order and ownership

| Document | Owns | Read it to understand |
| --- | --- | --- |
| [Architecture](architecture.md) | Components, stack, topology, isolation boundaries | What runs where and how data moves |
| [Data models](data-models.md) | Fields, relationships, IDs, database constraints, storage layout | What we persist and how records connect |
| [Authentication](auth-design.md) | Project/Admin permissions, credentials, sessions, CSRF, revocation, audit semantics | Who can do what and how access is enforced |
| [Lifecycle](lifecycle.md) | State transitions, completion evidence, deadlines, cancellation, recovery | How create/pause/resume/destroy actually work |
| [API contract](api-contract.md) | Admission, request retries, response/errors, files, streaming semantics | How clients interact with the service |
| [UI design](ui-design.md) | Screens, navigation, user flows, loading/error states | How Project users and Admins manage the installation |
| [Roadmap](roadmap.md) | Implementation sequence, exit gates, evidence, deferred work | What to build next and when it is ready |

Start with Architecture for a system overview. Backend contributors then read Data models, Lifecycle, API contract, and Authentication. UI contributors read UI design, Authentication, and API contract. Installation work starts with Roadmap; a working self-hosting guide will follow a validated installer.

For branches, commits, reviews, and local documentation checks, read [Contributing](../CONTRIBUTING.md). Security reports use the private channel in [Security](../SECURITY.md).

## Keeping the docs together

- Give each rule one authoritative home from the table. Other documents summarize and link rather than copying exact limits, phases, or permission matrices.
- Distinguish selected design, unresolved proposals, implemented behavior, and verified behavior. Do not mark a contract implemented without the code and applicable evidence.
- Each detailed contract has acceptance checks and open decisions. Add links to real test files/CI evidence as implementation lands; do not link to hypothetical test paths.
- Update the owning document with a change, then update affected links/examples and the delivery gate. Keep API examples consistent with OpenAPI once that specification exists.
- Record major new tradeoffs in `decisions/` when they are made. Mark superseded decisions rather than maintaining two contradictory current contracts.
- Keep the root README short. Add runnable development/deployment/operations guides when the underlying commands and procedures work, not as empty placeholders.

## Where the earlier documents went

| Previous document | Current home |
| --- | --- |
| `artitecture.md` | [Architecture](architecture.md), with detailed flows moved to [Lifecycle](lifecycle.md) |
| `identity-and-resources.md` | IDs/storage in [Data models](data-models.md); retries/routes/errors in [API contract](api-contract.md) |
| `implementation.md` | Components/security in [Architecture](architecture.md); execution/recovery in [Lifecycle](lifecycle.md); delivery/operations planning in [Roadmap](roadmap.md) |

The previous documents are retired rather than maintained in parallel. Their earlier versions remain available in Git history. The reorganization preserves the chosen stack, standalone service boundary, mandatory authentication, six resource models plus two UI security tables, and the core pause/resume milestone.
