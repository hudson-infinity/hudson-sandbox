# Sandbox Management UI

Status: selected user flows; UI implementation and usability validation pending. This document owns navigation, screens, action flows, and visible loading/error states. [Auth design](auth-design.md) owns permissions and session security; [lifecycle](lifecycle.md) owns actual action completion.

## Purpose

Help a project user manage their sandboxes, and an installation Admin manage projects and infrastructure. This UI does not plan agent work or contain Hudson conversations, business approvals, tasks, or deliverables. It is usable without Hudson and calls the same backend services as API clients.

## Navigation and screen ownership

```text
Login: Project access or Admin access
│
├── Project view
│   ├── Sandboxes → sandbox detail → operation/output and snapshot links
│   ├── Operations → operation detail
│   ├── Snapshots → snapshot detail
│   └── Usage
│
└── Admin view
    ├── Projects → project settings, limits, tokens, and project view
    ├── Hosts → host detail and maintenance drain
    └── Audit
```

The backend-derived access level determines available screens. Project access opens directly into its one project. Admin access starts at Projects and shows a persistent Admin access indicator plus the selected project when viewing project resources. Switching target projects changes the visible scope, not the login identity; clear old project data, pending forms, and streams before rendering another project. Hiding navigation never replaces server authorization.

Allocation information appears in sandbox/host details; it does not need a separate top-level screen. Project views show resource reservation and state without exposing host addresses. Admin views may link the allocation to its host.

| Screen | Primary content | Actions and feedback |
| --- | --- | --- |
| Sandboxes | Name/ID, state, resource limits, last observation, current operation | Create; open detail; filter by state |
| Sandbox detail | Current state and freshness, resource reservation, active/recent operations, current snapshot | Execute, pause, resume, destroy; expose only valid actions and show pending progress |
| Operations | Kind, target sandbox, status/phase, request time, deadline | Inspect result/output; request cancellation when supported |
| Snapshots | Sandbox, creation time, publication state, size, expiry | Inspect metadata and producing pause operation; no arbitrary rewind/fork action |
| Usage | Current reservations versus configured project limits | View pressure/limits; Admin may open project settings |
| Admin Projects | Project state, limits, usage, credential metadata | Create project, edit quotas, issue/revoke tokens, enter project view |
| Admin Hosts | Health, observation freshness, compatible resources, reserved/available capacity, drain state | Inspect resident allocations and request maintenance drain |
| Admin Audit | Actor credential label/ID, action, target, time, outcome, related operation | Filter and inspect redacted history |

Charts and filters must use actual data. Do not invent utilization percentages from reservations or imply a desired state is an observed state. Detailed API read/list/filter contracts must be defined before these screens are implemented.

## First setup and token flow

1. An installation administrator runs authenticated local setup to create the initial Admin credential. The UI cannot bootstrap an admin anonymously.
2. Open the login page, choose Admin access, and submit the credential over HTTPS. The server validates it and issues the session described in [auth design](auth-design.md#browser-login-and-session-flow).
3. If no projects exist, show a Create project empty state. Collect name and explicit limits; show validation errors without losing nonsecret form values.
4. Open the new project's Tokens panel and issue a project token. Display its raw value only in the one-time issuance response, with a copy action and a clear instruction to save it in the calling backend's secrets.
5. After leaving that view, show only key metadata, expiry, and revocation state. Never offer Reveal token. A lost issuance response links to the known key metadata and an explicit revoke/reissue flow, following [auth retry rules](auth-design.md#storage-and-audit).
6. Hudson or another application sends the project token as a bearer credential. A project user may also use it for Project access login to this management UI.

Project users cannot issue keys or change quotas. Rotating a project token is an Admin flow: issue replacement within the active-key limit, update the consuming backend, then explicitly revoke the old key. Explain that revocation ends access derived from that key but is not cancellation of already-executed work.

## Create and execute flow

The create form accepts a name, allowed image digest, and CPU/RAM/disk limits. Display configured limits and validate locally for convenience; the server is authoritative for image authorization and capacity. Do not expose a nonexistent image catalog or assume that an example digest is runnable.

On submit, generate one idempotency key for that logical request and reuse it across network retries. Disable duplicate submission while pending. When admitted, navigate to the sandbox/operation and show Creating with its operation ID. Mark Running only after the backend confirms readiness. If capacity is queued, show the queue/deadline state; if rejected, show the returned cause and permit an intentional new request.

Execute collects executable, argument array, working directory, nonsecret environment, deadline, and output bounds. Explicit shell execution remains a guest-only operation. Once admitted, open the operation's output view. Input/output limits and exit status must be visible; completed command output is not proof that an agent's business task succeeded.

## Pause, resume, destroy, and cancellation

| Action | What the UI communicates | Completion |
| --- | --- | --- |
| Pause | Saving memory/disk, publishing snapshot, then releasing compute | Show Paused only when snapshot and allocation-release evidence are confirmed |
| Resume | Reserving resources, restoring saved state, reconnecting safely | Show Running after the backend confirms readiness; same sandbox ID |
| Destroy | Confirm target sandbox and explain that future resume is prevented | Show Destroying until VM/allocation release is confirmed; report any retained/pending-deletion bytes separately |
| Cancel operation | Cancellation requested; action may already have effects | Show Cancelled only when the backend confirms a safe cancellation boundary |

Pause/resume need a clear pending state; destroy and credential revocation require explicit confirmation naming the affected resource. A confirmation is not an authorization grant. When a conflicting transition wins, link to its operation instead of silently retrying a new action. Repeated clicks and page reloads must not create duplicate work.

A paused sandbox can show an execute operation as Suspended; it keeps its original identity/deadline. Expired snapshots disable resume with an explanation. Destroyed sandboxes show a tombstone, not a resume button. Unknown outcomes show uncertainty and a link to current reconciliation progress; never turn them into a generic Retry command button.

## Output and session behavior

Output is a read-only stream of untrusted text. Show connection state and the last received cursor; distinguish a disconnected view from a failed or cancelled operation. Reconnect with the existing operation/cursor under fresh authorization. Indicate replay gaps and link to retained output when available. Pausing can end the stream; resuming attaches to the same execution without resubmitting it.

Session expiry or revocation closes protected views and prompts login. Preserve only safe navigation context; discard credentials and sensitive output from active view state. After login, reread the target state and operation before offering an action. A login must not automatically replay an unconfirmed mutation. The session/CSRF/Origin contracts are in [auth design](auth-design.md), not duplicated here.

Guest output never runs as HTML/scripts on the management origin. Use readable text, keyboard-accessible controls, labelled inputs, focus-managed confirmation dialogs, and status text alongside color. Never include credentials in analytics or client error reports.

## Admin maintenance and audit

Host detail displays both reported health and observation time. Unreachable is not empty capacity. Requesting drain records intent and prevents new placements; show remaining allocations and snapshot work until the backend confirms completion. Do not offer an action that merely edits database status to force apparent VM release.

Quota edits show current values and current reservations, with server-side validation. They do not implicitly kill workloads to satisfy a lower limit. Any future destructive enforcement policy needs its own explicit contract.

Admin action detail links its audit receipt and, when applicable, lifecycle operation. Clearly distinguish Requested, Accepted, and Completed. A shared credential label is not a verified human identity. Show only safe change summaries; no raw secrets or guest output in audit records.

## Shared states

| State | Required behavior |
| --- | --- |
| Loading | Show loading without presenting zero usage or no resources as confirmed facts |
| Empty | Give the next permitted action; no misleading Create project button for Project access |
| Validation/admission failure | Preserve safe input and show actionable server feedback |
| Stale or disconnected | Show last observation time; do not infer current VM health |
| Unauthorized/session expired | Stop protected requests and require login |
| Forbidden/not found | Explain lack of access without leaking another project's resource identity |
| Unknown execution outcome | Show uncertainty and reconciliation evidence; avoid blind retry |
| Partial cleanup | Identify remaining cleanup and retained resources separately from stopped execution |

## Acceptance checks

No UI code or tests exist yet. Before shipping, exercise:

1. Admin bootstrap/login → create project → issue token → backend request → Project login, without Hudson.
2. Project access cannot view other projects or admin screens/data; direct API attempts also fail.
3. Create/execute/pause/resume/destroy displays backend-confirmed progress and preserves request identity on retry/reload.
4. Expiry/revocation, live-output disconnection, replay gaps, and unknown outcomes never cause command resubmission.
5. Host draining and allocation release remain honest during stale observations and partial cleanup.
6. Token issuance is one-time, rotation/revocation are explicit, and raw credentials never enter browser persistence or telemetry.
7. Guest output stays inert; keyboard navigation and confirmation/error states are usable.

Link actual browser/integration tests here when implemented. Backend permission checks remain required even if UI tests pass.

## Open decisions

Choose frontend framework, visual design, list/pagination/filter contracts, usage sampling, output wire format, and concrete accessible components. Wireframes and a running UI are separate implementation artifacts; this document does not claim they exist. See [roadmap](roadmap.md) for delivery order.
