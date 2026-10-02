# Protocol behaviour notes

The wire protocol is defined in `crates/unlatch-proto/src/wire.rs`. This page records behaviour
that both sides rely on but that a reader of the type definitions would not guess. Each point
is what `unlatchd` and `unlatch-core` do today and is covered by tests.

## Credit (flow control)

`wire.rs` ("Credit measure") is the reference. In short:

- **Server → client:** every `ReadChunk`, `SnapshotChunk` and multi-part `ListingPart` costs its
  frame body length as sent (flags byte plus payload, so the compressed size for LZ4 frames; the
  4-byte length prefix excluded). That is what occupies the pipes and the ssh window below the
  scheduler, which is what credit exists to bound (design review D10). The client grants back
  the body length it received, not the decompressed length; on compressible data (sources, logs,
  snapshots compress 3–20×) a decompressed grant would stop the window bounding anything.
- **Client → server:** a `WriteChunk` costs `data.len()`; `unlatchd` grants back exactly what it
  consumed, including for chunks of cancelled or unknown uploads.
- A bulk frame waits for a positive balance and may take it below zero by at most one frame. A
  single-part `ListingPart` rides the interactive lane and is charged without waiting.
- After `Welcome`, `unlatchd` grants the client an extra 768 KiB (a 1 MiB upload window in total).

Test: `engine::tests::compressible_read_keeps_one_window_of_wire_bytes` (a 50 MB all-zero read
keeps the server's balance within one window).

## Replies and edge cases

- **`Unwatch`** has no dedicated response: `unlatchd` replies `Response::Entry(dir)` with the
  directory now `lazy: true` (and `seq` bumped; other sessions get the same upsert as an event).
  The collapsed children keep their ids server-side, so a later `ListDir` returns the same ids.
- **`Remove` with a stale `base`** returns `Error(VersionMismatch)`; the engine maps it to
  `DeletionRejected` plus the current item. A partial recursive delete returns
  `Removed { kept }`, stored in the ops table like any success.
- **Delete op ids** are `H(id, base, recursive, seen_seq)`, not `H(id, base)`: a directory's
  `base` does not change when its children change, so after a partial recursive delete every later
  delete of that folder would otherwise replay the stored refusal. A replay of the same system call
  keeps the same `seen_seq` unless the system consumed a newer anchor in between, and then running
  it again is correct. Test: `engine::tests::delete_rules`.
- **`Pong.seq` is a strong barrier.** Before answering a `Ping`, `unlatchd` drains inotify,
  reconciles, re-lists every polled directory (beyond the watch budget, or on a network
  filesystem) and publishes every throttled hot-file update. So "every event ≤ seq was sent before
  this" also covers every file-system change that completed before the `Ping` arrived. Tests and
  the fuzzer rely on this. Cost: with the engine's 5 s liveness ping, polled directories are
  re-listed about every 5 s while a client is connected, instead of the adaptive 1–30 s.
- **A directory that can never be expanded** (a second occurrence of the same `(dev, ino)`, from a
  bind mount or a loop) is exposed `lazy: true`, and `ListDir` on it returns an empty listing with
  `lazy` still `true`, not an error (Finder would show an error as a failed folder).
- **`Write` with `target` and `move_to`:** the move happens first (`renameat2(RENAME_NOREPLACE)`;
  an occupied destination gives `Exists` and nothing is written), then the content is replaced at
  the new location. A replay after a crash in between finds the item already at the destination
  and skips the move. A `move_to` into a folder that has vanished writes in place.
- **`Rename` into a vanished destination:** if the item resolves but `new_parent` does not
  (removed or replaced on the VM meanwhile), the reply is `Renamed { applied: false, entry:
  <current> }`, like a base mismatch, never `Error(NotFound)`, which the engine would rightly read
  as "this item is gone" and delete on the Mac.
- **`Mkdir` of a lazy-by-rule directory:** a directory a client creates (or adopts with
  `may_exist`) is expanded at once, even under a lazy name such as `node_modules` or inside an
  expanded lazy directory. fileproviderd never enumerates a folder it created itself, so otherwise
  nothing would make `unlatchd` watch it. A new directory is published exactly once, already
  `lazy: false`; the creating session counts as its lister for `Unwatch` and the idle collapse.
- **Tombstones of a removed subtree:** live sessions get one `Remove` for the root of the subtree.
  Resume additionally replays a tombstone (same seq) for every descendant, because a client that
  was away while an item was moved into that directory still has the item elsewhere.
- **`ServerInfo.warnings`** reflect the index at `Hello`. After a daemon restart the verify walk
  that assigns watches runs after `Welcome` (design review D17), so the first session after a
  restart may not list "N directories are polled" yet; the next `Hello` does.

## Daemon details

- **Socket name:** `\0unlatch/<uid>/p<major>/<hash>`, where `<hash>` covers the canonical root and
  the canonical state directory (tests run several state directories for one root; hashing the
  root alone would make them share one server and one `index.bin`). See
  `crates/unlatchd/src/lifecycle.rs`.
- **Fault token `overflow_after_events:<n>`:** with `UNLATCH_FAULT=overflow_after_events:<n>`, once
  the inotify reader has read `n` events it drops the rest of that read and queues
  `IN_Q_OVERFLOW` instead, which is what the kernel does when its queue is full (once per
  process). Used by the fuzz race `inotify_queue_overflow`.

## Engine host interface

- **Connection closed:** the C ABI reserves `reply_cb(ctx, NULL, 0, -1)` to mean "the engine closed
  this connection" (for example under `UNLATCH_FAULT=die_before_ipc_reply:<kind>`). It is delivered
  at most once, after every reply frame, and never after `unlatch_dispatcher_free` returns. The
  Swift agent invalidates the XPC connection when it sees it. See `crates/unlatch-ffi/include/unlatch.h` and
  `Dispatcher::with_close`.
- **Upload progress:** `Engine::create_with` / `Engine::modify_with` report `(0, size)` once
  staged, then after every 64 KiB chunk, ending with `(size, size)`, and honour a `CancelToken`
  while staging, between chunks and while waiting for the reply. A cancelled upload sends
  `Cancel` (unlatchd discards the staged data) and returns `Err(Cancelled)`.
