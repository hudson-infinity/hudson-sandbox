# Guest command runner

Status: the Linux guest component runs real processes and has local recovery tests plus [aarch64 Firecracker execution evidence](evidence/2026-09-21-aarch64-commands.json). The [authenticated guest protocol](guest-protocol.md) now connects it to a supervisor client for command and bounded output calls. The [real supervisor and public execute path](real-supervisor.md#command-dispatch-and-reconciliation) now integrate this runner with host watchdog ownership. [Retained output retrieval](output-storage.md) is implemented. [Public SSE](api-contract.md#implemented-output-streams) supplies live and archived output. Production images and file transfer remain unfinished. This does not pass either [Phase 1 gate](roadmap.md#scope-discipline-for-phase-1).

## Responsibility and trust

The [runner](../crates/sandbox-guest/src/runner.rs) owns one active command at a time inside a guest. A command receives its requested argv, environment and absolute working directory, with null stdin. It runs directly without an implicit shell; callers can explicitly invoke a shell. Its environment starts with PATH and HOME, plus requested variables, rather than inheriting agent credentials. The diagnostic binary reads one bounded JSON request and prints a receipt; it has no listening socket and must never be exposed as a host command service. The separate `serve` mode requires authenticated vsock, as specified in the guest protocol.

[Guest root remains the product contract](decisions/0003-guest-root-with-our-kernel.md). Commands retain root userspace privileges and a writable filesystem. Each command runs under a namespace init in a new PID and private mount namespace, with its own proc mount. The launcher joins an operation-owned cgroup before creating workload descendants. When the main command exits, the namespace init exits and the runner kills and removes the owned cgroup tree; background children do not survive command completion. A long-running service must remain the active command within its deadline. This is not the later sessions capability.

These guest mechanisms help process ownership and recovery; they are not a security boundary against hostile guest root. This implementation does not prevent root from manipulating guest files, cgroups, mounts, devices, the agent or its receipts. It is not the final protected guest-agent design. Guest output and metadata remain untrusted by the host. In particular, `cleanup_confirmed` describes observed guest command cleanup, never permission to release a host allocation. Host VM limits, host network enforcement, an independent watchdog and host-owned observations must provide the authoritative boundary before untrusted production use. Missing agent contact must fail the sandbox according to [lifecycle](lifecycle.md#resume).

The launcher has one narrowly allowed unsafe call for Linux `unshare_unsafe`, before runtime threads exist. It requests mount/PID namespaces only, not file-descriptor table separation. Other code retains the workspace unsafe-code denial.

## Admission, receipts and recovery

An execution is bound to allocation ID, generation and the current kernel boot ID. The state directory must be private to that context; the cgroup root must be a dedicated, empty subtree beneath `/sys/fs/cgroup`, with the pids controller enabled, while the agent remains outside it. Use an operator-controlled launcher path. An exclusive state-directory lock rejects a second agent. Changed allocation or boot context is rejected; snapshot/resume adoption is not implemented.

Before launching, the runner writes and syncs a `launch_intent` receipt, then syncs its directory entry. The digest covers the complete normalized request, including the deadline and output limit. An identical retry returns the existing operation; a changed payload under the same operation ID conflicts. Retry lookup precedes new-work deadline, capacity and busy checks, so an expired deadline does not invalidate an existing handle. Dropping the initiating caller does not cancel the owned task.

| Receipt state | Meaning |
| --- | --- |
| `launch_intent` | Accepted durably; execution may or may not have started |
| `exited` | The namespace launcher reported an exit code or signal and the owned process tree was cleaned |
| `timed_out` | The deadline was observed and cleanup was confirmed |
| `cancelled` | Cancellation was observed and cleanup was confirmed |
| `unknown` | Execution, output completion, receipt persistence or cleanup could not be confirmed |

`exited` does not mean exit code zero. Cancellation is persisted before acknowledgement and does not undo side effects. Deadlines use both an absolute wall-clock check and a monotonic timer; deadline and cancellation handling cover output draining and launcher input. Blocking guest kernel/storage failures still require the independent host watchdog.

On restart, a nonterminal receipt is never replayed. Recovery kills the owned cgroup tree, waits for emptiness, removes it, and saves `unknown`. Cgroup removal also prevents a delayed launcher from joining that old group. Unconfirmed cleanup, corrupted metadata, unknown cgroups and context mismatches fail closed. A failed terminal receipt write returns an in-memory `unknown` and disables new work; reopening reconciles the retained intent. A command with missing exit evidence is never reported as successful.

Output files are guest-local binary stdout/stderr streams, not UTF-8 logs or public download objects. Both pipes keep draining after their shared retention budget is spent, so excess output cannot deadlock a cooperating command. Capture workers flush each chunk before publishing its seen/stored/truncated counters in the in-memory receipt. Live reads expose only that captured prefix and distinguish temporary EOF from command completion. A completed receipt durably records final counters after syncing output and confirming cleanup. Recovery after interruption may leave partial files without complete counters; an `unknown` receipt is not an output-completeness claim. Command arguments and environment values are absent from receipts and request Debug output. Workload-generated output may itself contain sensitive values and requires the planned output access controls.

## Bounds and retained state

The internal [history retirement primitive](history-reclamation.md) can durably fence and prune an acknowledged terminal prefix. It is available through authenticated internal guest/supervisor RPCs with host fencing. No automatic caller exists yet; public admission and database reservation accounting retain their existing limits.

The current component limits are deliberately small and fixed; public configurable resource classes remain future integration work.

| Resource | Bound |
| --- | --- |
| Active commands | One per runner |
| Command arguments | 1–256 arguments, 32 KiB total |
| Environment | At most 128 variables, 16 KiB keys/values total |
| Encoded request | 64 KiB; unknown fields and NUL arguments/environment values rejected |
| Working directory | Absolute path, at most 4 KiB |
| New-command deadline | In the next six hours |
| Retained output | 1 byte–16 MiB combined stdout/stderr per operation |
| Retained history | 128 receipts and 64 MiB of output reservations per runner |
| Guest command processes | `pids.max=256`, including launcher helpers |

Retained reservations charge the requested output budget even if little output was produced. There is no silent receipt eviction: once full, new commands are rejected while retries remain inspectable. Host acknowledgement, output export and safe retention reclamation are follow-up work. Deleting the journal to free space would remove retry protection and is not a supported recovery procedure.

## Running the checks

The portable [model tests](../crates/sandbox-guest/tests/model.rs) run with `cargo test -p sandbox-guest`. The [Linux process tests](../crates/sandbox-guest/tests/linux_runner.rs) are explicitly ignored by normal CI because they require root, mount/PID namespaces and a writable cgroup v2 hierarchy. Do not report an ignored test as passed. Run reviewed code only in a dedicated disposable Linux development VM, such as the [local KVM environment](linux-development.md); these tests create unique cgroup subtrees and execute fixed test commands there. They do not exercise a VM isolation boundary.

Build as the ordinary development user, then run only the compiled privileged test artifact:

```sh
cargo test -p sandbox-guest --no-run --message-format=json > /tmp/hudson-guest-test-build.jsonl
guest_test_binary=$(python3 - <<'PY'
import json
from pathlib import Path
for line in Path('/tmp/hudson-guest-test-build.jsonl').read_text().splitlines():
    item = json.loads(line)
    if (item.get('reason') == 'compiler-artifact'
            and item['target']['name'] == 'linux_runner'
            and item.get('executable')):
        print(item['executable'])
        break
else:
    raise SystemExit('Linux test artifact not found')
PY
)
sudo env HUDSON_GUEST_TEST_VM=1 "$guest_test_binary" --ignored --test-threads=1
```

The original seven privileged tests passed on the nested aarch64 Linux host. They cover exit codes/signals, binary output, explicit environment, direct argv, root writes, PID visibility, descendant cleanup, bounded output, deadlines, cancellation, concurrent retries/conflicts, busy and expired admission, lock ownership, crash recovery without replay, context rejection, retention, corrupt metadata, failed receipt persistence and caller detachment. Four portable request tests also passed.

A separate disposable Firecracker experiment ran the release binary in a writable BusyBox microVM using the same [boot artifacts](evidence/2026-09-21-aarch64-boot.json). It observed UID 0 and a root filesystem write, exit code 7, a timed-out command, and `unknown` after killing and restarting the agent. The interrupted command's marker appeared once. All three receipts reported guest cleanup, then the actual Firecracker child exited zero and its host cgroup became empty and was removed. The [record](evidence/2026-09-21-aarch64-commands.json) includes the tested source and binary digests. The experiment's one-off image/init fixture is not a shipped image builder or automated VM gate.

Hostile root, egress, host resource exhaustion, x86_64 release compatibility, supervisor disappearance and the public API path were not validated by these checks. Their [required security validation](threat-model.md#required-validation) remains open.
