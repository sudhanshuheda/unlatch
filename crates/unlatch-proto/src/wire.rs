//! Client engine ⇄ `unlatchd` protocol (over the ssh stdio pipe).
//!
//! # Session
//! 0. Both sides write a raw [`crate::frame::Preamble`]; the client skips shell junk before it.
//!    The highest common proto version is used; none → the client reports "update unlatchd" and the
//!    engine re-bootstraps.
//! 1. client → [`ClientMsg::Hello`].
//! 2. server → [`ServerMsg::Welcome`], sent **immediately from the persisted index** (a background
//!    verification walk emits any differences as ordinary `Events`).
//!    * `WelcomeMode::Resume`: `resume.index == Welcome.index` and `resume.seq` is within the
//!      tombstone GC horizon (≥ 30 days). The server then sends one or more `Events` carrying every
//!      entry with `seq > resume.seq` (from persisted per-entry seqs + tombstones) and continues live.
//!    * `WelcomeMode::Snapshot`: `SnapshotChunk`s (breadth-first; the walker's queue is also fed
//!      with every directory upserted by `Events` during the snapshot that it has not sent yet),
//!      then `SnapshotDone`. Live `Events` may interleave. The client applies everything
//!      **last-writer-wins by `Entry.seq`** (and a tombstone's seq), so order does not matter.
//!      After `SnapshotDone` the client removes replica items that were not seen in the snapshot
//!      and have no newer event (one journal batch), never ids with in-flight local mutations.
//! 3. Steady state: server pushes `Events`; client sends `Request`s (any number in flight).
//!
//! # Lanes and flow control
//! Both directions have an *interactive* lane (control messages, requests, responses, events)
//! and a *bulk* lane (`ReadChunk`, `WriteChunk`, `SnapshotChunk`, `ListingPart` of large dirs).
//! Every frame is ≤ 64 KiB of data. Interactive frames always go first. Bulk bytes are sent only
//! against **credit** granted by the receiver (`Credit { bulk_bytes }`), so no more than one
//! credit window of bulk data is ever queued below the scheduler. Each side starts with an
//! implicit grant of 256 KiB. Receivers grant ≈ `clamp(1.25 × rate × min_rtt, 128 KiB, 1.5 MiB)`.
//!
//! **Credit measure:** server → client, every `ReadChunk`, `SnapshotChunk` and multi-part
//! `ListingPart` costs its **frame body length as sent** (flags byte + payload, i.e. the
//! *compressed* size for LZ4 frames; the 4-byte length prefix excluded); clients must grant back
//! the body length they received, not the decompressed size. Client → server, a `WriteChunk`
//! costs `data.len()` and the server grants back exactly what it consumed (also for chunks of
//! cancelled/unknown uploads). A bulk frame waits for a positive balance and may take it below
//! zero by at most one frame. A single-part `ListingPart` rides the interactive lane (charged the
//! same way, without waiting, so receivers grant back every `ListingPart` they get). After
//! `Welcome` the server grants the client an extra 768 KiB (upload window 1 MiB).
//!
//! # Ordering guarantees (server)
//! * `Events.seq` is strictly increasing across batches. A batch may span several frames
//!   (`batch_end = false` on all but the last). Within a batch, upserts that move items out of a
//!   directory precede a `Remove` of that directory; `Remove(dir)` implies its whole subtree.
//! * `Response`/`ListingPart` may overtake `Events` — clients use `Entry.seq` (LWW).
//!
//! # Mutations
//! Every mutation carries an [`OpId`]. Server algorithm for each op:
//! 1. `op` in the persisted ops table → reply with the stored response, do nothing else.
//! 2. Flush pending inotify events for the affected dirs.
//! 3. Resolve id → (parent dirfd, name) with `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS)` and
//!    require the opened fd's identity to match the index; mismatch → `NotFound` + rescan.
//! 4. Compare `base` against the **live** seq (after the flush), never a cached value.
//! 5. Execute, fsync, store `op → response` (fsync'd), reply.
//!
//! # Upload
//! `Request::Write` is followed by `ClientMsg::WriteChunk { req_id, .. }`s (bulk lane, under
//! credit) whose concatenated `data` is exactly `size` bytes with blake3 `content_hash`;
//! `last = true` on the final chunk (an empty file sends one empty chunk with `last = true`).
//! The server stages into `O_TMPFILE`, verifies size + hash, fsyncs, then publishes. `Cancel`
//! discards the staged data. One reply: `Response::Written` or an `Error`.

use crate::{Entry, IndexId, ItemId, OpId, ProtoError, Version};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    pub index: IndexId,
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        proto: u32,
        /// Absolute path on the VM, or starting with `~/`.
        root: String,
        /// Present when the client has a replica from an earlier session.
        resume: Option<Resume>,
        /// The index the client's replica belongs to (even when it can't resume). A different
        /// server index ⇒ the engine quarantines queued mutations and reimports.
        expect_index: Option<IndexId>,
        /// Suggested lazy names, used only when the server creates a new index for this root.
        default_lazy_names: Vec<String>,
        /// Sanitized client machine name (conflict file names). ≤ 32 bytes, no '/', NUL, controls.
        client_name: String,
    },
    Request {
        req_id: u32,
        req: Request,
    },
    /// Upload data for an in-flight `Request::Write` with the same `req_id`. Bulk lane.
    WriteChunk {
        req_id: u32,
        data: Vec<u8>,
        last: bool,
    },
    /// Abort an in-flight `Read` (no further chunks, no reply) or `Write` (staged data discarded,
    /// reply `Error(Cancelled)`).
    Cancel {
        req_id: u32,
    },
    /// Grant the server this many more bytes of bulk data.
    Credit {
        bulk_bytes: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Ping {
        nonce: u64,
    },
    /// Listing of **one level** of a directory, replied as one or more `Response::ListingPart`.
    /// For a lazy dir this scans + watches exactly that directory; child dirs that are lazy by
    /// name (or beyond the watch budget, or mount points) come back `lazy: true`, others are
    /// scanned recursively as normal (non-lazy) subtrees. A directory that can never be expanded
    /// (second occurrence of the same `(dev, ino)`, e.g. a bind mount) lists as empty and stays lazy.
    ListDir {
        dir: ItemId,
    },
    /// Collapse an expanded lazy dir back to lazy (drop its watches/children index).
    /// Reply: `Response::Entry(dir)` with `lazy: true` (the same upsert is broadcast as an Event).
    /// Collapsed children keep their ids server-side, so a later `ListDir` returns the same ids.
    Unwatch {
        dir: ItemId,
    },
    Stat {
        id: ItemId,
    },
    /// Stream file content (bulk lane). `len: None` = to EOF. If `expect` (a content seq) is set
    /// and differs from the live one, fails with `VersionMismatch` before sending data.
    /// The server fstat's before the first and after the last chunk; if the file changed during
    /// the stream it sends `Error(VersionMismatch)` instead of a `last = true` chunk.
    Read {
        id: ItemId,
        offset: u64,
        len: Option<u64>,
        expect: Option<u64>,
    },
    /// Create or replace a regular file (followed by `WriteChunk`s).
    /// * `target: None` → create `name` in `parent`. If the name is taken: when `may_exist` and the
    ///   existing file's blake3 equals `content_hash` → success with that entry; otherwise
    ///   `Exists` (the engine then picks `name 2.ext`, …).
    /// * `target: Some(id)` → replace content of file `id`. If `base` (content seq) is set and
    ///   differs from the live one: if the current content hash equals `content_hash` → success,
    ///   no conflict; else the upload lands as a conflict copy
    ///   `"<stem> (conflict from <client> <yyyy-mm-dd hh.mm>)<.ext>"` next to it
    ///   (`Written.conflict_copy = Some`) and the target is untouched.
    ///   Replace = `renameat2(RENAME_EXCHANGE)` of the staged file with the target; if the old
    ///   inode changed between check and exchange it is kept as a conflict copy, never unlinked.
    ///   Keeps `st_mode` unless `exec` is set; the item keeps its `ItemId`.
    /// * `move_to: Some((parent, name))` → also move/rename the item in the same op: the move
    ///   happens first (`RENAME_NOREPLACE`; taken → `Exists`, nothing written), then the content
    ///   is replaced at the new location. A replay finds the item already moved and skips the move.
    Write {
        op: OpId,
        parent: ItemId,
        name: String,
        target: Option<ItemId>,
        base: Option<u64>,
        size: u64,
        content_hash: [u8; 32],
        mtime_ns: Option<i64>,
        /// Set/clear the user-exec bit (and group/other exec where read is set). `None` = keep.
        exec: Option<bool>,
        move_to: Option<(ItemId, String)>,
        may_exist: bool,
    },
    /// `may_exist` and an existing dir at that name → success with it; else `Exists`.
    Mkdir {
        op: OpId,
        parent: ItemId,
        name: String,
        may_exist: bool,
    },
    Symlink {
        op: OpId,
        parent: ItemId,
        name: String,
        target: String,
    },
    /// Move/rename with `renameat2(RENAME_NOREPLACE)` (never overwrites; taken → `Exists`).
    /// If the item's live (parent, name) ≠ (`base_parent`, `base_name`) the server does nothing
    /// and replies `Renamed { applied: false, entry: <current> }`.
    Rename {
        op: OpId,
        id: ItemId,
        base_parent: ItemId,
        base_name: String,
        new_parent: ItemId,
        new_name: String,
    },
    /// Delete. `base` is checked for files (content+meta) and dirs (meta). Dirs need
    /// `recursive = true` unless empty. Recursive deletes walk by fd bottom-up, never follow
    /// symlinks, never cross `st_dev`, and **skip** every entry with `seq > seen_seq` (plus its
    /// ancestors), reporting them in `Removed { kept }`. A `base` mismatch → `Error(VersionMismatch)`.
    /// `Removed { kept }` is stored in the ops table like any success, so clients derive the op id
    /// from `(id, base, recursive, seen_seq)`.
    Remove {
        op: OpId,
        id: ItemId,
        base: Version,
        recursive: bool,
        seen_seq: u64,
    },
    SetAttr {
        op: OpId,
        id: ItemId,
        exec: Option<bool>,
        mtime_ns: Option<i64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WelcomeMode {
    Resume,
    Snapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub hostname: String,
    /// Canonical absolute root path on the VM.
    pub root_path: String,
    pub entries: u64,
    pub watches: u64,
    /// `true` when the root is on a network/virtual fs (NFS, CIFS, FUSE, 9p, virtiofs) → polled.
    pub polled: bool,
    /// Non-fatal warnings (watch budget reached → some dirs polled, etc.).
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    /// Item created or changed (any field, including a move).
    Upsert(Entry),
    /// Item (and, for dirs, its whole subtree) no longer exists. `seq` = tombstone seq.
    Remove { id: ItemId, seq: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        proto: u32,
        index: IndexId,
        seq: u64,
        mode: WelcomeMode,
        root: Entry,
        info: ServerInfo,
        /// Effective lazy names for this root (persisted server-side).
        lazy_names: Vec<String>,
    },
    SnapshotChunk {
        entries: Vec<Entry>,
        /// Directories whose complete (one-level) listing is now known to the client.
        complete_dirs: Vec<ItemId>,
    },
    SnapshotDone {
        seq: u64,
    },
    /// Changes committed up to `seq`. A batch may span frames (`batch_end`).
    Events {
        seq: u64,
        changes: Vec<Change>,
        batch_end: bool,
    },
    Response {
        req_id: u32,
        resp: Response,
    },
    ReadChunk {
        req_id: u32,
        offset: u64,
        data: Vec<u8>,
        last: bool,
        /// Content seq of the file as read (identical on every chunk of one Read).
        version: u64,
    },
    /// Grant the client this many more bytes of bulk data (WriteChunk).
    Credit {
        bulk_bytes: u32,
    },
    /// `req_id: None` = session-level error (the server closes the session afterwards).
    Error {
        req_id: Option<u32>,
        err: ProtoError,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// Full barrier: before answering, the server drains inotify, reconciles, and publishes every
    /// throttled hot-file update, so every file-system change that completed before the Ping
    /// arrived has been sent as an Event with seq ≤ `seq`.
    Pong {
        nonce: u64,
        seq: u64,
    },
    /// One part of a `ListDir` reply; `last = true` on the final part.
    ListingPart {
        dir: Entry,
        entries: Vec<Entry>,
        last: bool,
    },
    Entry(Entry),
    /// `conflict_copy: Some` ⇔ the upload went to a conflict copy (then `entry` = the target's
    /// current state, untouched).
    Written {
        entry: Entry,
        conflict_copy: Option<Entry>,
    },
    /// `applied = false` → base mismatch; `entry` is the server's current state.
    Renamed {
        entry: Entry,
        applied: bool,
    },
    /// Non-empty `kept` ⇒ partial delete; those items (newer than `seen_seq`) still exist.
    Removed {
        kept: Vec<ItemId>,
    },
}
