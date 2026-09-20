# Contributing to Hudson Sandbox

Hudson Sandbox is being designed in public by Hudson Labs. Start with the [README](README.md) and [documentation guide](docs/README.md). The runtime is not implemented yet; documentation checks work today, while VM integration tests and installation commands will arrive with the code.

License selection is pending. Resolve licensing with the maintainers before submitting substantial implementation contributions; public visibility alone does not grant an open-source license. No CLA or commit-signoff requirement is currently configured.

## From idea to pull request

1. **Agree on the problem.** Small fixes may go straight to a PR. Open an issue first for substantial features, architecture changes, or new dependencies. Describe the use case, scope, and acceptance criteria. Report vulnerabilities [privately](SECURITY.md).
2. **Create a branch.** Outside contributors fork the repository; maintainers use a branch in this repository. Start from current `main`. Use a descriptive name such as `feat/sandbox-create`, `fix/snapshot-cleanup`, or `docs/auth-flow`. Coding agents use `codex/` branches in isolated worktrees.
3. **Make one focused change.** Preserve unrelated edits. Update the authoritative document when behavior changes. Add tests that demonstrate the behavior or regression when executable code is involved. Do not claim a planned feature is implemented.
4. **Check locally.** Run the commands below and any tests relevant to the change. Report skipped checks and their reason.
5. **Open a PR against `main`.** Use the PR template. Draft PRs are welcome for early feedback; mark ready when the change and its validation are reviewable.
6. **Review and merge.** Address feedback, rerun affected checks after edits, and obtain approval on the current change. A maintainer squash-merges after required checks pass and review threads are resolved.

Maintainers have write/maintain/admin repository access. The initial code owners are the existing administrators, @itsafal and @dipeshbabu; either can review. The PR author cannot provide the required approval on their own PR. Auth, isolation, snapshot, and recovery changes require a maintainer familiar with the affected boundary; ownership alone is not evidence of expertise.

## Commit and PR titles

Use `type: short description` for the PR title and prefer the same style for commits:

```text
docs: explain sandbox pause and resume
feat: add sandbox creation
fix: retain disk reservations during cleanup
ci: validate documentation on pull requests
```

Common types are `feat`, `fix`, `docs`, `test`, `refactor`, `ci`, and `chore`; an optional scope is fine. Explain why in the body when useful. This is a writing convention, not a commit-message bot or release trigger. We squash merge, so intermediate commits do not need to be rewritten just to satisfy a message format.

## Checks available today

From the repository root, with Python 3.10 or newer:

```sh
python3 scripts/check_docs.py
git diff --check
```

The `docs-check` GitHub Actions job checks repository Markdown for local inline links and anchors, closed code fences, and valid JSON examples, plus whitespace errors in the proposed commit diff. It does not fetch external links, render Mermaid, or validate sandbox behavior. Use inline Markdown links and ATX (`#`) headings for checked document references.

Keep the job name stable because branch protection requires it. CI runs on every PR to `main` and on pushes to `main`, including fork PRs with read-only permissions and no repository secrets. GitHub may require maintainer approval before a first-time contributor's workflow runs. Use GitHub-hosted runners for untrusted PR checks. Do not run fork code on privileged sandbox hosts or introduce `pull_request_target` execution of contributor code.

As runtime code lands, add formatting, linting, unit/integration checks, and the applicable [roadmap gates](docs/roadmap.md). Real Firecracker tests need a controlled Linux/KVM environment. A docs-only PR does not need a VM test; a change to snapshot correctness does.

## Review and branch rules

`main` uses these repository settings:

- Pull requests with at least one approving review; code-owner review applies to owned files once CODEOWNERS is on the base branch.
- Stale approvals dismissed after changes, required `docs-check` success, and an up-to-date branch before merge.
- Resolved review conversations and linear history; squash is the enabled PR merge method.
- No force pushes, branch deletion, or administrator exemption from these protections.

CODEOWNERS routes review; GitHub settings enforce the gates. Changes to those settings must preserve the agreed review process. CI does not replace human review. If checks fail, fix the cause or document a genuine infrastructure failure for maintainer investigation; do not disable a required check to land a PR.

For the initial process PR, GitHub cannot use a CODEOWNERS file that has not reached `main` yet. The required approving review still applies. Future PRs use the merged ownership file.

## Coding agents

Agents follow the same contribution process: isolated worktree, focused branch, relevant validation, and a PR explaining results and limitations. They may not fabricate reviews, mark unrun checks as passed, or treat their own review as maintainer approval. Merging, tagging releases, and publishing artifacts require explicit maintainer authorization and must satisfy repository rules. Never put tokens, customer snapshots, or private output into commits or PRs.

## Releases

Merging is not releasing or deploying. There are no runtime releases or publishing workflow yet. Maintainers will publish versioned releases from reviewed `main` commits once the applicable delivery gates have evidence, with release notes, compatibility changes, upgrade instructions, and known limitations. Begin initial releases in `0.x`; select the first version when a usable artifact exists. Breaking changes must be called out even before `1.0`.

## Maintainer references

GitHub documents [protected branches](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches) and [private vulnerability reporting](https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/configure-vulnerability-reporting/configure-for-a-repository). Repository settings are managed on GitHub, not automatically applied by editing this file.
