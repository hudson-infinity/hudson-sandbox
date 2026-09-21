# File transfer

## Implementation status

The Linux guest library implements bounded upload staging, atomic file publication, retained mutation receipts and captured downloads. Shared descriptors live in [`sandbox-protocol::files`](../crates/sandbox-protocol/src/files.rs); the engine is [`sandbox-guest::files::Transfers`](../crates/sandbox-guest/src/files/mod.rs).

This foundation is not yet exposed through the guest wire protocol, supervisor RPCs or public API. The guest boot path does not instantiate it. Public file transfer remains unfinished under [issue #62](https://github.com/hudson-infinity/hudson-sandbox/issues/62). It needs authenticated admission, original allocation/boot ownership, dispatch fencing, staging transport, aggregate service quotas, retention, destroy coordination and real microVM acceptance before customers can use it.

## Workspace and path contract

`Transfers::open` accepts an existing absolute workspace directory selected by the caller and an allocation/generation/boot context. Future guest wiring must choose this directory from trusted configuration, never from a customer-supplied host path. The root's ancestors are trusted configuration; the final root component must be a directory and cannot be a symlink.

Transfer paths are canonical UTF-8 names relative to this root. Absolute paths, empty components, `.`/`..`, backslashes, control characters, components longer than 255 bytes and paths longer than 4096 bytes are rejected. Spaces and Unicode are allowed. There is no shell expansion, archive extraction or automatic parent-directory creation. The top-level `.hudson-transfers` name is reserved for internal state.

Subsequent lookup uses an open root descriptor and Linux `openat2` with `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS` and `RESOLVE_NO_XDEV`. Parent symlinks, magic links and mount crossings fail closed. There is no weaker fallback on kernels without these features. These are the kernel's [path-resolution restrictions](https://man7.org/linux/man-pages/man2/openat2.2.html).

Downloads and internal staged-file reads first pin an `O_PATH` descriptor, inspect its type and link count, then reopen the exact descriptor through an internally generated `/proc/self/fd` reference. This rejects FIFOs, sockets, devices, directories and multiply linked regular files before opening them for data I/O. Customer strings never become proc descriptor references.

Publication resolves the parent again, then uses that open directory descriptor. An existing final symlink or hardlink is replaced as a directory entry; its target is never opened or truncated. Existing directories cannot be replaced by uploaded files. This follows [rename semantics](https://man7.org/linux/man-pages/man2/rename.2.html). Renaming the already-open parent concurrently does not retarget its descriptor: publication applies to that pinned directory inode. This is not a promise that its pathname remains below the root after an adversary moves the directory.

## Upload and recovery contract

An upload descriptor binds an operation ID, relative path, declared size, SHA-256 and ordinary file mode (`0644` or `0755`). The versioned descriptor digest covers all fields. The retained receipt additionally binds allocation, generation and boot context. File bytes are absent from metadata and debug output.

1. `begin` checks the descriptor and current parent, reserves retained capacity, creates and syncs a staging file, then persists a staging receipt. An exact operation retry returns its existing receipt; a changed descriptor conflicts.
2. `write_chunk` accepts at most 32 KiB at a contiguous offset. Repeated bytes must match the existing prefix exactly. A partial write can resume by checking its written prefix and appending the rest. Gaps, mismatches, overflow and writes beyond the declared size fail. An acknowledgement follows the file sync.
3. `commit` verifies exact length and SHA-256, checks for detected changes during hashing, resolves the parent and applies the allowed file mode. It then persists and syncs `commit_intent` **before** renaming the staging file into place.
4. After rename, both affected directories are synced, then a `committed` receipt is persisted. Until publication, an existing destination remains unchanged. A completed operation never republishes on retry, even if a later workload changes or deletes the destination.
5. Any failure after commit intent leaves uncertainty. Reopening converts retained `commit_intent` to `unknown`; it never guesses whether the rename happened and never repeats it. `commit` on an unknown, committed or aborted receipt only returns that receipt.

A failed receipt persistence fences the current engine until it is reopened. Startup validates context, descriptor digests, retained counts and staging lengths. Missing staging for an admitted staging receipt or corrupt history fails closed. Validated orphan staging and temporary metadata can be removed; their names never authorize replay. A workspace lock prevents two engines from owning the same state concurrently.

`abort` applies only to staging: it retains an aborted receipt before removing bytes. It does not undo a committed or unknown mutation. Receipts and declared reservations remain until a future acknowledged reclamation protocol can safely discard them. Deleting history to recover capacity would break retry safety and is unsupported.

## Downloads and bounds

`capture` pins a regular file, checks the size, reads a bounded byte sequence and rejects detected size/mtime/ctime changes during capture. Its digest covers exactly the captured bytes. Subsequent range reads use the captured buffer, so path replacement or later file writes cannot splice different versions into that handle. This is a captured byte sequence, **not an atomic filesystem snapshot**; metadata checks cannot prove consistency against malicious guest-root activity.

| Bound | Current behavior |
| --- | --- |
| File size | 8 MiB, including zero-length uploads |
| Chunk size | 32 KiB for uploads and captured download reads |
| Retained upload descriptors | 128 per workspace, including aborted, unknown and completed uploads |
| Retained upload reservations | 64 MiB of declared sizes per workspace; no automatic refund |
| Live captured downloads | Eight per workspace, each at most 8 MiB plus a one-byte growth check |
| Persisted metadata | At most 16 KiB per JSON file; bounded directory scanning |

A capture holds workspace ownership until dropped, including when the engine itself has been dropped. Reopening cannot bypass the live-capture bound. Returned chunks are borrowed slices; the transport must bound copies, connections, request lifetimes and total workspaces separately. The synchronous engine belongs in a bounded blocking worker. Byte bounds do not imply a disk-I/O deadline on a stalled filesystem.

Uploaded destination files consume the guest filesystem's ordinary capacity. The transfer reservation is not a substitute for disk quotas, and it cannot limit files that workload processes write themselves. A dedicated local writable filesystem with working `openat2`, directory fsync, file locks and same-filesystem rename is required; guest init and release packaging still need to enforce that configuration.

## Trust and evidence

The guest runs customer commands as guest root. They can tamper with workspace data, agent state and reported receipts. The workspace path checks defend the file interface against unintended traversal; they do not turn the guest agent into an isolation boundary. Host allocation fencing, VM isolation, project authorization and host-side validation remain required. Guest metadata or `committed` is never proof of host cleanup, customer authorization or external-effect correctness.

[Protocol tests](../crates/sandbox-protocol/src/files.rs) cover canonical paths and descriptor identity. The [unprivileged Linux filesystem suite](../crates/sandbox-guest/tests/files.rs) covers bounded chunk retries, partial writes, restart, both durable commit crash windows, unknown rename failure, abort, history corruption, parent substitution, destination links, special-file rejection, retained limits, maximum-size round trip and stable bounded downloads. Run it on Linux with:

```sh
cargo test -p sandbox-guest --test files
```

The suite uses temporary directories and needs neither root nor KVM. These tests do not exercise the future authenticated transfer route or prove supported-host isolation. Cross-project authorization, concurrent transport limits, mount-policy acceptance, destroy interaction and real microVM upload/execute/download remain acceptance work for the complete feature.
