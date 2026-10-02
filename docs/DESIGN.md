# Unlatch — design

> Your cloud VM in Finder, as snappy as a local folder.

Unlatch shows a remote Linux machine's files in the macOS Finder sidebar. Browsing is instant because
**Finder never waits on the network**: a replica of the whole metadata tree lives on the Mac, and a
small daemon on the VM *pushes* every change the moment it happens. File contents download on first
open (and small files are prefetched), then are served locally.

This is the same trick that makes Cursor / VS Code Remote-SSH feel instant (a server on the VM, one
persistent connection, a remote file watcher) — surfaced through Apple's File Provider framework so
*every* Mac app gets it: Finder, Quick Look, drag into Slack, ⌘C/⌘V, Preview, Spotlight.

## 1. Why existing tools feel slow

| Tool | Model | Consequence |
|---|---|---|
| sshfs / macFUSE | Every VFS op → SFTP round-trip; attr cache with TTL | `ls` costs ≥1 RTT per dir; stale for TTL; Finder's xattr/.DS_Store probing multiplies round-trips |
| Mountain Duck / ExpanDrive | SFTP + local cache; polls open folders ~every 60 s | agent edits invisible for up to a minute |
| SSHMount | File Provider, fetch-on-open, no cache, no push | every open waits on the network |
| Cursor / VS Code Remote | server on VM + push watcher | instant — but only inside the editor |

SFTP has no change notifications, so any SFTP-only design must poll. Unlatch runs its own daemon.

## 2. Architecture

```
 Finder / any app
      │  (kernel + fileproviderd)
      ▼
 UnlatchFileProvider.appex  ── Swift shim, no logic ──┐
      │ libunlatch (Rust) IPC client                  │  macOS
      ▼  unix socket in app-group container         │
 Unlatch.app (menu bar, login item)                   │
      │ libunlatch ENGINE (Rust): replica, cache,     │
      │ sync, conflict policy, IPC server           │
      ▼  `ssh -T host unlatchd connect …` (stdio)     ┘
 ─────────────────────────── network ───────────────────────────
 unlatchd connect  ⇄ unix socket ⇄  unlatchd serve (per root, long-lived)
                                   index + inotify watcher + event log
                                                         Linux VM
```

* **All logic is Rust and runs on Linux in tests.** The Swift layer only marshals File Provider
  calls to `libunlatch` and calls `signalEnumerator` when the engine says so.
* **The engine lives in the host app, not the extension.** fileproviderd kills idle extensions; the
  connection (and push stream) must outlive them. The extension is a stateless IPC client.
* **System `ssh` is the transport.** Inherits `~/.ssh/config`, ssh-agent/1Password agent, ProxyJump,
  Tailscale, known_hosts, certificates. No credential storage in Unlatch.
* **`unlatchd serve` outlives the ssh session.** `unlatchd connect` spawns/attaches to a per-root
  background server (like VS Code server / mutagen agent). While the laptop sleeps, the server keeps
  watching and keeps an event log, so reconnect = replay a few events, not a rescan.

## 3. Identity and versions

* `ItemId: u64`, assigned by `unlatchd`, **stable across renames and across atomic replace**
  (editor/agent "write temp + rename over"):
  * inode already known → same id (it moved / changed);
  * new inode appearing at a `(parent, name)` whose previous inode is gone → **reuse** that id
    (replaced in place);
  * otherwise → new id.
* `ItemId(1)` is always the configured root. Ids are persisted with the daemon index
  (`~/.unlatch/state/<root-hash>/index.bin`) so they survive daemon restarts / VM reboots.
* `Version { content: u64, meta: u64 }` — hashes of `(size, mtime_ns, ino)` and
  `(mode, parent, name, ctime_ns, symlink target)`. Maps 1:1 onto `NSFileProviderItemVersion`.

## 4. Sync protocol (summary — see PROTOCOL.md)

1. Client → `Hello { proto, root, resume: Option<(epoch, seq)> }`.
2. Daemon → `Welcome { epoch, seq, root_id, mode: Resume | Snapshot }`.
   * `Resume`: daemon replays `Events` after `seq` from its in-memory log (bounded, default 1M events).
   * `Snapshot`: daemon streams `SnapshotChunk`s **breadth-first**, each carrying complete directory
     listings, then `SnapshotDone`. Client diffs against its replica (no content transfer).
     Directories become browsable as soon as their listing lands; a Finder request for a
     not-yet-synced directory issues a priority `ListDir` that jumps the queue.
3. Steady state: daemon pushes `Events { seq, changes: Vec<Change> }` where
   `Change = Upsert(Entry) | Remove(ItemId)`. inotify events are **coalesced** adaptively: a
   batch closes after 2 ms without new events (a lone change goes out at once), and while a
   burst continues it keeps growing in 8 ms steps, up to 50 ms; then dirty paths are re-stat'ed
   once.
4. Requests (multiplexed by `req_id`): `ListDir`, `Expand`, `Read` (streamed chunks), `Write`
   (streamed), `Mkdir`, `Symlink`, `Rename`, `Remove`, `SetAttr`, `Ping`.

**Lazy directories.** Heavy directories (`node_modules`, `.git`, `target`, `.venv`, `__pycache__`,
`.next`, `dist`, `build`, `.cache`, configurable) are listed as items but not scanned or watched
until first opened (`Expand`). Keeps initial sync and inotify watches small.

**Priorities.** The daemon's writer has two queues — *interactive* (replies to ListDir/Read of small
files/metadata ops/events) and *bulk* (snapshot, large file chunks). Bulk frames are ≤ 64 KiB and
interactive frames always go first, so a 2 GB download never delays a Finder listing.

**Compression.** Per-frame LZ4 (pure Rust `lz4_flex`) when the payload is ≥ 512 B and compresses
≥ 10%. ssh compression is disabled (`-o Compression=no`); LZ4 is ~10× cheaper than zlib.

## 5. Client engine

* **Replica**: in-memory `HashMap<ItemId, Entry>` + children index, persisted to SQLite (WAL,
  batched) in the app-group container. After a Mac reboot Finder shows the last-known tree
  immediately, before the network is up.
* **File Provider anchor**: engine-local monotonic `anchor: u64`, bumped per applied batch, with a
  bounded change journal (`anchor → changed ids / removed ids`). `changes_since(anchor)` beyond
  the journal → `AnchorExpired` → the system re-enumerates.
* **Working set** = items inside containers the system has enumerated ("materialized containers").
  Changes elsewhere are applied to the replica but not reported; the system sees them when it
  enumerates that container. Keeps fileproviderd's database small for 1M-file trees.
* **Content cache**: `<cache>/<id>-<content-version>`; LRU by byte budget (default 5 GiB).
  Entries being read are pinned from the moment they enter the cache, so a file larger than
  the whole budget still opens; the cache exceeds the budget by such a file only until the next
  insert evicts it (once nothing reads it).
  `fetch` returns a path from the cache or downloads (pipelined, 4 × 256 KiB in flight).
* **Prefetch**: when a container is enumerated, files ≤ 256 KiB in it are prefetched in the
  background (budget 8 MiB/container, 64 MiB global in flight) so double-click / Quick Look is
  local. Remote changes to prefetched files re-prefetch.
* **Writes**: `create`/`modify` upload with `base_version`. Daemon writes a temp file in the same
  directory and renames over (atomic). If `base_version` ≠ current: **never overwrite** — the
  upload lands as `name (conflict from <mac-name> <date>).ext` and both versions are kept.
* **Connection supervisor**: app-level `Ping` every 5 s, dead after 10 s without traffic; reconnect
  with backoff 0.25 s → 8 s (±20% jitter) and immediately on network-change / wake signals.

## 6. Performance targets (checked by `scripts/verify.sh`)

Measured against the same synthetic repo tree over ssh via an injected-latency proxy (RTT 0, 40,
100 ms). "Local" = the same operation on the local disk (the native target).

| # | Metric | Target |
|---|---|---|
| T1 | warm `list_dir` (1 000 entries), engine API | p50 ≤ 0.5 ms, **independent of RTT** |
| T2 | warm `stat`, engine API | p50 ≤ 20 µs |
| T3 | FUSE `ls -la` of 1 000-entry dir (Linux frontend) | ≤ 2× local disk; ≥ 10× faster than sshfs at RTT 40 ms |
| T4 | remote change → visible in engine | p50 ≤ RTT/2 + 15 ms |
| T5 | burst: 1 000 files written in 1 s on VM | all visible ≤ 300 ms + RTT after burst ends |
| T6 | open small file (≤ 256 KiB), after container enumerated | p50 ≤ 1 ms (prefetched) |
| T7 | open small file, cold | ≤ 1 RTT + 5 ms |
| T8 | large file (256 MiB) throughput | ≥ 80% of `ssh host cat file` |
| T9 | upload small file | ≤ 1 RTT + 5 ms until durable on VM |
| T10 | reconnect after connection kill, changes during outage | caught up ≤ 1.5 s at RTT 40 ms |
| T11 | initial sync 100k entries (+30k in lazy dirs) | ≤ 3 s at RTT 40 ms |
| T12 | daemon RSS | ≤ 250 B/entry |

Correctness gate (must be 100%): randomized concurrent-op fuzz (VM-side and client-side ops
interleaved, including atomic replace, rename cycles, rm -rf, churn) converges to identical trees
with identical content hashes for every seed; no client write is ever silently lost.

## 7. Verification loop

`scripts/verify.sh` = fmt + clippy + unit tests → **fpsim** fuzz (a simulator of fileproviderd that
drives the *exact* IPC API the Swift shim calls) → FUSE fuzz → bench (unlatch vs sshfs vs local at
each RTT) → `target/bench/latest.json` + `SCORECARD.md`, non-zero exit on any failed gate or
regression > 10% vs `bench/baseline.json`. CI runs a short version; macOS CI builds the universal
`libunlatch.a` and the Xcode project unsigned.

## 8. macOS specifics

* `NSFileProviderReplicatedExtension` (macOS 13+). One `NSFileProviderDomain` per VM root.
* Item identifier string = decimal `ItemId`; root = `.rootContainer` ↔ `ItemId(1)`; trash is not
  supported (deletes are real deletes on the VM; Finder asks for confirmation).
* Case: Linux is case-sensitive, APFS usually isn't. The engine reports both `Foo` and `foo`;
  fileproviderd resolves the local collision (it bounces one name). Documented limitation.
* Non-UTF-8 names, sockets, FIFOs and device files are not exposed (logged once).
* Symlinks are exposed as symlink items (`symlinkTargetPath`); never followed out of the root.
* Distribution: notarized DMG (Developer ID). The extension is sandboxed; the host app is not (it
  must exec `/usr/bin/ssh` and read `~/.ssh`).

## 9. Security

* No listening TCP ports anywhere. `unlatchd serve` listens on the abstract unix socket
  `\0unlatch/<uid>/p<proto>/<hash>` (review §2(d)10; no file, never stale). The abstract namespace
  has no permissions, so the hash covers a random nonce kept in the 0700 state dir
  (`sock.nonce`, 0600) and a server whose name is taken re-rolls it; the peer uid is checked
  with SO_PEERCRED in both directions (a foreign-uid listener is treated as no server).
* unlatchd never indexes or watches its own files, matched by `(dev, ino)`: its state dir, its log
  file, and its install dir when that directory is unlatchd's own — `$UNLATCH_HOME` (default
  `~/.unlatch`; the bootstrap always exports the dir it probed), or, without it, the binary's
  folder when it is one of the bootstrap's probed dirs. A binary run from a folder of the
  user's (`~/bin`, `~/.local/bin`, `~/.cargo/bin`, `<root>/bin`) hides only itself (and the
  default `state/` container beside it when the state dir is in it), never the user's other
  files there. With the default roots — `~` in the Mac's "Add VM", the current directory for
  `npx unlatch share` — these are inside the served root.
* `unlatchd stop` signals only the verified `unlatchd serve` that owns the state dir's serve.lock
  (/proc/locks owner, executable, argv, open fd), never the pid in serve.pid alone.
* Every client op names `ItemId`s; the daemon resolves them through its index, validates names
  (no `/`, `.`/`..`, NUL), and opens with `O_NOFOLLOW` under a root-relative walk — no escapes via
  symlinks or `..`.
* The daemon binary is uploaded over ssh and its SHA-256 verified before first exec.
* Engine socket is 0600 inside the app-group container.

## 10. Non-goals (v1)

Windows client, multi-user roots, running on non-Linux VMs, password auth UI (use keys/agent;
`unlatch setup-key` helper prints the one-liner), rsync-style delta uploads (v2), multiple ssh channels
for high-BDP links (v2).
