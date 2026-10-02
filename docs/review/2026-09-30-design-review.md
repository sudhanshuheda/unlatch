> Historical record — the project was codenamed Hatch at the time.

# Hatch design review: lead-architect decisions and amendments

Every critic claim that decides an amendment was checked. How each was checked:

- **Apple API availability:** checked against Apple's documentation JSON.
  - `contentPolicy` and `NSFileProviderContentPolicy`: macOS 13+.
  - `excludedFromSync`: macOS 13+.
  - `signalErrorResolved`: macOS 11+.
- **sshdrive quirk catalog:** `alecdwm/sshdrive` exists, and its `docs/quirks/macos.md` says what the critics quote for MQ-001, 004, 005, 006, 009, 013, 014, 016 and 075. I treat it as measured evidence.
- **lz4_flex allocation:** confirmed locally. `decompress_size_prepended` calls `vec![0; prefix]`, and the prefix is not bounded by the frame size.
- **OpenSSH window:** `CHAN_SES_WINDOW_DEFAULT` is 2 MiB.
- **Kernel on this box:** 6.8, so it has coarse (jiffy) timestamps.
- **Network shaping:** unprivileged `unshare -rn` plus `tc netem` works here. RTT measured 40.08 ms, and `tcp_slow_start_after_idle` is 1.

sshdrive is also direct prior art: a File Provider SSH drive whose source is public but unlicensed. It belongs in §1 of DESIGN.md.

Finding ids such as `fp-10` or `perf-6` name the critics' raw findings. Each decision row below
summarises the findings it settles; the raw findings file is not part of this repository.

## 1. Decisions (deduplicated)

| # | Theme (source findings) | Decision | One-line rationale |
|---|---|---|---|
| D1 | Idempotency and replays: fp-1, cons-4 | **ACCEPT-MOD** | Replays are documented Apple behaviour; without an op id we get stuck creates and conflict copies of the user's own saves. Resumable uploads (`UploadStatus`) are deferred to v2. |
| D2 | Mutations checked against a stale index, TOCTOU, lost agent writes: cons-1, sec-3 | **ACCEPT** | This is the most likely silent loss of agent work. |
| D3 | Versions built from `(size, mtime, ino)`, ctime in `meta`, torn reads: fp-7b, perf-3, cons-8 | **ACCEPT** | On 6.8 kernels, 80–97% of same-size rewrites leave the tuple unchanged (measured by the perf critic). |
| D4 | Identity: id space reset, kind change, inode reuse, st_dev, hardlinks, batch order, overflow, root replaced: fp-5, cons-2, cons-6, cons-7, sec-2 | **ACCEPT-MOD** | Rejected part: cons-2's engine-local id remap layer. `index_id` plus `reimportItems` is enough and simpler. |
| D5 | Anchor, journal and working set: fp-2, perf-8, cons-10 | **ACCEPT** | Apple requires both the old and the new parent to be checked against the materialized set. `Dematerialize` has no Apple source. An expired anchor loses changes. |
| D6 | Create/modify reply semantics, including conflicts, still-pending fields and `beforeFirstSync`: fp-3, cons-3 | **ACCEPT** | MQ-013: the system trusts the version in the reply, so the Mac and the VM diverge silently. |
| D7 | Trash and delete semantics, including recursive deletes that kill unseen agent files: fp-4, cons-5 | **ACCEPT** | `supportsSyncingTrash` defaults to YES (MQ-009 and MQ-075 show the loops). `rm -rf` of files the Mac never saw is data loss. |
| D8 | Mac-only metadata, `.DS_Store`, exec bit, capabilities, packages: fp-8, perf-10 | **ACCEPT-MOD** | In v1, packages get `excludedFromSync` with a status warning; tree upload comes in v1.1. |
| D9 | Re-download storm on hot files: fp-7, perf-2 | **ACCEPT-MOD** | Use `.downloadLazilyAndEvictOnRemoteUpdate` (macOS 13) plus a daemon publish throttle. `NSFileProviderIncrementalContentFetching` is deferred to v2. |
| D10 | Head-of-line blocking below the scheduler, unlaned uploads, huge frames, T8 cap: perf-1 | **ACCEPT-MOD** | Add credit flow control and ≤64 KiB frames in both directions. A second ssh process stays in v2. |
| D11 | Engine host lifecycle and IPC transport (menu-bar app, socket, auth, no progress or cancel, 104-byte path, confused deputy): fp-6, perf-5, sec-6 | **ACCEPT-MOD** | Rejected part: the extension reading SQLite directly. Launching the engine on demand fixes availability without a second read path. Partial content fetching is deferred to v2. |
| D12 | VM symlinks escaping on the Mac, executable drops: fp-9, sec-1 | **ACCEPT-MOD** | Use sec-1's inductive target rule. Exec bit hidden by default. Quarantine is tested with the probe only (unverified). |
| D13 | Opening a lazy directory expands its whole subtree: perf-4 | **ACCEPT** | One click on `node_modules` would mean tens of thousands of watches that never go away. |
| D14 | inotify budget, overflow, moves, network filesystems: perf-7, cons-7 | **ACCEPT-MOD** | Rejected part: perf-7's `Interest` request. Budget cap, `Unwatch` and polling cover it. |
| D15 | Snapshot/event interleaving: cons-9 | **ACCEPT-MOD** | Instead of a persistent structural map, the snapshot walker is fed by Events, and every entry carries a per-entry `seq` with last-writer-wins. |
| D16 | Case and normalization collisions: fp-5d, cons-11 | **ACCEPT-MOD** | Display-name mapping in the engine. The alternative, hiding newcomers, is worse for a file explorer. |
| D17 | Daemon restart forces a full snapshot and a cold rescan: perf-3d | **ACCEPT** | Persist seq and tombstones, send Welcome from the persisted index, and verify in the background. |
| D18 | `serve` lifecycle (spawn races, stdio hang, no exit, watch leak): sec-4 | **ACCEPT** | |
| D19 | Version negotiation, shell noise, mixed versions, per-session `lazy_names`, `client_name` read from `/etc/hostname` on the Mac: sec-5 | **ACCEPT** | `hostname_or()` always returns "mac" on macOS, a confirmed bug in `hatch-core`. |
| D20 | Signing, app group, distribution: sec-7 | **ACCEPT-MOD** | Add a `Signing.xcconfig`, CI entitlement assertions and install-tier docs. The Homebrew specifics are out of scope. |
| D21 | ssh first run and launchd environment (BatchMode, agent, PATH, 2FA, Local Network): sec-8 | **ACCEPT-MOD** | Add askpass, ControlMaster, login-shell PATH and a `NeedsUser` state. |
| D22 | Remote install placement, NFS, noexec: sec-9 | **ACCEPT-MOD** | Probe the install directory, upload atomically, poll roots on network filesystems. Container guidance goes into the docs. |
| D23 | LZ4 prefix bypasses `MAX_FRAME`: sec-10 | **ACCEPT** | Verified in lz4_flex 0.11.6 source. |
| D24 | Test loop cannot see the Mac, and the latency proxy cannot see bandwidth: fp-10, perf-6 | **ACCEPT** | netns+netem verified on this box. |
| D25 | Prefetch budget and thumbnails: perf-9 | **ACCEPT-MOD** | v1: prefetch only for viewer requests, plus a global token bucket. `NSFileProviderThumbnailing` in v1.1. |
| — | `Fetch.is_system` flag (perf-2 fix 2) | **REJECT (v1)** | The evict-on-remote-update policy removes most system refetches. |
| — | `evictItem` retry after a conflict (MQ-017) | **Probe-only** | Keep it as a shim fallback, turned on only if `hatch-probe` shows that `should_fetch_content` alone is insufficient. |

## 2. Amendments

### (a) DESIGN.md text changes

1. **§1:** Add a table row for **sshdrive**: File Provider over SFTP, no push daemon, cites the measured macOS quirk catalog. Add the line: "Hatch's differentiator is the push daemon. We rely on sshdrive's measured quirks (MQ-xxx) and cite them in code comments. Check the licence before borrowing any code."
2. **§2 Architecture:** replace "Hatch.app (menu bar, login item) … unix socket" with the following:
   - The engine runs in `Hatch.app/Contents/MacOS/Hatch --agent`, registered with `SMAppService.agent`, with the MachService `$(TeamIdentifierPrefix)$(HATCH_BUNDLE_PREFIX).hatch.engine`. launchd starts it on demand and keeps it alive with KeepAlive.
   - The extension and the menu-bar UI are both XPC clients, and the engine enforces `setCodeSigningRequirement` (same team ID, allow-listed bundle IDs).
   - The XPC payload is the same postcard `IpcFrame` bytes, so Linux tests exercise the identical codec over a unix socket (the test transport only).
   - Content moves as file handles, never as paths chosen by the client.
3. **§3 Identity and versions:** rewrite as follows.
   - **Identity key:**
     - Directories and files with `nlink == 1`: `(st_dev, st_ino, btime)`, where `btime` comes from `statx`.
     - Where `btime` is unavailable, an inode match at restart is accepted only if the path is also unchanged.
     - Regular files with `nlink > 1`: `(st_dev, st_ino, parent, name)`. Every link gets its own id, and an event on the inode dirties all of them.
     - Rule: *when unsure, split (new id), never merge.*
   - **Reuse rule:** a name is reused only if all of the following hold: the old inode was not found anywhere in the batch, the kind is identical, and the old id has not been emitted in a `Remove`. On rename-over, the destination's id survives, deterministically.
   - **`index_id: u128`:**
     - Random when `index.bin` is created and persisted with it.
     - Rebuilt whenever any of these change: `/etc/machine-id`, the root `statfs.f_fsid`, the root `(dev, ino)`, or the index format.
     - Every id and version is scoped to it.
   - **Id allocation:** hi/lo. Persist and fsync `hwm = next + 65536` before handing out any id in the block.
   - **Versions are daemon-assigned sequence numbers, never stat hashes:**
     - `content` = the index seq at the last observed content change. Triggers: `IN_MODIFY`, `IN_CLOSE_WRITE`, `IN_CREATE`, `IN_MOVED_TO`, replace, or any difference in the stat tuple `(size, mtime, ctime, ino, btime)`.
     - `meta` = the seq at the last change of `(parent, name, mode & 0o7777, symlink_target)`. No ctime.
     - At startup, apply git's "racy" rule: bump the version of any entry whose mtime or ctime is within 2 ticks of the last persisted scan.
4. **§4 Sync:**
   - **Handshake:** a raw preamble comes before postcard (see (b)).
   - **Resume:** Resume is keyed by `(index_id, seq)`. The `epoch` goes away. Resume replays from persisted `changed_seq` plus tombstones (GC horizon 30 days), not from an in-memory log. `Snapshot` is sent only when the index id differs or the client's seq is older than the GC horizon.
   - **Snapshot:** the snapshot walker's queue is fed both by the directories it emits and by every directory upserted in `Events` during the snapshot whose listing it has not yet sent.
   - **Lazy directories:** laziness is inherited. `ListDir` scans and watches exactly one level; child directories come back `lazy: true`. `Unwatch` collapses a directory back to lazy.
   - **Priorities:** credit flow control in both directions, every frame ≤ 64 KiB (listings and event batches are split), and uploads go through the bulk lane.
   - **Liveness:** "dead" means nothing received for 10 s **and** a Ping written at least 10 s ago still unanswered.
5. **§5 Engine:**
   - **Change journal:** the replica rows carry `changed_seq`, and a `tombstones(id, old_parent, seq)` table replaces the bounded journal. Both are written in the same SQLite transaction as the rows they describe.
   - **Anchor:** opaque bytes `(replica_uuid: u128, seq: u64)`. `AnchorExpired` is returned only below the tombstone GC horizon.
   - **Working set:** a change is reported iff `id ∈ M || old_parent ∈ M || new_parent ∈ M`. M is persisted and maintained from `materializedItemsDidChange` plus `enumeratorForMaterializedItems()`, not from `Enumerate` calls.
   - **Directory removal:** emits a tombstone for every known descendant, children first.
   - **Signalling:** only after the commit. Signal again after `finishEnumeratingChanges` if a commit landed during the enumeration. Never return an empty page while `seq > anchor`.
   - **Downloads:** "4 × 256 KiB pipelined" becomes one streamed `Read` under credit.
   - **Prefetch:** only on `isFileViewerRequest` enumerations, cancelled when the enumerator is invalidated, with a global token bucket of 64 MiB/min and a 16 MiB burst.
   - **Writes:** add "Conflicts never error: the reply carries `should_fetch_content`" and "Mass-deletion guard" (see (c)).
6. **§6 Targets:**
   - T1, T2 and T6 are labelled *engine API* numbers.
   - T7 becomes "≤ 1 RTT + 5 ms for ≤ 12 KiB; otherwise ≤ (1+⌈log2(size/14.6 KiB)⌉)·RTT + size/bw after idle".
   - The bench uses kernel shaping in a netns, not a ProxyCommand proxy.
   - Add T13–T17 (see (f)).
   - The correctness gate adds "**no VM-side write is ever silently lost**" and "no path outside the root is ever touched".
7. **§7:** Add `hatch-probe` (signed, run on the user's Mac) and the CI entitlement assertions. Replace "builds unsigned" with "builds and asserts that the entitlements match. Tag builds are signed and notarized."
8. **§8 macOS specifics:**
   - **Trash:** `supportsSyncingTrash = false`, never `.allowsTrashing`, and the trash enumerator returns `NSFeatureUnsupportedError`.
   - **Case and normalization:** display-name mapping (D16).
   - **Symlinks:** exposed only when they pass the in-root rule (D12). Anything else becomes a read-only placeholder.
   - **Root content policy:** `contentPolicy = .downloadLazilyAndEvictOnRemoteUpdate` on the root.
   - **Excluded names:** `.DS_Store`, `._*`, `Icon\r`, `.localized` and `*.nosync` are `excludedFromSync`, but only for items that do not exist on the VM.
9. **§9 Security:**
   - Add the threat model: *the VM, and everything running as the VM user, is untrusted input to the Mac.*
   - Daemon path resolution: `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS)`, falling back to a per-component `openat(O_NOFOLLOW)`. Then check that the final fd's identity equals the index identity for that id.
   - Recursive deletes are fd-based, never follow symlinks and never cross `st_dev`.
   - The daemon socket is abstract, with `SO_PEERCRED` checked in both directions.
   - The binary hash is re-checked on every connect, not only before first exec.
   - The XPC code-signing requirement replaces "socket 0600".
10. **§10 Non-goals:** add partial and incremental content fetching, resumable uploads, thumbnails (v1.1) and package upload (v1.1).

### (b) hatch-proto type changes

**`frame.rs`**
- `decode_body`:
  1. Read the 4-byte LE prefix by hand.
  2. Reject it if it is greater than `MAX_FRAME`.
  3. Allocate `vec![0; n]` and decompress with `lz4_flex::block::decompress_into`.
  4. Require the decompressed length to equal `n`.
- Add a preamble. It is raw and never postcard-encoded:

```rust
pub const MAGIC: [u8; 8] = *b"\0HATCH\0\x01";
pub struct Preamble { pub proto_min: u16, pub proto_max: u16, pub build_id: [u8; 16] } // 8+2+2+16 bytes, LE
/// Client: skip ≤ 64 KiB of junk before MAGIC; surface it as "remote shell printed: …".
pub fn read_preamble<R: Read>(r: &mut R) -> Result<(Preamble, Vec<u8> /*junk*/), FrameError>;
```

- Encoding freeze: new fields only as new enum variants or a trailing `ext: Vec<(u16, Vec<u8>)>`. Golden-bytes tests for every v1 message.

**`lib.rs`**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IndexId(pub u128);
pub type OpId = [u8; 16];

/// Daemon-assigned seqs (see DESIGN §3). Not stat-derived.
pub struct Version { pub content: u64, pub meta: u64 }
/// `None` = NSFileProviderItemVersion.beforeFirstSyncComponent / unknown base.
pub struct BaseVersion { pub content: Option<u64>, pub meta: Option<u64> }

pub struct Entry {
    // existing fields…
    pub seq: u64,      // NEW: index seq of the last change to this entry (LWW on the client)
    pub access: u8,    // NEW: ACCESS_R=1|ACCESS_W=2|ACCESS_X=4, via faccessat(AT_EACCESS)
}

pub enum ErrorCode { /* existing */ IndexChanged, DeletionRejected, CannotSync, ExcludedFromSync, NeedsUser, RootReplaced }
```

**`wire.rs`**

```rust
pub struct Resume { pub index: IndexId, pub seq: u64 }          // epoch removed

pub enum ClientMsg {
    Hello { proto: u32, root: String, resume: Option<Resume>,
            expect_index: Option<IndexId>, client_name: String },   // lazy_names removed (server-side per-root config)
    Request { req_id: u32, req: Request },
    WriteChunk { req_id: u32, data: Vec<u8>, last: bool },           // bulk lane, ≤ 64 KiB, under credit
    Cancel { req_id: u32 },                                          // now also aborts a Write (discard O_TMPFILE)
    Credit { bulk_bytes: u32 },                                      // NEW
}

pub enum Request {
    Ping { nonce: u64 },
    ListDir { dir: ItemId },                          // ONE level; child dirs of a lazy dir come back lazy
    Unwatch { dir: ItemId },                          // NEW: collapse to lazy, drop watches
    Stat { id: ItemId },
    Read { id: ItemId, offset: u64, len: Option<u64>, expect: Option<u64> },   // expect = content seq
    Write { op: OpId, parent: ItemId, name: String, target: Option<ItemId>,
            base: Option<u64>, size: u64, content_hash: [u8; 32] /*blake3*/,
            mtime_ns: Option<i64>, exec: Option<bool>,           // replaces absolute `mode`
            move_to: Option<(ItemId, String)>,                   // rename+content as one op
            may_exist: bool },
    Mkdir   { op: OpId, parent: ItemId, name: String, may_exist: bool },
    Symlink { op: OpId, parent: ItemId, name: String, target: String },
    Rename  { op: OpId, id: ItemId, base_parent: ItemId, base_name: String,
              new_parent: ItemId, new_name: String },
    Remove  { op: OpId, id: ItemId, base: Version, recursive: bool, seen_seq: u64 },
    SetAttr { op: OpId, id: ItemId, exec: Option<bool>, mtime_ns: Option<i64> },
}

pub enum ServerMsg {
    Welcome { proto: u32, index: IndexId, seq: u64, mode: WelcomeMode, root: Entry,
              info: ServerInfo, lazy_names: Vec<String> },
    SnapshotChunk { entries: Vec<Entry>, complete_dirs: Vec<ItemId> },
    SnapshotDone { seq: u64 },
    Events { seq: u64, changes: Vec<Change>, batch_end: bool },   // ≤ 64 KiB/frame; one batch may span frames
    Response { req_id: u32, resp: Response },
    ReadChunk { req_id: u32, offset: u64, data: Vec<u8>, last: bool, version: u64 },
    Credit { bulk_bytes: u32 },                                    // NEW
    Error { req_id: Option<u32>, err: ProtoError },
}

pub enum Response {
    Pong { nonce: u64 },
    ListingPart { dir: Entry, entries: Vec<Entry>, last: bool },   // replaces Listing
    Entry(Entry),
    Written { entry: Entry, conflict_copy: Option<Entry> },        // conflict ⇔ Some
    Renamed { entry: Entry, applied: bool },                       // applied=false → server state returned
    Removed { kept: Vec<ItemId> },                                 // non-empty ⇒ engine answers DeletionRejected
}
```

**Ordering doc:**
- Within one batch, Upserts that move items out of a directory precede any `Remove` of that directory.
- `Remove(dir)` implies its subtree.
- `Listing`/`Entry` responses may overtake `Events`, because the client applies last-writer-wins by `Entry.seq`.

**`ipc.rs`**

The IPC becomes multiplexed. Every message is wrapped as `IpcFrame { call: u64, msg }`, and replies may arrive out of order.

```rust
pub struct IpcFrame<T> { pub call: u64, pub msg: T }

pub struct LocalMeta { pub tag_data: Option<Vec<u8>>, pub last_used_ns: Option<i64>,
    pub favorite_rank: Option<u64>, pub creation_ns: Option<i64>,
    pub xattrs: Vec<(String, Vec<u8>)>, pub hidden: bool, pub type_creator: Option<(u32, u32)> }

pub struct IpcItem { pub entry: Entry, pub display_name: String, pub caps: u32 /*NSFileProviderItemCapabilities*/,
    pub local: LocalMeta, pub user_exec: bool, pub symlink_blocked: bool }

pub enum CreateKind { File, Dir, Symlink, Package, Alias }

pub enum IpcRequest {
    Hello { proto: u32, domain: String },
    Item { id: ItemId },
    Enumerate { container: ItemId, cursor: Option<Vec<u8>>, limit: u32, viewer: bool },
    CurrentAnchor,
    ChangesSince { anchor: Vec<u8>, limit: u32 },
    MaterializedChanged { added: Vec<ItemId>, removed: Vec<ItemId>, full: bool }, // replaces Dematerialize
    Fetch { id: ItemId, version: Option<u64>, dest_dir: String /*temporaryDirectoryURL*/ },
    Create { template_id: String, parent: ItemId, name: String, kind: CreateKind,
             has_content: bool /*fd attached out-of-band*/, symlink_target: Option<String>,
             mtime_ns: Option<i64>, user_exec: Option<bool>, changed_fields: u32, local: LocalMeta,
             may_already_exist: bool, deletion_conflicted: bool },
    Modify { id: ItemId, base: BaseVersion, changed_fields: u32, new_parent: Option<ItemId>,
             new_name: Option<String>, has_content: bool, mtime_ns: Option<i64>,
             user_exec: Option<bool>, local: LocalMeta },
    Delete { id: ItemId, base: BaseVersion, recursive: bool },
    Cancel { call: u64 },
    Status,
}

pub enum IpcResponse {
    Hello { proto: u32, domain: String },
    Item(IpcItem),
    Page { items: Vec<IpcItem>, next: Option<Vec<u8>> },
    Anchor(Vec<u8>),
    Changes { updated: Vec<IpcItem>, removed: Vec<ItemId>, anchor: Vec<u8>, more: bool },
    Progress { done: u64, total: u64 },                      // streamed before the final reply
    Fetched { path: String, item: IpcItem },
    Done { item: IpcItem, still_pending: u32, should_fetch_content: bool, conflict_copy: Option<IpcItem> },
    Deleted,
    Status(EngineStatus),
    Error { code: ErrorCode, msg: String, current: Option<IpcItem> },  // current: for DeletionRejected
}
```

- **Content file descriptors:** the fd travels as an XPC `FileHandle` in production and via `SCM_RIGHTS` in the Linux test transport. `content_path: String` is removed.
- **Status fields:** `EngineStatus.anchor` becomes `Vec<u8>`. Add `ConnState::NeedsUser { reason: String, url: Option<String> }` and `ConnState::Paused { reason: String }`, the latter for the mass-deletion guard.

### (c) hatch-core Engine API changes

```rust
pub enum EngineEvent {
    WorkingSetChanged { anchor: Vec<u8> },            // emitted only after SQLite commit
    ReplicaChanged { ids: Vec<ItemId>, parents: Vec<ItemId> },
    StatusChanged(EngineStatus),
    ErrorResolved,                                     // NEW: host calls signalErrorResolved(.serverUnreachable)
    Reimport { below: ItemId },                        // NEW: host calls reimportItems(below:)
    NeedsUser { reason: String, url: Option<String> }, // NEW
}
```

**Method signature changes:**
- `anchor() -> Vec<u8>`
- `changes_since(&[u8], limit)`
- `materialized_changed(added, removed, full)` replaces `dematerialize`
- `list(container, cursor, limit, viewer: bool)`
- `fetch(id, version: Option<u64>, dest_dir: &Path, progress: &dyn Fn(u64, u64), cancel: &CancelToken) -> Fetched`. It uses `clonefile()` from the cache into `dest_dir` (same volume), never a byte copy.
- `create(CreateRequest { template_id, may_already_exist, deletion_conflicted, content: Option<File>, changed_fields, local, … }) -> Modified`
- `modify(id, BaseVersion, ModifyRequest { changed_fields, content: Option<File>, local, user_exec, … }) -> Modified`
- `Modified { item, still_pending: u32, should_fetch_content: bool, conflict_copy: Option<Entry> }`
- `delete(id, BaseVersion, recursive)`

**`EngineConfig` changes:**
- `client_name` is **required**, supplied by the host from `SCDynamicStoreCopyComputerName`. Delete `hostname_or()`.
- `lazy_names` is only a *default for first creation* on the server.
- Add `temp_dir: PathBuf` and `ssh_env: Vec<(String, String)>`.
- Add `interactive_auth: bool`, false for background reconnects.

**Engine rules:**
1. **Op ids:**
   - Create: `op = H(domain, template_id)`.
   - Modify: `op = H(id, base, changed_fields, blake3(content))`.
   - Delete: `op = H(id, base)`.
2. **Create never surfaces `Exists` or `filenameCollision`:**
   - If an item already exists at `(parent, name)` with the same kind, and either `may_already_exist` is set or the blake3 hashes are equal, return that item. Set `should_fetch_content` if the content differs.
   - Otherwise create `name 2.ext` (then 3, …) and return the item with that filename.
3. **Content conflict:** return the original item at the server's version with `should_fetch_content = true`, then emit `WorkingSetChanged` so the conflict copy appears.
4. **Metadata:** metadata never errors. If the rename base `(parent, name)` does not match, return the server's state.
5. **Local-only fields:** tags, lastUsed, favoriteRank, creation date, xattrs, type/creator and hidden are stored in a `local_meta` table keyed by ItemId, merged into every `IpcItem`, and a modify that touches only these fields generates no network traffic. Any field the engine does not handle is returned in `still_pending`.
6. **Delete:**
   - An unknown id returns success.
   - `seen_seq` is the daemon seq covered by the last anchor the system consumed, recorded as anchor→seq in the journal.
   - A non-empty `kept` list or a base mismatch returns `DeletionRejected` together with `current`.
   - A non-recursive delete of a non-empty directory also returns `DeletionRejected`.
   - A reparent to `.trashContainer` is treated as a delete.
7. **Index id:**
   - When `index_id` changes, quarantine queued mutations and emit `Reimport{below: ROOT}`.
   - Creates replayed with `may_already_exist` then match items by path.
   - Never execute an id-addressed op across index ids.
8. **Mass-deletion guard:** triggered when a snapshot diff or an event batch would remove more than 20% of M or more than 1000 materialized items, or when the root is replaced. The engine then:
   - stops applying,
   - keeps items with pending edits and their ancestors,
   - moves to `ConnState::Paused`,
   - and waits for the user to confirm from the menu bar.
9. **Symlink rule:** a symlink is shown as a symlink iff its target is `"../"×k` followed by normal components (no `.`, `..`, empty component or leading `/`), with k ≤ the depth of the link's parent. Absolute targets inside the root are rewritten to relative ones first. Anything else becomes a read-only file (`symlink_blocked`, no write capability) whose content is the target text.
10. **Exec bit:** `user_exec` is false by default. It can be enabled per domain, but never for `.command`, `.tool` or `.terminal` files or for anything under `*.app/Contents/MacOS/`.
11. **Name collisions:** within a directory, names that collide under `casefold(NFD(name))` are resolved like this:
    - the first name in byte order keeps it;
    - the others are shown as `stem (Hatch N).ext`, with a display→real mapping used for every outgoing op;
    - renames of mapped items that only look like the system's own "bounce" renames stay local.
12. **Writer lanes:** the engine writer has interactive and bulk lanes. `WriteChunk`s go under server credit. Grant size: `clamp(1.25 × rate × min_rtt, 128 KiB, 1.5 MiB)`, shrunk when the RTT under load exceeds `min_rtt + 25 ms`.
13. **Replica apply:** entries are applied last-writer-wins by `Entry.seq`. The post-snapshot drop is one journal batch and never drops ids that have in-flight local mutations.

### (d) hatchd behaviour rules

1. **Mutation execution, for every op:**
   1. Look up `op` in the persisted `ops(op_id → encoded Response)` table (TTL 7 days or 100k rows, fsync'd together with the mutation). If found, reply with the stored response.
   2. Otherwise `flush(dirs)`: drain the inotify fd and apply the pending dirty paths for the affected directories.
   3. Resolve the id to (parent dirfd, name) using `openat2`/`O_NOFOLLOW`, then require the fd's identity to equal the index identity. On a mismatch return `NotFound` and rescan the parent.
   4. Compare `base` against the **live** seq, never against a cached version.
2. **Creates:**
   - Content goes into `open(dir, O_TMPFILE)`.
   - Verify the byte count and the blake3 hash, then fsync.
   - Publish with `linkat(…, name)`. `EEXIST` means `Exists`, which is handled per `may_exist`.
   - fsync the parent directory before replying.
   - `Mkdir`, `Symlink` and `Rename` use `renameat2(RENAME_NOREPLACE)`, never check-then-rename.
   - Fallback where `O_TMPFILE` is unsupported: a named `.hatch-<op>` file, which the watcher ignores and startup sweeps.
3. **Replace:**
   1. Hold the target fd open and check it.
   2. `linkat` the temp file to `.hatch-<op>`.
   3. `renameat2(RENAME_EXCHANGE)` the temp file and the target.
   4. fstat the held fd (the old inode, now at the temp name). If its size, mtime or ctime changed since the check, rename it to `<stem> (conflict from <client> <date>)<ext>` instead of unlinking it.
   5. When `exec` is None, keep the old `st_mode`. Try `fchown` and copy `user.*` xattrs.
   6. A `Write` whose blake3 equals the current content returns success with no conflict copy.
4. **Remove:**
   - Check `base.meta` for directories as well as for files.
   - Delete bottom-up with an fd walk that never follows symlinks and never crosses `st_dev` or a mount (both taken from the removed item's *parent*: a removed mount point, a same-filesystem bind mount included, is kept).
   - Skip, and report in `kept`, every entry with `seq > seen_seq` together with its ancestors — and, for a directory that arrived after `seen_seq` (its meta seq is newer: moved, renamed or created), its whole subtree.
   - Compare the base (and, in the walk, each indexed file) with the **live** stat: where it differs from the index (polled directories lag the disk) the item is observed first / counts as newer.
5. **`Read`:** fstat before the first chunk and after the last. If the seq or size changed during the stream, send `Error(VersionMismatch)` instead of `last = true`.
6. **Hot files:** internal seqs are bumped on every event, which keeps base checks correct. Publication of `Events` for a content-changed file is throttled: the first change goes out immediately, then at most one per second while `IN_MODIFY` keeps arriving, then a final one on `IN_CLOSE_WRITE` or after 300 ms of quiet.
7. **inotify:**
   - A dedicated reader thread drains the fd into an unbounded queue.
   - `IN_Q_OVERFLOW` marks every watched directory dirty and runs a reconcile; ids survive it.
   - Watches are added before `readdir`, via `/proc/self/fd/<dirfd>`.
   - An unmatched `MOVED_FROM` becomes a subtree `Remove` plus `rm_watch` after pairing. rename(2) queues `MOVED_FROM` and `MOVED_TO` one after the other, not atomically, so a batch never ends between them: the pairing is not a timeout but a wait for the source directory's inode lock (one `getdents64`), which rename holds until both events are queued; the `MOVED_TO` and the events before it then join the batch. Unmatched after that = moved out of the watched tree (TESTING row 51).
   - Watch budget: `min(config, 50% of the per-uid free watches)`, counted from `/proc/*/fdinfo`. Priority goes to recently listed directories, then depth ≤ 2. Directories beyond the budget are polled at 1–30 s, and a `ServerInfo.warnings` entry reports it.
   - Roots on NFS, CIFS, FUSE, 9p or virtiofs are always polled, and `.nfs*` files are filtered out.
   - `/proc/self/mountinfo` is watched with POLLPRI.
   - Mount points below the root become lazy directories. A second occurrence of the same `(dev, ino)` cannot be expanded.
8. **Root identity:** hold an `O_PATH` fd on the root. `IN_DELETE_SELF`, `IN_MOVE_SELF`, or a `(dev, ino)` mismatch at connect triggers `RootReplaced` plus a new `index_id`.
9. **Persistence:** `index.bin` holds `index_id`, `seq`, the id hwm, per-entry seqs, tombstones, the expanded set, `lazy_names` and the ops table. Welcome is sent immediately from the persisted index. Verification runs in the background with a parallel walker of 16–64 threads and emits differences as `Events`.
10. **Lifecycle:**
    - `serve` holds `flock(serve.lock)` for its whole life, on the abstract socket `\0hatch/<uid>/p<major>/<root-hash>`, and checks `SO_PEERCRED` both ways.
    - `connect` never unlinks anything, and spawns only while holding `spawn.lock` after `LOCK_NB` on `serve.lock` succeeds.
    - Daemonize properly: double fork, `setsid`, stdio to `/dev/null` plus a log file, close every other fd.
    - Exit after 24 h without clients. Collapse expansions nobody has listed for 30 min.
    - Add `hatchd status|stop|gc`.
11. **Install directory and bootstrap:**
    - Probe `$HATCH_HOME`, `$XDG_DATA_HOME/hatch`, `~/.hatch`, `/var/tmp/hatch-$UID`, `/tmp/hatch-$UID` in order. Take the first that is 0700, owned by us, not a symlink, on a local filesystem and passes an exec test.
    - Upload to `.tmp`, fsync, verify sha256 against the checksum bundled in the app, rename into place, and re-verify on every connect.
    - Use `$HOME`, never `getpwuid`.
    - Bootstrap runs in a single `sh -s` session.
12. **`client_name`:** sanitized: no `/`, NUL or control characters, at most 32 bytes. Conflict names are truncated at a UTF-8 boundary so they fit in 255 bytes, and get ` 2`, ` 3`, … on `Exists`.

### (e) macOS shim rules

1. **Engine host:**
   - The engine runs as the `SMAppService.agent` MachService described in (a)2, and both sides enforce `setCodeSigningRequirement`.
   - The shim forwards postcard bytes plus a `FileHandle`.
   - An invalidated connection means "cancel my calls", not "engine gone".
   - Do not put `com.apple.application-identifier` in the agent's entitlements (MQ-066).
2. **At `add(domain)`:** set `supportsSyncingTrash = false` and never set `.allowsTrashing`. `enumerator(for: .trashContainer)` throws `NSCocoaErrorDomain`/`NSFeatureUnsupportedError`, never `noSuchItem`.
3. **Root item:** `contentPolicy = .downloadLazilyAndEvictOnRemoteUpdate`. `contentType` is `.folder` for every VM directory and `.symbolicLink` only for items that are not `symlink_blocked`.
4. **Create:**
   - Pass through `itemTemplate.itemIdentifier` as `template_id`, plus the options.
   - `.package` and `.aliasFile` templates throw `excludedFromSync` in v1, as do `.DS_Store`, `._*`, `Icon\r`, `.localized` and `*.nosync`. Never do this for items the VM already has.
5. **Completion handlers:** pass `still_pending` and `should_fetch_content` through unchanged.
6. **Error mapping:**

   | Engine code | NSFileProvider error |
   |---|---|
   | `DeletionRejected` | `fileProviderErrorForRejectedDeletion(of: current)` |
   | `CannotSync` / `Permission` | `.cannotSynchronize` |
   | `Offline` | `.serverUnreachable` |
   | `IndexChanged` | reimport |

   Unknown-id deletes succeed.
7. **Capabilities** are derived from `IpcItem.caps`. The engine computes them from `access`, and `symlink_blocked` items are read-only.
8. **Host calls:**
   - `signalErrorResolved(.serverUnreachable)` for every domain at agent start, on every transition to `Live`, and on `ErrorResolved`; then `signalEnumerator(.workingSet)`.
   - `reimportItems(below:)` on `Reimport`.
   - The host forwards `materializedItemsDidChange` to the engine as `MaterializedChanged` by walking `enumeratorForMaterializedItems()`.
9. **Info.plist and pipelines:**
   - Explicit `NSExtensionFileProviderDownloadPipelineDepth` of 16 and `UploadPipelineDepth` of 4.
   - `NSLocalNetworkUsageDescription`. For RFC1918, link-local and `.local` hosts, open and cancel an `NWConnection` before spawning ssh.
10. **ssh launch:**
    - Resolve the login-shell environment once per launch with `$SHELL -l -i -c 'env -0'` (5 s timeout) and use its PATH.
    - Interactive connect: `SSH_ASKPASS` pointing at `hatch-askpass`, `SSH_ASKPASS_REQUIRE=force`, no BatchMode.
    - Background reconnects: BatchMode, `ControlMaster=auto`, `ControlPath=~/.ssh/hatch-%C`, `ControlPersist=10m`.
    - Classify exit 255 from stderr. Host-key, auth and "visit URL" failures become `NeedsUser` with no retry loop.
11. **Signing:**
    - `Signing.xcconfig` holds `HATCH_TEAM_ID` and `HATCH_BUNDLE_PREFIX`. The app group is `$(TeamIdentifierPrefix)$(HATCH_BUNDLE_PREFIX).hatch`, read at runtime from an Info.plist `HatchAppGroup` key.
    - The README states that ad-hoc builds cannot run the Finder integration.

### (f) Verification tests and targets

1. **Codec:**
   - LZ4 prefix `0xFFFFFFFF` → `Decode` error, with peak RSS < 10 MB.
   - A cargo-fuzz or proptest target on `decode_body` for all four message enums.
   - Golden-bytes tests.
   - The preamble is found after 10 KiB of junk.
2. **fpsim models measured fileproviderd behaviour.** Each rule gets a failing-first scenario:
   - a folder is enumerated once (MQ-001);
   - an empty change set at a held anchor drops the change (MQ-004);
   - failing enumerations back off until `ErrorResolved` (MQ-005);
   - expiry resumes from a fresh anchor with no rescan (MQ-006);
   - the returned version is believed (MQ-013);
   - a create collision retries forever (MQ-014);
   - case collisions are renamed locally with no call (MQ-016);
   - `noSuchItem` on the trash container loops (MQ-009);
   - writes are retried forever (MQ-035);
   - replays keep the template id;
   - a directory stays until its children are deleted;
   - an evict-on-update item goes dataless when its version changes.

   fpsim's namespace is case- and normalization-insensitive.
3. **Crash and replay matrix:** kill after the daemon commit but before the reply, at both the IPC hop and the wire hop, for create, modify, rename, delete and mkdir. Pass condition: no duplicates, no conflict copies, no stuck items.
4. **Fuzz additions:**
   - A Mac write racing an agent write, where the agent's bytes must survive.
   - Rename-over with an already-observed temp file.
   - `mv a b; touch a` in one batch.
   - `rm f; mkdir f`.
   - Injected `IN_Q_OVERFLOW`.
   - A directory moved during a snapshot.
   - `rm -rf` in Finder while the agent writes into that directory.
   - An intermediate directory swapped for a symlink out of the root.
   - Hardlinks and a bind mount inside the root.

   Assertions: no VM-side write is lost; nothing outside the root changes; realpath of every materialized replica entry stays inside the domain.
5. **Identity:**
   - Deleting `index.bin` or re-imaging the VM → `IndexChanged`, a reimport, and zero id-addressed ops executed.
   - kill -9 of `serve` plus 8 concurrent connects → exactly one serve.
   - ssh exits within 1 s of the client disconnecting.
6. **Network bench** in `unshare -rn` with `tc netem`. The profiles are {RTT 0, 40, 100 ms} × {20, 50, 200 Mbit/s} × {operation after idle: yes, no}. A veth pair gives asymmetric links (100 Mbit/s down, 10 Mbit/s up). The ProxyCommand proxy is dropped.
7. **New targets:**

   | # | Metric | Target |
   |---|---|---|
   | T13 | p99 Stat/ListDir/Pong latency during a 2 GiB download plus a 500 MiB upload, at 20 and 50 Mbit/s | ≤ RTT + 30 ms |
   | T14 | Bytes moved for a materialized 10 MB log growing 1 KB every 10 ms | ≤ 2× the appended bytes (with evict-on-update: ~0) |
   | T15 | Single `ListDir` of a 500k-file `node_modules` fixture | ≤ 1 level of entries sent; watches added ≤ 1 + child dirs of that level |
   | T16 | Daemon restart with no changes | Welcome in `Resume` mode ≤ 200 ms; 0 snapshot bytes |
   | T17 | Same-size rewrite within one tick (1000 trials) | 100% produce a new content version |

   T8 is re-measured with a single streamed `Read` under credit.
8. **`hatch-probe`:** a signed CLI the user runs on the Mac. It writes the same JSON scorecard with these checks:
   - VM write → visible under `~/Library/CloudStorage/…` (FSEvents);
   - first `ls` of a never-listed 1k-entry directory;
   - cold and prefetched `cat`;
   - rename and save round-trips;
   - a conflict, checked with `fileproviderctl evaluate` that the local content equals the VM content;
   - Move to Trash, which must show Finder's "deleted immediately" alert;
   - quarantine xattr honoured? (speculative);
   - whether `evictItem` is needed after a conflict (MQ-017).

   The probe records the macOS version, and the documented support floor is what the probe has actually run on.
9. **CI:**
   - Always: `codesign --verify --strict`, plus a check that the app, appex and agent entitlements share the same team-prefixed group and that `NSExtensionFileProviderDocumentGroup` matches it.
   - On tags: sign with Developer ID, notarize, staple, and run `spctl -a -vv`.