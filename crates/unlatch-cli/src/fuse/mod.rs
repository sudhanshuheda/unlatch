//! Linux FUSE frontend (`unlatch mount`).
//!
//! Performance model: the kernel caches aggressively (long entry/attr TTLs, negative dentries,
//! readdir caching, `FOPEN_KEEP_CACHE`) and the engine's push channel keeps those caches honest:
//! every `EngineEvent::ReplicaChanged` is turned into `inval_entry` / `inval_inode` notifications
//! on a dedicated thread (see [`notify`]). Handlers that may wait on the network run on a worker
//! pool; handlers that only read the local replica run inline.
//!
//! Writes go to a per-inode scratch file and are uploaded on `flush` (close) / `fsync` /
//! `release` with the base version captured when the content was loaded, so a concurrent VM
//! edit becomes a conflict copy on the VM instead of being overwritten (D2/D6). A file created
//! locally gets a provisional inode and is created on the VM by its first upload (one round
//! trip per new file); an unlink before that never touches the VM.
//!
//! No write that write(2) acknowledged is dropped: when the mount stops (signal, external
//! unmount) with descriptors still open, further writes are refused and every dirty file is
//! uploaded, or kept in `unsynced/` if the VM is unreachable; scratch content holding
//! unuploaded writes carries a `.dirty` marker, so after a crash the next start moves it to
//! `unsynced/` instead of clearing it.
//!
//! The operations are plain methods on [`Shared`] returning `Result<_, errno>`, so they are
//! unit-testable without a kernel; [`fs`] adapts them to `fuser::Filesystem`.

pub mod attr;
pub mod fs;
pub mod notify;
pub mod pool;

#[cfg(test)]
mod mem;
#[cfg(test)]
mod tests;

use crate::backend::{list_all, Backend};
use crate::errno::errno;
use crate::inode::{InodeMap, ROOT_INO};
use attr::{attr_of, now_ns, pending_attr};
use fuser::FileAttr;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tracing::{debug, error, warn};
use unlatch_core::{CreateKind, CreateRequest, EngineEvent, EventHandler, Modified, ModifyRequest};
use unlatch_proto::ipc::{fields, ConnState, IpcItem, LocalMeta};
use unlatch_proto::{BaseVersion, ErrorCode, ItemId, Kind};

pub type Errno = i32;

/// `FOPEN_NOFLUSH` (FUSE 7.35); fuser 0.15 only knows the ABI up to 7.31.
pub const FOPEN_NOFLUSH: u32 = 1 << 5;
pub type OpResult<T> = Result<T, Errno>;

/// Lock ignoring poisoning: a panicked worker must not wedge the whole mount.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone, Debug)]
pub struct FsOptions {
    /// Entry/attr TTL handed to the kernel (safe because of push invalidation).
    pub ttl: Duration,
    /// Directory listings count as viewer requests (engine prefetches small files).
    pub prefetch: bool,
    pub workers: usize,
    /// Scratch files for writes. Same volume as the engine cache for cheap clones.
    pub scratch_dir: PathBuf,
    /// Content of uploads that failed for good is moved here instead of being dropped.
    pub unsynced_dir: PathBuf,
    pub uid: u32,
    pub gid: u32,
}

/// Work for the invalidation thread.
#[derive(Clone, Debug, PartialEq)]
pub enum Inval {
    Replica {
        ids: Vec<ItemId>,
        parents: Vec<ItemId>,
    },
    Reimport,
    /// Drop cached data + attrs of one inode (e.g. after a conflict kept the VM's content).
    Inode {
        ino: u64,
    },
    /// Drop a dentry (e.g. a create landed under another name).
    Entry {
        parent: ItemId,
        name: String,
    },
    /// Stop the invalidation thread (unmount).
    Shutdown,
}

/// Receives engine events (on an engine thread): bumps the change epoch and queues work for the
/// invalidation thread. Never blocks.
#[derive(Clone)]
pub struct EventSink {
    epoch: Arc<AtomicU64>,
    /// The engine is `Live` (its replica is complete and receiving pushes).
    live: Arc<AtomicBool>,
    tx: Sender<Inval>,
}

pub struct EventQueue {
    pub(crate) rx: Receiver<Inval>,
}

pub fn event_channel() -> (EventSink, EventQueue) {
    let (tx, rx) = channel();
    (
        EventSink {
            epoch: Arc::new(AtomicU64::new(0)),
            live: Arc::new(AtomicBool::new(false)),
            tx,
        },
        EventQueue { rx },
    )
}

impl EventSink {
    pub fn on_event(&self, ev: EngineEvent) {
        match ev {
            EngineEvent::ReplicaChanged { ids, parents } => {
                // Bump before queueing: a reply computed before this change and sent after it
                // sees a different epoch and goes out with TTL 0 (see `Shared::ttl_since`).
                self.epoch.fetch_add(1, Ordering::SeqCst);
                let _ = self.tx.send(Inval::Replica { ids, parents });
            }
            EngineEvent::Reimport { .. } => {
                self.epoch.fetch_add(1, Ordering::SeqCst);
                let _ = self.tx.send(Inval::Reimport);
            }
            EngineEvent::StatusChanged(st) => {
                debug!(state = ?st.state, "engine status");
                self.live
                    .store(matches!(st.state, ConnState::Live), Ordering::SeqCst);
            }
            EngineEvent::NeedsUser { reason, url } => {
                warn!(%reason, url = url.as_deref().unwrap_or(""), "the connection needs you (run `unlatch doctor`)")
            }
            EngineEvent::ErrorResolved => {
                debug!("connection live again");
                self.live.store(true, Ordering::SeqCst);
            }
            EngineEvent::WorkingSetChanged { .. } => {}
        }
    }

    pub fn set_live(&self, live: bool) {
        self.live.store(live, Ordering::SeqCst);
    }

    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }

    pub fn handler(&self) -> EventHandler {
        let sink = self.clone();
        Arc::new(move |ev| sink.on_event(ev))
    }

    pub(crate) fn queue(&self, inval: Inval) {
        let _ = self.tx.send(inval);
    }
}

/// Write state of one inode, shared by every handle open on it. `handles` lives outside the
/// mutex so opening/releasing never waits behind an upload in progress.
pub(crate) struct OpenFile {
    pub handles: AtomicU32,
    pub st: Mutex<FileState>,
}

impl OpenFile {
    fn new(st: FileState, handles: u32) -> Arc<OpenFile> {
        Arc::new(OpenFile {
            handles: AtomicU32::new(handles),
            st: Mutex::new(st),
        })
    }
}

pub(crate) struct FileState {
    /// `None` until a locally created file's create has been uploaded.
    pub id: Option<ItemId>,
    pub parent: ItemId,
    pub name: String,
    /// Stable across retries of the same create (the engine derives the op id from it, D1).
    pub template_id: String,
    pub create_mode: u32,
    /// Scratch copy of the content, once loaded.
    pub temp: Option<(File, PathBuf)>,
    pub dirty: bool,
    /// On-disk marker (`<scratch>.dirty`, holding the file name) while the scratch content
    /// holds writes that are not on the VM: after a crash the next start keeps that content in
    /// unsynced/ instead of clearing it with the other scratch files.
    pub marker: Option<PathBuf>,
    /// Version the scratch content is based on.
    pub base: BaseVersion,
    pub unlinked: bool,
    pub mtime_override: Option<i64>,
    pub dirty_mtime_ns: i64,
}

impl FileState {
    fn for_item(item: &IpcItem) -> FileState {
        FileState {
            id: Some(item.entry.id),
            parent: item.entry.parent,
            name: item.display_name.clone(),
            template_id: String::new(),
            create_mode: item.entry.mode,
            temp: None,
            dirty: false,
            marker: None,
            base: item.entry.version.into(),
            unlinked: false,
            mtime_override: None,
            dirty_mtime_ns: item.entry.mtime_ns,
        }
    }

    fn temp_len(&self) -> u64 {
        self.temp
            .as_ref()
            .and_then(|(f, _)| f.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0)
    }

    fn drop_temp(&mut self) {
        if let Some((_, path)) = self.temp.take() {
            let _ = std::fs::remove_file(path);
        }
        self.unmark();
    }

    fn unmark(&mut self) {
        if let Some(m) = self.marker.take() {
            let _ = std::fs::remove_file(m);
        }
    }

    /// The content is on the VM (or deliberately dropped, or preserved elsewhere).
    fn clear_dirty(&mut self) {
        self.dirty = false;
        self.unmark();
    }
}

impl Drop for FileState {
    fn drop(&mut self) {
        self.drop_temp();
    }
}

pub(crate) struct Handle {
    /// Present for handles that take part in the inode's write state.
    pub file: Option<Arc<OpenFile>>,
    /// Content of a blocked symlink (served locally; the engine exposes it as a read-only file).
    pub blocked_target: Option<Arc<Vec<u8>>>,
}

pub(crate) struct DirListing {
    pub items: Vec<IpcItem>,
    pub epoch: u64,
}

pub struct Shared<B: Backend> {
    pub(crate) backend: Arc<B>,
    pub(crate) opts: FsOptions,
    pub(crate) inodes: Mutex<InodeMap>,
    pub(crate) files: Mutex<HashMap<u64, Arc<OpenFile>>>,
    /// Local creates not uploaded yet, by `(parent, name)` → inode.
    pending: Mutex<HashMap<(ItemId, String), u64>>,
    pub(crate) handles: Mutex<HashMap<u64, Handle>>,
    pub(crate) dirs: Mutex<HashMap<u64, Option<Arc<DirListing>>>>,
    next_fh: AtomicU64,
    scratch_seq: AtomicU64,
    pub(crate) sink: EventSink,
    /// Set once the kernel has passed a permission check on the root (see `plan`).
    pub(crate) root_ready: AtomicBool,
    /// The mount is going away: new writes are refused (never acknowledged and then lost)
    /// while [`Shared::stop_and_flush`] uploads what was acknowledged.
    stopping: AtomicBool,
}

/// What an inode refers to.
enum Target {
    Item(ItemId),
    Pending(Arc<OpenFile>),
}

/// Suffix of the dirty marker next to a scratch file (see [`FileState::marker`]).
const DIRTY_MARKER: &str = ".dirty";

fn random_template_id() -> String {
    format!("fuse-{:032x}", rand::random::<u128>())
}

fn name_str(name: &std::ffi::OsStr) -> OpResult<&str> {
    // Non-UTF-8 names are never exposed (DESIGN §8) and cannot be created.
    let s = name.to_str().ok_or(libc::EINVAL)?;
    if !unlatch_proto::valid_name(s) {
        return Err(libc::EINVAL);
    }
    Ok(s)
}

impl<B: Backend> Shared<B> {
    pub fn new(
        backend: Arc<B>,
        opts: FsOptions,
        sink: EventSink,
    ) -> std::io::Result<Arc<Shared<B>>> {
        std::fs::create_dir_all(&opts.scratch_dir)?;
        recover_scratch(&opts.scratch_dir, &opts.unsynced_dir);
        Ok(Arc::new(Shared {
            backend,
            opts,
            inodes: Mutex::new(InodeMap::new()),
            files: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            handles: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            scratch_seq: AtomicU64::new(0),
            sink,
            root_ready: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
        }))
    }

    /// Unmount path (signal, external `fusermount -u`, drop): refuse new writes from now on,
    /// then upload every open file's acknowledged-but-unsynced content; what cannot be
    /// uploaded goes to unsynced/. Nothing write(2) acknowledged is dropped.
    pub fn stop_and_flush(&self) {
        let files: Vec<(u64, Arc<OpenFile>)> = {
            // The flag flips under the `files` lock: a create racing this either registers
            // its file before the snapshot below or sees the flag (see `op_create`).
            let files = lock(&self.files);
            self.stopping.store(true, Ordering::SeqCst);
            files.iter().map(|(k, v)| (*k, Arc::clone(v))).collect()
        };
        for (ino, of) in files {
            // Each write marks the state dirty under this lock after checking the flag, so
            // once we hold it no further write to this file can be acknowledged.
            let mut st = lock(&of.st);
            if !st.dirty {
                continue;
            }
            match self.upload(ino, &mut st) {
                Ok(()) => debug!(file = %st.name, "uploaded at unmount"),
                Err(e) => {
                    warn!(file = %st.name, errno = e, "upload at unmount failed");
                    self.preserve_unsynced(&mut st);
                }
            }
        }
    }

    /// Mark scratch content as holding writes not yet on the VM. Refused once stopping.
    fn mark_dirty(&self, st: &mut FileState) -> OpResult<()> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(libc::EIO);
        }
        if st.marker.is_none() {
            if let Some((_, path)) = &st.temp {
                let mut m = path.clone().into_os_string();
                m.push(DIRTY_MARKER);
                let m = PathBuf::from(m);
                match std::fs::write(&m, st.name.as_bytes()) {
                    Ok(()) => st.marker = Some(m),
                    // Best effort: only crash recovery depends on it.
                    Err(e) => warn!(file = %st.name, error = %e, "cannot write dirty marker"),
                }
            }
        }
        st.dirty = true;
        Ok(())
    }

    pub fn epoch(&self) -> u64 {
        self.sink.epoch.load(Ordering::SeqCst)
    }

    /// The configured TTL if nothing changed since `e0`, else zero (the reply may describe a
    /// state that an invalidation already raced past).
    pub fn ttl_since(&self, e0: u64) -> Duration {
        if self.epoch() == e0 {
            self.opts.ttl
        } else {
            Duration::ZERO
        }
    }

    pub fn generation(&self) -> u64 {
        lock(&self.inodes).generation()
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    fn target(&self, ino: u64) -> OpResult<Target> {
        let id = {
            let inodes = lock(&self.inodes);
            match inodes.node(ino) {
                None => return Err(libc::ESTALE),
                Some(n) => n.id,
            }
        };
        match id {
            Some(id) => Ok(Target::Item(id)),
            None => self.open_file(ino).map(Target::Pending).ok_or(libc::ESTALE),
        }
    }

    fn dir_id(&self, ino: u64) -> OpResult<ItemId> {
        match self.target(ino)? {
            Target::Item(id) => Ok(id),
            Target::Pending(_) => Err(libc::ENOTDIR),
        }
    }

    fn item_id(&self, ino: u64) -> OpResult<ItemId> {
        match self.target(ino)? {
            Target::Item(id) => Ok(id),
            Target::Pending(_) => Err(libc::EIO),
        }
    }

    pub(crate) fn open_file(&self, ino: u64) -> Option<Arc<OpenFile>> {
        lock(&self.files).get(&ino).cloned()
    }

    /// Whether `getattr` must look at write state that an upload may be holding.
    pub(crate) fn has_open_file(&self, ino: u64) -> bool {
        lock(&self.files).contains_key(&ino)
    }

    /// Could flushing/releasing `fh` need the network? (Dirty state, or an upload in progress
    /// on the same inode that we must not wait for on the request loop.)
    pub(crate) fn handle_may_upload(&self, fh: u64) -> bool {
        let file = lock(&self.handles).get(&fh).and_then(|h| h.file.clone());
        match file {
            None => false,
            Some(of) => match of.st.try_lock() {
                Ok(st) => st.dirty,
                Err(_) => true,
            },
        }
    }

    /// A pending (not yet uploaded) local create at `(parent, name)`.
    fn pending_at(&self, parent: ItemId, name: &str) -> Option<(u64, Arc<OpenFile>)> {
        let ino = *lock(&self.pending).get(&(parent, name.to_string()))?;
        self.open_file(ino).map(|of| (ino, of))
    }

    fn forget_pending(&self, parent: ItemId, name: &str) {
        lock(&self.pending).remove(&(parent, name.to_string()));
    }

    fn attr_for_item(&self, ino: u64, item: &IpcItem) -> FileAttr {
        attr_of(ino, item, self.opts.uid, self.opts.gid)
    }

    /// Attributes of `ino`, preferring local dirty/pending state.
    fn attr_with_local(
        &self,
        ino: u64,
        item: Option<&IpcItem>,
        st: Option<&FileState>,
    ) -> OpResult<FileAttr> {
        match (item, st) {
            (_, Some(st)) if st.id.is_none() => Ok(pending_attr(
                ino,
                st.temp_len(),
                st.create_mode,
                st.mtime_override.unwrap_or(st.dirty_mtime_ns),
                self.opts.uid,
                self.opts.gid,
            )),
            (Some(item), Some(st)) if st.dirty => {
                let mut a = self.attr_for_item(ino, item);
                a.size = st.temp_len();
                a.blocks = a.size.div_ceil(512);
                a.mtime = attr::time_of_ns(st.mtime_override.unwrap_or(st.dirty_mtime_ns));
                a.ctime = a.mtime;
                Ok(a)
            }
            (Some(item), _) => Ok(self.attr_for_item(ino, item)),
            (None, _) => Err(libc::ESTALE),
        }
    }

    fn new_scratch(&self, ino: u64) -> OpResult<(File, PathBuf)> {
        let n = self.scratch_seq.fetch_add(1, Ordering::Relaxed);
        let path = self
            .opts
            .scratch_dir
            .join(format!("w-{ino}-{n}-{:08x}", rand::random::<u32>()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io_errno)?;
        Ok((f, path))
    }

    /// Make sure the state has scratch content: empty when `truncate`, else the current VM
    /// content (and then the base is the version actually fetched).
    fn ensure_loaded(&self, ino: u64, st: &mut FileState, truncate: bool) -> OpResult<()> {
        if truncate {
            st.drop_temp();
            st.temp = Some(self.new_scratch(ino)?);
            return Ok(());
        }
        if st.temp.is_some() {
            return Ok(());
        }
        match st.id {
            None => st.temp = Some(self.new_scratch(ino)?),
            Some(id) => {
                let fetched = self
                    .backend
                    .fetch(id, &self.opts.scratch_dir)
                    .map_err(|e| errno(&e))?;
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&fetched.path)
                    .map_err(io_errno)?;
                st.base = fetched.item.entry.version.into();
                st.temp = Some((f, fetched.path));
            }
        }
        Ok(())
    }

    /// Upload dirty content (create for pending files, modify with the loaded base otherwise).
    fn upload(&self, ino: u64, st: &mut FileState) -> OpResult<()> {
        if !st.dirty {
            return Ok(());
        }
        if st.unlinked {
            st.clear_dirty();
            return Ok(());
        }
        let content = {
            let (f, _) = st.temp.as_ref().ok_or(libc::EIO)?;
            let mut c = f.try_clone().map_err(io_errno)?;
            c.seek(SeekFrom::Start(0)).map_err(io_errno)?;
            c
        };
        let mut changed = fields::CONTENTS;
        if st.mtime_override.is_some() {
            changed |= fields::CONTENT_MODIFICATION_DATE;
        }
        let m: Modified = match st.id {
            None => {
                let req = CreateRequest {
                    template_id: st.template_id.clone(),
                    parent: st.parent,
                    name: st.name.clone(),
                    kind: CreateKind::File,
                    content: Some(content),
                    symlink_target: None,
                    mtime_ns: st.mtime_override,
                    user_exec: (st.create_mode & 0o100 != 0).then_some(true),
                    changed_fields: changed,
                    local: LocalMeta::default(),
                    may_already_exist: false,
                    deletion_conflicted: false,
                };
                let m = self.backend.create(req).map_err(|e| errno(&e))?;
                let id = m.item.entry.id;
                st.id = Some(id);
                self.forget_pending(st.parent, &st.name);
                lock(&self.inodes).bind(ino, id, m.item.entry.parent, &m.item.display_name);
                if m.item.display_name != st.name {
                    // Something else took the name on the VM meanwhile; the engine created ours
                    // as `name 2.ext` (creates never fail, D6). Drop our stale dentry.
                    warn!(wanted = %st.name, got = %m.item.display_name, "create collided on the VM; saved under another name");
                    self.sink.queue(Inval::Entry {
                        parent: st.parent,
                        name: st.name.clone(),
                    });
                    st.name = m.item.display_name.clone();
                }
                m
            }
            Some(id) => {
                let req = ModifyRequest {
                    changed_fields: changed,
                    content: Some(content),
                    mtime_ns: st.mtime_override,
                    ..Default::default()
                };
                self.backend
                    .modify(id, st.base, req)
                    .map_err(|e| errno(&e))?
            }
        };
        st.clear_dirty();
        st.mtime_override = None;
        st.base = m.item.entry.version.into();
        let conflict = m.should_fetch_content || m.conflict_copy.is_some();
        if conflict {
            // The VM changed underneath us: the engine kept the VM's content and stored ours as
            // a conflict copy (D6). The scratch copy is stale now; reload on the next write.
            warn!(
                file = %st.name,
                copy = m.conflict_copy.as_ref().map(|c| c.display_name.as_str()).unwrap_or("?"),
                "edit conflicted with a change on the VM; your version was saved as a conflict copy"
            );
            st.drop_temp();
            self.sink.queue(Inval::Inode { ino });
        }
        if let Some(node) = lock(&self.inodes).node_mut(ino) {
            node.cached_content = (!conflict).then_some(m.item.entry.version.content);
        }
        Ok(())
    }

    /// Keep content that could not be uploaded (a client write is never silently lost).
    fn preserve_unsynced(&self, st: &mut FileState) {
        if let Some((_, path)) = st.temp.take() {
            let dest = self
                .opts
                .unsynced_dir
                .join(format!("{}-{}", now_ns(), st.name));
            let saved = std::fs::create_dir_all(&self.opts.unsynced_dir)
                .and_then(|_| std::fs::rename(&path, &dest));
            match saved {
                Ok(()) => {
                    error!(file = %st.name, kept = %dest.display(), "upload failed; content preserved")
                }
                Err(e) => {
                    error!(file = %st.name, error = %e, "upload failed and content could not be preserved")
                }
            }
        }
        st.clear_dirty();
    }

    // ---- operations -------------------------------------------------------------------------

    /// The kernel only sends lookups/opendir under the root after its permission check on the
    /// root passed, i.e. after it applied real root attributes.
    fn note_root_used(&self, ino: u64) {
        if ino == ROOT_INO {
            self.root_ready.store(true, Ordering::SeqCst);
        }
    }

    pub fn op_lookup(&self, parent: u64, name: &std::ffi::OsStr) -> OpResult<(FileAttr, Duration)> {
        self.note_root_used(parent);
        let e0 = self.epoch();
        let pid = self.dir_id(parent)?;
        let name = name.to_str().ok_or(libc::ENOENT)?;
        if let Some((ino, of)) = self.pending_at(pid, name) {
            let a = {
                let st = lock(&of.st);
                self.attr_with_local(ino, None, Some(&st))?
            };
            if let Some(n) = lock(&self.inodes).node_mut(ino) {
                n.nlookup += 1;
            }
            return Ok((a, Duration::ZERO));
        }
        match self.backend.lookup(pid, name) {
            Ok(item) => {
                let ino = lock(&self.inodes).remember(item.entry.id, pid, &item.display_name);
                match self.open_file(ino) {
                    Some(of) => {
                        let st = lock(&of.st);
                        Ok((
                            self.attr_with_local(ino, Some(&item), Some(&st))?,
                            Duration::ZERO,
                        ))
                    }
                    None => Ok((self.attr_for_item(ino, &item), self.ttl_since(e0))),
                }
            }
            Err(e) if e.code == ErrorCode::NotFound => {
                // Negative entry (ino 0) cached for the TTL, unless a change raced us — or the
                // replica is not complete yet (initial sync, offline), when "not found" may
                // only mean "not received yet".
                let ttl = if self.sink.is_live() {
                    self.ttl_since(e0)
                } else {
                    Duration::ZERO
                };
                if ttl.is_zero() {
                    Err(libc::ENOENT)
                } else {
                    Ok((attr::negative_attr(), ttl))
                }
            }
            Err(e) => Err(errno(&e)),
        }
    }

    pub fn op_forget(&self, ino: u64, n: u64) {
        lock(&self.inodes).forget(ino, n);
    }

    pub fn op_getattr(&self, ino: u64) -> OpResult<(FileAttr, Duration)> {
        let e0 = self.epoch();
        match self.target(ino)? {
            Target::Pending(of) => {
                let st = lock(&of.st);
                Ok((self.attr_with_local(ino, None, Some(&st))?, Duration::ZERO))
            }
            Target::Item(id) => {
                let item = match self.backend.item(id) {
                    Ok(item) => item,
                    // Before the first sync the replica has no root yet. ENOENT on the root
                    // would make the mount point itself vanish; show an empty directory until
                    // the snapshot lands (and do not let the kernel cache it).
                    Err(e) if ino == ROOT_INO && e.code == ErrorCode::NotFound => {
                        return Ok((self.placeholder_root_attr(), Duration::ZERO));
                    }
                    Err(e) => return Err(errno(&e)),
                };
                match self.open_file(ino) {
                    Some(of) => {
                        let st = lock(&of.st);
                        let ttl = if st.dirty {
                            Duration::ZERO
                        } else {
                            self.ttl_since(e0)
                        };
                        Ok((self.attr_with_local(ino, Some(&item), Some(&st))?, ttl))
                    }
                    None => Ok((self.attr_for_item(ino, &item), self.ttl_since(e0))),
                }
            }
        }
    }

    pub fn op_readlink(&self, ino: u64) -> OpResult<Vec<u8>> {
        let id = self.item_id(ino)?;
        let item = self.backend.item(id).map_err(|e| errno(&e))?;
        match (&item.entry.kind, &item.entry.symlink_target) {
            (Kind::Symlink, Some(t)) if !item.symlink_blocked => Ok(t.as_bytes().to_vec()),
            _ => Err(libc::EINVAL),
        }
    }

    /// Returns `(fh, open flags)`.
    pub fn op_open(&self, ino: u64, flags: i32) -> OpResult<(u64, u32)> {
        let write = flags & libc::O_ACCMODE != libc::O_RDONLY;
        let trunc = flags & libc::O_TRUNC != 0;
        match self.target(ino)? {
            Target::Pending(of) => {
                of.handles.fetch_add(1, Ordering::SeqCst);
                if trunc {
                    let mut st = lock(&of.st);
                    if let Err(e) = self
                        .ensure_loaded(ino, &mut st, true)
                        .and_then(|()| self.mark_dirty(&mut st))
                    {
                        of.handles.fetch_sub(1, Ordering::SeqCst);
                        return Err(e);
                    }
                    st.dirty_mtime_ns = now_ns();
                }
                let fh = self.alloc_fh();
                lock(&self.inodes).open_inc(ino);
                lock(&self.handles).insert(
                    fh,
                    Handle {
                        file: Some(of),
                        blocked_target: None,
                    },
                );
                Ok((fh, 0))
            }
            Target::Item(id) => {
                let item = self.backend.item(id).map_err(|e| errno(&e))?;
                match item.entry.kind {
                    Kind::Dir => return Err(libc::EISDIR),
                    Kind::Symlink if !item.symlink_blocked => return Err(libc::ELOOP),
                    _ => {}
                }
                let blocked_target = item.symlink_blocked.then(|| {
                    Arc::new(
                        item.entry
                            .symlink_target
                            .clone()
                            .unwrap_or_default()
                            .into_bytes(),
                    )
                });
                if write && blocked_target.is_some() {
                    return Err(libc::EACCES);
                }
                let file = if write {
                    let of = {
                        let mut files = lock(&self.files);
                        let of = files
                            .entry(ino)
                            .or_insert_with(|| OpenFile::new(FileState::for_item(&item), 0));
                        of.handles.fetch_add(1, Ordering::SeqCst);
                        Arc::clone(of)
                    };
                    if trunc {
                        let mut st = lock(&of.st);
                        if let Err(e) = self
                            .ensure_loaded(ino, &mut st, true)
                            .and_then(|()| self.mark_dirty(&mut st))
                        {
                            drop(st);
                            self.release_file(ino, &of);
                            return Err(e);
                        }
                        // Truncating open: the base is what we saw at open (D2).
                        st.base = item.entry.version.into();
                        st.dirty_mtime_ns = now_ns();
                    }
                    Some(of)
                } else {
                    None
                };
                // A read-only descriptor has nothing to upload at close: FOPEN_NOFLUSH (kernel
                // 5.18+, ignored by older ones) spares close() a synchronous FLUSH round trip.
                let mut open_flags = if file.is_none() { FOPEN_NOFLUSH } else { 0 };
                {
                    let mut inodes = lock(&self.inodes);
                    inodes.open_inc(ino);
                    if let Some(node) = inodes.node_mut(ino) {
                        // Pages cached from an earlier open are still valid iff the content
                        // version did not move (changes in between were invalidated by push).
                        if node.cached_content == Some(item.entry.version.content) {
                            open_flags |= fuser::consts::FOPEN_KEEP_CACHE;
                        }
                        node.cached_content = Some(item.entry.version.content);
                    }
                }
                let fh = self.alloc_fh();
                lock(&self.handles).insert(
                    fh,
                    Handle {
                        file,
                        blocked_target,
                    },
                );
                Ok((fh, open_flags))
            }
        }
    }

    pub fn op_read(&self, ino: u64, fh: u64, offset: i64, size: u32) -> OpResult<Vec<u8>> {
        let offset = u64::try_from(offset).map_err(|_| libc::EINVAL)?;
        let blocked = lock(&self.handles)
            .get(&fh)
            .and_then(|h| h.blocked_target.clone());
        if let Some(t) = blocked {
            let start = usize::try_from(offset).unwrap_or(usize::MAX).min(t.len());
            let end = start.saturating_add(size as usize).min(t.len());
            return Ok(t[start..end].to_vec());
        }
        if let Some(of) = self.open_file(ino) {
            let st = lock(&of.st);
            if let Some((f, _)) = &st.temp {
                let mut buf = vec![0u8; size as usize];
                let mut filled = 0;
                while filled < buf.len() {
                    match f.read_at(&mut buf[filled..], offset + filled as u64) {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(io_errno(e)),
                    }
                }
                buf.truncate(filled);
                return Ok(buf);
            }
            if st.id.is_none() {
                return Ok(Vec::new());
            }
        }
        let id = self.item_id(ino)?;
        self.backend.read(id, offset, size).map_err(|e| errno(&e))
    }

    pub fn op_write(&self, ino: u64, fh: u64, offset: i64, data: &[u8]) -> OpResult<u32> {
        let offset = u64::try_from(offset).map_err(|_| libc::EINVAL)?;
        let of = lock(&self.handles)
            .get(&fh)
            .and_then(|h| h.file.clone())
            .ok_or(libc::EBADF)?;
        let mut st = lock(&of.st);
        self.ensure_loaded(ino, &mut st, false)?;
        // Before the bytes land: once stopping, nothing more is acknowledged.
        self.mark_dirty(&mut st)?;
        let (f, _) = st.temp.as_ref().ok_or(libc::EIO)?;
        f.write_all_at(data, offset).map_err(io_errno)?;
        st.dirty_mtime_ns = now_ns();
        u32::try_from(data.len()).map_err(|_| libc::EINVAL)
    }

    /// `close()` of one descriptor: upload if dirty and report failure to the caller.
    pub fn op_flush(&self, ino: u64, fh: u64) -> OpResult<()> {
        let of = lock(&self.handles).get(&fh).and_then(|h| h.file.clone());
        match of {
            Some(of) => {
                let mut st = lock(&of.st);
                self.upload(ino, &mut st)
            }
            None => Ok(()),
        }
    }

    pub fn op_fsync(&self, ino: u64, fh: u64) -> OpResult<()> {
        self.op_flush(ino, fh)
    }

    /// Drop one handle's reference to the inode's write state; the last one removes it.
    fn release_file(&self, ino: u64, of: &Arc<OpenFile>) {
        if of.handles.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        let removed = {
            let mut files = lock(&self.files);
            let ours = files
                .get(&ino)
                .is_some_and(|cur| Arc::ptr_eq(cur, of) && cur.handles.load(Ordering::SeqCst) == 0);
            if ours {
                files.remove(&ino)
            } else {
                None
            }
        };
        if let Some(of) = removed {
            let st = lock(&of.st);
            if st.id.is_none() {
                self.forget_pending(st.parent, &st.name);
            }
        }
    }

    pub fn op_release(&self, ino: u64, fh: u64) {
        let handle = lock(&self.handles).remove(&fh);
        let Some(handle) = handle else { return };
        if let Some(of) = &handle.file {
            {
                let mut st = lock(&of.st);
                if st.dirty {
                    if let Err(e) = self.upload(ino, &mut st) {
                        warn!(file = %st.name, errno = e, "upload at release failed");
                        if of.handles.load(Ordering::SeqCst) <= 1 {
                            self.preserve_unsynced(&mut st);
                        }
                    }
                }
            }
            self.release_file(ino, of);
        }
        lock(&self.inodes).open_dec(ino);
    }

    pub fn op_create(
        &self,
        parent: u64,
        name: &std::ffi::OsStr,
        mode: u32,
        umask: u32,
    ) -> OpResult<(FileAttr, u64)> {
        let pid = self.dir_id(parent)?;
        let name = name_str(name)?;
        let ino = lock(&self.inodes).alloc_provisional(pid, name);
        let mut st = FileState {
            id: None,
            parent: pid,
            name: name.to_string(),
            template_id: random_template_id(),
            create_mode: mode & !umask & 0o7777,
            temp: None,
            dirty: false,
            marker: None,
            base: BaseVersion::default(),
            unlinked: false,
            mtime_override: None,
            dirty_mtime_ns: now_ns(),
        };
        // An empty file must still be created on the VM at close: dirty from the start.
        if let Err(e) = self
            .ensure_loaded(ino, &mut st, true)
            .and_then(|()| self.mark_dirty(&mut st))
        {
            lock(&self.inodes).forget(ino, 1);
            return Err(e);
        }
        let a = self.attr_with_local(ino, None, Some(&st))?;
        let of = OpenFile::new(st, 1);
        {
            let mut files = lock(&self.files);
            if self.stopping.load(Ordering::SeqCst) {
                // `stop_and_flush` already took its snapshot: do not acknowledge the create.
                drop(files);
                lock(&self.inodes).forget(ino, 1);
                return Err(libc::EIO);
            }
            files.insert(ino, Arc::clone(&of));
        }
        lock(&self.pending).insert((pid, name.to_string()), ino);
        lock(&self.inodes).open_inc(ino);
        let fh = self.alloc_fh();
        lock(&self.handles).insert(
            fh,
            Handle {
                file: Some(of),
                blocked_target: None,
            },
        );
        Ok((a, fh))
    }

    fn create_node(
        &self,
        pid: ItemId,
        name: &str,
        kind: CreateKind,
        target: Option<String>,
    ) -> OpResult<(FileAttr, Duration)> {
        let e0 = self.epoch();
        let req = CreateRequest {
            template_id: random_template_id(),
            parent: pid,
            name: name.to_string(),
            kind,
            content: None,
            symlink_target: target,
            mtime_ns: None,
            user_exec: None,
            changed_fields: 0,
            local: LocalMeta::default(),
            may_already_exist: false,
            deletion_conflicted: false,
        };
        let m = self.backend.create(req).map_err(|e| errno(&e))?;
        if m.item.display_name != name || m.item.entry.parent != pid {
            // The engine never fails a create; it picked another name because ours was taken
            // on the VM. For mkdir/symlink the caller asked for exactly this name: undo, EEXIST.
            let _ = self
                .backend
                .delete(m.item.entry.id, m.item.entry.version.into(), false);
            return Err(libc::EEXIST);
        }
        let ino = lock(&self.inodes).remember(m.item.entry.id, pid, name);
        Ok((self.attr_for_item(ino, &m.item), self.ttl_since(e0)))
    }

    pub fn op_mkdir(&self, parent: u64, name: &std::ffi::OsStr) -> OpResult<(FileAttr, Duration)> {
        let pid = self.dir_id(parent)?;
        let name = name_str(name)?;
        self.create_node(pid, name, CreateKind::Dir, None)
    }

    pub fn op_symlink(
        &self,
        parent: u64,
        name: &std::ffi::OsStr,
        target: &Path,
    ) -> OpResult<(FileAttr, Duration)> {
        let pid = self.dir_id(parent)?;
        let name = name_str(name)?;
        let target = target.to_str().ok_or(libc::EINVAL)?;
        if target.is_empty() || target.contains('\0') {
            return Err(libc::EINVAL);
        }
        self.create_node(pid, name, CreateKind::Symlink, Some(target.to_string()))
    }

    /// Writes still buffered for a deleted item must not resurrect it.
    fn mark_unlinked(&self, id: ItemId) {
        let ino = lock(&self.inodes).ino_of(id);
        if let Some(of) = ino.and_then(|ino| self.open_file(ino)) {
            let mut st = lock(&of.st);
            st.unlinked = true;
            st.dirty = false;
        }
    }

    pub fn op_unlink(&self, parent: u64, name: &std::ffi::OsStr) -> OpResult<()> {
        let pid = self.dir_id(parent)?;
        let name = name.to_str().ok_or(libc::ENOENT)?;
        if let Some((_, of)) = self.pending_at(pid, name) {
            // Never uploaded: nothing to delete on the VM.
            let mut st = lock(&of.st);
            st.unlinked = true;
            st.dirty = false;
            self.forget_pending(pid, name);
            return Ok(());
        }
        let item = self.backend.lookup(pid, name).map_err(|e| errno(&e))?;
        if item.entry.kind == Kind::Dir {
            return Err(libc::EISDIR);
        }
        self.backend
            .delete(item.entry.id, item.entry.version.into(), false)
            .map_err(|e| errno(&e))?;
        self.mark_unlinked(item.entry.id);
        Ok(())
    }

    fn delete_dir(&self, item: &IpcItem) -> OpResult<()> {
        match self
            .backend
            .delete(item.entry.id, item.entry.version.into(), false)
        {
            Ok(()) => Ok(()),
            Err(e) if matches!(e.code, ErrorCode::DeletionRejected | ErrorCode::NotEmpty) => {
                let non_empty = list_all(&*self.backend, item.entry.id, false)
                    .map(|c| !c.is_empty())
                    .unwrap_or(true);
                Err(if non_empty {
                    libc::ENOTEMPTY
                } else {
                    libc::EBUSY
                })
            }
            Err(e) => Err(errno(&e)),
        }
    }

    pub fn op_rmdir(&self, parent: u64, name: &std::ffi::OsStr) -> OpResult<()> {
        let pid = self.dir_id(parent)?;
        let name = name.to_str().ok_or(libc::ENOENT)?;
        let item = self.backend.lookup(pid, name).map_err(|e| errno(&e))?;
        if item.entry.kind != Kind::Dir {
            return Err(libc::ENOTDIR);
        }
        self.delete_dir(&item)
    }

    pub fn op_rename(
        &self,
        parent: u64,
        name: &std::ffi::OsStr,
        newparent: u64,
        newname: &std::ffi::OsStr,
        flags: u32,
    ) -> OpResult<()> {
        if flags & (libc::RENAME_EXCHANGE | libc::RENAME_WHITEOUT) != 0 {
            return Err(libc::EINVAL);
        }
        let noreplace = flags & libc::RENAME_NOREPLACE != 0;
        let pid = self.dir_id(parent)?;
        let npid = self.dir_id(newparent)?;
        let name = name.to_str().ok_or(libc::ENOENT)?;
        let newname = name_str(newname)?;
        // A file created here but not uploaded yet: create it first, then move it.
        if let Some((ino, of)) = self.pending_at(pid, name) {
            let mut st = lock(&of.st);
            self.upload(ino, &mut st)?;
        }
        let src = self.backend.lookup(pid, name).map_err(|e| errno(&e))?;
        match self.backend.lookup(npid, newname) {
            Ok(dst) => {
                if dst.entry.id == src.entry.id {
                    return Ok(());
                }
                if noreplace {
                    return Err(libc::EEXIST);
                }
                match (src.entry.kind, dst.entry.kind) {
                    (Kind::Dir, Kind::Dir) => self.delete_dir(&dst)?,
                    (Kind::Dir, _) => return Err(libc::ENOTDIR),
                    (_, Kind::Dir) => return Err(libc::EISDIR),
                    _ => {
                        // POSIX rename replaces the destination, but the daemon renames with
                        // RENAME_NOREPLACE (never overwrites): remove the old target first.
                        self.backend
                            .delete(dst.entry.id, dst.entry.version.into(), false)
                            .map_err(|e| errno(&e))?;
                        self.mark_unlinked(dst.entry.id);
                    }
                }
            }
            Err(e) if e.code == ErrorCode::NotFound => {}
            Err(e) => return Err(errno(&e)),
        }
        let mut changed = fields::FILENAME;
        if pid != npid {
            changed |= fields::PARENT;
        }
        let req = ModifyRequest {
            changed_fields: changed,
            new_parent: (pid != npid).then_some(npid),
            new_name: Some(newname.to_string()),
            ..Default::default()
        };
        let m = self
            .backend
            .modify(src.entry.id, src.entry.version.into(), req)
            .map_err(|e| errno(&e))?;
        if m.item.entry.parent != npid || m.item.display_name != newname {
            // Metadata ops never error in the engine: it returned the VM's current state because
            // the item moved there meanwhile (engine rule 4). Tell the caller it did not happen.
            self.sink.queue(Inval::Replica {
                ids: vec![src.entry.id],
                parents: vec![pid, npid],
            });
            return Err(libc::EBUSY);
        }
        let mut inodes = lock(&self.inodes);
        if let Some(ino) = inodes.ino_of(src.entry.id) {
            if let Some(node) = inodes.node_mut(ino) {
                node.dentry = Some((npid, newname.to_string()));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn op_setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        mtime_ns: Option<i64>,
    ) -> OpResult<(FileAttr, Duration)> {
        if uid.is_some_and(|u| u != self.opts.uid) || gid.is_some_and(|g| g != self.opts.gid) {
            return Err(libc::EPERM);
        }
        let e0 = self.epoch();
        let id = match self.target(ino)? {
            Target::Pending(of) => {
                let mut st = lock(&of.st);
                if let Some(sz) = size {
                    self.ensure_loaded(ino, &mut st, false)?;
                    let (f, _) = st.temp.as_ref().ok_or(libc::EIO)?;
                    f.set_len(sz).map_err(io_errno)?;
                    st.dirty_mtime_ns = now_ns();
                }
                if let Some(m) = mode {
                    st.create_mode = m & 0o7777;
                }
                if let Some(t) = mtime_ns {
                    st.mtime_override = Some(t);
                }
                return Ok((self.attr_with_local(ino, None, Some(&st))?, Duration::ZERO));
            }
            Target::Item(id) => id,
        };
        let item = self.backend.item(id).map_err(|e| errno(&e))?;
        if item.symlink_blocked && (size.is_some() || mode.is_some()) {
            return Err(libc::EACCES);
        }
        let mut pending_mtime = mtime_ns;
        if let Some(sz) = size {
            if item.entry.kind == Kind::Dir {
                return Err(libc::EISDIR);
            }
            let existing = self.open_file(ino);
            let transient = existing.is_none();
            let of = existing.unwrap_or_else(|| OpenFile::new(FileState::for_item(&item), 0));
            let mut st = lock(&of.st);
            self.ensure_loaded(ino, &mut st, sz == 0)?;
            if sz == 0 && transient {
                st.base = item.entry.version.into();
            }
            self.mark_dirty(&mut st)?;
            let (f, _) = st.temp.as_ref().ok_or(libc::EIO)?;
            f.set_len(sz).map_err(io_errno)?;
            st.dirty_mtime_ns = now_ns();
            if let Some(t) = pending_mtime.take() {
                st.mtime_override = Some(t);
            }
            if transient {
                // `truncate(1)` on a path: no descriptor will flush this, upload now.
                self.upload(ino, &mut st)?;
            }
        }
        if let Some(t) = pending_mtime {
            if let Some(of) = self.open_file(ino) {
                let mut st = lock(&of.st);
                if st.dirty {
                    st.mtime_override = Some(t);
                    pending_mtime = None;
                }
            }
        }
        let mut req = ModifyRequest::default();
        if let Some(t) = pending_mtime {
            req.changed_fields |= fields::CONTENT_MODIFICATION_DATE;
            req.mtime_ns = Some(t);
        }
        if let Some(m) = mode {
            if item.entry.kind == Kind::File {
                let exec = m & 0o100 != 0;
                if exec != item.user_exec {
                    req.changed_fields |= fields::FILE_SYSTEM_FLAGS;
                    req.user_exec = Some(exec);
                }
            }
            // Other permission bits are not synced (the VM keeps its mode); accepting the call
            // keeps `cp -p`, editors and git working.
        }
        let item = if req.changed_fields != 0 {
            let fresh = self.backend.item(id).map_err(|e| errno(&e))?;
            self.backend
                .modify(id, fresh.entry.version.into(), req)
                .map_err(|e| errno(&e))?
                .item
        } else if size.is_some() {
            self.backend.item(id).map_err(|e| errno(&e))?
        } else {
            item
        };
        match self.open_file(ino) {
            Some(of) => {
                let st = lock(&of.st);
                Ok((
                    self.attr_with_local(ino, Some(&item), Some(&st))?,
                    Duration::ZERO,
                ))
            }
            None => Ok((self.attr_for_item(ino, &item), self.ttl_since(e0))),
        }
    }

    pub fn op_opendir(&self, ino: u64) -> OpResult<(u64, u32)> {
        self.note_root_used(ino);
        self.dir_id(ino)?;
        let fh = self.alloc_fh();
        lock(&self.dirs).insert(fh, None);
        // The kernel keeps the listing in the dir's page cache until push invalidation drops it.
        Ok((
            fh,
            fuser::consts::FOPEN_CACHE_DIR | fuser::consts::FOPEN_KEEP_CACHE,
        ))
    }

    pub fn op_releasedir(&self, fh: u64) {
        lock(&self.dirs).remove(&fh);
    }

    /// The listing snapshot of an open directory handle, taken at the first readdir (not at
    /// opendir: when the kernel serves the listing from its cache it never asks).
    pub(crate) fn dir_listing(&self, ino: u64, fh: u64, offset: i64) -> OpResult<Arc<DirListing>> {
        if offset != 0 {
            if let Some(Some(l)) = lock(&self.dirs).get(&fh) {
                return Ok(Arc::clone(l));
            }
        }
        let id = self.dir_id(ino)?;
        let e0 = self.epoch();
        let items = list_all(&*self.backend, id, self.opts.prefetch).map_err(|e| errno(&e))?;
        let listing = Arc::new(DirListing { items, epoch: e0 });
        if let Some(slot) = lock(&self.dirs).get_mut(&fh) {
            *slot = Some(Arc::clone(&listing));
        }
        Ok(listing)
    }

    /// Parent inode for "..".
    pub(crate) fn parent_ino(&self, ino: u64) -> u64 {
        if ino == ROOT_INO {
            return ROOT_INO;
        }
        let inodes = lock(&self.inodes);
        inodes
            .node(ino)
            .and_then(|n| n.dentry.as_ref())
            .and_then(|(p, _)| inodes.ino_of(*p))
            .unwrap_or(ROOT_INO)
    }

    pub(crate) fn dir_attr(&self, ino: u64) -> OpResult<FileAttr> {
        let id = self.dir_id(ino)?;
        match self.backend.item(id) {
            Ok(item) => Ok(self.attr_for_item(ino, &item)),
            Err(e) if ino == ROOT_INO && e.code == ErrorCode::NotFound => {
                Ok(self.placeholder_root_attr())
            }
            Err(e) => Err(errno(&e)),
        }
    }

    fn placeholder_root_attr(&self) -> FileAttr {
        let mut a = pending_attr(ROOT_INO, 0, 0o755, now_ns(), self.opts.uid, self.opts.gid);
        a.kind = fuser::FileType::Directory;
        a.nlink = 2;
        a
    }
}

/// Start-up: clear scratch files of a previous run, except content its dirty marker says was
/// never uploaded (the previous run crashed or was killed); that is moved to `unsynced` as
/// `<ts>-<name>` (like an upload that failed for good), never deleted.
fn recover_scratch(scratch: &Path, unsynced: &Path) {
    let Ok(rd) = std::fs::read_dir(scratch) else {
        return;
    };
    let paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    for m in paths
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(DIRTY_MARKER))
    {
        let s = m.to_string_lossy();
        let data = PathBuf::from(&s[..s.len() - DIRTY_MARKER.len()]);
        if data.exists() {
            let name = std::fs::read_to_string(m).unwrap_or_default();
            let name = if unlatch_proto::valid_name(&name) {
                name
            } else {
                "recovered".to_string()
            };
            let dest = unsynced.join(format!("{}-{}", now_ns(), name));
            match std::fs::create_dir_all(unsynced).and_then(|_| std::fs::rename(&data, &dest)) {
                Ok(()) => {
                    error!(file = %name, kept = %dest.display(), "unsynced writes of an earlier run (it stopped before uploading them) preserved")
                }
                Err(e) => {
                    // Leave both in place rather than delete the only copy.
                    error!(file = %name, error = %e, "cannot preserve unsynced writes of an earlier run; left in {}", scratch.display());
                    continue;
                }
            }
        }
        let _ = std::fs::remove_file(m);
    }
    // Everything else is unreferenced by definition.
    if let Ok(rd) = std::fs::read_dir(scratch) {
        for e in rd.flatten() {
            let p = e.path();
            let mut m = p.clone().into_os_string();
            m.push(DIRTY_MARKER);
            if !PathBuf::from(m).exists() && !p.to_string_lossy().ends_with(DIRTY_MARKER) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

pub(crate) fn io_errno(e: std::io::Error) -> Errno {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// A live mount: the kernel session thread plus the invalidation thread.
pub struct Mounted<B: Backend> {
    session: Option<fuser::BackgroundSession>,
    inval_thread: Option<std::thread::JoinHandle<()>>,
    pub shared: Arc<Shared<B>>,
}

impl<B: Backend> Mounted<B> {
    /// `false` once the session loop ended (e.g. `fusermount3 -u` from outside).
    pub fn is_alive(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| !s.guard.is_finished())
    }

    /// Unmount (lazily if busy) and stop the invalidation thread.
    pub fn unmount(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if self.session.is_none() && self.inval_thread.is_none() {
            return;
        }
        // Dropping the session runs `fusermount3 -u -z`; its loop then ends on ENODEV — but
        // with a descriptor still open the kernel keeps the connection, and that writer's
        // later close never reaches us. So: refuse further writes, and upload (or keep in
        // unsynced/) everything already acknowledged before returning.
        drop(self.session.take());
        self.shared.stop_and_flush();
        self.shared.sink.queue(Inval::Shutdown);
        if let Some(t) = self.inval_thread.take() {
            let _ = t.join();
        }
    }
}

impl<B: Backend> Drop for Mounted<B> {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Mount `backend` at `mountpoint`. Returns once the kernel accepted the mount.
pub fn mount<B: Backend>(
    backend: Arc<B>,
    opts: FsOptions,
    sink: EventSink,
    queue: EventQueue,
    mountpoint: &Path,
    fsname: &str,
) -> std::io::Result<Mounted<B>> {
    use fuser::MountOption;
    let shared = Shared::new(backend, opts, sink)?;
    let fs = fs::UnlatchFs::new(Arc::clone(&shared))?;
    let options = [
        MountOption::FSName(fsname.to_string()),
        MountOption::Subtype("unlatch".to_string()),
        // Let the kernel enforce the permission bits we report (owner triad = VM access).
        MountOption::DefaultPermissions,
        MountOption::NoAtime,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::RW,
    ];
    let session = fuser::Session::new(fs, mountpoint, &options)?;
    let notifier = session.notifier();
    let session = session.spawn()?;
    let sh = Arc::clone(&shared);
    let inval_thread = std::thread::Builder::new()
        .name("unlatch-inval".to_string())
        .spawn(move || notify::run(sh, notifier, queue.rx))?;
    Ok(Mounted {
        session: Some(session),
        inval_thread: Some(inval_thread),
        shared,
    })
}
