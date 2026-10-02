//! File operations requested by clients (review §2(d)1–5). Every mutation:
//! 1. answers a replayed `op` from the persisted ops table;
//! 2. flushes pending inotify events (the index is live);
//! 3. resolves ids through `openat2(RESOLVE_BENEATH|NO_SYMLINKS|NO_MAGICLINKS)` and requires the
//!    opened object's identity to equal the index identity (mismatch → NotFound + rescan);
//! 4. compares `base` with the live seq;
//! 5. executes with no-replace primitives, fsyncs, reconciles its own effect through the same
//!    path as any other change, stores `op → response` durably, and replies.

use crate::core::{io_err, perr, CommitOpts, Core, State};
use crate::fault;
use crate::index::{NKind, Slot};
use crate::persist::OpRec;
use crate::reconcile::{self, Batch};
use crate::sys::{self, Stat};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::mpsc::Receiver;
use std::sync::MutexGuard;
use unlatch_proto::wire::Response;
use unlatch_proto::{valid_name, Entry, ErrorCode, ItemId, Kind, OpId, ProtoError, Version};

pub type R<T> = Result<T, ProtoError>;

/// One upload chunk routed from the session reader to the write worker.
pub struct Chunk {
    pub data: Vec<u8>,
    pub last: bool,
}

pub struct WriteReq {
    pub op: OpId,
    pub parent: ItemId,
    pub name: String,
    pub target: Option<ItemId>,
    pub base: Option<u64>,
    pub size: u64,
    pub content_hash: [u8; 32],
    pub mtime_ns: Option<i64>,
    pub exec: Option<bool>,
    pub move_to: Option<(ItemId, String)>,
    pub may_exist: bool,
}

/// An item resolved to (identity-checked parent dir fd, name).
pub struct Resolved {
    pub slot: Slot,
    pub parent: Slot,
    pub pfd: OwnedFd,
    pub name: String,
    pub st: Stat,
}

/// The bytes a Write published, described by facts fixed **before** they were published: the
/// staged inode's stat taken before the link/exchange (or, written in place, the target's
/// identity), plus the request's size and blake3 hash. An agent writing between the publish and
/// unlatchd's own observation lands on this inode; whatever the observation and the later fstat
/// record is compared with these facts, never with a stat taken after the publish (which could
/// already include the agent's bytes).
struct Ours {
    ino: u64,
    size: u64,
    /// The mtime our bytes were published with (`None`: written in place without a requested
    /// mtime — set by our own writes, not known before them).
    mtime_ns: Option<i64>,
    /// ctime of the staged inode before the publish (see [`Ours::stat_proves_writes`]).
    ctime_before: Option<i64>,
    hash: [u8; 32],
    /// mtime shown in a reply that describes our bytes.
    show_mtime: i64,
}

/// Files up to this size are always re-hashed after the publish; larger ones only when their
/// stat cannot prove that no write landed (see [`Ours::stat_proves_writes`]).
const REHASH_ALWAYS_MAX: u64 = 64 << 20;

/// Whether a Write may take the exclusive lease on its staged file (tests turn it off to cover
/// the re-hash fallback).
fn lease_allowed() -> bool {
    #[cfg(test)]
    if tests::NO_LEASE.with(|c| c.get()) {
        return false;
    }
    true
}

fn rehash_always_max() -> u64 {
    #[cfg(test)]
    if let Some(v) = tests::REHASH_MAX.with(|c| c.get()) {
        return v;
    }
    REHASH_ALWAYS_MAX
}

impl Ours {
    /// Staged inode `pre` (stat taken before the publish) for request `req`.
    fn staged(pre: &Stat, req: &WriteReq) -> Ours {
        Ours {
            ino: pre.ino,
            size: req.size,
            mtime_ns: Some(pre.mtime_ns),
            ctime_before: Some(pre.ctime_ns),
            hash: req.content_hash,
            show_mtime: pre.mtime_ns,
        }
    }

    /// Any write after the publish moves the mtime away from ours: the filesystem stamps it
    /// with its clock, which (stepping forward) is at least the staged inode's ctime before the
    /// publish, already newer than our mtime. Equal coarse ticks make this false for a fresh
    /// upload (mtime == ctime); then only the content hash can tell.
    fn stat_proves_writes(&self) -> bool {
        matches!((self.mtime_ns, self.ctime_before), (Some(m), Some(c)) if m < c)
    }
}

/// Test hook points, in the order a Write passes them after publishing our bytes.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    /// Right after the link / exchange / in-place write + fsync.
    Published,
    /// Before unlatchd observes its own change (inotify flush or explicit observation).
    BeforeObserve,
    /// After the observation, before the post-publish hash and fstat.
    BeforeFstat,
    /// After the post-publish fstat, before the reply is decided.
    AfterFstat,
    /// In-place write only: after the base check, before our bytes are written.
    BeforeInPlace,
}

#[cfg(test)]
pub(crate) const POINTS: [Point; 4] = [
    Point::Published,
    Point::BeforeObserve,
    Point::BeforeFstat,
    Point::AfterFstat,
];

fn op_hex(op: &OpId) -> String {
    op.iter().map(|b| format!("{b:02x}")).collect()
}

/// Staging name for an op (ignored by the watcher, swept at startup).
pub fn staging_name(op: &OpId) -> String {
    format!(".unlatch-{}", op_hex(op))
}

/// Sanitize a client machine name (§2(d)12): no '/', NUL or control characters, ≤ 32 bytes.
pub fn sanitize_client(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c == '/' || c == '\0' || c.is_control() {
            continue;
        }
        if out.len() + c.len_utf8() > 32 {
            break;
        }
        out.push(c);
    }
    let t = out.trim().to_string();
    if t.is_empty() {
        "mac".to_string()
    } else {
        t
    }
}

fn local_stamp() -> String {
    // A timespec carries the platform's time_t without naming it (musl deprecates the alias).
    // SAFETY: timespec and tm are POD.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    ts.tv_sec = sys::now_secs() as _;
    // SAFETY: as above.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers.
    let ok = unsafe { !libc::localtime_r(&ts.tv_sec, &mut tm).is_null() };
    if !ok {
        return "0000-00-00 00.00".to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}.{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

/// `"<stem> (conflict from <client> <yyyy-mm-dd hh.mm>)<.ext>"`, with ` N` appended on
/// collisions, always ≤ 255 bytes (stem truncated at a UTF-8 boundary).
pub fn conflict_name(name: &str, client: &str, stamp: &str, n: u32) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let ext = truncate_utf8(ext, 32);
    let suffix = if n > 1 {
        format!(" {n}")
    } else {
        String::new()
    };
    let tag = format!(" (conflict from {client} {stamp}){suffix}");
    let room = 255usize.saturating_sub(tag.len() + ext.len());
    format!("{}{}{}", truncate_utf8(stem, room), tag, ext)
}

fn apply_exec(perm: u32, exec: bool) -> u32 {
    if exec {
        let mut m = perm | 0o100;
        if perm & 0o040 != 0 {
            m |= 0o010;
        }
        if perm & 0o004 != 0 {
            m |= 0o001;
        }
        m
    } else {
        perm & !0o111
    }
}

fn hash_fd(fd: RawFd) -> std::io::Result<[u8; 32]> {
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut off = 0u64;
    loop {
        let n = sys::pread(fd, &mut buf, off)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        off += n as u64;
    }
    Ok(*h.finalize().as_bytes())
}

pub struct Ops<'a> {
    pub core: &'a Core,
    pub client: String,
    pub session: u64,
}

impl<'a> Ops<'a> {
    fn lock(&self) -> R<MutexGuard<'a, State>> {
        self.core.plock()
    }

    fn stored(&self, st: &State, op: &OpId) -> Option<Response> {
        let ix = st.ix.as_ref()?;
        let rec = ix.ops.get(op)?;
        postcard::from_bytes(&rec.resp).ok()
    }

    /// Store `op → resp` durably (fsync'd with the mutation's journal records), then honour
    /// `die_after_commit:<name>`.
    fn finish(&self, st: &mut State, op: &OpId, resp: &Response, name: &str) -> R<()> {
        let bytes = postcard::to_stdvec(resp).map_err(|e| perr(ErrorCode::Io, e.to_string()))?;
        let rec = OpRec {
            op: *op,
            resp: bytes,
            time: sys::now_secs(),
        };
        self.core
            .commit(
                st,
                Default::default(),
                CommitOpts {
                    durable: true,
                    op: Some(rec),
                    ..Default::default()
                },
            )
            .map_err(|e| io_err(&e, "journal"))?;
        fault::after_commit(name);
        Ok(())
    }

    fn root_fd(st: &State) -> RawFd {
        st.root_fd.as_raw_fd()
    }

    /// Open a directory by id (identity-checked).
    pub fn resolve_dir(&self, st: &mut State, id: ItemId) -> R<(Slot, OwnedFd)> {
        match self.resolve_dir_once(st, id, false) {
            // The path the index had failed (and its parent was re-listed): if the id still
            // exists it moved — resolve it where it is now. NotFound means the item is gone.
            Err(e) if e.code == ErrorCode::NotFound => {
                self.core.flush(st, false);
                self.resolve_dir_once(st, id, true)
            }
            r => r,
        }
    }

    /// `last`: a path that still runs into a symlink or off the root (ELOOP/EXDEV) after the
    /// re-list is a race with the VM, not proof the directory is gone (retryable `Io`);
    /// before it, it is the stale-path NotFound that `resolve_dir` retries.
    fn resolve_dir_once(&self, st: &mut State, id: ItemId, last: bool) -> R<(Slot, OwnedFd)> {
        let root_fd = Self::root_fd(st);
        let ix = st
            .ix
            .as_ref()
            .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
        let s = ix
            .idx
            .slot_of(id.0)
            .ok_or_else(|| perr(ErrorCode::NotFound, "no such directory"))?;
        if ix.idx.node(s).kind() != NKind::Dir {
            return Err(perr(ErrorCode::NotDir, "not a directory"));
        }
        let rel = ix
            .idx
            .rel_path(s)
            .ok_or_else(|| perr(ErrorCode::NotFound, "detached"))?;
        let fd = match sys::open_beneath(root_fd, &rel, sys::DIR_FLAGS) {
            Ok(fd) => fd,
            Err(e) => {
                let p = ix.idx.node(s).parent;
                self.core.rescan_dir(st, p);
                let moved = matches!(e.raw_os_error(), Some(libc::ELOOP | libc::EXDEV));
                return Err(if moved && !last {
                    perr(ErrorCode::NotFound, format!("open directory: {e}"))
                } else {
                    io_err(&e, "open directory")
                });
            }
        };
        if !reconcile::fd_matches(&ix.idx, s, fd.as_raw_fd()) {
            let p = ix.idx.node(s).parent;
            self.core.rescan_dir(st, p);
            return Err(perr(ErrorCode::NotFound, "directory changed on the VM"));
        }
        Ok((s, fd))
    }

    /// Resolve an item to its identity-checked parent fd + name.
    pub fn resolve(&self, st: &mut State, id: ItemId) -> R<Resolved> {
        match self.resolve_once(st, id) {
            // As in resolve_dir: a stale path is re-listed; retry where the id is now.
            Err(e) if e.code == ErrorCode::NotFound => {
                self.core.flush(st, false);
                self.resolve_once(st, id)
            }
            r => r,
        }
    }

    fn resolve_once(&self, st: &mut State, id: ItemId) -> R<Resolved> {
        let (s, p, name) = {
            let ix = st
                .ix
                .as_ref()
                .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
            let s = ix
                .idx
                .slot_of(id.0)
                .ok_or_else(|| perr(ErrorCode::NotFound, "no such item"))?;
            if s == ix.idx.root {
                return Err(perr(
                    ErrorCode::Permission,
                    "operation not allowed on the root",
                ));
            }
            let n = ix.idx.node(s);
            (s, n.parent, ix.idx.name(s).to_string())
        };
        let pid = ItemId(st.ix.as_ref().map(|ix| ix.idx.node(p).id).unwrap_or(0));
        let (_, pfd) = self.resolve_dir(st, pid)?;
        let ist = match sys::statat(pfd.as_raw_fd(), name.as_bytes()) {
            Ok(x) => x,
            Err(e) => {
                self.core.rescan_dir(st, p);
                return Err(io_err(&e, "stat"));
            }
        };
        let same = st
            .ix
            .as_ref()
            .map(|ix| ix.idx.same_identity(s, &ist))
            .unwrap_or(false);
        if !same {
            self.core.rescan_dir(st, p);
            return Err(perr(ErrorCode::NotFound, "item changed on the VM"));
        }
        Ok(Resolved {
            slot: s,
            parent: p,
            pfd,
            name,
            st: ist,
        })
    }

    fn entry_at(&self, st: &State, dir: Slot, name: &str) -> R<Entry> {
        let ix = st
            .ix
            .as_ref()
            .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
        let s = ix
            .idx
            .lookup(dir, name)
            .ok_or_else(|| perr(ErrorCode::Io, "item vanished right after the operation"))?;
        Ok(ix.idx.entry(s))
    }

    fn entry_of(&self, st: &State, s: Slot) -> R<Entry> {
        let ix = st
            .ix
            .as_ref()
            .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
        if !ix.idx.alive(s) {
            return Err(perr(ErrorCode::NotFound, "item vanished"));
        }
        Ok(ix.idx.entry(s))
    }

    /// Our own change is reconciled like any other (inotify flush, or an explicit observation
    /// in polled mode). Afterwards the name must hold the inode we created.
    fn observe_own(&self, st: &mut State, dir: Slot, name: &str) {
        self.observe_own_ex(st, dir, name, false)
    }

    /// [`Ops::observe_own`]; `expand_new`: a directory created at `name` is expanded at once
    /// even if it is lazy by rule (see [`Batch::expand_new`]).
    fn observe_own_ex(&self, st: &mut State, dir: Slot, name: &str, expand_new: bool) {
        self.core.flush(st, false);
        let root_fd = Self::root_fd(st);
        let polled = st.polled_root;
        let Some(ix) = st.ix.as_mut() else { return };
        // Polled dirs (and a lagging watch) get an explicit observation.
        let mut env = crate::core::CoreEnv::new(root_fd, &self.core.watcher, polled);
        let mut batch = Batch::default();
        if expand_new {
            batch.expand_new = Some((dir, name.to_string()));
        }
        batch.observe(&mut ix.idx, &mut env, dir, name, 0);
        batch.settle(&mut ix.idx, &mut env);
        let txn = std::mem::take(&mut batch.txn);
        let _ = self.core.commit(
            st,
            txn,
            CommitOpts {
                no_throttle: true,
                ..Default::default()
            },
        );
    }

    /// The reply entry for the bytes we published (`ours`), called after unlatchd observed its
    /// own change into slot `s`. `ro` is a read-only handle on our inode: with `leased`, the
    /// staged file's only descriptor, holding the read lease taken once its bytes were written
    /// and synced (see [`Ops::write`]); otherwise a handle for the re-hash.
    ///
    /// Our bytes are what the index recorded only if the observation, a post-publish fstat
    /// and the proof that nobody else wrote to the inode all agree with `ours`. The proof is
    /// the lease, still whole (no open of the inode for writing, and no truncate, was even
    /// attempted since the lease was taken — readers neither break nor wait for it — so no
    /// other write can have landed; a lease being broken counts as a race; an
    /// `O_RDONLY | O_TRUNC` open, the one content change that gets past a read lease, always
    /// changes the size, which the stat checks see), or without a lease a re-hash of our inode
    /// (unless the stat alone proves it). The lease check is O(1); the re-hash reads the whole
    /// file while the core lock is held — every request of every session waits for it — so it
    /// is only the fallback (no lease on this filesystem, a named staging file, a write in
    /// place).
    ///
    /// Any difference means another writer may have touched the inode between our publish and
    /// these checks, and the index's version may name their bytes: we take a fresh seq `a`
    /// for our bytes and move the item to a newer seq `b > a`. The reply carries `a` (size and
    /// mtime of ours), so the client's `content != item version` check fetches `b`, and a
    /// later save based on `a` is a base mismatch (conflict copy) — never a version of someone
    /// else's bytes handed to the Mac. A write after these checks is an ordinary later change:
    /// its event (or the live-stat check before the next base compare) moves the item on.
    /// Under the lease such a write cannot start before the Write lets go of the inode (a
    /// reader is not held back: the agent that reads the file the moment it changes — an
    /// editor's auto-reload, an LSP, an indexer — is no race and gets no conflict copy).
    fn reply_for_ours(
        &self,
        st: &mut State,
        s: Slot,
        ours: &Ours,
        ro: Option<&OwnedFd>,
        leased: bool,
    ) -> R<Entry> {
        #[cfg(test)]
        tests::hook(Point::BeforeFstat);
        let alive = st.ix.as_ref().is_some_and(|ix| ix.idx.alive(s));
        let mut raced = !alive;
        if alive {
            let n = st.ix.as_ref().map(|ix| ix.idx.node(s).clone());
            let n = n.ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
            raced |= n.ino != ours.ino
                || n.size != ours.size
                || ours.mtime_ns.is_some_and(|m| m != n.mtime_ns);
            // Re-read our inode: an agent's bytes of the same size within the same timestamp
            // tick leave the stat as it was.
            let rehash = ours.size <= rehash_always_max() || !ours.stat_proves_writes();
            // Without a readable handle (written in place without read permission) the stat
            // comparison above is all there is.
            if let Some(fd) = ro {
                if leased {
                    // Before the fstat below: from here on any open of the inode for writing
                    // waits for the lease, which is held until the reply is decided.
                    raced |= !sys::shared_lease_intact(fd.as_raw_fd());
                } else if !raced && rehash {
                    #[cfg(test)]
                    tests::REHASHED.with(|c| c.set(c.get() + ours.size));
                    raced |= hash_fd(fd.as_raw_fd()).map_or(true, |h| h != ours.hash);
                }
                // After the hash: a write while hashing shows up here (ctime included, the
                // publish itself changed it before the observation).
                match sys::fstat(fd.as_raw_fd()) {
                    Ok(f) => {
                        raced |= f.ino != n.ino
                            || f.size != n.size
                            || f.mtime_ns != n.mtime_ns
                            || f.ctime_ns != n.ctime_ns
                    }
                    Err(_) => raced = true,
                }
            }
        }
        #[cfg(test)]
        tests::hook(Point::AfterFstat);
        let Some(ix) = st.ix.as_mut() else {
            return Err(perr(ErrorCode::Offline, "no index"));
        };
        if !ix.idx.alive(s) {
            return Err(perr(ErrorCode::NotFound, "item vanished"));
        }
        if !raced {
            return Ok(ix.idx.entry(s));
        }
        let mut t = crate::index::Txn::default();
        let a = ix.idx.bump();
        let b = ix.idx.bump();
        let nm = ix.idx.node_mut(s);
        nm.content_seq = b;
        nm.seq = b;
        t.touch(s, crate::index::CH_CONTENT | crate::index::CH_OTHER);
        let mut entry = ix.idx.entry(s);
        entry.version.content = a;
        entry.seq = a;
        entry.size = ours.size;
        entry.mtime_ns = ours.show_mtime;
        let _ = self.core.commit(
            st,
            t,
            CommitOpts {
                no_throttle: true,
                ..Default::default()
            },
        );
        Ok(entry)
    }

    /// Base checks must compare against the live file (§2(d)1.4). In a polled directory (or
    /// with a watch event not read yet) the index can lag the disk: when the live stat of the
    /// item disagrees with what the index recorded, observe it now so its seqs are current.
    fn refresh_if_stale(&self, st: &mut State, s: Slot, parent: Slot, name: &str, live: &Stat) {
        let stale = st
            .ix
            .as_ref()
            .is_some_and(|ix| ix.idx.alive(s) && stat_differs(ix.idx.node(s), live));
        if stale {
            self.observe_own(st, parent, name);
        }
    }

    // ---- Write -----------------------------------------------------------------------------

    /// Stage the upload (O_TMPFILE in the destination dir), verify size + blake3, fsync, then
    /// publish (create) or exchange (replace).
    /// `chunks` is drained by the caller afterwards (credit for anything left over).
    pub fn write(
        &self,
        req: WriteReq,
        chunks: &Receiver<Chunk>,
        on_consumed: &dyn Fn(usize),
    ) -> R<Response> {
        if !valid_name(&req.name) {
            return Err(perr(ErrorCode::InvalidName, "invalid name"));
        }
        if let Some((_, n)) = &req.move_to {
            if !valid_name(n) {
                return Err(perr(ErrorCode::InvalidName, "invalid name"));
            }
        }
        // 1. replay + staging location
        let (stage_dir, named_tmp) = {
            let mut st = self.lock()?;
            if let Some(r) = self.stored(&st, &req.op) {
                return Ok(r);
            }
            // Resolve against the live tree (§2(d)1): with a stale index a directory swapped
            // for a symlink (or moved) meanwhile fails the beneath-walk, and NotFound tells the
            // engine the *item* is gone (fuzz seed 152: the Mac's edit became a new "x 2").
            self.core.flush(&mut st, false);
            let dfd = match req.target {
                Some(t) => self.resolve(&mut st, t)?.pfd,
                None => self.resolve_dir(&mut st, req.parent)?.1,
            };
            (dfd, None::<String>)
        };
        let (tmp, named_tmp) = match sys::open_tmpfile(stage_dir.as_raw_fd(), 0o600) {
            Ok(f) => (f, named_tmp),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EOPNOTSUPP) | Some(libc::EISDIR) | Some(libc::EINVAL)
                ) =>
            {
                // Filesystem without O_TMPFILE: a named staging file the watcher ignores.
                let n = format!("{}.tmp", staging_name(&req.op));
                let _ = sys::unlinkat(stage_dir.as_raw_fd(), n.as_bytes(), false);
                let f = sys::openat(
                    stage_dir.as_raw_fd(),
                    n.as_bytes(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                    0o600,
                )
                .map_err(|e| io_err(&e, "staging file"))?;
                (f, Some(n))
            }
            Err(e) => return Err(io_err(&e, "staging file")),
        };
        // An unnamed O_TMPFILE on a local filesystem gets a read lease once its bytes are
        // written and synced (below). Only on a local filesystem: a lease holds back this
        // kernel's openers, not another machine's (NFS, SMB, FUSE, …).
        let local =
            sys::fstatfs(stage_dir.as_raw_fd()).is_ok_and(|f| !sys::is_network_fs(f.f_type));
        let leasable = named_tmp.is_none() && local && lease_allowed();
        // A named staging file (no O_TMPFILE): a read-only handle on the staged inode for the
        // post-publish re-hash. It re-reads what the name holds without a write-close event
        // (the Write must not report a change of its own after replying).
        let ro = if leasable {
            None
        } else {
            sys::reopen_ro(tmp.as_raw_fd()).ok()
        };
        let cleanup = |named: &Option<String>| {
            if let Some(n) = named {
                let _ = sys::unlinkat(stage_dir.as_raw_fd(), n.as_bytes(), false);
            }
        };
        // 2. receive
        let mut hasher = blake3::Hasher::new();
        let mut written = 0u64;
        loop {
            let Ok(c) = chunks.recv() else {
                cleanup(&named_tmp);
                return Err(perr(ErrorCode::Cancelled, "upload cancelled"));
            };
            let len = c.data.len();
            if written + len as u64 > req.size {
                cleanup(&named_tmp);
                on_consumed(len);
                return Err(perr(
                    ErrorCode::Protocol,
                    "more data than the declared size",
                ));
            }
            if let Err(e) = sys::pwrite_all(tmp.as_raw_fd(), &c.data, written) {
                cleanup(&named_tmp);
                on_consumed(len);
                return Err(io_err(&e, "write"));
            }
            hasher.update(&c.data);
            written += len as u64;
            on_consumed(len);
            if c.last {
                break;
            }
        }
        if written != req.size {
            cleanup(&named_tmp);
            return Err(perr(
                ErrorCode::Protocol,
                format!("size mismatch: got {written}, declared {}", req.size),
            ));
        }
        if *hasher.finalize().as_bytes() != req.content_hash {
            cleanup(&named_tmp);
            return Err(perr(ErrorCode::Io, "content hash mismatch"));
        }
        // A new file's mode and mtime depend only on the request: set them before the data
        // sync so that one fsync makes content, mode and mtime durable before the name exists.
        // (A replace takes them from the target at exchange time, under the lock.)
        if req.target.is_none() {
            Self::prepare_new_mode(tmp.as_raw_fd(), req.exec, req.mtime_ns)?;
        }
        sys::fsync(tmp.as_raw_fd()).map_err(|e| io_err(&e, "fsync"))?;
        let (tmp, ro, leased) = if leasable {
            Self::lease_staged(tmp)
        } else {
            (tmp, ro, false)
        };
        // 3. commit
        let mut st = self.lock()?;
        if let Some(r) = self.stored(&st, &req.op) {
            cleanup(&named_tmp);
            return Ok(r);
        }
        self.core.flush(&mut st, false);
        let res = match req.target {
            None => self.create_file(&mut st, &req, tmp, ro, leased, named_tmp.as_deref()),
            Some(t) => self.replace_file(&mut st, &req, t, tmp, ro, leased, named_tmp.as_deref()),
        };
        if res.is_err() {
            cleanup(&named_tmp);
        }
        let resp = res?;
        self.finish(&mut st, &req.op, &resp, "write")?;
        Ok(resp)
    }

    /// Trade the staged O_TMPFILE's writable descriptor (its only one: unnamed, nobody else
    /// can open it) for a read-only one holding a read lease (`F_SETLEASE F_RDLCK`, granted
    /// only while the inode is open for writing nowhere). From now until that descriptor is
    /// closed — after the reply is decided — an open of the inode for writing, or a truncate,
    /// by anyone breaks the lease and waits for us, so no other write can land on our bytes
    /// and the post-publish checks need no re-hash under the core lock; a reader (an editor's
    /// auto-reload, an LSP, an indexer) neither breaks it nor waits. The rest of the Write
    /// needs no writable descriptor: mode, owner, xattrs and mtime are set through the
    /// read-only one (as the owner), the publish links it by `/proc/self/fd/N`, and none of
    /// these break the lease. Closing the writable descriptor here raises its write-close event
    /// under the O_TMPFILE's own name ("#<ino>", which no listing holds), before any publish.
    ///
    /// Returns the staged descriptor to publish, a read-only handle for the re-hash when no
    /// lease could be taken (`None` with the lease: the staged descriptor is the proof), and
    /// whether it is leased. Without a read-only reopen the writable descriptor is kept (stat
    /// checks only, as for a file we cannot read).
    fn lease_staged(tmp: OwnedFd) -> (OwnedFd, Option<OwnedFd>, bool) {
        let Ok(ro) = sys::reopen_ro(tmp.as_raw_fd()) else {
            return (tmp, None, false);
        };
        drop(tmp);
        if sys::lease_shared(ro.as_raw_fd()).is_ok() {
            return (ro, None, true);
        }
        // No leases here: the re-hash reads through a second handle (the first is closed
        // before observing, to keep the create path's order of events).
        let again = ro.try_clone().ok();
        (ro, again, false)
    }

    fn prepare_new_mode(tmp: RawFd, exec: Option<bool>, mtime: Option<i64>) -> R<()> {
        let base = 0o666 & !sys::umask();
        let mode = apply_exec(base, exec.unwrap_or(false));
        sys::fchmod(tmp, mode).map_err(|e| io_err(&e, "chmod"))?;
        if let Some(m) = mtime {
            sys::set_mtime_fd(tmp, m).map_err(|e| io_err(&e, "set mtime"))?;
        }
        Ok(())
    }

    /// Give the staged content a name; `EEXIST` never overwrites.
    fn publish_tmp(pfd: RawFd, tmp: RawFd, named: Option<&str>, name: &str) -> std::io::Result<()> {
        match named {
            None => sys::link_fd(tmp, pfd, name.as_bytes()),
            Some(n) => sys::renameat2(
                pfd,
                n.as_bytes(),
                pfd,
                name.as_bytes(),
                sys::RENAME_NOREPLACE,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_file(
        &self,
        st: &mut State,
        req: &WriteReq,
        tmp: OwnedFd,
        ro: Option<OwnedFd>,
        leased: bool,
        named: Option<&str>,
    ) -> R<Response> {
        let (pslot, pfd) = self.resolve_dir(st, req.parent)?;
        // Mode and mtime were set before the data sync (`write`). Our bytes are described
        // before they get a name (anyone can write to them afterwards).
        let pre = sys::fstat(tmp.as_raw_fd()).map_err(|e| io_err(&e, "fstat"))?;
        let ours = Ours::staged(&pre, req);
        match Self::publish_tmp(pfd.as_raw_fd(), tmp.as_raw_fd(), named, &req.name) {
            Ok(()) => {
                #[cfg(test)]
                tests::hook(Point::Published);
            }
            Err(e) if sys::is_errno(&e, libc::EEXIST) => {
                if req.may_exist {
                    // Same content already there (a replayed create after a lost reply)?
                    if let Ok(Some((est, _))) = reconcile::stat_child(pfd.as_raw_fd(), &req.name) {
                        if est.is_file() {
                            let f = sys::openat(
                                pfd.as_raw_fd(),
                                req.name.as_bytes(),
                                libc::O_RDONLY | libc::O_NOFOLLOW,
                                0,
                            )
                            .map_err(|e| io_err(&e, "open existing"))?;
                            if hash_fd(f.as_raw_fd()).map_err(|e| io_err(&e, "read existing"))?
                                == req.content_hash
                            {
                                drop(f);
                                self.observe_own(st, pslot, &req.name);
                                let entry = self.entry_at(st, pslot, &req.name)?;
                                return Ok(Response::Written {
                                    entry,
                                    conflict_copy: None,
                                });
                            }
                        }
                    }
                }
                return Err(perr(
                    ErrorCode::Exists,
                    format!("{} already exists", req.name),
                ));
            }
            Err(e) => return Err(io_err(&e, "create")),
        }
        // Close before observing: a write-close event must land in this flush. A leased
        // O_TMPFILE stays open (and leased) until the reply is decided: it is read-only (its
        // writer closed before the publish), so closing it raises no watched event.
        let tmp = if leased {
            Some(tmp)
        } else {
            drop(tmp);
            None
        };
        sys::fsync(pfd.as_raw_fd()).map_err(|e| io_err(&e, "fsync dir"))?;
        drop(pfd);
        #[cfg(test)]
        tests::hook(Point::BeforeObserve);
        self.observe_own(st, pslot, &req.name);
        let entry = self.entry_at(st, pslot, &req.name)?;
        let s = st
            .ix
            .as_ref()
            .and_then(|ix| ix.idx.slot_of(entry.id.0))
            .ok_or_else(|| perr(ErrorCode::Io, "item vanished right after the operation"))?;
        let entry = self.reply_for_ours(st, s, &ours, tmp.as_ref().or(ro.as_ref()), leased)?;
        drop(tmp);
        Ok(Response::Written {
            entry,
            conflict_copy: None,
        })
    }

    /// Link the staged content under a free conflict name next to `name`.
    fn conflict_copy(
        &self,
        st: &mut State,
        pslot: Slot,
        pfd: RawFd,
        tmp: OwnedFd,
        named: Option<&str>,
        name: &str,
    ) -> R<Entry> {
        let stamp = local_stamp();
        for n in 1..1000u32 {
            let cn = conflict_name(name, &self.client, &stamp, n);
            match Self::publish_tmp(pfd, tmp.as_raw_fd(), named, &cn) {
                Ok(()) => {
                    let _ = sys::fsync(pfd);
                    // Close before observing: the close event must land in this flush, not
                    // bump the copy's version after we replied.
                    drop(tmp);
                    self.observe_own(st, pslot, &cn);
                    return self.entry_at(st, pslot, &cn);
                }
                Err(e) if sys::is_errno(&e, libc::EEXIST) => continue,
                Err(e) => return Err(io_err(&e, "conflict copy")),
            }
        }
        Err(perr(ErrorCode::Exists, "no free conflict name"))
    }

    #[allow(clippy::too_many_arguments)]
    fn replace_file(
        &self,
        st: &mut State,
        req: &WriteReq,
        target: ItemId,
        tmp: OwnedFd,
        ro: Option<OwnedFd>,
        leased: bool,
        named: Option<&str>,
    ) -> R<Response> {
        // Optional move first (rename + content as one op). Never overwrites.
        if let Some((np, nn)) = &req.move_to {
            let r = self.resolve(st, target)?;
            let (cur_parent_id, cur_name) = {
                let ix = st
                    .ix
                    .as_ref()
                    .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
                (ItemId(ix.idx.node(r.parent).id), r.name.clone())
            };
            let dest = if cur_parent_id != *np || cur_name != *nn {
                match self.resolve_dir(st, *np) {
                    Ok(x) => Some(x),
                    // The destination folder is gone: write in place, report where it is
                    // (metadata never errors; NotFound would mean the item itself is gone).
                    Err(e) if matches!(e.code, ErrorCode::NotFound | ErrorCode::NotDir) => None,
                    Err(e) => return Err(e),
                }
            } else {
                None
            };
            if let Some((npslot, npfd)) = dest {
                match sys::renameat2(
                    r.pfd.as_raw_fd(),
                    r.name.as_bytes(),
                    npfd.as_raw_fd(),
                    nn.as_bytes(),
                    sys::RENAME_NOREPLACE,
                ) {
                    Ok(()) => {
                        let _ = sys::fsync(npfd.as_raw_fd());
                        let _ = sys::fsync(r.pfd.as_raw_fd());
                        self.observe_own(st, r.parent, &r.name);
                        self.observe_own(st, npslot, nn);
                    }
                    // Onto another filesystem (a mount point): write in place, report where
                    // it is — as for a vanished destination.
                    Err(e) if sys::is_errno(&e, libc::EXDEV) => {}
                    Err(e) => return Err(io_err(&e, "rename")),
                }
            }
        }
        let r = self.resolve(st, target)?;
        let kind = st
            .ix
            .as_ref()
            .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?
            .idx
            .node(r.slot)
            .kind();
        if kind != NKind::File {
            return Err(perr(
                if kind == NKind::Dir {
                    ErrorCode::IsDir
                } else {
                    ErrorCode::Unsupported
                },
                "not a regular file",
            ));
        }
        let pfd = r.pfd.as_raw_fd();
        // Hold the target open: its identity is checked, and after the exchange we can tell
        // whether someone wrote to the old inode in between (§2(d)3).
        let (held, held_readable) = match sys::openat(
            pfd,
            r.name.as_bytes(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        ) {
            Ok(f) => (f, true),
            Err(_) => (
                sys::openat(pfd, r.name.as_bytes(), libc::O_PATH | libc::O_NOFOLLOW, 0)
                    .map_err(|e| io_err(&e, "open target"))?,
                false,
            ),
        };
        let check = sys::fstat(held.as_raw_fd()).map_err(|e| io_err(&e, "fstat"))?;
        if check.dev != r.st.dev || check.ino != r.st.ino {
            self.core.rescan_dir(st, r.parent);
            return Err(perr(ErrorCode::NotFound, "item changed on the VM"));
        }
        // The base is compared with the live content (polled dirs lag the disk).
        self.refresh_if_stale(st, r.slot, r.parent, &r.name, &check);
        let content_seq = st
            .ix
            .as_ref()
            .filter(|ix| ix.idx.alive(r.slot))
            .map(|ix| ix.idx.node(r.slot).content_seq)
            .ok_or_else(|| perr(ErrorCode::Io, "item changed on the VM; retry"))?;
        if let Some(b) = req.base {
            if b != content_seq {
                // Someone else changed it: same bytes → success; else conflict copy, target
                // untouched (D2, §2(d)3.6).
                if let Ok(h) = hash_fd(held.as_raw_fd()) {
                    if h == req.content_hash {
                        if let Some(n) = named {
                            let _ = sys::unlinkat(pfd, n.as_bytes(), false);
                        }
                        let entry = self.entry_of(st, r.slot)?;
                        return Ok(Response::Written {
                            entry,
                            conflict_copy: None,
                        });
                    }
                }
                Self::prepare_new_mode(
                    tmp.as_raw_fd(),
                    req.exec.or(Some(check.perm() & 0o100 != 0)),
                    req.mtime_ns,
                )?;
                let copy = self.conflict_copy(st, r.parent, pfd, tmp, named, &r.name)?;
                let entry = self.entry_of(st, r.slot)?;
                return Ok(Response::Written {
                    entry,
                    conflict_copy: Some(copy),
                });
            }
        }
        // Keep st_mode (unless exec is set), owner and user.* xattrs (§2(d)3.5).
        let perm = match req.exec {
            None => check.perm(),
            Some(x) => apply_exec(check.perm(), x),
        };
        let t = tmp.as_raw_fd();
        sys::fchmod(t, perm).map_err(|e| io_err(&e, "chmod"))?;
        let _ = sys::fchown(t, check.uid, check.gid);
        sys::copy_user_xattrs(held.as_raw_fd(), t);
        if let Some(m) = req.mtime_ns {
            sys::set_mtime_fd(t, m).map_err(|e| io_err(&e, "set mtime"))?;
        }
        if check.nlink > 1 {
            // Hard-linked target: an exchange would split the links. Write in place under the
            // same identity check instead.
            #[cfg(test)]
            tests::hook(Point::BeforeInPlace);
            let w = sys::openat(pfd, r.name.as_bytes(), libc::O_WRONLY | libc::O_NOFOLLOW, 0)
                .map_err(|e| io_err(&e, "open for write"))?;
            let wst = sys::fstat(w.as_raw_fd()).map_err(|e| io_err(&e, "fstat"))?;
            if wst.ino != check.ino || wst.dev != check.dev {
                return Err(perr(ErrorCode::NotFound, "item changed on the VM"));
            }
            if wst.size != check.size
                || wst.mtime_ns != check.mtime_ns
                || wst.ctime_ns != check.ctime_ns
            {
                // Written since the base check: keep both, as for a base mismatch.
                drop(w);
                self.observe_own(st, r.parent, &r.name);
                let copy = self.conflict_copy(st, r.parent, pfd, tmp, named, &r.name)?;
                let entry = self.entry_of(st, r.slot)?;
                return Ok(Response::Written {
                    entry,
                    conflict_copy: Some(copy),
                });
            }
            // Our bytes in place: the target's identity, the request's size and hash, and
            // the requested mtime (else our own writes set it).
            let mut ours = Ours {
                ino: check.ino,
                size: req.size,
                mtime_ns: req.mtime_ns,
                ctime_before: None,
                hash: req.content_hash,
                show_mtime: req.mtime_ns.unwrap_or(0),
            };
            let mut buf = vec![0u8; 256 * 1024];
            let mut off = 0u64;
            loop {
                let n = sys::pread(t, &mut buf, off).map_err(|e| io_err(&e, "read staged"))?;
                if n == 0 {
                    break;
                }
                sys::pwrite_all(w.as_raw_fd(), &buf[..n], off).map_err(|e| io_err(&e, "write"))?;
                off += n as u64;
            }
            sys::ftruncate(w.as_raw_fd(), off).map_err(|e| io_err(&e, "truncate"))?;
            if let Some(m) = req.mtime_ns {
                // Not the owner (a shared, group-writable file): our writes set the mtime.
                if sys::set_mtime_fd(w.as_raw_fd(), m).is_err() {
                    ours.mtime_ns = None;
                }
            }
            if let Some(x) = req.exec {
                let _ = sys::fchmod(w.as_raw_fd(), apply_exec(check.perm(), x));
            }
            sys::fsync(w.as_raw_fd()).map_err(|e| io_err(&e, "fsync"))?;
            #[cfg(test)]
            tests::hook(Point::Published);
            if ours.mtime_ns.is_none() {
                // Shown in a reply that describes our bytes only; never compared.
                ours.show_mtime = sys::fstat(w.as_raw_fd()).map_or(0, |s| s.mtime_ns);
            }
            drop(w);
            drop(tmp);
            if let Some(n) = named {
                let _ = sys::unlinkat(pfd, n.as_bytes(), false);
            }
            #[cfg(test)]
            tests::hook(Point::BeforeObserve);
            self.observe_own(st, r.parent, &r.name);
            self.bump_content(st, r.slot);
            // `held` is the target inode (unless it could only be opened O_PATH).
            let entry =
                self.reply_for_ours(st, r.slot, &ours, held_readable.then_some(&held), false)?;
            drop(held);
            return Ok(Response::Written {
                entry,
                conflict_copy: None,
            });
        }
        // Our bytes, described before anyone else can reach them.
        let pre = sys::fstat(t).map_err(|e| io_err(&e, "fstat"))?;
        let ours = Ours::staged(&pre, req);
        // linkat the staged file to .unlatch-<op>, then RENAME_EXCHANGE it with the target.
        let stage = staging_name(&req.op);
        match named {
            None => {
                if let Err(e) = sys::link_fd(t, pfd, stage.as_bytes()) {
                    if sys::is_errno(&e, libc::EEXIST) {
                        let _ = sys::unlinkat(pfd, stage.as_bytes(), false);
                        sys::link_fd(t, pfd, stage.as_bytes()).map_err(|e| io_err(&e, "stage"))?;
                    } else {
                        return Err(io_err(&e, "stage"));
                    }
                }
            }
            Some(n) => {
                sys::renameat2(pfd, n.as_bytes(), pfd, stage.as_bytes(), 0)
                    .map_err(|e| io_err(&e, "stage"))?;
            }
        }
        if let Err(e) = sys::renameat2(
            pfd,
            stage.as_bytes(),
            pfd,
            r.name.as_bytes(),
            sys::RENAME_EXCHANGE,
        ) {
            let _ = sys::unlinkat(pfd, stage.as_bytes(), false);
            return Err(io_err(&e, "exchange"));
        }
        #[cfg(test)]
        tests::hook(Point::Published);
        // Close before observing: a write-close event must land in the observation below. A
        // leased O_TMPFILE stays open (and leased) until the reply is decided: it is read-only
        // (its writer closed before the publish), so closing it raises no watched event.
        let tmp = if leased {
            Some(tmp)
        } else {
            drop(tmp);
            None
        };
        // The old inode now sits at `stage`. If it changed since the check, keep it as a
        // conflict copy — never unlink someone else's bytes.
        let after = sys::fstat(held.as_raw_fd()).map_err(|e| io_err(&e, "fstat"))?;
        if after.size != check.size || after.mtime_ns != check.mtime_ns {
            drop(held);
            if !keep_as_conflict(pfd, &stage, &r.name, &self.client) {
                crate::log!(
                    "could not keep the concurrently modified old version; left at {stage}"
                );
            }
        } else if sys::lease_exclusive(held.as_raw_fd()).is_ok() {
            // Nobody else has the old inode open, so no write can land on it any more. One may
            // still have landed (and its writer closed) between `after` and the lease: check
            // again under the lease, ctime included (every write moves it).
            let unchanged = sys::fstat(held.as_raw_fd()).is_ok_and(|p| same_bytes(&p, &after));
            drop(held);
            if unchanged {
                let _ = sys::unlinkat(pfd, stage.as_bytes(), false);
            } else if !keep_as_conflict(pfd, &stage, &r.name, &self.client) {
                crate::log!(
                    "could not keep the concurrently modified old version; left at {stage}"
                );
            }
        } else {
            // Someone still holds it open (an agent mid-write, an editor, a reader) and may
            // write through that handle after this check: park it under its staging name until
            // it is closed (then unlinked) or written (then kept as a conflict copy).
            match r.pfd.try_clone() {
                Ok(dir) => park(
                    st,
                    Parked {
                        dir,
                        stage: stage.clone(),
                        name: r.name.clone(),
                        client: self.client.clone(),
                        held,
                        check: after,
                        since: std::time::Instant::now(),
                    },
                ),
                Err(_) => {
                    drop(held);
                    let _ = keep_as_conflict(pfd, &stage, &r.name, &self.client);
                }
            }
        }
        sys::fsync(pfd).map_err(|e| io_err(&e, "fsync dir"))?;
        #[cfg(test)]
        tests::hook(Point::BeforeObserve);
        self.observe_own(st, r.parent, &r.name);
        // Replace keeps the item id (D4 reuse rule); make sure the index agrees.
        let s = {
            let ix = st
                .ix
                .as_ref()
                .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
            ix.idx.lookup(r.parent, &r.name)
        };
        let s = s.ok_or_else(|| perr(ErrorCode::Io, "item vanished right after the operation"))?;
        let entry = self.reply_for_ours(st, s, &ours, tmp.as_ref().or(ro.as_ref()), leased)?;
        drop(tmp);
        Ok(Response::Written {
            entry,
            conflict_copy: None,
        })
    }

    fn bump_content(&self, st: &mut State, s: Slot) {
        let Some(ix) = st.ix.as_mut() else { return };
        if !ix.idx.alive(s) {
            return;
        }
        let mut t = crate::index::Txn::default();
        let seq = ix.idx.bump();
        let n = ix.idx.node_mut(s);
        n.content_seq = seq;
        n.seq = seq;
        t.touch(s, crate::index::CH_CONTENT);
        let _ = self.core.commit(
            st,
            t,
            CommitOpts {
                no_throttle: true,
                ..Default::default()
            },
        );
    }

    // ---- Mkdir / Symlink ---------------------------------------------------------------

    pub fn mkdir(&self, op: OpId, parent: ItemId, name: &str, may_exist: bool) -> R<Response> {
        if !valid_name(name) {
            return Err(perr(ErrorCode::InvalidName, "invalid name"));
        }
        let mut st = self.lock()?;
        if let Some(r) = self.stored(&st, &op) {
            return Ok(r);
        }
        self.core.flush(&mut st, false);
        let (pslot, pfd) = self.resolve_dir(&mut st, parent)?;
        match sys::mkdirat(pfd.as_raw_fd(), name.as_bytes(), 0o777) {
            Ok(()) => {}
            Err(e) if sys::is_errno(&e, libc::EEXIST) => {
                let is_dir = matches!(reconcile::stat_child(pfd.as_raw_fd(), name), Ok(Some((s, _))) if s.is_dir());
                if !(may_exist && is_dir) {
                    return Err(perr(ErrorCode::Exists, format!("{name} already exists")));
                }
            }
            Err(e) => return Err(io_err(&e, "mkdir")),
        }
        // Until the reply, any reconcile of this name (the inotify flush included) expands it.
        st.expand_new = Some((pslot, name.to_string()));
        let synced = sys::fsync(pfd.as_raw_fd()).map_err(|e| io_err(&e, "fsync dir"));
        if let Err(e) = synced {
            st.expand_new = None;
            return Err(e);
        }
        drop(pfd);
        // The client made (or adopted) this folder: it holds its listing from now on, so a
        // lazy one (`node_modules`, or any dir inside an expanded lazy dir) is scanned and
        // watched now — fileproviderd never enumerates a folder it created itself, and nothing
        // else would make unlatchd watch it (children the agent writes there would never reach
        // the Mac). A new dir is published already expanded (one upsert, `lazy: false`), so
        // no client ever sees it as a lazy dir whose listing it lacks.
        self.observe_own_ex(&mut st, pslot, name, true);
        st.expand_new = None;
        let made = self.entry_at(&st, pslot, name)?;
        if made.kind == Kind::Dir {
            // An existing lazy dir (`may_exist`): expand it like a ListDir would.
            self.core.expand(&mut st, self.session, made.id, None)?;
            let expanded = st.ix.as_ref().is_some_and(|ix| {
                ix.idx
                    .slot_of(made.id.0)
                    .is_some_and(|s| ix.idx.node(s).has(crate::index::F_EXPANDED))
            });
            if expanded {
                self.core.note_listed(&mut st, self.session, made.id.0);
            }
        }
        let resp = Response::Entry(self.entry_at(&st, pslot, name)?);
        self.finish(&mut st, &op, &resp, "mkdir")?;
        Ok(resp)
    }

    pub fn symlink(&self, op: OpId, parent: ItemId, name: &str, target: &str) -> R<Response> {
        if !valid_name(name) {
            return Err(perr(ErrorCode::InvalidName, "invalid name"));
        }
        if target.is_empty() || target.len() > 4095 || target.contains('\0') {
            return Err(perr(ErrorCode::InvalidName, "invalid symlink target"));
        }
        let mut st = self.lock()?;
        if let Some(r) = self.stored(&st, &op) {
            return Ok(r);
        }
        self.core.flush(&mut st, false);
        let (pslot, pfd) = self.resolve_dir(&mut st, parent)?;
        sys::symlinkat(target.as_bytes(), pfd.as_raw_fd(), name.as_bytes())
            .map_err(|e| io_err(&e, "symlink"))?;
        sys::fsync(pfd.as_raw_fd()).map_err(|e| io_err(&e, "fsync dir"))?;
        drop(pfd);
        self.observe_own(&mut st, pslot, name);
        let resp = Response::Entry(self.entry_at(&st, pslot, name)?);
        self.finish(&mut st, &op, &resp, "symlink")?;
        Ok(resp)
    }

    // ---- Rename ------------------------------------------------------------------------

    pub fn rename(
        &self,
        op: OpId,
        id: ItemId,
        base_parent: ItemId,
        base_name: &str,
        new_parent: ItemId,
        new_name: &str,
    ) -> R<Response> {
        if !valid_name(new_name) {
            return Err(perr(ErrorCode::InvalidName, "invalid name"));
        }
        let mut st = self.lock()?;
        if let Some(r) = self.stored(&st, &op) {
            return Ok(r);
        }
        self.core.flush(&mut st, false);
        let r = self.resolve(&mut st, id)?;
        let cur_parent = st
            .ix
            .as_ref()
            .map(|ix| ItemId(ix.idx.node(r.parent).id))
            .unwrap_or(ItemId(0));
        if cur_parent != base_parent || r.name != base_name {
            // Metadata never errors: the item moved meanwhile → report the server's state.
            let entry = self.entry_of(&st, r.slot)?;
            return Ok(Response::Renamed {
                entry,
                applied: false,
            });
        }
        if cur_parent == new_parent && r.name == new_name {
            let entry = self.entry_of(&st, r.slot)?;
            let resp = Response::Renamed {
                entry,
                applied: true,
            };
            self.finish(&mut st, &op, &resp, "rename")?;
            return Ok(resp);
        }
        let (npslot, npfd) = match self.resolve_dir(&mut st, new_parent) {
            Ok(x) => x,
            Err(e) if matches!(e.code, ErrorCode::NotFound | ErrorCode::NotDir) => {
                // The destination folder is gone (removed or replaced on the VM meanwhile).
                // Metadata never errors (rule 4): report the item where it still is. A bare
                // NotFound would read as "this item is gone" and delete it on the Mac.
                let entry = self.entry_of(&st, r.slot)?;
                let resp = Response::Renamed {
                    entry,
                    applied: false,
                };
                self.finish(&mut st, &op, &resp, "rename")?;
                return Ok(resp);
            }
            Err(e) => return Err(e),
        };
        let into_self = st
            .ix
            .as_ref()
            .map(|ix| ix.idx.is_ancestor_or_self(r.slot, npslot))
            .unwrap_or(true);
        if into_self {
            return Err(perr(
                ErrorCode::InvalidName,
                "cannot move a directory into itself",
            ));
        }
        if let Err(e) = sys::renameat2(
            r.pfd.as_raw_fd(),
            r.name.as_bytes(),
            npfd.as_raw_fd(),
            new_name.as_bytes(),
            sys::RENAME_NOREPLACE,
        ) {
            if sys::is_errno(&e, libc::EXDEV) {
                // Onto another filesystem (a mount point below the root): not a move. The item
                // is where it was — report it there (rule 4), never NotFound ("gone").
                let entry = self.entry_of(&st, r.slot)?;
                let resp = Response::Renamed {
                    entry,
                    applied: false,
                };
                self.finish(&mut st, &op, &resp, "rename")?;
                return Ok(resp);
            }
            return Err(io_err(&e, "rename"));
        }
        sys::fsync(npfd.as_raw_fd()).map_err(|e| io_err(&e, "fsync dir"))?;
        if npslot != r.parent {
            sys::fsync(r.pfd.as_raw_fd()).map_err(|e| io_err(&e, "fsync dir"))?;
        }
        drop(npfd);
        let (old_parent, old_name, slot) = (r.parent, r.name.clone(), r.slot);
        drop(r);
        self.core.flush(&mut st, false);
        // Polled dirs: observe both ends explicitly (a move is one batch).
        {
            let root_fd = st.root_fd.as_raw_fd();
            let polled = st.polled_root;
            if let Some(ix) = st.ix.as_mut() {
                let mut env = crate::core::CoreEnv::new(root_fd, &self.core.watcher, polled);
                let mut batch = Batch::default();
                batch.observe(&mut ix.idx, &mut env, old_parent, &old_name, 0);
                batch.observe(&mut ix.idx, &mut env, npslot, new_name, H_RENAMED);
                batch.settle(&mut ix.idx, &mut env);
                let txn = std::mem::take(&mut batch.txn);
                let _ = self.core.commit(
                    &mut st,
                    txn,
                    CommitOpts {
                        no_throttle: true,
                        ..Default::default()
                    },
                );
            }
        }
        let entry = match self.entry_of(&st, slot) {
            Ok(e) => e,
            Err(_) => self.entry_at(&st, npslot, new_name)?,
        };
        let resp = Response::Renamed {
            entry,
            applied: true,
        };
        self.finish(&mut st, &op, &resp, "rename")?;
        Ok(resp)
    }

    // ---- Remove ------------------------------------------------------------------------

    pub fn remove(
        &self,
        op: OpId,
        id: ItemId,
        base: Version,
        recursive: bool,
        seen_seq: u64,
    ) -> R<Response> {
        let mut st = self.lock()?;
        if let Some(r) = self.stored(&st, &op) {
            return Ok(r);
        }
        self.core.flush(&mut st, false);
        let r = self.resolve(&mut st, id)?;
        // The base is compared with the live item (polled dirs lag the disk).
        self.refresh_if_stale(&mut st, r.slot, r.parent, &r.name, &r.st);
        if !st.ix.as_ref().is_some_and(|ix| ix.idx.alive(r.slot)) {
            return Err(perr(
                ErrorCode::VersionMismatch,
                "the item changed since it was last seen",
            ));
        }
        // Never cross st_dev (§2(d)4) nor any mount boundary: a mount point below the root
        // (another filesystem, or a bind mount of a directory from anywhere) is kept, with
        // whatever is on it. Directories only: overlayfs may report a lower layer's st_dev for
        // files, and a bind-mounted file cannot be unlinked anyway (EBUSY).
        let pdev = sys::fstat(r.pfd.as_raw_fd())
            .map_err(|e| io_err(&e, "fstat"))?
            .dev;
        let pmnt = sys::mount_id(r.pfd.as_raw_fd());
        if r.st.is_dir() && other_mount(r.pfd.as_raw_fd(), &r.name, pdev, pmnt) {
            let kept = st
                .ix
                .as_ref()
                .map(|ix| vec![ItemId(ix.idx.node(r.slot).id)])
                .unwrap_or_default();
            crate::log!("remove {id}: a mount point, kept");
            let resp = Response::Removed { kept };
            self.finish(&mut st, &op, &resp, "remove")?;
            return Ok(resp);
        }
        let (kind, cseq, mseq) = {
            let ix = st
                .ix
                .as_ref()
                .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
            let n = ix.idx.node(r.slot);
            (n.kind(), n.content_seq, n.meta_seq)
        };
        let base_ok = match kind {
            NKind::Dir => base.meta == mseq,
            _ => base.meta == mseq && base.content == cseq,
        };
        if !base_ok {
            return Err(perr(
                ErrorCode::VersionMismatch,
                "the item changed since it was last seen",
            ));
        }
        let mut kept: Vec<ItemId> = Vec::new();
        match kind {
            NKind::File | NKind::Symlink => {
                sys::unlinkat(r.pfd.as_raw_fd(), r.name.as_bytes(), false)
                    .map_err(|e| io_err(&e, "unlink"))?;
            }
            NKind::Dir if !recursive => {
                sys::unlinkat(r.pfd.as_raw_fd(), r.name.as_bytes(), true)
                    .map_err(|e| io_err(&e, "rmdir"))?;
            }
            NKind::Dir => {
                let seen_time = st.ix.as_mut().and_then(|ix| {
                    ix.idx.check_clock();
                    ix.idx.time_of_seq(seen_seq)
                });
                let ix = st
                    .ix
                    .as_ref()
                    .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
                let mut rm = Remover {
                    idx: &ix.idx,
                    seen_seq,
                    seen_time,
                    dev: pdev,
                    mnt: pmnt,
                    kept: &mut kept,
                };
                rm.dir(r.pfd.as_raw_fd(), &r.name, Some(r.slot), false);
                crate::log!(
                    "remove {id} recursive: seen_seq {seen_seq} (time {seen_time:?}), kept {}",
                    kept.len()
                );
            }
        }
        sys::fsync(r.pfd.as_raw_fd()).map_err(|e| io_err(&e, "fsync dir"))?;
        let (p, name) = (r.parent, r.name.clone());
        drop(r);
        self.observe_own(&mut st, p, &name);
        let resp = Response::Removed { kept };
        self.finish(&mut st, &op, &resp, "remove")?;
        Ok(resp)
    }

    // ---- SetAttr -----------------------------------------------------------------------

    pub fn setattr(
        &self,
        op: OpId,
        id: ItemId,
        exec: Option<bool>,
        mtime_ns: Option<i64>,
    ) -> R<Response> {
        let mut st = self.lock()?;
        if let Some(r) = self.stored(&st, &op) {
            return Ok(r);
        }
        self.core.flush(&mut st, false);
        let is_root = st
            .ix
            .as_ref()
            .map(|ix| ix.idx.slot_of(id.0) == Some(ix.idx.root))
            .unwrap_or(false);
        if is_root {
            let root_fd = st.root_fd.as_raw_fd();
            if let Some(m) = mtime_ns {
                sys::set_mtime_fd(root_fd, m).map_err(|e| io_err(&e, "set mtime"))?;
            }
            self.core.flush(&mut st, false);
            let s = st
                .ix
                .as_ref()
                .map(|ix| ix.idx.root)
                .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
            let resp = Response::Entry(self.entry_of(&st, s)?);
            self.finish(&mut st, &op, &resp, "setattr")?;
            return Ok(resp);
        }
        let r = self.resolve(&mut st, id)?;
        let kind = st
            .ix
            .as_ref()
            .map(|ix| ix.idx.node(r.slot).kind())
            .unwrap_or(NKind::File);
        if kind == NKind::Symlink {
            if let Some(m) = mtime_ns {
                sys::set_mtime_at(r.pfd.as_raw_fd(), r.name.as_bytes(), m)
                    .map_err(|e| io_err(&e, "set mtime"))?;
            }
        } else {
            let f = sys::openat(
                r.pfd.as_raw_fd(),
                r.name.as_bytes(),
                libc::O_PATH | libc::O_NOFOLLOW,
                0,
            )
            .map_err(|e| io_err(&e, "open"))?;
            let fst = sys::fstat(f.as_raw_fd()).map_err(|e| io_err(&e, "fstat"))?;
            if fst.ino != r.st.ino || fst.dev != r.st.dev {
                return Err(perr(ErrorCode::NotFound, "item changed on the VM"));
            }
            if let Some(x) = exec {
                sys::chmod_fd_path(f.as_raw_fd(), apply_exec(fst.perm(), x))
                    .map_err(|e| io_err(&e, "chmod"))?;
            }
            if let Some(m) = mtime_ns {
                sys::set_mtime_fd(f.as_raw_fd(), m).map_err(|e| io_err(&e, "set mtime"))?;
            }
        }
        let (p, name, slot) = (r.parent, r.name.clone(), r.slot);
        drop(r);
        self.observe_own(&mut st, p, &name);
        let resp = Response::Entry(self.entry_of(&st, slot)?);
        self.finish(&mut st, &op, &resp, "setattr")?;
        Ok(resp)
    }

    // ---- Read --------------------------------------------------------------------------

    /// Open a file for streaming: identity-checked fd, its content seq, and fstat (§2(d)5).
    pub fn open_read(&self, id: ItemId, expect: Option<u64>) -> R<(OwnedFd, u64, Stat)> {
        let mut st = self.lock()?;
        self.core.flush(&mut st, false);
        let is_dir = st
            .ix
            .as_ref()
            .and_then(|ix| {
                ix.idx
                    .slot_of(id.0)
                    .map(|s| ix.idx.node(s).kind() == NKind::Dir)
            })
            .unwrap_or(false);
        if is_dir {
            return Err(perr(ErrorCode::IsDir, "is a directory"));
        }
        let r = self.resolve(&mut st, id)?;
        let (kind, cseq) = {
            let ix = st
                .ix
                .as_ref()
                .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
            let n = ix.idx.node(r.slot);
            (n.kind(), n.content_seq)
        };
        if kind == NKind::Dir {
            return Err(perr(ErrorCode::IsDir, "is a directory"));
        }
        if kind == NKind::Symlink {
            return Err(perr(ErrorCode::Unsupported, "symlinks have no content"));
        }
        if let Some(e) = expect {
            if e != cseq {
                return Err(perr(ErrorCode::VersionMismatch, "content changed"));
            }
        }
        let fd = sys::openat(
            r.pfd.as_raw_fd(),
            r.name.as_bytes(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )
        .map_err(|e| io_err(&e, "open"))?;
        let fst = sys::fstat(fd.as_raw_fd()).map_err(|e| io_err(&e, "fstat"))?;
        if fst.ino != r.st.ino || fst.dev != r.st.dev {
            return Err(perr(ErrorCode::VersionMismatch, "content changed"));
        }
        Ok((fd, cseq, fst))
    }

    /// Current content seq of an id (after a flush), for the end-of-stream check.
    pub fn content_seq(&self, id: ItemId) -> Option<u64> {
        let mut st = self.lock().ok()?;
        self.core.flush(&mut st, false);
        let ix = st.ix.as_ref()?;
        ix.idx.slot_of(id.0).map(|s| ix.idx.node(s).content_seq)
    }
}

/// Rename the staging name `stage` in `dirfd` to a free conflict name next to `name`.
fn keep_as_conflict(dirfd: RawFd, stage: &str, name: &str, client: &str) -> bool {
    let stamp = local_stamp();
    for n in 1..1000u32 {
        let cn = conflict_name(name, client, &stamp, n);
        match sys::renameat2(
            dirfd,
            stage.as_bytes(),
            dirfd,
            cn.as_bytes(),
            sys::RENAME_NOREPLACE,
        ) {
            Ok(()) => {
                let _ = sys::fsync(dirfd);
                return true;
            }
            Err(e) if sys::is_errno(&e, libc::EEXIST) => continue,
            Err(_) => return false,
        }
    }
    false
}

/// An old inode a replace exchanged out (now at its staging name, which no listing shows)
/// while another open file description still referred to it: a write through that handle
/// after the exchange would land on an inode nobody can reach any more. It stays parked until
/// [`sweep_parked`] sees it written (kept as a conflict copy, so the bytes show up) or closed
/// by everyone else (the exclusive lease is granted: unlinked).
pub struct Parked {
    dir: OwnedFd,
    stage: String,
    name: String,
    client: String,
    held: OwnedFd,
    check: Stat,
    since: std::time::Instant,
}

/// At most this many parked old inodes (open fds); beyond it the oldest is kept as a
/// conflict copy.
const MAX_PARKED: usize = 256;
/// Without leases (not the owner, a filesystem without them) an unchanged parked inode is
/// unlinked after this long.
const PARK_NO_LEASE: std::time::Duration = std::time::Duration::from_secs(10);

fn park(st: &mut State, p: Parked) {
    st.parked.push(p);
    while st.parked.len() > MAX_PARKED {
        let o = st.parked.remove(0);
        if !keep_as_conflict(o.dir.as_raw_fd(), &o.stage, &o.name, &o.client) {
            crate::log!("could not keep a parked old version; left at {}", o.stage);
        }
    }
}

/// Settle parked old inodes: written since → conflict copy; nobody else holds it → unlinked;
/// no lease possible → unlinked once unchanged for [`PARK_NO_LEASE`]. `stopping`: whatever is
/// still open elsewhere is kept as a conflict copy (the startup walk removes staging names).
pub(crate) fn sweep_parked(st: &mut State, stopping: bool) {
    if st.parked.is_empty() {
        return;
    }
    st.parked.retain(|p| {
        let dfd = p.dir.as_raw_fd();
        // `check` is the stat taken after the exchange, so ctime is comparable here.
        let changed = match sys::fstat(p.held.as_raw_fd()) {
            Ok(f) => !same_bytes(&f, &p.check),
            Err(_) => true,
        };
        if changed {
            if !keep_as_conflict(dfd, &p.stage, &p.name, &p.client) {
                crate::log!("could not keep a written old version; left at {}", p.stage);
            }
            return false;
        }
        match sys::lease_exclusive(p.held.as_raw_fd()) {
            // A write (and close) between the fstat above and the lease: re-check under it.
            Ok(()) if !sys::fstat(p.held.as_raw_fd()).is_ok_and(|f| same_bytes(&f, &p.check)) => {
                if !keep_as_conflict(dfd, &p.stage, &p.name, &p.client) {
                    crate::log!("could not keep a written old version; left at {}", p.stage);
                }
                false
            }
            Ok(()) => {
                let _ = sys::unlinkat(dfd, p.stage.as_bytes(), false);
                false
            }
            Err(e) if !stopping && !sys::is_errno(&e, libc::EAGAIN) => {
                if p.since.elapsed() >= PARK_NO_LEASE {
                    let _ = sys::unlinkat(dfd, p.stage.as_bytes(), false);
                    false
                } else {
                    true
                }
            }
            Err(_) if stopping => {
                let _ = keep_as_conflict(dfd, &p.stage, &p.name, &p.client);
                false
            }
            Err(_) => true,
        }
    });
}

/// The old inode still holds exactly the bytes it had at `check`: size, mtime and ctime all
/// unchanged. Same-size writes that put the mtime back within one timestamp tick are not seen
/// (documented limit; closing it would need a content hash of the old inode).
fn same_bytes(now: &Stat, check: &Stat) -> bool {
    now.size == check.size && now.mtime_ns == check.mtime_ns && now.ctime_ns == check.ctime_ns
}

const H_RENAMED: u8 = reconcile::H_MOVED_TO;

/// The live stat of an indexed item disagrees with what the index recorded for it: a change
/// not observed yet (a polled directory, or a watch event not read yet).
fn stat_differs(n: &crate::index::Node, live: &Stat) -> bool {
    n.ino != live.ino
        || n.mtime_ns != live.mtime_ns
        || n.ctime_ns != live.ctime_ns
        || n.perm != live.perm() as u16
        || (n.kind() != NKind::Dir && n.size != live.size)
}

/// Directory `name` in `pfd` is the root of another mount than its parent (`pdev`, `pmnt`), or
/// cannot be told apart from one (it cannot be opened).
fn other_mount(pfd: RawFd, name: &str, pdev: u64, pmnt: Option<u64>) -> bool {
    match sys::openat(
        pfd,
        name.as_bytes(),
        libc::O_PATH | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        0,
    ) {
        Ok(fd) => {
            sys::fstat(fd.as_raw_fd()).map_or(true, |st| st.dev != pdev)
                || (pmnt.is_some() && sys::mount_id(fd.as_raw_fd()) != pmnt)
        }
        Err(_) => true,
    }
}

/// Recursive delete by fd (§2(d)4): bottom-up, never follows symlinks, never crosses st_dev or
/// a mount (`dev`/`mnt` are those of the removed item's parent, so a removed mount point is
/// kept, a same-filesystem bind mount included),
/// keeps every entry newer than `seen_seq` (and its ancestors), and keeps whole any folder
/// that arrived (moved, renamed or created) after it.
struct Remover<'a> {
    idx: &'a crate::index::Index,
    seen_seq: u64,
    seen_time: Option<i64>,
    dev: u64,
    /// Mount id of the walk (None: the kernel does not report one; st_dev only).
    mnt: Option<u64>,
    kept: &'a mut Vec<ItemId>,
}

impl Remover<'_> {
    fn newer(&self, slot: Option<Slot>, st: &Stat) -> bool {
        match slot {
            // Indexed: by its seq — unless the live item differs from what the index holds
            // (a polled dir not polled yet): then the index cannot vouch that the client saw
            // this state, so it is newer.
            Some(s) => {
                let n = self.idx.node(s);
                n.seq > self.seen_seq || (!st.is_dir() && stat_differs(n, st))
            }
            None => self.changed_after_seen(st),
        }
    }

    /// Not in the index (lazy dir contents, or created after the flush): newer iff it
    /// changed after the client's seen point. When that point's time is unknown — the
    /// seq→time samples are checkpointed, not journaled, so after a crash a client's seen
    /// seq can predate all of them, and a wall clock that stepped back invalidates them —
    /// nothing proves the change is older: keep it (fuzz seed 406: the agent's files in an
    /// unexpanded node_modules were deleted).
    fn changed_after_seen(&self, st: &Stat) -> bool {
        match self.seen_time {
            Some(t) => st.ctime_ns > t || st.mtime_ns > t,
            None => true,
        }
    }

    /// A directory that arrived at its place after the seen point (moved in, renamed or
    /// created: its meta seq moved) is new to the client as a whole: moving a folder does not
    /// touch its descendants' seqs, so judging them one by one would delete files the client
    /// never saw there. Unindexed: by ctime (a rename sets it), as for any entry.
    fn arrived_after_seen(&self, slot: Option<Slot>, st: &Stat) -> bool {
        match slot {
            Some(s) => self.idx.node(s).meta_seq > self.seen_seq,
            None => self.changed_after_seen(st),
        }
    }

    fn child_slot(&self, dir: Option<Slot>, name: &str, st: &Stat) -> Option<Slot> {
        let d = dir?;
        let c = self.idx.lookup(d, name)?;
        self.idx.same_identity(c, st).then_some(c)
    }

    /// Returns true when `name` was removed entirely.
    fn dir(&mut self, parent_fd: RawFd, name: &str, slot: Option<Slot>, keep_self: bool) -> bool {
        let dfd = match sys::openat(parent_fd, name.as_bytes(), sys::DIR_FLAGS, 0) {
            Ok(f) => f,
            Err(_) => {
                self.keep(slot);
                return false;
            }
        };
        let dst = match sys::fstat(dfd.as_raw_fd()) {
            Ok(s) => s,
            Err(_) => {
                self.keep(slot);
                return false;
            }
        };
        if dst.dev != self.dev || (self.mnt.is_some() && sys::mount_id(dfd.as_raw_fd()) != self.mnt)
        {
            self.keep(slot); // mount point: never cross st_dev or a mount
            return false;
        }
        let slot = slot.filter(|&s| self.idx.same_identity(s, &dst));
        let mut kept_any = false;
        let ents = match sys::read_dir_fd(dfd.as_raw_fd()) {
            Ok(v) => v,
            Err(_) => {
                self.keep(slot);
                return false;
            }
        };
        for e in ents {
            let Ok(cname) = String::from_utf8(e.name.clone()) else {
                kept_any = true; // never delete what we cannot show
                continue;
            };
            let cst = match sys::statat(dfd.as_raw_fd(), &e.name) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if self.idx.is_excluded(&cst) {
                kept_any = true; // unlatchd's own install/state dir: never the Mac's to delete
                continue;
            }
            let cslot = self.child_slot(slot, &cname, &cst);
            let newer = self.newer(cslot, &cst);
            if cst.is_dir() && self.arrived_after_seen(cslot, &cst) {
                kept_any = true;
                self.keep(cslot);
            } else if cst.is_dir() {
                if !self.dir(dfd.as_raw_fd(), &cname, cslot, newer) {
                    kept_any = true;
                }
            } else if newer || sys::unlinkat(dfd.as_raw_fd(), &e.name, false).is_err() {
                kept_any = true;
                self.keep(cslot);
            }
        }
        if kept_any || keep_self {
            self.keep(slot);
            return false;
        }
        match sys::unlinkat(parent_fd, name.as_bytes(), true) {
            Ok(()) => true,
            Err(_) => {
                // Something appeared meanwhile: it is newer by definition.
                self.keep(slot);
                false
            }
        }
    }

    fn keep(&mut self, slot: Option<Slot>) {
        // Bounded so `Removed { kept }` stays one small frame; any non-empty list already
        // means DeletionRejected, and the survivors arrive as Events anyway.
        const MAX_KEPT: usize = 4096;
        if let Some(s) = slot {
            let id = ItemId(self.idx.node(s).id);
            if self.kept.len() < MAX_KEPT && !self.kept.contains(&id) {
                self.kept.push(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    type Hook = (Point, Box<dyn FnOnce()>);

    thread_local! {
        /// Runs once when a Write on this thread reaches the given [`Point`] after publishing
        /// our bytes: the windows in which an agent's write lands on our inode.
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    thread_local! {
        /// Overrides [`REHASH_ALWAYS_MAX`] for Writes on this thread.
        pub(super) static REHASH_MAX: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    }

    thread_local! {
        /// Bytes re-hashed after a publish by Writes on this thread (under the core lock).
        pub(super) static REHASHED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        /// Writes on this thread take no lease on their staged file (the re-hash fallback).
        pub(super) static NO_LEASE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Run `agent` on its own thread; return once it is done, or once it waits for a lease of
    /// ours on `f`'s inode (an open of a leased inode for writing blocks until the Write lets
    /// go of it — on this thread it would wait for itself). Join the handle after the Write
    /// returned.
    fn run_agent(
        f: &std::path::Path,
        agent: impl FnOnce() + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        use std::os::unix::fs::MetadataExt;
        let ino = std::fs::metadata(f).ok().map(|m| m.ino());
        let h = std::thread::spawn(agent);
        let t0 = std::time::Instant::now();
        while !h.is_finished() && t0.elapsed() < std::time::Duration::from_secs(5) {
            let breaking = ino.is_some_and(|i| {
                let suffix = format!(":{i} ");
                std::fs::read_to_string("/proc/locks").is_ok_and(|l| {
                    l.lines().any(|l| {
                        l.contains("LEASE") && l.contains("BREAKING") && l.contains(&suffix)
                    })
                })
            });
            if breaking {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        h
    }

    type AgentSlot = std::rc::Rc<RefCell<Option<std::thread::JoinHandle<()>>>>;

    fn join_agent(slot: &AgentSlot) {
        if let Some(h) = slot.borrow_mut().take() {
            h.join().unwrap();
        }
    }

    pub(super) fn hook(p: Point) {
        let f = HOOK.with(|h| {
            let mut h = h.borrow_mut();
            if h.as_ref().is_some_and(|(at, _)| *at == p) {
                h.take().map(|(_, f)| f)
            } else {
                None
            }
        });
        if let Some(f) = f {
            f();
        }
    }

    fn set_hook(p: Point, f: impl FnOnce() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some((p, Box::new(f))));
    }

    fn hook_pending() -> bool {
        HOOK.with(|h| h.borrow().is_some())
    }

    fn write_req(
        op: u8,
        parent: ItemId,
        name: &str,
        target: Option<ItemId>,
        base: Option<u64>,
        data: &[u8],
    ) -> (WriteReq, Receiver<Chunk>) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Chunk {
            data: data.to_vec(),
            last: true,
        })
        .unwrap();
        let req = WriteReq {
            op: [op; 16],
            parent,
            name: name.into(),
            target,
            base,
            size: data.len() as u64,
            content_hash: *blake3::hash(data).as_bytes(),
            mtime_ns: None,
            exec: None,
            move_to: None,
            may_exist: false,
        };
        (req, rx)
    }

    /// How the agent writes into the window.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Agent {
        /// Appends a line (size and mtime move).
        Append,
        /// Rewrites the bytes in place with others of the same size and puts the mtime back,
        /// as a write within the same timestamp tick leaves it: only the content tells.
        SameStat,
        /// `open(O_RDONLY | O_TRUNC)`: truncates to 0 bytes without breaking a read lease (the
        /// size tells).
        TruncRo,
    }

    impl Agent {
        fn write(self, f: &std::path::Path) {
            use std::io::{Seek, Write};
            match self {
                Agent::Append => {
                    let mut h = std::fs::OpenOptions::new().append(true).open(f).unwrap();
                    h.write_all(b"AGENT\n").unwrap();
                }
                Agent::SameStat => {
                    let m = std::fs::metadata(f).unwrap().modified().unwrap();
                    let mut h = std::fs::OpenOptions::new().write(true).open(f).unwrap();
                    h.seek(std::io::SeekFrom::Start(0)).unwrap();
                    h.write_all(b"MAC\n").unwrap();
                    h.set_modified(m).unwrap();
                }
                Agent::TruncRo => {
                    let c = sys::cstr(f.as_os_str().as_encoded_bytes()).unwrap();
                    // SAFETY: valid C string; the fd is closed at once.
                    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_TRUNC) };
                    assert!(fd >= 0, "{}", std::io::Error::last_os_error());
                    // SAFETY: our fd.
                    unsafe { libc::close(fd) };
                }
            }
        }

        fn result(self) -> &'static [u8] {
            match self {
                Agent::Append => b"mac\nAGENT\n",
                Agent::SameStat => b"MAC\n",
                Agent::TruncRo => b"",
            }
        }
    }

    /// An agent writes to the file at every point between unlatchd publishing (create),
    /// exchanging (replace) or writing in place (hard-linked target) the Mac's bytes and the
    /// checks that decide the reply — with inotify and polled. Up to the post-publish fstat
    /// the reply must be reported as a race: its version names the Mac's bytes only, older
    /// than the item's current version (the engine fetches the agent's). After the checks the
    /// write is an ordinary later change. Either way, `Read` at the reply's version never
    /// returns the agent's bytes, and a save based on the reply's version keeps the agent's
    /// bytes (conflict copy) — never overwrites them. `TruncRo` (an `O_RDONLY | O_TRUNC` open,
    /// which a read lease lets through) is seen by the size.
    #[test]
    fn reply_version_never_names_an_agent_write_racing_the_publish() {
        use crate::config::Config;
        use crate::core::Core;
        let mut runs = 0;
        // With the staged file's lease (an agent's open for writing waits until the reply is
        // decided; one attempted before the checks is reported as a race) and without (the
        // re-hash).
        for lease in [true, false] {
            NO_LEASE.with(|c| c.set(!lease));
            for polled in [false, true] {
                for case in ["create", "exchange", "in-place"] {
                    for point in POINTS {
                        for agent in [Agent::Append, Agent::SameStat, Agent::TruncRo] {
                            // Polling cannot tell a same-size write in the same timestamp tick
                            // from no write at all once it has observed the file (any polled
                            // file, not only ours); before that observation the hash does.
                            if polled && point == Point::AfterFstat && agent == Agent::SameStat {
                                continue;
                            }
                            let what = format!(
                                "{case} at {point:?}, {agent:?}, {}, lease {lease}",
                                if polled { "polled" } else { "inotify" }
                            );
                            race_case(polled, case, point, agent, &what);
                            runs += 1;
                        }
                    }
                }
            }
        }
        NO_LEASE.with(|c| c.set(false));
        // 45 writing cases (Append, SameStat) and 24 truncating ones per lease mode.
        assert_eq!(runs, 2 * (2 * 3 * 4 * 2 - 3 + 2 * 3 * 4));

        fn race_case(polled: bool, case: &str, point: Point, agent: Agent, what: &str) {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let f = root.path().join("f.txt");
            if case != "create" {
                std::fs::write(&f, b"v0\n").unwrap();
            }
            if case == "in-place" {
                std::fs::hard_link(&f, root.path().join("link.txt")).unwrap();
            }
            let mut cfg = Config::from_env();
            cfg.force_poll = polled;
            let core = Core::new(root.path(), state.path(), cfg).unwrap();
            let (target, base) = {
                let mut st = core.lock().unwrap();
                core.ensure_index(&mut st, &[]).unwrap();
                assert_eq!(st.polled_root, polled, "{what}");
                let ix = st.ix.as_ref().unwrap();
                match ix.idx.lookup(ix.idx.root, "f.txt") {
                    Some(s) => (
                        Some(ItemId(ix.idx.node(s).id)),
                        Some(ix.idx.node(s).content_seq),
                    ),
                    None => (None, None),
                }
            };
            let ops = Ops {
                core: &core,
                client: "t".into(),
                session: 0,
            };
            let ff = f.clone();
            let slot = AgentSlot::default();
            let s2 = slot.clone();
            set_hook(point, move || {
                let fa = ff.clone();
                *s2.borrow_mut() = Some(run_agent(&ff, move || agent.write(&fa)));
            });
            let (req, rx) = write_req(1, ItemId::ROOT, "f.txt", target, base, b"mac\n");
            let entry = match ops.write(req, &rx, &|_| {}) {
                Ok(Response::Written {
                    entry,
                    conflict_copy: None,
                }) => entry,
                o => panic!("{what}: {o:?}"),
            };
            assert!(!hook_pending(), "{what}: hook did not run");
            join_agent(&slot);
            assert_eq!(std::fs::read(&f).unwrap(), agent.result(), "{what}");
            let cur = ops.content_seq(entry.id).unwrap();
            if point != Point::AfterFstat {
                assert!(
                    entry.version.content < cur,
                    "{what}: reply version {} names the agent's bytes (current {cur})",
                    entry.version.content
                );
                assert_eq!(entry.size, 4, "{what}: the reply describes the Mac's bytes");
            }
            if !polled {
                // Polled: a later change waits for the poll (or the next op's live check).
                assert!(entry.version.content < cur, "{what}: no newer version");
                assert!(
                    matches!(ops.open_read(entry.id, Some(entry.version.content)), Err(e) if e.code == ErrorCode::VersionMismatch),
                    "{what}: Read at the reply's version returns the agent's bytes"
                );
            }
            // The Mac's next save, based on the version it was told it holds.
            let (req, rx) = write_req(
                2,
                ItemId::ROOT,
                "f.txt",
                Some(entry.id),
                Some(entry.version.content),
                b"mac next\n",
            );
            match ops.write(req, &rx, &|_| {}) {
                Ok(Response::Written {
                    conflict_copy: Some(_),
                    ..
                }) => {}
                o => panic!("{what}: next save {o:?}"),
            }
            assert_eq!(
                std::fs::read(&f).unwrap(),
                agent.result(),
                "{what}: agent bytes kept"
            );
            core.stop();
        }
    }
    /// An agent that only reads the file — an editor reloading it, an LSP, an indexer — at
    /// every point between the publish and the reply, with inotify and polled, with the lease
    /// and without: its read is not held back (it completes while the Write waits at that
    /// point), and it is no race: the reply carries the item's current version, `Read` at it
    /// returns the Mac's bytes, and the Mac's next save based on it lands without a conflict
    /// copy. With `hold` the reader keeps its handle across that next save (whose exchange
    /// parks the old inode until the handle is closed, then unlinks it: no copy).
    #[test]
    fn a_reader_racing_the_publish_is_no_race_and_never_waits() {
        use crate::config::Config;
        use crate::core::Core;
        let mut runs = 0;
        for lease in [true, false] {
            NO_LEASE.with(|c| c.set(!lease));
            for polled in [false, true] {
                for case in ["create", "exchange", "in-place"] {
                    for point in POINTS {
                        for hold in [false, true] {
                            let what = format!(
                                "{case} at {point:?}, hold {hold}, {}, lease {lease}",
                                if polled { "polled" } else { "inotify" }
                            );
                            reader_case(polled, case, point, hold, &what);
                            runs += 1;
                        }
                    }
                }
            }
        }
        NO_LEASE.with(|c| c.set(false));
        assert_eq!(runs, 2 * 2 * 3 * 4 * 2);

        fn reader_case(polled: bool, case: &str, point: Point, hold: bool, what: &str) {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let f = root.path().join("f.txt");
            if case != "create" {
                std::fs::write(&f, b"v0\n").unwrap();
            }
            if case == "in-place" {
                std::fs::hard_link(&f, root.path().join("link.txt")).unwrap();
            }
            let mut cfg = Config::from_env();
            cfg.force_poll = polled;
            let core = Core::new(root.path(), state.path(), cfg).unwrap();
            let (target, base) = {
                let mut st = core.lock().unwrap();
                core.ensure_index(&mut st, &[]).unwrap();
                let ix = st.ix.as_ref().unwrap();
                match ix.idx.lookup(ix.idx.root, "f.txt") {
                    Some(s) => (
                        Some(ItemId(ix.idx.node(s).id)),
                        Some(ix.idx.node(s).content_seq),
                    ),
                    None => (None, None),
                }
            };
            let ops = Ops {
                core: &core,
                client: "t".into(),
                session: 0,
            };
            type Held = std::rc::Rc<RefCell<Option<std::fs::File>>>;
            let held: Held = Default::default();
            let (ff, h2, w2) = (f.clone(), held.clone(), what.to_string());
            set_hook(point, move || {
                // On its own thread (on this one a wait for our lease would be a wait for
                // ourselves); the Write stays at this point until the read is done or 5 s.
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut h = std::fs::File::open(&ff).unwrap();
                    let mut b = Vec::new();
                    h.read_to_end(&mut b).unwrap();
                    let _ = tx.send((h, b));
                });
                let (h, b) = rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap_or_else(|_| panic!("{w2}: a reader waited for the Write"));
                assert_eq!(b, b"mac\n", "{w2}: the reader sees the Mac's bytes");
                if hold {
                    *h2.borrow_mut() = Some(h);
                }
            });
            let (req, rx) = write_req(1, ItemId::ROOT, "f.txt", target, base, b"mac\n");
            let entry = match ops.write(req, &rx, &|_| {}) {
                Ok(Response::Written {
                    entry,
                    conflict_copy: None,
                }) => entry,
                o => panic!("{what}: {o:?}"),
            };
            assert!(!hook_pending(), "{what}: hook did not run");
            assert_eq!(
                Some(entry.version.content),
                ops.content_seq(entry.id),
                "{what}: a reader was reported as a race"
            );
            let (fd, v, _) = ops
                .open_read(entry.id, Some(entry.version.content))
                .unwrap_or_else(|e| panic!("{what}: Read at the reply's version: {e:?}"));
            assert_eq!(v, entry.version.content, "{what}");
            let mut b = Vec::new();
            std::io::Read::read_to_end(&mut std::fs::File::from(fd), &mut b).unwrap();
            assert_eq!(b, b"mac\n", "{what}");
            // The Mac's next save, based on the version it was told it holds.
            let (req, rx) = write_req(
                2,
                ItemId::ROOT,
                "f.txt",
                Some(entry.id),
                Some(entry.version.content),
                b"mac next\n",
            );
            match ops.write(req, &rx, &|_| {}) {
                Ok(Response::Written {
                    conflict_copy: None,
                    ..
                }) => {}
                o => panic!("{what}: next save {o:?}"),
            }
            assert_eq!(std::fs::read(&f).unwrap(), b"mac next\n", "{what}");
            held.borrow_mut().take();
            {
                let mut st = core.lock().unwrap();
                sweep_parked(&mut st, false);
                assert!(st.parked.is_empty(), "{what}: still parked");
            }
            let mut names: Vec<String> = std::fs::read_dir(root.path())
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            let want: &[&str] = if case == "in-place" {
                &["f.txt", "link.txt"]
            } else {
                &["f.txt"]
            };
            assert_eq!(names, want, "{what}: no copies");
            core.stop();
        }
    }

    /// An agent holds the file open (its handle is on the old inode) across a Mac save that
    /// exchanges that inode out, and writes through the handle at each point after the
    /// exchange, after the Write replied, or never. Its bytes never vanish: written, the old
    /// inode is kept as a conflict copy (at once, or by the sweep); closed unwritten, it is
    /// unlinked (no copy).
    #[test]
    fn agent_handle_on_the_replaced_inode_never_loses_its_writes() {
        use crate::config::Config;
        use crate::core::Core;
        use std::io::Write;
        type Handle = std::rc::Rc<RefCell<Option<std::fs::File>>>;
        let write_through = |h: &Handle| {
            h.borrow_mut()
                .as_mut()
                .unwrap()
                .write_all(b"AGENT\n")
                .unwrap()
        };
        for when in [
            Some(Point::Published),
            Some(Point::BeforeObserve),
            Some(Point::BeforeFstat),
            Some(Point::AfterFstat),
            None,
        ] {
            for later in [true, false] {
                if when.is_some() && !later {
                    continue;
                }
                // when=None, later=true: written after the reply; later=false: never written.
                let what = format!("{when:?} later={later}");
                let root = tempfile::tempdir().unwrap();
                let state = tempfile::tempdir().unwrap();
                let f = root.path().join("f.txt");
                std::fs::write(&f, b"v0\n").unwrap();
                let core = Core::new(root.path(), state.path(), Config::from_env()).unwrap();
                let (id, base) = {
                    let mut st = core.lock().unwrap();
                    core.ensure_index(&mut st, &[]).unwrap();
                    let ix = st.ix.as_ref().unwrap();
                    let s = ix.idx.lookup(ix.idx.root, "f.txt").unwrap();
                    (ItemId(ix.idx.node(s).id), ix.idx.node(s).content_seq)
                };
                let ops = Ops {
                    core: &core,
                    client: "t".into(),
                    session: 0,
                };
                let h: Handle = std::rc::Rc::new(RefCell::new(Some(
                    std::fs::OpenOptions::new().append(true).open(&f).unwrap(),
                )));
                if let Some(p) = when {
                    let h2 = h.clone();
                    set_hook(p, move || write_through(&h2));
                }
                let (req, rx) = write_req(1, ItemId::ROOT, "f.txt", Some(id), Some(base), b"mac\n");
                match ops.write(req, &rx, &|_| {}) {
                    Ok(Response::Written {
                        conflict_copy: None,
                        ..
                    }) => {}
                    o => panic!("{what}: {o:?}"),
                }
                assert!(!hook_pending(), "{what}");
                assert_eq!(std::fs::read(&f).unwrap(), b"mac\n", "{what}");
                if when.is_none() {
                    if later {
                        write_through(&h);
                    } else {
                        h.borrow_mut().take();
                    }
                }
                {
                    let mut st = core.lock().unwrap();
                    sweep_parked(&mut st, false);
                    assert!(st.parked.is_empty(), "{what}: still parked");
                }
                let mut others: Vec<Vec<u8>> = std::fs::read_dir(root.path())
                    .unwrap()
                    .flatten()
                    .filter(|e| e.file_name() != "f.txt")
                    .map(|e| std::fs::read(e.path()).unwrap())
                    .collect();
                others.sort();
                let wrote = when.is_some() || later;
                if wrote {
                    assert_eq!(
                        others,
                        [b"v0\nAGENT\n".to_vec()],
                        "{what}: agent bytes kept"
                    );
                } else {
                    assert!(others.is_empty(), "{what}: no copy without a write");
                }
                drop(h);
                core.stop();
            }
        }
    }

    /// A hard-linked target written by an agent after the base check, before unlatchd writes the
    /// Mac's bytes in place: both versions are kept (conflict copy), as for a base mismatch.
    #[test]
    fn in_place_write_after_the_base_check_keeps_both() {
        use crate::config::Config;
        use crate::core::Core;
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let f = root.path().join("f.txt");
        std::fs::write(&f, b"v0\n").unwrap();
        std::fs::hard_link(&f, root.path().join("link.txt")).unwrap();
        let core = Core::new(root.path(), state.path(), Config::from_env()).unwrap();
        let (id, base) = {
            let mut st = core.lock().unwrap();
            core.ensure_index(&mut st, &[]).unwrap();
            let ix = st.ix.as_ref().unwrap();
            let s = ix.idx.lookup(ix.idx.root, "f.txt").unwrap();
            (ItemId(ix.idx.node(s).id), ix.idx.node(s).content_seq)
        };
        let ops = Ops {
            core: &core,
            client: "t".into(),
            session: 0,
        };
        let ff = f.clone();
        set_hook(Point::BeforeInPlace, move || Agent::Append.write(&ff));
        let (req, rx) = write_req(1, ItemId::ROOT, "f.txt", Some(id), Some(base), b"mac\n");
        let (entry, copy) = match ops.write(req, &rx, &|_| {}) {
            Ok(Response::Written {
                entry,
                conflict_copy: Some(c),
            }) => (entry, c),
            o => panic!("{o:?}"),
        };
        assert!(!hook_pending());
        assert_eq!(std::fs::read(&f).unwrap(), b"v0\nAGENT\n");
        assert_eq!(
            std::fs::read(root.path().join(&copy.name)).unwrap(),
            b"mac\n"
        );
        assert_eq!(Some(entry.version.content), ops.content_seq(id));
        assert!(entry.version.content > base);
        core.stop();
    }

    /// A file too large to be re-hashed whose stat proves writes (published with an mtime
    /// older than the staged inode's ctime): a same-size agent write at any point before the
    /// checks moves the mtime away from ours and is reported as a race without the hash.
    #[test]
    fn large_upload_race_is_seen_by_stat_alone() {
        use crate::config::Config;
        use crate::core::Core;
        use std::io::Write;
        REHASH_MAX.with(|c| c.set(Some(0)));
        for (lease, case) in [
            (false, "create"),
            (false, "exchange"),
            (true, "create"),
            (true, "exchange"),
        ] {
            NO_LEASE.with(|c| c.set(!lease));
            for point in [Point::Published, Point::BeforeObserve, Point::BeforeFstat] {
                let what = format!("{case} at {point:?}, lease {lease}");
                let root = tempfile::tempdir().unwrap();
                let state = tempfile::tempdir().unwrap();
                let f = root.path().join("f.txt");
                if case == "exchange" {
                    std::fs::write(&f, b"v0\n").unwrap();
                }
                let core = Core::new(root.path(), state.path(), Config::from_env()).unwrap();
                let (target, base) = {
                    let mut st = core.lock().unwrap();
                    core.ensure_index(&mut st, &[]).unwrap();
                    let ix = st.ix.as_ref().unwrap();
                    match ix.idx.lookup(ix.idx.root, "f.txt") {
                        Some(s) => (
                            Some(ItemId(ix.idx.node(s).id)),
                            Some(ix.idx.node(s).content_seq),
                        ),
                        None => (None, None),
                    }
                };
                let ops = Ops {
                    core: &core,
                    client: "t".into(),
                    session: 0,
                };
                let ff = f.clone();
                let slot = AgentSlot::default();
                let s2 = slot.clone();
                set_hook(point, move || {
                    let fa = ff.clone();
                    *s2.borrow_mut() = Some(run_agent(&ff, move || {
                        let mut h = std::fs::OpenOptions::new().write(true).open(&fa).unwrap();
                        h.write_all(b"MAC\n").unwrap();
                    }));
                });
                let (mut req, rx) = write_req(1, ItemId::ROOT, "f.txt", target, base, b"mac\n");
                req.mtime_ns = Some(1_000_000_000);
                let entry = match ops.write(req, &rx, &|_| {}) {
                    Ok(Response::Written {
                        entry,
                        conflict_copy: None,
                    }) => entry,
                    o => panic!("{what}: {o:?}"),
                };
                assert!(!hook_pending(), "{what}: hook did not run");
                join_agent(&slot);
                assert_eq!(std::fs::read(&f).unwrap(), b"MAC\n", "{what}");
                let cur = ops.content_seq(entry.id).unwrap();
                assert!(
                    entry.version.content < cur,
                    "{what}: not reported as a race"
                );
                assert_eq!(entry.mtime_ns, 1_000_000_000, "{what}: the Mac's mtime");
                core.stop();
            }
        }
        REHASH_MAX.with(|c| c.set(None));
        NO_LEASE.with(|c| c.set(false));
    }

    /// The interactive path never waits for a re-read of the upload: with the staged file's
    /// lease, a create or replace re-hashes nothing under the core lock (every request of every
    /// session waits on that lock; a 64 MiB re-hash held it 30–60 ms). Without a lease the
    /// fallback re-hash still runs (the counter sees it).
    #[test]
    fn leased_write_rehashes_nothing_under_the_core_lock() {
        use crate::config::Config;
        use crate::core::Core;
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        // Leases need a filesystem that has them (and fs.leases-enable).
        let probe = sys::open_tmpfile(
            sys::openat(
                libc::AT_FDCWD,
                root.path().as_os_str().as_encoded_bytes(),
                sys::DIR_FLAGS,
                0,
            )
            .unwrap()
            .as_raw_fd(),
            0o600,
        )
        .unwrap();
        let (probe, _, leased) = Ops::lease_staged(probe);
        if !leased {
            eprintln!("SKIP: no leases on {}", root.path().display());
            return;
        }
        drop(probe);
        let core = Core::new(root.path(), state.path(), Config::from_env()).unwrap();
        {
            let mut st = core.lock().unwrap();
            core.ensure_index(&mut st, &[]).unwrap();
        }
        let ops = Ops {
            core: &core,
            client: "t".into(),
            session: 0,
        };
        let data = vec![7u8; 4 << 20];
        for lease in [true, false] {
            NO_LEASE.with(|c| c.set(!lease));
            REHASHED.with(|c| c.set(0));
            let name = format!("big-{lease}.bin");
            let (req, rx) = write_req(lease as u8 * 10 + 1, ItemId::ROOT, &name, None, None, &data);
            let e = match ops.write(req, &rx, &|_| {}) {
                Ok(Response::Written {
                    entry,
                    conflict_copy: None,
                }) => entry,
                o => panic!("create, lease {lease}: {o:?}"),
            };
            let cur = ops.content_seq(e.id);
            assert_eq!(
                Some(e.version.content),
                cur,
                "create, lease {lease}: no race"
            );
            let (req, rx) = write_req(
                lease as u8 * 10 + 2,
                ItemId::ROOT,
                &name,
                Some(e.id),
                Some(e.version.content),
                &data[1..],
            );
            let e = match ops.write(req, &rx, &|_| {}) {
                Ok(Response::Written {
                    entry,
                    conflict_copy: None,
                }) => entry,
                o => panic!("replace, lease {lease}: {o:?}"),
            };
            assert_eq!(
                Some(e.version.content),
                ops.content_seq(e.id),
                "replace, lease {lease}"
            );
            let rehashed = REHASHED.with(|c| c.get());
            if lease {
                assert_eq!(rehashed, 0, "a leased Write re-hashed under the core lock");
            } else {
                assert_eq!(rehashed, 2 * (4 << 20) - 1, "the fallback re-hash ran");
            }
        }
        NO_LEASE.with(|c| c.set(false));
        core.stop();
    }

    /// No agent write: the reply carries the item's current version (no extra fetch).
    #[test]
    fn reply_version_is_current_without_a_racing_write() {
        use crate::config::Config;
        use crate::core::Core;
        for polled in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("f.txt"), b"v0\n").unwrap();
            std::fs::write(root.path().join("h.txt"), b"v0\n").unwrap();
            std::fs::hard_link(root.path().join("h.txt"), root.path().join("l.txt")).unwrap();
            let mut cfg = Config::from_env();
            cfg.force_poll = polled;
            let core = Core::new(root.path(), state.path(), cfg).unwrap();
            let ids = {
                let mut st = core.lock().unwrap();
                core.ensure_index(&mut st, &[]).unwrap();
                let ix = st.ix.as_ref().unwrap();
                ["f.txt", "h.txt"].map(|n| {
                    let s = ix.idx.lookup(ix.idx.root, n).unwrap();
                    (ItemId(ix.idx.node(s).id), ix.idx.node(s).content_seq)
                })
            };
            let ops = Ops {
                core: &core,
                client: "t".into(),
                session: 0,
            };
            // exchange, in place (hard-linked), create; then with a requested mtime.
            for (i, mtime) in [(0u8, None), (10, Some(1_000_000_000))] {
                let cur = |id: ItemId| ops.content_seq(id);
                let (f, h) = (ids[0].0, ids[1].0);
                let reqs = [
                    write_req(i + 1, ItemId::ROOT, "f.txt", Some(f), cur(f), b"mac\n"),
                    write_req(i + 2, ItemId::ROOT, "h.txt", Some(h), cur(h), b"mac\n"),
                    write_req(
                        i + 3,
                        ItemId::ROOT,
                        &format!("g{i}.txt"),
                        None,
                        None,
                        b"new\n",
                    ),
                ];
                for (mut req, rx) in reqs {
                    req.mtime_ns = mtime;
                    let what = format!("{} polled={polled} mtime={mtime:?}", req.name);
                    let entry = match ops.write(req, &rx, &|_| {}) {
                        Ok(Response::Written {
                            entry,
                            conflict_copy: None,
                        }) => entry,
                        o => panic!("{what}: {o:?}"),
                    };
                    assert_eq!(
                        Some(entry.version.content),
                        ops.content_seq(entry.id),
                        "{what}: spurious race (an extra fetch)"
                    );
                    assert_eq!(entry.size, 4, "{what}");
                }
            }
            assert!(
                core.lock().unwrap().parked.is_empty(),
                "nobody else held the old inodes: unlinked at once"
            );
            core.stop();
        }
    }

    /// The wall clock steps back after the Mac synced: the seq→time sample of its seen seq is
    /// later than the agent's new file in an unexpanded lazy dir. A recursive delete must keep
    /// that file (the sample is in the future: unknown, keep).
    #[test]
    fn clock_step_back_keeps_unseen_unindexed_entries() {
        use crate::config::Config;
        use crate::core::Core;
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("proj/node_modules")).unwrap();
        std::fs::write(root.path().join("proj/old.txt"), b"o").unwrap();
        let core = Core::new(root.path(), state.path(), Config::from_env()).unwrap();
        let (proj, base, seen) = {
            let mut st = core.lock().unwrap();
            core.ensure_index(&mut st, &[]).unwrap();
            let ix = st.ix.as_mut().unwrap();
            let p = ix.idx.lookup(ix.idx.root, "proj").unwrap();
            let n = ix.idx.node(p);
            let r = (
                ItemId(n.id),
                Version {
                    content: n.content_seq,
                    meta: n.meta_seq,
                },
                ix.idx.seq,
            );
            // Synced at T; the clock has since stepped back 10 minutes.
            ix.idx.seq_times.clear();
            ix.idx
                .seq_times
                .push((ix.idx.seq, crate::sys::now_ns() + 600_000_000_000));
            r
        };
        std::fs::write(root.path().join("proj/node_modules/agent.js"), b"agent").unwrap();
        let ops = Ops {
            core: &core,
            client: "t".into(),
            session: 0,
        };
        match ops.remove([9; 16], proj, base, true, seen) {
            Ok(Response::Removed { kept }) => assert!(kept.contains(&proj), "{kept:?}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read(root.path().join("proj/node_modules/agent.js")).unwrap(),
            b"agent"
        );
        assert!(!root.path().join("proj/old.txt").exists(), "seen files go");
        core.stop();
    }

    #[test]
    fn conflict_names() {
        let n = conflict_name("report.txt", "Sudhanshu's Mac", "2026-09-30 10.00", 1);
        assert_eq!(
            n,
            "report (conflict from Sudhanshu's Mac 2026-09-30 10.00).txt"
        );
        assert_eq!(
            conflict_name(".bashrc", "m", "s", 2),
            ".bashrc (conflict from m s) 2"
        );
        let long = "é".repeat(200) + ".rs";
        let c = conflict_name(&long, "mac", "2026-09-30 10.00", 3);
        assert!(c.len() <= 255);
        assert!(c.ends_with(" 3.rs"));
        assert!(valid_name(&c));
    }

    #[test]
    fn client_names_sanitized() {
        assert_eq!(sanitize_client("a/b\0c\nd"), "abcd");
        assert_eq!(sanitize_client(""), "mac");
        assert!(sanitize_client(&"x".repeat(100)).len() <= 32);
        assert_eq!(sanitize_client("ééééééééééééééééé").len(), 32);
    }

    #[test]
    fn exec_bits() {
        assert_eq!(apply_exec(0o644, true), 0o755);
        assert_eq!(apply_exec(0o600, true), 0o700);
        assert_eq!(apply_exec(0o755, false), 0o644);
    }

    /// The index still maps ids through `a/b`, but `a` was swapped for a symlink to a directory
    /// outside the root and the watcher has not caught up (events dropped): every op must fail
    /// or act inside the root — never follow the symlink (D2/§9).
    #[test]
    fn stale_index_path_through_symlink_never_escapes() {
        use crate::config::Config;
        use crate::core::Core;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(root.path().join("a/b/f"), b"inside").unwrap();
        std::fs::create_dir_all(outside.path().join("b")).unwrap();
        std::fs::write(outside.path().join("b/f"), b"outside").unwrap();
        let core = Core::new(root.path(), state.path(), Config::from_env()).unwrap();
        let (b_id, f_id, f_ver) = {
            let mut st = core.lock().unwrap();
            core.ensure_index(&mut st, &[]).unwrap();
            let ix = st.ix.as_ref().unwrap();
            let a = ix.idx.lookup(ix.idx.root, "a").unwrap();
            let b = ix.idx.lookup(a, "b").unwrap();
            let f = ix.idx.lookup(b, "f").unwrap();
            (
                ItemId(ix.idx.node(b).id),
                ItemId(ix.idx.node(f).id),
                ix.idx.node(f).content_seq,
            )
        };
        {
            // Swap while holding the lock, then drop the events on the floor.
            let _st = core.lock().unwrap();
            std::fs::rename(root.path().join("a"), root.path().join("a.old")).unwrap();
            std::os::unix::fs::symlink(outside.path(), root.path().join("a")).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            let _ = core.watcher.take();
        }
        let ops = Ops {
            core: &core,
            client: "t".into(),
            session: 0,
        };
        for fallback in [false, true] {
            sys::force_openat2_fallback(fallback);
            assert!(ops
                .mkdir([fallback as u8; 16], b_id, "evil", false)
                .is_err());
            assert!(ops
                .setattr([2 + fallback as u8; 16], f_id, Some(true), None)
                .is_err());
            assert!(ops
                .remove(
                    [4 + fallback as u8; 16],
                    f_id,
                    Version {
                        content: f_ver,
                        meta: 0
                    },
                    false,
                    0
                )
                .is_err());
            assert!(ops.open_read(f_id, None).is_err());
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(Chunk {
                data: b"pwn".to_vec(),
                last: true,
            })
            .unwrap();
            let req = WriteReq {
                op: [6 + fallback as u8; 16],
                parent: b_id,
                name: "x".into(),
                target: Some(f_id),
                base: Some(f_ver),
                size: 3,
                content_hash: *blake3::hash(b"pwn").as_bytes(),
                mtime_ns: None,
                exec: None,
                move_to: None,
                may_exist: false,
            };
            assert!(ops.write(req, &rx, &|_| {}).is_err());
        }
        sys::force_openat2_fallback(false);
        assert_eq!(
            std::fs::read(outside.path().join("b/f")).unwrap(),
            b"outside"
        );
        assert!(!outside.path().join("b/evil").exists());
        assert_eq!(
            std::fs::read_dir(outside.path().join("b")).unwrap().count(),
            1
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(outside.path().join("b/f"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        core.stop();
    }

    /// Fuzz seed 406: a recursive delete reaching contents the index does not hold (an
    /// unexpanded lazy dir) judges them by the time of the client's seen seq; with that time
    /// unknown (samples lost in a crash) they must be kept, never deleted.
    #[test]
    fn unknown_seen_time_keeps_unindexed_entries() {
        use crate::config::Config;
        use crate::core::Core;
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("proj/node_modules")).unwrap();
        std::fs::write(root.path().join("proj/old.txt"), b"o").unwrap();
        std::fs::write(root.path().join("proj/node_modules/agent.js"), b"agent").unwrap();
        let core = Core::new(root.path(), state.path(), Config::from_env()).unwrap();
        let (proj, base, seen) = {
            let mut st = core.lock().unwrap();
            core.ensure_index(&mut st, &[]).unwrap();
            let ix = st.ix.as_mut().unwrap();
            let p = ix.idx.lookup(ix.idx.root, "proj").unwrap();
            let nm = ix.idx.lookup(p, "node_modules").unwrap();
            assert!(ix.idx.lookup(nm, "agent.js").is_none(), "lazy: not indexed");
            // As after a crash: no seq→time sample covers the client's seen seq.
            ix.idx.seq_times.clear();
            let n = ix.idx.node(p);
            (
                ItemId(n.id),
                Version {
                    content: n.content_seq,
                    meta: n.meta_seq,
                },
                ix.idx.seq,
            )
        };
        let ops = Ops {
            core: &core,
            client: "t".into(),
            session: 0,
        };
        match ops.remove([9; 16], proj, base, true, seen) {
            Ok(Response::Removed { kept }) => assert!(kept.contains(&proj), "{kept:?}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read(root.path().join("proj/node_modules/agent.js")).unwrap(),
            b"agent"
        );
        assert!(!root.path().join("proj/old.txt").exists(), "seen files go");
        core.stop();
    }

    #[test]
    fn staging_names_are_ignored() {
        assert!(reconcile::ignored_name(
            staging_name(&[0xab; 16]).as_bytes()
        ));
        assert!(reconcile::ignored_name(
            format!("{}.tmp", staging_name(&[1; 16])).as_bytes()
        ));
        assert!(!reconcile::ignored_name(b".unlatch-notes"));
    }
}
