//! The per-root server core: index + persistence + watcher + publication to sessions.
//!
//! One `Core` per root (the `serve` process, or the `stdio` process). All index access happens
//! under `Core::st`. The actor thread debounces inotify events, reconciles them, commits
//! (journal) and publishes `Events` to every session. Request handlers take the same lock and
//! *flush* pending events first (§2(d)1), so every decision sees the live file system.

use crate::config::Config;
use crate::index::{
    Index, NKind, Slot, Txn, CH_CONTENT, CH_META, CH_NEW, CH_OTHER, F_EXPANDED, F_NOEXPAND,
    F_POLLED, F_SCANNED,
};
use crate::outbox::{SessionShared, SnapFeed};
use crate::persist::{self, JRec, OpRec, OpTable, Origin, Store};
use crate::reconcile::{self, Batch, Env, Observed, StatResult, H_CLOSE, H_CONTENT, H_MOVED_TO};
use crate::sys::{self, Stat};
use crate::watch::{self, Watcher};
use std::collections::HashMap;
use std::collections::{HashSet, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use unlatch_proto::frame;
use unlatch_proto::wire::{Change, ServerInfo, ServerMsg, WelcomeMode};
use unlatch_proto::{Entry, ErrorCode, IndexId, ItemId, ProtoError};

/// Largest encoded size of one Events / SnapshotChunk / ListingPart frame's entries (≤ 64 KiB).
pub const FRAME_BUDGET: usize = 60 * 1024;

/// Racy-git window (D3): entries changed within this much of the last observation get a
/// content bump at the next start (2 ticks of a 100 Hz coarse clock).
const RACY_WINDOW: Duration = Duration::from_millis(20);
/// Quiet this long after a commit → journal an observation (just over the racy window).
const QUIET_MARK_AFTER: Duration = Duration::from_millis(25);
/// A failed checkpoint is retried after this, doubling per failure up to `CP_RETRY_MAX`.
const CP_RETRY_MIN: Duration = Duration::from_secs(5);
const CP_RETRY_MAX: Duration = Duration::from_secs(600);
/// `ServerInfo.warnings` prefixes of the persistence failures (replaced, never duplicated).
const WARN_CHECKPOINT: &str = "the index cannot be saved on the VM";
const WARN_HELD: &str = "changes on the VM are held back";
/// Ids/seqs a client mutation may allocate without another alloc.bin write: reserved before
/// the mutation touches the VM, so a full disk fails it cleanly instead of after the fact.
const MUTATION_HEADROOM: u64 = 4096;

pub fn perr(code: ErrorCode, msg: impl Into<String>) -> ProtoError {
    ProtoError::new(code, msg)
}

/// Map an OS error to a protocol error.
pub fn io_err(e: &io::Error, what: &str) -> ProtoError {
    let code = match e.raw_os_error() {
        Some(libc::ENOENT) => ErrorCode::NotFound,
        Some(libc::EEXIST) => ErrorCode::Exists,
        Some(libc::ENOTDIR) => ErrorCode::NotDir,
        Some(libc::EISDIR) => ErrorCode::IsDir,
        Some(libc::ENOTEMPTY) => ErrorCode::NotEmpty,
        Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::EROFS) => ErrorCode::Permission,
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => ErrorCode::NoSpace,
        Some(libc::ENAMETOOLONG) => ErrorCode::InvalidName,
        // NotFound tells the engine the *item* is gone (it deletes it on the Mac): never for
        // these. EXDEV: a move onto another filesystem (a mount point below the root), the item
        // is where it was. ELOOP: a symlink now where an indexed name was — the path changed
        // under us; the caller retries after a flush (`Ops::resolve_dir`), else it is retried.
        Some(libc::EXDEV) => ErrorCode::CannotSync,
        Some(libc::ELOOP) => ErrorCode::Io,
        _ => ErrorCode::Io,
    };
    perr(code, format!("{what}: {e}"))
}

/// A failed id/seq reservation as the client sees it: retryable (`NoSpace` or `Io`), never a
/// per-item verdict such as `Permission`.
pub fn reserve_err(e: &io::Error) -> ProtoError {
    let code = match e.raw_os_error() {
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => ErrorCode::NoSpace,
        _ => ErrorCode::Io,
    };
    perr(
        code,
        format!(
            "unlatchd cannot reserve item ids on the VM (disk full or state dir not writable): {e}"
        ),
    )
}

/// `(dev, ino)` of unlatchd's own files and directories — the state dir, the install dir
/// (binaries, every root's state) and an `UNLATCHD_LOG` file — unless one of them is the root
/// itself. With the default layout (`UNLATCH_HOME=~/.unlatch`) and the default roots (`~` on the
/// Mac's "Add VM", the current directory for `npx unlatch share`, often `~`) they sit inside
/// the served root: indexed, each journal append was an event that was committed and journaled
/// again — a loop that never went idle — and Finder showed (and could delete) index.bin and the
/// binaries. Matched by identity, so a renamed, bind-mounted or `UNLATCH_HOME`-relocated dir is
/// caught too.
///
/// The install dir is excluded whole only when it is unlatchd's own
/// ([`crate::lifecycle::dedicated_install_dir`]). A binary run from a directory of the user's
/// (`<root>/bin`, `~/.local/bin`, `~/.cargo/bin`, …) excludes only itself — the user's other
/// files there stay visible — and the default state container next to it (`<dir>/state`) when
/// the state dir in use is inside it.
fn own_dirs(state_dir: &Path, root: &Stat) -> Vec<(u64, u64)> {
    let mut paths = vec![state_dir.to_path_buf()];
    match crate::lifecycle::dedicated_install_dir() {
        Some(d) => paths.push(d),
        None => {
            // The running binary's inode, wherever it is linked.
            paths.push(PathBuf::from("/proc/self/exe"));
            let container = crate::lifecycle::install_dir().join("state");
            let id = |p: &Path| {
                use std::os::unix::fs::MetadataExt;
                std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
            };
            if state_dir
                .parent()
                .and_then(id)
                .is_some_and(|s| Some(s) == id(&container))
            {
                paths.push(container);
            }
        }
    }
    if let Some(l) = std::env::var_os("UNLATCHD_LOG") {
        paths.push(PathBuf::from(l));
    }
    let mut out = Vec::new();
    for p in paths {
        use std::os::unix::fs::MetadataExt;
        if let Ok(m) = std::fs::metadata(&p) {
            let id = (m.dev(), m.ino());
            if id != (root.dev, root.ino) && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// Every entry with seq > `since` (parents first) + tombstones > `since` (D17 Resume).
fn changes_since(idx: &Index, since: u64) -> Vec<Change> {
    let mut changes: Vec<Change> = idx
        .bfs()
        .into_iter()
        .filter(|&s| idx.node(s).seq > since && s != idx.root)
        .map(|s| Change::Upsert(idx.entry(s)))
        .collect();
    if idx.node(idx.root).seq > since {
        changes.insert(0, Change::Upsert(idx.entry(idx.root)));
    }
    for t in idx.tombs.iter().filter(|t| t.seq > since) {
        changes.push(Change::Remove {
            id: ItemId(t.id),
            seq: t.seq,
        });
    }
    changes
}

/// Upper bound of the encoded size of one change (postcard varints; conservative).
pub fn change_size(c: &Change) -> usize {
    match c {
        Change::Upsert(e) => {
            100 + e.name.len() + e.symlink_target.as_ref().map(|t| t.len()).unwrap_or(0)
        }
        Change::Remove { .. } => 24,
    }
}

pub fn entry_size(e: &Entry) -> usize {
    100 + e.name.len() + e.symlink_target.as_ref().map(|t| t.len()).unwrap_or(0)
}

/// Split one committed batch into ≤ 64 KiB `Events` frames (`batch_end` on the last).
pub fn event_frames(seq: u64, changes: Vec<Change>) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut cur: Vec<Change> = Vec::new();
    let mut size = 0;
    let total = changes.len();
    for (i, c) in changes.into_iter().enumerate() {
        let sz = change_size(&c);
        if !cur.is_empty() && size + sz > FRAME_BUDGET {
            let msg = ServerMsg::Events {
                seq,
                changes: std::mem::take(&mut cur),
                batch_end: false,
            };
            if let Ok(f) = frame::encode(&msg, true) {
                frames.push(f);
            }
            size = 0;
        }
        size += sz;
        cur.push(c);
        if i + 1 == total {
            let msg = ServerMsg::Events {
                seq,
                changes: std::mem::take(&mut cur),
                batch_end: true,
            };
            if let Ok(f) = frame::encode(&msg, true) {
                frames.push(f);
            }
        }
    }
    frames
}

/// Hot-file publish throttle (D9, §2(d)6).
#[derive(Clone, Copy)]
struct Throttle {
    last_pub: Instant,
    pending_since: Option<Instant>,
}

pub struct Ix {
    pub idx: Index,
    pub index_id: u128,
    pub origin: Origin,
    pub ops: OpTable,
    pub suspect: Vec<(u64, u64)>,
    pub observed_ns: i64,
}

pub struct State {
    pub ix: Option<Ix>,
    /// Loaded from disk but not yet adopted (adopted at the first Hello).
    loaded: Option<persist::Loaded>,
    pub store: Store,
    pub root_fd: OwnedFd,
    pub root_dev: u64,
    pub root_ino: u64,
    pub root_fsid: u64,
    pub published_seq: u64,
    throttle: HashMap<u64, Throttle>,
    pub subs: Vec<Arc<SessionShared>>,
    pub warnings: Vec<String>,
    pub polled_root: bool,
    last_listed: HashMap<u64, Instant>,
    /// Sessions that listed each expanded dir: `Unwatch` collapses only when none is left
    /// (one per-root server may serve several Macs).
    expanders: HashMap<u64, HashSet<u64>>,
    verify_pending: bool,
    pub root_replaced: bool,
    last_poll: Instant,
    poll_interval: Duration,
    last_budget: Instant,
    last_checkpoint: Instant,
    journal_dirty: bool,
    /// When to journal a quiet observation after the last one (see [`Core::mark_quiet`]).
    quiet_due: Option<Instant>,
    /// A fresh index is checkpointed by the actor, after Welcome.
    checkpoint_due: bool,
    /// Mount points below the root as last seen in /proc/self/mountinfo.
    mounts: HashSet<PathBuf>,
    /// A client `Mkdir` in progress: `(parent, name)` of the dir it creates, which every
    /// reconcile until the reply expands at once (see [`Batch::expand_new`]).
    pub expand_new: Option<(Slot, String)>,
    /// A full stat audit is owed since this instant (a batch saw a created name whose inode
    /// it could not observe; see `Core::process_events`).
    audit_due: Option<Instant>,
    /// When the last batch with inotify events ran (quiet detection for `audit_due`).
    last_events: Instant,
    /// Names that held a file inode created in the previous batch at its end, and that already
    /// had events queued when that batch had stat'ed them: the next batch treats them as
    /// created (see `Core::process_batch`).
    carried_created: Vec<(u64, String)>,
    /// Old inodes a replace exchanged out while someone else still had them open (see
    /// [`crate::ops::Parked`]); swept by the actor.
    pub parked: Vec<crate::ops::Parked>,
    /// Publication is held back since this published seq: the index holds ids/seqs above the
    /// durable reservation (alloc.bin could not be rewritten: disk full, state dir not
    /// writable). Nothing above the reservation may reach a client (a crash would hand those
    /// ids to other items); every change since is published once a reservation succeeds.
    held_since: Option<u64>,
    /// Every journal append since the hold succeeded (else the release journals the diff).
    held_journal_ok: bool,
    last_hold_try: Instant,
    /// Consecutive failed checkpoints and when to try again (exponential backoff).
    cp_failures: u32,
    cp_retry_at: Option<Instant>,
}

pub struct Core {
    pub cfg: Config,
    pub root: PathBuf,
    pub state_dir: PathBuf,
    st: Mutex<State>,
    pub watcher: Arc<Watcher>,
    pub shutdown: AtomicBool,
    pub sessions: AtomicUsize,
    pub last_client: Mutex<Instant>,
    next_session: AtomicU64,
    pub hostname: String,
    /// Set by the mountinfo watcher (POLLPRI on /proc/self/mountinfo, §2(d)7).
    mounts_changed: AtomicBool,
    /// Mirrors `State::held_since.is_some()` (lock-free check before every reply).
    held: AtomicBool,
    /// `(dev, ino)` of unlatchd's own dirs (see [`Index::excluded`]).
    excluded: Vec<(u64, u64)>,
    /// Test hook: runs in a walker thread right before it opens a directory (its root-relative
    /// path), to inject a change between reading the parent's listing and opening the child.
    #[cfg(test)]
    walk_hook: WalkHook,
}

#[cfg(test)]
type WalkHook = Mutex<Option<Box<dyn Fn(&[u8]) + Send + Sync>>>;

/// Expand `~/` with `$HOME` (never getpwuid, §2(d)11).
pub fn expand_root(root: &str) -> PathBuf {
    if let Some(rest) = root.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(rest);
        }
    }
    if root == "~" {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h);
        }
    }
    PathBuf::from(root)
}

fn root_name(root: &Path) -> String {
    root.file_name()
        .and_then(|n| n.to_str())
        .filter(|n| unlatch_proto::valid_name(n))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "root".to_string())
}

fn open_root(root: &Path) -> io::Result<(OwnedFd, Stat, sys::FsInfo)> {
    use std::os::unix::ffi::OsStrExt;
    let fd = sys::open_path(
        root.as_os_str().as_bytes(),
        libc::O_PATH | libc::O_DIRECTORY,
    )?;
    let st = sys::fstat(fd.as_raw_fd())?;
    let fs = sys::fstatfs(fd.as_raw_fd())?;
    Ok((fd, st, fs))
}

/// Serial reconcile environment: resolves dirs through the index from the root fd (openat2
/// beneath, identity-checked), watches before reading.
pub struct CoreEnv<'a> {
    pub root_fd: RawFd,
    pub watcher: &'a Watcher,
    pub polled: bool,
    fds: HashMap<Slot, OwnedFd>,
}

impl<'a> CoreEnv<'a> {
    pub fn new(root_fd: RawFd, watcher: &'a Watcher, polled: bool) -> Self {
        CoreEnv {
            root_fd,
            watcher,
            polled,
            fds: HashMap::new(),
        }
    }

    fn dir_fd(&mut self, idx: &Index, dir: Slot) -> Option<RawFd> {
        self.open_dir(idx, dir).ok()
    }

    /// Open `dir` at its indexed path. `Err(true)`: the dir is not there (it moved — the move
    /// not applied yet — or it is gone); `Err(false)`: any other failure (permissions, …).
    fn open_dir(&mut self, idx: &Index, dir: Slot) -> Result<RawFd, bool> {
        if let Some(f) = self.fds.get(&dir) {
            return Ok(f.as_raw_fd());
        }
        let rel = idx.rel_path(dir).ok_or(true)?;
        let fd =
            sys::open_beneath(self.root_fd, &rel, sys::DIR_FLAGS).map_err(|e| not_there(&e))?;
        if !reconcile::fd_matches(idx, dir, fd.as_raw_fd()) {
            return Err(true);
        }
        let raw = fd.as_raw_fd();
        self.fds.insert(dir, fd);
        Ok(raw)
    }
}

impl Env for CoreEnv<'_> {
    fn stat(&mut self, idx: &Index, dir: Slot, name: &str) -> StatResult {
        let Some(fd) = self.dir_fd(idx, dir) else {
            return StatResult::DirUnavailable;
        };
        match reconcile::stat_child(fd, name) {
            Ok(Some((st, t))) => StatResult::Present(st, t),
            Ok(None) => StatResult::Absent,
            Err(e) => {
                crate::log!("stat {name}: {e}");
                StatResult::DirUnavailable
            }
        }
    }

    fn scan(&mut self, idx: &Index, dir: Slot) -> Option<(Vec<Observed>, bool)> {
        let fd = match self.open_dir(idx, dir) {
            Ok(fd) => fd,
            Err(moved) => {
                if moved {
                    self.watcher.mark_stale(idx.node(dir).id, None);
                }
                return None;
            }
        };
        let watched = if self.polled {
            false
        } else {
            match self.watcher.add(fd, idx.node(dir).id) {
                Ok(w) => w.is_some(),
                Err(e) => {
                    crate::log!("inotify_add_watch: {e}");
                    false
                }
            }
        };
        match reconcile::read_listing(fd) {
            Ok(v) => Some((v, watched)),
            Err(e) => {
                crate::log!("readdir: {e}");
                None
            }
        }
    }

    fn dir_removed(&mut self, id: u64) {
        self.watcher.remove_dir(id);
    }

    fn dir_remapped(&mut self, from: u64, to: u64) {
        self.watcher.remap(from, to);
    }

    fn mark_stale(&mut self, id: u64, name: Option<(String, u8)>) {
        self.watcher.mark_stale(id, name);
    }

    fn take_stale(&mut self) -> Vec<(u64, Vec<(String, u8)>)> {
        self.watcher.take_stale()
    }

    fn watching(&mut self, id: u64) -> bool {
        self.polled || !self.watcher.enabled() || self.watcher.is_watched(id)
    }
}

/// `seq`: a batch's name events in order, as (name index, mask, cookie). True when a file
/// name created in the batch was moved away and its inode is at no watched name at the end of
/// the batch: the rename's MOVED_TO is missing (moved out of view) or the landing name was
/// renamed over / deleted later in the batch (renames from it are followed by cookie).
#[cfg(test)]
fn lost_created_inode(seq: &[(usize, u32, u32)]) -> bool {
    !lost_created_names(seq).is_empty()
}

#[cfg(test)]
fn lost_created_names(seq: &[(usize, u32, u32)]) -> Vec<usize> {
    created_inode_fates(seq).0
}

/// Where the file inodes created in a batch (`seq` as for [`lost_created_inode`]) are at its
/// end: `(lost, held)`. `lost`: the names (indices) a lost created inode had in the batch —
/// the created name and every name it was renamed to; empty when every created inode is
/// observed where it landed. `held`: the name each other created inode is at when the batch
/// ends — the created name itself (no later rename from it, unlink or rename over it) or the
/// name it landed on.
fn created_inode_fates(seq: &[(usize, u32, u32)]) -> (Vec<usize>, Vec<usize>) {
    let moves = sys::IN_MOVED_FROM | sys::IN_MOVED_TO | sys::IN_DELETE;
    let mut acc: HashMap<usize, u32> = HashMap::new();
    let mut moved_away: Vec<(usize, u32)> = Vec::new();
    let mut to_by_cookie: HashMap<u32, usize> = HashMap::new();
    let mut on_name: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut created: Vec<(usize, usize)> = Vec::new();
    for (p, &(k, m, c)) in seq.iter().enumerate() {
        if m & sys::IN_CREATE != 0 && m & sys::IN_ISDIR == 0 {
            created.push((p, k));
        }
        let before = acc.entry(k).or_default();
        if m & sys::IN_MOVED_FROM != 0
            && *before & sys::IN_CREATE != 0
            && (*before | m) & sys::IN_ISDIR == 0
        {
            moved_away.push((p, c));
        }
        *before |= m;
        if m & sys::IN_MOVED_TO != 0 {
            to_by_cookie.insert(c, p);
        }
        if m & moves != 0 {
            on_name.entry(k).or_default().push(p);
        }
    }
    // Follows a created inode from its MOVED_FROM at `p` (cookie `c`), collecting the names it
    // landed on; true when it is still at the last of them at the end of the batch.
    let lands = |mut p: usize, mut c: u32, names: &mut Vec<usize>| -> bool {
        for _ in 0..seq.len() {
            let Some(&q) = to_by_cookie.get(&c).filter(|&&q| q > p) else {
                return false;
            };
            let k = seq[q].0;
            names.push(k);
            let later = on_name
                .get(&k)
                .and_then(|v| v.get(v.partition_point(|&x| x <= q)));
            match later {
                None => return true,
                Some(&r) if seq[r].1 & sys::IN_MOVED_FROM != 0 => (p, c) = (r, seq[r].2),
                Some(_) => return false,
            }
        }
        false
    };
    let mut lost = Vec::new();
    let mut held = Vec::new();
    for &(p, c) in &moved_away {
        let mut names = vec![seq[p].0];
        if lands(p, c, &mut names) {
            held.extend(names.last());
        } else {
            lost.extend(names);
        }
    }
    for (p, k) in created {
        let later = on_name
            .get(&k)
            .and_then(|v| v.get(v.partition_point(|&x| x <= p)));
        if later.is_none() {
            held.push(k);
        }
    }
    lost.sort_unstable();
    lost.dedup();
    held.sort_unstable();
    held.dedup();
    (lost, held)
}

/// `(wd, cookie)` of every IN_MOVED_FROM in `events` whose IN_MOVED_TO is not in `events`.
fn unpaired_moves(events: &[sys::InotifyEvent]) -> Vec<(i32, u32)> {
    let to: HashSet<u32> = events
        .iter()
        .filter(|e| e.mask & sys::IN_MOVED_TO != 0)
        .map(|e| e.cookie)
        .collect();
    events
        .iter()
        .filter(|e| e.mask & sys::IN_MOVED_FROM != 0 && !to.contains(&e.cookie))
        .map(|e| (e.wd, e.cookie))
        .collect()
}

/// An open of an indexed path failed because nothing (or a non-directory, or a symlink) is
/// there now — the dir moved or went away — rather than for permissions or resources.
fn not_there(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP | libc::EXDEV)
    )
}

#[derive(Default)]
pub struct CommitOpts {
    pub durable: bool,
    pub op: Option<OpRec>,
    /// Do not publish to this session (it receives the same entries in its reply).
    pub exclude: Option<u64>,
    /// Bypass the hot-file throttle (mutation results, barrier flushes).
    pub no_throttle: bool,
}

impl Core {
    pub fn new(root: &Path, state_dir: &Path, cfg: Config) -> io::Result<Arc<Core>> {
        let root = std::fs::canonicalize(root)?;
        let (root_fd, rst, fs) = open_root(&root)?;
        if !rst.is_dir() {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let polled_root = cfg.force_poll || sys::is_network_fs(fs.f_type);
        let mut warnings = Vec::new();
        if polled_root {
            warnings.push(if cfg.force_poll {
                "UNLATCHD_POLL=1: every directory is polled".to_string()
            } else {
                "root is on a network/virtual filesystem: changes are detected by polling (1–30 s)"
                    .to_string()
            });
        }
        let watcher = Watcher::new(!polled_root, watch::provisional_budget(cfg.max_watches))?;
        let mut store = Store::open(state_dir)?;
        let t0 = Instant::now();
        let loaded = match store.load() {
            Ok(l) => l,
            Err(e) => {
                crate::log!("loading index: {e}; starting fresh");
                None
            }
        };
        crate::log!("index loaded in {:?}", t0.elapsed());
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let st = State {
            ix: None,
            loaded,
            store,
            root_fd,
            root_dev: rst.dev,
            root_ino: rst.ino,
            root_fsid: fs.fsid,
            published_seq: 0,
            throttle: HashMap::new(),
            subs: Vec::new(),
            warnings,
            polled_root,
            last_listed: HashMap::new(),
            expanders: HashMap::new(),
            verify_pending: false,
            root_replaced: false,
            last_poll: Instant::now(),
            poll_interval: Duration::from_secs(1),
            last_budget: Instant::now(),
            last_checkpoint: Instant::now(),
            journal_dirty: false,
            quiet_due: None,
            checkpoint_due: false,
            mounts: HashSet::new(),
            expand_new: None,
            audit_due: None,
            last_events: Instant::now(),
            carried_created: Vec::new(),
            parked: Vec::new(),
            held_since: None,
            held_journal_ok: true,
            last_hold_try: Instant::now(),
            cp_failures: 0,
            cp_retry_at: None,
        };
        let core = Arc::new(Core {
            cfg,
            root,
            state_dir: state_dir.to_path_buf(),
            st: Mutex::new(st),
            watcher,
            shutdown: AtomicBool::new(false),
            sessions: AtomicUsize::new(0),
            last_client: Mutex::new(Instant::now()),
            next_session: AtomicU64::new(1),
            hostname,
            mounts_changed: AtomicBool::new(false),
            held: AtomicBool::new(false),
            excluded: own_dirs(state_dir, &rst),
            #[cfg(test)]
            walk_hook: Mutex::new(None),
        });
        if !polled_root {
            core.watcher.compute_budget_async(core.cfg.max_watches);
        }
        let c2 = core.clone();
        std::thread::Builder::new()
            .name("actor".into())
            .spawn(move || c2.actor_loop())?;
        let weak = Arc::downgrade(&core);
        std::thread::Builder::new()
            .name("mountinfo".into())
            .spawn(move || watch_mountinfo(weak))?;
        Ok(core)
    }

    pub fn lock(&self) -> io::Result<MutexGuard<'_, State>> {
        self.st
            .lock()
            .map_err(|_| io::Error::other("core state poisoned"))
    }

    pub fn plock(&self) -> Result<MutexGuard<'_, State>, ProtoError> {
        self.st
            .lock()
            .map_err(|_| perr(ErrorCode::Io, "core state poisoned"))
    }

    pub fn new_session(&self) -> Arc<SessionShared> {
        Arc::new(SessionShared::new(
            self.next_session.fetch_add(1, Ordering::Relaxed),
        ))
    }

    fn origin_now(&self, st: &State) -> Origin {
        Origin {
            machine_id: persist::machine_id(),
            root_fsid: st.root_fsid,
            root_dev: st.root_dev,
            root_ino: st.root_ino,
            root_path: self.root.to_string_lossy().into_owned(),
        }
    }

    // ---- index lifecycle ----------------------------------------------------------------

    /// Make sure an index exists: adopt the persisted one (Welcome immediately, verify in the
    /// background) or build a fresh one (new index id, full scan).
    pub fn ensure_index(&self, st: &mut State, default_lazy: &[String]) -> io::Result<()> {
        if st.ix.is_some() {
            return Ok(());
        }
        let origin = self.origin_now(st);
        let mut lazy_names: Vec<String> = if default_lazy.is_empty() {
            unlatch_proto::DEFAULT_LAZY_NAMES
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            default_lazy.to_vec()
        };
        if let Some(l) = st.loaded.take() {
            let root_ok = l.index.root != crate::index::NIL && {
                let r = l.index.node(l.index.root);
                r.ino == st.root_ino && l.index.dev_of(r) == st.root_dev
            };
            if l.origin == origin && root_ok {
                let mut idx = l.index;
                // Persisted before the exclusion existed: the verify walk drops them.
                idx.excluded = self.excluded.clone();
                idx.gc_tombs(self.cfg.tomb_horizon_secs, self.cfg.tomb_max);
                idx.shrink();
                // Expansions survive restarts; their idle-collapse clock restarts now.
                let now = Instant::now();
                for s in idx.bfs() {
                    if idx.node(s).has(F_EXPANDED) {
                        st.last_listed.insert(idx.node(s).id, now);
                    }
                }
                st.published_seq = idx.seq;
                // Load may have given never-published throttled changes fresh seqs (above the
                // reserved block): reserve them before any is published.
                st.store.reserve(idx.next_id, idx.seq)?;
                st.ix = Some(Ix {
                    idx,
                    index_id: l.index_id,
                    origin,
                    ops: l.ops,
                    suspect: l.suspect,
                    observed_ns: l.observed_ns,
                });
                st.verify_pending = true;
                st.mounts = mounts_below(&self.root);
                self.watcher.nudge();
                crate::log!("adopted persisted index (replayed journal: {})", l.replayed);
                return Ok(());
            }
            crate::log!(
                "index origin changed ({:?} → {:?}); new index id",
                l.origin,
                origin
            );
            // The per-root lazy list is server-side config: keep it across rebuilds.
            lazy_names = l.index.lazy_names.clone();
        }
        st.store.reset()?;
        let index_id: u128 = rand::random();
        let mut idx = Index::new(lazy_names);
        idx.excluded = self.excluded.clone();
        let rst = sys::fstat(st.root_fd.as_raw_fd())?;
        // Seqs continue above anything a previous index issued (clients compare per index id,
        // but a monotone seq keeps logs readable). So do ids, and for them it matters: a client
        // still holds the old index's ids (fileproviderd reconciles a reimport by identifier,
        // and retries queued writes by identifier), so an old id must name nothing in the new
        // index — never another item (review (c)7).
        idx.seq = st.store.seq_hwm;
        idx.next_id = st.store.id_hwm.max(2);
        let root = idx.create_root(&root_name(&self.root), &rst);
        let mut ix = Ix {
            idx,
            index_id,
            origin,
            ops: OpTable::default(),
            suspect: Vec::new(),
            observed_ns: 0,
        };
        let mut batch = Batch::default();
        batch.to_scan.push_back(root);
        let t0 = Instant::now();
        self.walk(st, &mut ix, &mut batch);
        ix.idx.shrink();
        crate::log!(
            "initial scan: {} entries in {:?}; {}",
            ix.idx.len(),
            t0.elapsed(),
            ix.idx.mem_report()
        );
        st.published_seq = ix.idx.seq;
        ix.observed_ns = sys::now_ns();
        // Ids/seqs are about to be published: reserve them durably now; the (slow) checkpoint
        // runs on the actor after Welcome. A crash before it just means a new index id.
        st.store.reserve(ix.idx.next_id, ix.idx.seq)?;
        st.ix = Some(ix);
        st.checkpoint_due = true;
        st.mounts = mounts_below(&self.root);
        self.watcher.nudge();
        Ok(())
    }

    /// Parallel walker (D17: 16–64 threads) over `batch.to_scan`, merging results into the
    /// batch; then settles the batch. Holds the state lock throughout (callers own `st`).
    fn walk(&self, st: &mut State, ix: &mut Ix, batch: &mut Batch) {
        let root_fd = st.root_fd.as_raw_fd();
        let polled = st.polled_root;
        let watcher = &*self.watcher;
        let nthreads = self.cfg.walkers.max(1);
        #[cfg(test)]
        let hook = &self.walk_hook;
        type Work = (u64, Vec<u8>, u64, u64);
        type Res = (u64, Option<(Vec<Observed>, bool)>);
        let (work_tx, work_rx) = std::sync::mpsc::channel::<Work>();
        let work_rx = Mutex::new(work_rx);
        let (res_tx, res_rx) = std::sync::mpsc::channel::<Res>();
        std::thread::scope(|scope| {
            for _ in 0..nthreads {
                let res_tx = res_tx.clone();
                let work_rx = &work_rx;
                scope.spawn(move || loop {
                    let item = match work_rx.lock() {
                        Ok(rx) => rx.recv(),
                        Err(_) => return,
                    };
                    let Ok((id, rel, dev, ino)) = item else {
                        return;
                    };
                    #[cfg(test)]
                    if let Some(h) = hook.lock().ok().as_ref().and_then(|h| h.as_ref()) {
                        h(&rel);
                    }
                    let r = (|| {
                        // Not there (moved after its parent was listed, the MOVED pair not
                        // applied yet; or gone): stale, re-listed once reachable. Without
                        // this its whole subtree would go unwatched in this process.
                        let fd = match sys::open_beneath(root_fd, &rel, sys::DIR_FLAGS) {
                            Ok(fd) => fd,
                            Err(e) => {
                                if not_there(&e) {
                                    watcher.mark_stale(id, None);
                                }
                                return None;
                            }
                        };
                        let fst = sys::fstat(fd.as_raw_fd()).ok()?;
                        if fst.dev != dev || fst.ino != ino {
                            watcher.mark_stale(id, None);
                            return None;
                        }
                        let watched =
                            !polled && matches!(watcher.add(fd.as_raw_fd(), id), Ok(Some(_)));
                        let v = reconcile::read_listing_ex(fd.as_raw_fd(), true).ok()?;
                        Some((v, watched))
                    })();
                    if res_tx.send((id, r)).is_err() {
                        return;
                    }
                });
            }
            drop(res_tx);
            let mut outstanding = 0usize;
            let dispatch = |idx: &Index, batch: &mut Batch, outstanding: &mut usize| {
                while let Some(d) = batch.to_scan.pop_front() {
                    if !idx.alive(d) || idx.is_detached(d) || idx.node(d).kind() != NKind::Dir {
                        continue;
                    }
                    let Some(rel) = idx.rel_path(d) else { continue };
                    let n = idx.node(d);
                    if work_tx.send((n.id, rel, idx.dev_of(n), n.ino)).is_ok() {
                        *outstanding += 1;
                    }
                }
            };
            dispatch(&ix.idx, batch, &mut outstanding);
            while outstanding > 0 {
                let Ok((id, r)) = res_rx.recv() else { break };
                outstanding -= 1;
                if let (Some(s), Some((entries, watched))) = (ix.idx.slot_of(id), r) {
                    if !ix.idx.is_detached(s) {
                        let was = ix.idx.node(s).scanned();
                        let polled_flag = if watched { 0 } else { F_POLLED };
                        ix.idx.set_flags(
                            s,
                            F_SCANNED | polled_flag,
                            if watched { F_POLLED } else { 0 },
                        );
                        if !was {
                            ix.idx.touch_other(s, &mut batch.txn);
                        }
                        batch.apply_listing(&mut ix.idx, s, &entries);
                    }
                }
                dispatch(&ix.idx, batch, &mut outstanding);
            }
            drop(work_tx);
        });
        let mut env = CoreEnv::new(root_fd, watcher, polled);
        batch.settle(&mut ix.idx, &mut env);
    }

    /// Startup verification walk of an adopted index (D17): every scanned dir is re-read in
    /// parallel and differences are published as ordinary Events. Racy-git rule (D3): entries
    /// whose mtime/ctime is within 2 ticks of the moment the previous process stopped watching
    /// (clean stop) or last journaled an observation (crash) get a content bump: a same-size
    /// rewrite in that tick would be invisible in the stat tuple.
    fn verify(&self, st: &mut State) {
        let Some(mut ix) = st.ix.take() else { return };
        let t0 = Instant::now();
        let mut batch = Batch::default();
        batch.rescan_existing = true;
        batch.restart_mode = true;
        if ix.observed_ns > 0 {
            batch.racy_after_ns = Some(ix.observed_ns - RACY_WINDOW.as_nanos() as i64);
        }
        // Re-apply the root's own stat first.
        if let Ok(rst) = sys::fstat(st.root_fd.as_raw_fd()) {
            let r = ix.idx.root;
            ix.idx
                .update_stat(r, &rst, None, false, true, &mut batch.txn);
        }
        batch.to_scan.push_back(ix.idx.root);
        self.walk(st, &mut ix, &mut batch);
        ix.observed_ns = sys::now_ns();
        crate::log!(
            "verify: {} entries, {} changes, {} removals in {:?}",
            ix.idx.len(),
            batch.txn.slots().len(),
            batch.txn.removed.len(),
            t0.elapsed()
        );
        let txn = std::mem::take(&mut batch.txn);
        st.ix = Some(ix);
        if let Err(e) = self.commit(st, txn, CommitOpts::default()) {
            crate::log!("verify commit: {e}");
        }
        sys::trim_heap();
        crate::log!(
            "after verify: heap (in use, free) {:?}; {}",
            sys::heap_stats(),
            st.ix
                .as_ref()
                .map(|ix| ix.idx.mem_report())
                .unwrap_or_default()
        );
    }

    // ---- commit & publish -------------------------------------------------------------

    /// Journal a txn and publish its changes (throttled unless `no_throttle`).
    pub fn commit(&self, st: &mut State, mut txn: Txn, opts: CommitOpts) -> io::Result<()> {
        let now = Instant::now();
        let Some(ix) = st.ix.as_mut() else {
            return Ok(());
        };
        let empty = txn.is_empty();
        if empty && opts.op.is_none() {
            ix.idx.finish_txn(&mut txn);
            return Ok(());
        }
        // Throttle decision per content-only file change.
        let mut deferred: HashSet<Slot> = HashSet::new();
        if !opts.no_throttle {
            for &s in txn.slots() {
                if !ix.idx.alive(s) {
                    continue;
                }
                let k = txn.kind_of(s);
                let n = ix.idx.node(s);
                if n.kind() != NKind::File
                    || k & (CH_NEW | CH_META | CH_OTHER) != 0
                    || k & CH_CONTENT == 0
                {
                    continue;
                }
                let closed = txn.closed.contains(&s);
                let t = st.throttle.entry(n.id).or_insert(Throttle {
                    last_pub: now.checked_sub(Duration::from_secs(10)).unwrap_or(now),
                    pending_since: None,
                });
                if closed || now.duration_since(t.last_pub) >= self.cfg.hot_interval {
                    t.last_pub = now;
                    t.pending_since = None;
                } else {
                    t.pending_since = Some(now);
                    deferred.insert(s);
                }
            }
        }
        let changes = ix.idx.changes(&txn, |s, _| !deferred.contains(&s));
        let mut recs: Vec<JRec> = ix.idx.pnodes(&txn).into_iter().map(JRec::Node).collect();
        for &s in &deferred {
            recs.push(JRec::Deferred(ix.idx.node(s).id));
        }
        let tnow = sys::now_secs();
        for &(id, seq) in &txn.removed {
            recs.push(JRec::Remove {
                id,
                seq,
                tomb: true,
                time: tnow,
            });
        }
        for &(id, seq) in &txn.implicit_tombs {
            recs.push(JRec::Remove {
                id,
                seq,
                tomb: true,
                time: tnow,
            });
        }
        for &id in &txn.dropped {
            recs.push(JRec::Remove {
                id,
                seq: 0,
                tomb: false,
                time: tnow,
            });
        }
        if !changes.is_empty() && ix.idx.seq <= st.published_seq {
            ix.idx.bump();
        }
        recs.push(JRec::Seq(ix.idx.seq));
        if let Some(op) = &opts.op {
            recs.push(JRec::Op(op.clone()));
            ix.ops.insert(op.clone());
        }
        ix.observed_ns = sys::now_ns();
        recs.push(JRec::Observed(ix.observed_ns));
        st.quiet_due = Some(Instant::now() + QUIET_MARK_AFTER);
        let seq = ix.idx.seq;
        // Ids/seqs may only be published once their hwm is durable (D4). A failed journal
        // append (disk full) still publishes: the in-memory index is the truth and the startup
        // verify walk repairs a stale checkpoint; only the op record's durability is reported.
        // A failed reservation publishes nothing (see `State::held_since`); the journal append
        // is still attempted: replay never reissues an id it has seen (`next_id` follows them).
        let reserved = st.store.reserve(ix.idx.next_id, seq);
        let appended = st.store.append(&recs, opts.durable);
        st.journal_dirty = true;
        ix.idx.finish_txn(&mut txn);
        ix.idx.note_seq_time();
        if ix.idx.tombs.len() > self.cfg.tomb_max.saturating_add(self.cfg.tomb_max / 8) {
            ix.idx
                .gc_tombs(self.cfg.tomb_horizon_secs, self.cfg.tomb_max);
        }
        if let Err(e) = reserved {
            self.hold(st, &e, appended.is_ok());
            return Err(e);
        }
        if let Err(e) = &appended {
            crate::log!("journal append: {e}");
        }
        if let Some(since) = st.held_since {
            // This commit's changes are above `since` too: the release publishes them.
            self.release_held(st, since)?;
        } else if !changes.is_empty() {
            st.published_seq = seq;
            let frames = event_frames(seq, changes.clone());
            for s in &st.subs {
                if Some(s.id) != opts.exclude {
                    s.publish(&frames, &changes);
                }
            }
        }
        if opts.op.is_some() {
            appended?;
        }
        Ok(())
    }

    /// Publish throttled entries that are due (or all of them with `force`).
    fn publish_throttled(&self, st: &mut State, force: bool) {
        let now = Instant::now();
        let mut due: Vec<u64> = Vec::new();
        st.throttle.retain(|&id, t| {
            if let Some(since) = t.pending_since {
                if force
                    || now.duration_since(since) >= self.cfg.hot_quiet
                    || now.duration_since(t.last_pub) >= self.cfg.hot_interval
                {
                    due.push(id);
                    t.pending_since = None;
                    t.last_pub = now;
                }
                true
            } else {
                now.duration_since(t.last_pub) < Duration::from_secs(5)
            }
        });
        if due.is_empty() {
            return;
        }
        let Some(ix) = st.ix.as_mut() else { return };
        let mut recs: Vec<JRec> = due.iter().map(|&id| JRec::Published(id)).collect();
        // A held-back entry's seq sits below seqs published since. Publish it under a fresh
        // seq: a client that is not connected right now (link cut, reconnecting) resumes from
        // the last seq it applied and must still get it (seeds 170, 134, 159).
        let mut changes: Vec<Change> = Vec::new();
        for &id in &due {
            if let Some(s) = ix.idx.slot_of(id) {
                let seq = ix.idx.bump();
                ix.idx.node_mut(s).seq = seq;
                recs.push(JRec::Node(ix.idx.pnode(s)));
                changes.push(Change::Upsert(ix.idx.entry(s)));
            }
        }
        if changes.is_empty() {
            let _ = st.store.append(&recs, false);
            return;
        }
        let reserved = st.store.reserve(ix.idx.next_id, ix.idx.seq);
        recs.push(JRec::Seq(ix.idx.seq));
        // The journal says these went out before any client can see them go out.
        let appended = st.store.append(&recs, false);
        if let Err(e) = reserved {
            self.hold(st, &e, appended.is_ok());
            return;
        }
        if let Some(since) = st.held_since {
            let _ = self.release_held(st, since);
            return;
        }
        let seq = ix.idx.seq;
        st.published_seq = seq;
        let frames = event_frames(seq, changes.clone());
        for s in &st.subs {
            s.publish(&frames, &changes);
        }
    }

    // ---- watcher → index ---------------------------------------------------------------

    /// Drain inotify and reconcile everything pending (§2(d)1 "flush"). `force`: also publish
    /// every throttled entry (barriers: Ping, mutations).
    ///
    /// A startup verify walk still pending runs first: until it has, the adopted index may be
    /// stale and nothing is watched yet, so no decision (and no Ping barrier) may rely on it.
    /// Only `Hello` skips it (Welcome goes out from the persisted index at once, D17).
    pub fn flush(&self, st: &mut State, force: bool) {
        if st.verify_pending {
            st.verify_pending = false;
            self.verify(st);
        }
        self.process_events(st);
        if self.watcher.has_stale() {
            self.retry_stale(st);
        }
        if force {
            self.audit_if_due(st, true);
            self.publish_throttled(st, true);
        }
    }

    /// Re-try the dirs earlier batches could not read at their indexed path (and the names
    /// blocked under them) when no event brought a batch to do it.
    fn retry_stale(&self, st: &mut State) {
        let root_fd = st.root_fd.as_raw_fd();
        let polled = st.polled_root;
        let Some(ix) = st.ix.as_mut() else { return };
        let mut env = CoreEnv::new(root_fd, &self.watcher, polled);
        let mut batch = Batch::default();
        batch.settle(&mut ix.idx, &mut env);
        let txn = std::mem::take(&mut batch.txn);
        if let Err(e) = self.commit(st, txn, CommitOpts::default()) {
            if !self.held.load(Ordering::Relaxed) {
                crate::log!("commit: {e}");
            }
        }
    }

    fn process_events(&self, st: &mut State) {
        let (events, injected) = self.watcher.take();
        self.process_taken(st, events, injected);
    }

    /// Reconcile the events just taken from the watcher (what is still queued is the next
    /// batch's), completing the renames they cut in half first.
    fn process_taken(&self, st: &mut State, mut events: Vec<sys::InotifyEvent>, injected: bool) {
        if !injected {
            self.complete_moves(st, &mut events);
        }
        self.process_batch(st, events, injected);
    }

    /// Extend a batch so that no rename is cut in half. rename(2) queues IN_MOVED_FROM and
    /// IN_MOVED_TO one after the other, not atomically: a batch taken in between (the renaming
    /// thread preempted — one CPU, a busy VM) holds the IN_MOVED_FROM alone. Reconciled alone,
    /// the old name's node is missing with its inode found nowhere: removed — or, if a new file
    /// already took the name (`mv a b; echo > a`), merged into it — and the next batch's
    /// IN_MOVED_TO gives the moved inode a new id (`mv a b; touch a`: b lost a's id, CI
    /// 2026-10-01). Ids follow inodes (D4), so the pair must be in one batch.
    ///
    /// Not a timeout: rename holds the inode locks of both parent dirs until both events are
    /// queued, so taking the source dir's lock ([`sys::dir_lock_barrier`]) waits for any rename
    /// in progress there; the IN_MOVED_TO, if the destination is watched, is then queued and
    /// taken into this batch with the events before it. A move out of the watched tree has no
    /// IN_MOVED_TO and stays unpaired (one getdents of its source dir per batch).
    fn complete_moves(&self, st: &mut State, events: &mut Vec<sys::InotifyEvent>) {
        if events.iter().any(|e| e.mask & sys::IN_Q_OVERFLOW != 0) {
            return; // every scanned dir is re-listed anyway
        }
        let root_fd = st.root_fd.as_raw_fd();
        let polled = st.polled_root;
        let Some(ix) = st.ix.as_ref() else { return };
        let mut env = CoreEnv::new(root_fd, &self.watcher, polled);
        let mut synced: HashSet<u32> = HashSet::new();
        // Each round takes events up to the IN_MOVED_TOs it waited for; renames that began in
        // between may leave new halves. Bounded: a rename storm cannot hold the batch.
        for _ in 0..8 {
            let unpaired: Vec<(i32, u32)> = unpaired_moves(events)
                .into_iter()
                .filter(|(_, c)| !synced.contains(c))
                .collect();
            if unpaired.is_empty() {
                return;
            }
            let mut dirs: HashSet<Slot> = HashSet::new();
            for &(wd, cookie) in &unpaired {
                synced.insert(cookie);
                if let Some(s) = self.watcher.dir_of(wd).and_then(|d| ix.idx.slot_of(d)) {
                    dirs.insert(s);
                }
            }
            for d in dirs {
                if let Ok(fd) = env.open_dir(&ix.idx, d) {
                    sys::dir_lock_barrier(fd);
                }
            }
            let more = self.watcher.take_moved_to(&synced);
            if more.is_empty() {
                return;
            }
            events.extend(more);
        }
    }

    /// Reconcile one batch of inotify events (`injected`: a simulated queue overflow). Tests
    /// call it directly to force a batch boundary at a chosen point of a syscall sequence.
    fn process_batch(&self, st: &mut State, events: Vec<sys::InotifyEvent>, injected: bool) {
        if st.ix.is_none() || (events.is_empty() && !injected) {
            return;
        }
        let mut overflow = injected;
        let mut dirty: Vec<(u64, String, u8)> = Vec::new();
        let mut pos: HashMap<(u64, Vec<u8>), usize> = HashMap::new();
        // Per dirty name: every inotify mask seen for it in this batch (transient links).
        let mut masks: Vec<u32> = Vec::new();
        // Every name event in order, as (dirty index, mask, cookie): see `lost_created_inode`.
        let mut seq: Vec<(usize, u32, u32)> = Vec::new();
        let mut root_gone = false;
        if !events.is_empty() {
            st.last_events = Instant::now();
        }
        // Carried over from the previous batch (see below): created here, as far as the
        // created-inode rules go. No content hint — only their own events bring one.
        for (dir_id, name) in std::mem::take(&mut st.carried_created) {
            let key = (dir_id, name.clone().into_bytes());
            if pos.contains_key(&key) {
                continue;
            }
            pos.insert(key, dirty.len());
            seq.push((dirty.len(), sys::IN_CREATE, 0));
            dirty.push((dir_id, name, 0));
            masks.push(sys::IN_CREATE);
        }
        for ev in events {
            if ev.mask & sys::IN_Q_OVERFLOW != 0 {
                overflow = true;
                continue;
            }
            if ev.mask & sys::IN_IGNORED != 0 {
                self.watcher.forget_wd(ev.wd);
                continue;
            }
            let Some(dir_id) = self.watcher.dir_of(ev.wd) else {
                continue;
            };
            if ev.mask & (sys::IN_DELETE_SELF | sys::IN_MOVE_SELF) != 0 {
                if dir_id == ItemId::ROOT.0 {
                    root_gone = true;
                }
                continue;
            }
            if ev.name.is_empty() || reconcile::ignored_name(&ev.name) {
                continue;
            }
            let mut hint = 0u8;
            // A move is not a content change (MOVED_TO only lets identity follow the inode).
            if ev.mask & (sys::IN_MODIFY | sys::IN_CLOSE_WRITE | sys::IN_CREATE) != 0 {
                hint |= H_CONTENT;
            }
            if ev.mask & sys::IN_CLOSE_WRITE != 0 {
                hint |= H_CLOSE;
            }
            if ev.mask & sys::IN_MOVED_TO != 0 {
                hint |= H_MOVED_TO;
            }
            let key = (dir_id, ev.name);
            if let Some(&i) = pos.get(&key) {
                dirty[i].2 |= hint;
                masks[i] |= ev.mask;
                seq.push((i, ev.mask, ev.cookie));
            } else if let Ok(name) = String::from_utf8(key.1.clone()) {
                pos.insert(key, dirty.len());
                seq.push((dirty.len(), ev.mask, ev.cookie));
                dirty.push((dir_id, name, hint));
                masks.push(ev.mask);
            }
        }
        // A file name created and then deleted or renamed over within one batch may have been a
        // hard link to an indexed file, written through before it went away: that write changed
        // the other link's inode with no event naming it (inotify reports it for the name used
        // only, and link(2) itself only to the inode's own watches). A created name moved away
        // is not suspicious if its inode is observed (with its nlink) where it landed — but it
        // is when the landing name lost it again in this batch (`ln f x; echo >> x; mv x y; mv
        // tmp y`), or it left the watched tree.
        //
        // Nothing in the events tells such a link (`ln f x`) from a fresh file (`git` writing
        // index.lock twice): both are one IN_CREATE, and the other link may sit in any
        // directory with nlink back where it was. So the files of the directories those names
        // were in are re-stat'ed now (the common case: a link next to its file), and a full
        // stat audit of every indexed file is owed — run once for any number of such batches,
        // when events go quiet, at the next barrier, or `audit_max_delay` after it fell due
        // (see `audit_if_due`; a Pong covers every change before its Ping).
        //
        // A batch boundary can split such a sequence: batch 1 reads `x` CREATE+MODIFY but `x`
        // is renamed away before it is stat'ed (the inode is never observed), batch 2 reads `x`
        // → `idx` and `idx.lock` → `idx` (no IN_CREATE for `x` in it); or batch 1 sees `x`
        // land on `idx`, but `idx` holds another inode by the time it is stat'ed. Neither
        // batch alone sees a lost created inode. A batch's stats are only stale for a name
        // whose next events were already queued when it stat'ed it — so after the batch, each
        // name holding a created inode that has queued events is carried into the next batch
        // as created there (above), which then follows the inode on through its events. An
        // atomic save split between its write and its rename still lands where it is seen.
        let (lost, held) = created_inode_fates(&seq);
        let held: Vec<(u64, String)> = held
            .into_iter()
            .map(|i| (dirty[i].0, dirty[i].1.clone()))
            .collect();
        let mut suspect: Vec<usize> = masks
            .iter()
            .enumerate()
            .filter(|(_, &m)| {
                m & sys::IN_ISDIR == 0
                    && m & sys::IN_CREATE != 0
                    && m & (sys::IN_DELETE | sys::IN_MOVED_TO) != 0
            })
            .map(|(i, _)| i)
            .collect();
        suspect.extend(lost);
        let suspect_dirs: HashSet<u64> = suspect.iter().map(|&i| dirty[i].0).collect();
        if root_gone {
            self.root_replaced(st);
            return;
        }
        let root_fd = st.root_fd.as_raw_fd();
        let polled = st.polled_root;
        let expand_new = st.expand_new.clone();
        let Some(ix) = st.ix.as_mut() else { return };
        let mut env = CoreEnv::new(root_fd, &self.watcher, polled);
        let mut batch = Batch::default();
        batch.expand_new = expand_new;
        if overflow {
            // IN_Q_OVERFLOW (D14): every scanned dir is re-listed; identity matching keeps ids.
            crate::log!("inotify queue overflow: reconciling every scanned directory");
            for s in ix.idx.bfs() {
                if ix.idx.node(s).kind() == NKind::Dir && ix.idx.node(s).scanned() {
                    batch.to_scan.push_back(s);
                }
            }
        }
        for (dir_id, name, hint) in dirty {
            let Some(d) = ix.idx.slot_of(dir_id) else {
                continue;
            };
            if !ix.idx.node(d).scanned() {
                continue;
            }
            batch.observe(&mut ix.idx, &mut env, d, &name, hint);
        }
        batch.settle(&mut ix.idx, &mut env);
        let txn = std::mem::take(&mut batch.txn);
        if let Err(e) = self.commit(st, txn, CommitOpts::default()) {
            if !self.held.load(Ordering::Relaxed) {
                crate::log!("commit: {e}");
            }
        }
        if !held.is_empty() && !overflow {
            // Taken after every stat of this batch: a name with no event queued by now was
            // stat'ed while it still held what this batch's events put there.
            let queued = self.watcher.queued_names();
            st.carried_created = held
                .into_iter()
                .filter(|(d, n)| queued.contains(&(*d, n.as_bytes().to_vec())))
                .collect();
        }
        if !suspect_dirs.is_empty() && !overflow {
            st.audit_due.get_or_insert_with(Instant::now);
            let dirs: HashSet<Slot> = match st.ix.as_ref() {
                Some(ix) => suspect_dirs
                    .iter()
                    .filter_map(|&d| ix.idx.slot_of(d))
                    .collect(),
                None => HashSet::new(),
            };
            if !dirs.is_empty() {
                self.audit(st, Some(&dirs));
            }
        }
    }

    /// Run the owed full audit (see `process_events`) when `force` (a barrier, a stop), once
    /// events have been quiet for `audit_quiet`, or `audit_max_delay` after it fell due.
    fn audit_if_due(&self, st: &mut State, force: bool) {
        let Some(due) = st.audit_due else { return };
        if force
            || st.last_events.elapsed() >= self.cfg.audit_quiet
            || due.elapsed() >= self.cfg.audit_max_delay
        {
            st.audit_due = None;
            self.audit(st, None);
        }
    }

    /// Re-stat every indexed regular file (in parallel, one open per directory) and reconcile
    /// those whose stat no longer matches the index — the changes inotify cannot attribute to
    /// a name (a write through a short-lived hard link). Cost: one statx per file.
    pub fn audit_files(&self, st: &mut State) {
        st.audit_due = None;
        self.audit(st, None);
    }

    /// [`Core::audit_files`], restricted to the files directly in `dirs` when given.
    fn audit(&self, st: &mut State, dirs: Option<&HashSet<Slot>>) {
        let scoped = dirs.is_some();
        let root_fd = st.root_fd.as_raw_fd();
        let polled = st.polled_root;
        let Some(ix) = st.ix.as_mut() else { return };
        let t0 = Instant::now();
        let mut by_dir: HashMap<Slot, Vec<Slot>> = HashMap::new();
        match dirs {
            Some(dirs) => {
                for &d in dirs {
                    if !ix.idx.alive(d) || ix.idx.is_detached(d) {
                        continue;
                    }
                    let files: Vec<Slot> = ix
                        .idx
                        .children(d)
                        .filter(|&c| ix.idx.node(c).kind() == NKind::File)
                        .collect();
                    if !files.is_empty() {
                        by_dir.insert(d, files);
                    }
                }
            }
            None => {
                for s in ix.idx.bfs() {
                    let n = ix.idx.node(s);
                    if n.kind() == NKind::File && !ix.idx.is_detached(s) {
                        by_dir.entry(n.parent).or_default().push(s);
                    }
                }
            }
        }
        let nfiles: usize = by_dir.values().map(|v| v.len()).sum();
        let dirs: Vec<(Slot, Vec<Slot>)> = by_dir.into_iter().collect();
        let nthreads = self.cfg.walkers.clamp(1, dirs.len().max(1));
        let idx = &ix.idx;
        let stale: Vec<(Slot, String)> = std::thread::scope(|scope| {
            let chunks: Vec<&[(Slot, Vec<Slot>)]> =
                dirs.chunks(dirs.len().div_ceil(nthreads).max(1)).collect();
            let handles: Vec<_> = chunks
                .into_iter()
                .map(|chunk| {
                    scope.spawn(move || {
                        let mut out = Vec::new();
                        for (d, files) in chunk {
                            let Some(rel) = idx.rel_path(*d) else {
                                continue;
                            };
                            let Ok(fd) = sys::open_beneath(root_fd, &rel, sys::DIR_FLAGS) else {
                                continue;
                            };
                            if !reconcile::fd_matches(idx, *d, fd.as_raw_fd()) {
                                continue;
                            }
                            for &f in files {
                                let name = idx.name(f);
                                let n = idx.node(f);
                                let same = match sys::statat(fd.as_raw_fd(), name.as_bytes()) {
                                    Ok(x) => {
                                        x.ino == n.ino
                                            && x.size == n.size
                                            && x.mtime_ns == n.mtime_ns
                                            && x.ctime_ns == n.ctime_ns
                                    }
                                    Err(_) => false,
                                };
                                if !same {
                                    out.push((*d, name.to_string()));
                                }
                            }
                        }
                        out
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        });
        if dirs.is_empty() || (stale.is_empty() && scoped) {
            return;
        }
        crate::log!(
            "audit ({}): {} stale of {nfiles} files in {} dirs, in {:?}",
            if scoped { "scoped" } else { "full" },
            stale.len(),
            dirs.len(),
            t0.elapsed()
        );
        if stale.is_empty() {
            return;
        }
        let mut env = CoreEnv::new(root_fd, &self.watcher, polled);
        let mut batch = Batch::default();
        for (d, name) in stale {
            batch.observe(&mut ix.idx, &mut env, d, &name, H_CONTENT);
        }
        batch.settle(&mut ix.idx, &mut env);
        let txn = std::mem::take(&mut batch.txn);
        if let Err(e) = self.commit(st, txn, CommitOpts::default()) {
            crate::log!("audit commit: {e}");
        }
    }

    /// Reconcile explicit dirty names (e.g. after a failed identity check: rescan the parent).
    pub fn rescan_dir(&self, st: &mut State, dir: Slot) {
        let root_fd = st.root_fd.as_raw_fd();
        let polled = st.polled_root;
        let Some(ix) = st.ix.as_mut() else { return };
        if !ix.idx.alive(dir) || !ix.idx.node(dir).scanned() {
            return;
        }
        let mut env = CoreEnv::new(root_fd, &self.watcher, polled);
        let mut batch = Batch::default();
        batch.to_scan.push_back(dir);
        batch.settle(&mut ix.idx, &mut env);
        let txn = std::mem::take(&mut batch.txn);
        let _ = self.commit(st, txn, CommitOpts::default());
    }

    /// The root was deleted/moved/replaced (§2(d)8): sessions get `Error(RootReplaced)`, the
    /// index is discarded, and the next Hello builds a new index id.
    fn root_replaced(&self, st: &mut State) {
        crate::log!("root replaced");
        st.root_replaced = true;
        let err = ServerMsg::Error {
            req_id: None,
            err: perr(
                ErrorCode::RootReplaced,
                "the root directory was deleted, moved or replaced",
            ),
        };
        if let Ok(f) = frame::encode(&err, false) {
            for s in &st.subs {
                s.out.push_inter(f.clone());
                s.close();
            }
        }
        self.sessions.fetch_sub(st.subs.len(), Ordering::Relaxed);
        st.subs.clear();
        st.expanders.clear();
        st.last_listed.clear();
        st.throttle.clear();
        for d in self.watcher.watched_dirs() {
            self.watcher.remove_dir(d);
        }
        st.ix = None;
        st.loaded = None;
        let _ = st.store.reset();
    }

    /// At Hello: the root path must still be the inode we hold (§2(d)8).
    fn check_root(&self, st: &mut State) -> Result<(), ProtoError> {
        use std::os::unix::ffi::OsStrExt;
        let cur = sys::lstat_path(self.root.as_os_str().as_bytes()).map_err(|e| {
            perr(
                ErrorCode::RootReplaced,
                format!("root {}: {e}", self.root.display()),
            )
        })?;
        if !cur.is_dir() {
            return Err(perr(ErrorCode::RootReplaced, "root is not a directory"));
        }
        if cur.dev != st.root_dev || cur.ino != st.root_ino || st.root_replaced {
            let (fd, rst, fs) = open_root(&self.root).map_err(|e| io_err(&e, "open root"))?;
            if st.ix.is_some() || st.loaded.is_some() {
                // Sessions on the old index must not see ids of the new one.
                self.root_replaced(st);
            }
            st.root_fd = fd;
            st.root_dev = rst.dev;
            st.root_ino = rst.ino;
            st.root_fsid = fs.fsid;
            st.root_replaced = false;
        }
        Ok(())
    }

    // ---- handshake ----------------------------------------------------------------------

    /// Handle `Hello`: queue `Welcome` (+ resume Events) into the session's outbox and register
    /// it for publication. Returns true when the caller must run a snapshot walker.
    pub fn hello(
        &self,
        sess: &Arc<SessionShared>,
        root: &str,
        resume: Option<(IndexId, u64)>,
        default_lazy: &[String],
        proto: u32,
    ) -> Result<bool, ProtoError> {
        let want = expand_root(root);
        let want = std::fs::canonicalize(&want).unwrap_or(want);
        if want != self.root {
            return Err(perr(
                ErrorCode::Protocol,
                format!(
                    "this unlatchd serves {} but the client asked for {}",
                    self.root.display(),
                    want.display()
                ),
            ));
        }
        let mut st = self.plock()?;
        self.check_root(&mut st)?;
        self.ensure_index(&mut st, default_lazy)
            .map_err(|e| io_err(&e, "index"))?;
        // Not `flush`: the verify walk of an adopted index runs after Welcome (D17).
        self.process_events(&mut st);
        let st = &mut *st;
        // Welcome carries the seq and the root's id; Resume/Snapshot every other id.
        self.ensure_reserved(st).map_err(|e| reserve_err(&e))?;
        let Some(ix) = st.ix.as_ref() else {
            return Err(perr(ErrorCode::Io, "no index"));
        };
        let idx = &ix.idx;
        let mode = match resume {
            Some((id, seq))
                if id.0 == ix.index_id
                    && seq >= idx.gc_seq
                    && seq <= idx.seq
                    && !ix.suspect.iter().any(|&(lo, hi)| seq > lo && seq <= hi)
                    // More removals than a snapshot has entries: the snapshot is cheaper.
                    && idx.tombs_after(seq) <= idx.len() + 10_000 =>
            {
                WelcomeMode::Resume
            }
            _ => WelcomeMode::Snapshot,
        };
        let mut warnings = st.warnings.clone();
        warnings.extend(self.watcher.warnings());
        if !st.polled_root && idx.may_have_polled() {
            let polled = idx
                .bfs()
                .into_iter()
                .filter(|&s| idx.node(s).has(F_POLLED))
                .count();
            if polled > 0 {
                warnings.push(format!(
                    "inotify watch budget ({}) reached: {polled} directories are polled every 1–30 s",
                    self.watcher.budget()
                ));
            }
        }
        let info = ServerInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            hostname: self.hostname.clone(),
            root_path: self.root.to_string_lossy().into_owned(),
            entries: idx.len() as u64,
            watches: self.watcher.count(),
            polled: st.polled_root,
            warnings,
        };
        let welcome = ServerMsg::Welcome {
            proto,
            index: IndexId(ix.index_id),
            seq: idx.seq,
            mode: mode.clone(),
            root: idx.entry(idx.root),
            info,
            lazy_names: idx.lazy_names.clone(),
        };
        let f =
            frame::encode(&welcome, true).map_err(|e| perr(ErrorCode::Protocol, e.to_string()))?;
        sess.out.push_inter(f);
        let snapshot = mode == WelcomeMode::Snapshot;
        if let (WelcomeMode::Resume, Some((_, since))) = (&mode, resume) {
            let changes = changes_since(idx, since);
            let seq = idx.seq;
            if !changes.is_empty() {
                sess.out.push_inter_many(&event_frames(seq, changes));
            }
            st.published_seq = st.published_seq.max(seq);
        } else if let Ok(mut g) = sess.snap.lock() {
            let mut feed = SnapFeed::default();
            feed.push(ItemId::ROOT.0);
            *g = Some(feed);
        }
        st.subs.push(sess.clone());
        self.sessions.fetch_add(1, Ordering::Relaxed);
        Ok(snapshot)
    }

    /// One step of a session's snapshot walker: the next chunk of directory listings
    /// (≤ 64 KiB), or `None` when done (then `SnapshotDone{seq}` is returned as `Err(seq)`).
    pub fn snapshot_step(&self, sess: &SessionShared) -> Result<Option<Vec<u8>>, u64> {
        let Ok(mut st) = self.st.lock() else {
            return Err(0);
        };
        let st = &mut *st;
        if self.held.load(Ordering::Relaxed) && self.ensure_reserved(st).is_err() {
            // Entries above the reservation must not go out: the walker retries shortly.
            return Ok(None);
        }
        let Some(ix) = st.ix.as_ref() else {
            return Err(0);
        };
        let idx = &ix.idx;
        let Ok(mut g) = sess.snap.lock() else {
            return Err(0);
        };
        let Some(feed) = g.as_mut() else {
            return Err(idx.seq);
        };
        let mut entries: Vec<Entry> = Vec::new();
        let mut complete: Vec<ItemId> = Vec::new();
        let mut size = 0usize;
        loop {
            if let Some((dir_id, rest)) = feed.partial.as_mut() {
                while let Some(e) = rest.front() {
                    let sz = entry_size(e);
                    if !entries.is_empty() && size + sz > FRAME_BUDGET {
                        break;
                    }
                    size += sz;
                    if let Some(e) = rest.pop_front() {
                        entries.push(e);
                    }
                }
                if rest.is_empty() {
                    complete.push(ItemId(*dir_id));
                    feed.partial = None;
                } else {
                    break;
                }
                if size >= FRAME_BUDGET {
                    break;
                }
                continue;
            }
            let Some(dir_id) = feed.queue.pop_front() else {
                break;
            };
            feed.queued.remove(&dir_id);
            let Some(d) = idx.slot_of(dir_id) else {
                continue;
            };
            if feed.sent.contains(&dir_id) || !idx.node(d).scanned() {
                continue;
            }
            // Capture the whole listing now: pieces of one directory must not shift while it
            // is sent across several chunks (later changes arrive as Events, LWW by seq).
            let list: VecDeque<Entry> = idx.children(d).map(|c| idx.entry(c)).collect();
            for e in &list {
                if e.kind == unlatch_proto::Kind::Dir && !e.lazy {
                    feed.push(e.id.0);
                }
            }
            feed.sent.insert(dir_id);
            feed.partial = Some((dir_id, list));
        }
        if entries.is_empty() && complete.is_empty() {
            *g = None;
            sys::trim_heap();
            let seq = idx.seq;
            st.published_seq = st.published_seq.max(seq);
            return Err(seq);
        }
        let msg = ServerMsg::SnapshotChunk {
            entries,
            complete_dirs: complete,
        };
        frame::encode(&msg, true).map(Some).map_err(|_| 0)
    }

    pub fn remove_session(&self, sess: &Arc<SessionShared>) {
        sess.close();
        if let Ok(mut st) = self.st.lock() {
            let before = st.subs.len();
            st.subs.retain(|s| s.id != sess.id);
            if st.subs.len() < before {
                self.sessions.fetch_sub(1, Ordering::Relaxed);
            }
            // Its expansions stay until the idle collapse (30 min) — another Mac may use them.
            for set in st.expanders.values_mut() {
                set.remove(&sess.id);
            }
        }
        if let Ok(mut t) = self.last_client.lock() {
            *t = Instant::now();
        }
        self.watcher.nudge();
    }

    // ---- listing ------------------------------------------------------------------------

    /// `ListDir` (one level; D13). Expands a lazy dir: watch it, read it, child dirs lazy.
    pub fn list_dir(&self, sess_id: u64, id: ItemId) -> Result<(Entry, Vec<Entry>), ProtoError> {
        let mut st = self.plock()?;
        self.flush(&mut st, false);
        let st = &mut *st;
        // The requester gets the entries in its ListingPart; others get Events.
        self.expand(st, sess_id, id, Some(sess_id))?;
        let ix = st
            .ix
            .as_ref()
            .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
        let d = ix
            .idx
            .slot_of(id.0)
            .ok_or_else(|| perr(ErrorCode::NotFound, "no such item"))?;
        let dir = ix.idx.entry(d);
        let kids = ix.idx.children(d).map(|c| ix.idx.entry(c)).collect();
        Ok((dir, kids))
    }

    /// Scan and watch one level of directory `id` if it is lazy (D13), on behalf of session
    /// `sess_id` (which then counts as listing it for `Unwatch` / the idle collapse). The new
    /// children are published to every session except `exclude`.
    pub fn expand(
        &self,
        st: &mut State,
        sess_id: u64,
        id: ItemId,
        exclude: Option<u64>,
    ) -> Result<(), ProtoError> {
        let root_fd = st.root_fd.as_raw_fd();
        let polled = st.polled_root;
        let ix = st
            .ix
            .as_mut()
            .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
        let d = ix
            .idx
            .slot_of(id.0)
            .ok_or_else(|| perr(ErrorCode::NotFound, "no such item"))?;
        if ix.idx.node(d).kind() != NKind::Dir {
            return Err(perr(ErrorCode::NotDir, "not a directory"));
        }
        let mut txn_opt = None;
        if !ix.idx.node(d).scanned() && !ix.idx.node(d).has(F_NOEXPAND) {
            let mut env = CoreEnv::new(root_fd, &self.watcher, polled);
            let mut batch = Batch::default();
            batch.cold_dirs.insert(id.0);
            ix.idx.set_flags(d, F_EXPANDED, 0);
            batch.scan_one(&mut ix.idx, &mut env, d);
            if !ix.idx.node(d).scanned() {
                ix.idx.set_flags(d, 0, F_EXPANDED);
                return Err(perr(ErrorCode::NotFound, "directory could not be read"));
            }
            batch.settle(&mut ix.idx, &mut env);
            ix.idx.cold.remove(&id.0);
            txn_opt = Some(std::mem::take(&mut batch.txn));
        }
        if ix.idx.node(d).has(F_EXPANDED) {
            self.note_listed(st, sess_id, id.0);
        }
        if let Some(txn) = txn_opt {
            self.commit(
                st,
                txn,
                CommitOpts {
                    exclude,
                    no_throttle: true,
                    ..Default::default()
                },
            )
            .map_err(|e| io_err(&e, "journal"))?;
        }
        Ok(())
    }

    /// Session `sess_id` holds the listing of the expanded lazy dir `dir_id` (for `Unwatch` and
    /// the idle collapse).
    pub fn note_listed(&self, st: &mut State, sess_id: u64, dir_id: u64) {
        st.last_listed.insert(dir_id, Instant::now());
        st.expanders.entry(dir_id).or_default().insert(sess_id);
    }

    /// `Unwatch` / idle collapse: drop the subtree below `dir` (children kept as cold ids),
    /// remove watches, mark it lazy (D13).
    pub fn collapse(&self, st: &mut State, dir_id: u64) -> Result<(), ProtoError> {
        let ix = st
            .ix
            .as_mut()
            .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
        let d = ix
            .idx
            .slot_of(dir_id)
            .ok_or_else(|| perr(ErrorCode::NotFound, "no such item"))?;
        if d == ix.idx.root {
            return Err(perr(ErrorCode::Unsupported, "cannot collapse the root"));
        }
        if !ix.idx.node(d).scanned() {
            return Ok(());
        }
        let mut txn = Txn::default();
        let kids: Vec<Slot> = ix.idx.children(d).collect();
        let cold: Vec<_> = kids.iter().map(|&k| ix.idx.cold_child(k)).collect();
        for k in kids {
            for r in ix.idx.remove_subtree(k, false, &mut txn) {
                let n = ix.idx.node(r);
                if n.kind() == NKind::Dir {
                    self.watcher.remove_dir(n.id);
                    st.last_listed.remove(&n.id);
                    st.expanders.remove(&n.id);
                }
            }
        }
        self.watcher.remove_dir(dir_id);
        ix.idx.cold.insert(dir_id, cold);
        ix.idx.set_flags(d, 0, F_SCANNED | F_EXPANDED | F_POLLED);
        ix.idx.touch_other(d, &mut txn);
        st.last_listed.remove(&dir_id);
        st.expanders.remove(&dir_id);
        self.commit(
            st,
            txn,
            CommitOpts {
                no_throttle: true,
                ..Default::default()
            },
        )
        .map_err(|e| io_err(&e, "journal"))
    }

    /// Mounts changed below the root: re-list the parent of every mount point that appeared or
    /// went away, so the mount point's identity change is reconciled (it becomes / stops being a
    /// lazy mount point, §2(d)7).
    fn remount(&self, st: &mut State) {
        let now = mounts_below(&self.root);
        let changed: Vec<PathBuf> = now.symmetric_difference(&st.mounts).cloned().collect();
        st.mounts = now;
        for p in changed {
            let Ok(rel) = p.strip_prefix(&self.root) else {
                continue;
            };
            let Some(parent_rel) = rel.parent() else {
                continue;
            };
            let dir = st.ix.as_ref().and_then(|ix| {
                let mut cur = ix.idx.root;
                for comp in parent_rel.components() {
                    cur = ix.idx.lookup(cur, comp.as_os_str().to_str()?)?;
                }
                Some(cur)
            });
            if let Some(d) = dir {
                crate::log!("mount change at {}", p.display());
                self.rescan_dir(st, d);
            }
        }
    }

    // ---- periodic work --------------------------------------------------------------------

    /// Poll dirs without watches (budget / network fs, D14): re-list them; adaptive 1–30 s.
    /// `force`: now, whatever the schedule (a Ping barrier must cover polled dirs too).
    fn poll(&self, st: &mut State, force: bool) {
        if !force && st.last_poll.elapsed() < st.poll_interval {
            return;
        }
        let root_fd = st.root_fd.as_raw_fd();
        let polled = st.polled_root;
        if !polled && st.ix.as_ref().is_some_and(|ix| !ix.idx.may_have_polled()) {
            // Every directory is watched: nothing to poll, and no walk to find that out.
            st.last_poll = Instant::now();
            return;
        }
        let Some(ix) = st.ix.as_mut() else { return };
        let dirs: Vec<Slot> = ix
            .idx
            .bfs()
            .into_iter()
            .filter(|&s| {
                let n = ix.idx.node(s);
                n.kind() == NKind::Dir && n.scanned() && (polled || n.has(F_POLLED))
            })
            .collect();
        st.last_poll = Instant::now();
        if dirs.is_empty() {
            return;
        }
        let t0 = Instant::now();
        let mut env = CoreEnv::new(root_fd, &self.watcher, polled);
        let mut batch = Batch::default();
        for d in dirs {
            batch.to_scan.push_back(d);
        }
        batch.settle(&mut ix.idx, &mut env);
        let changed = !batch.txn.is_empty();
        let txn = std::mem::take(&mut batch.txn);
        let _ = self.commit(st, txn, CommitOpts::default());
        let cost = t0.elapsed();
        st.poll_interval = if changed {
            Duration::from_secs(1)
        } else {
            (st.poll_interval * 2)
                .clamp(Duration::from_secs(1), Duration::from_secs(30))
                .max(cost * 10)
        }
        .min(Duration::from_secs(30));
    }

    fn periodic(&self, st: &mut State) {
        if st.ix.is_none() {
            return;
        }
        self.audit_if_due(st, false);
        self.publish_throttled(st, false);
        crate::ops::sweep_parked(st, false);
        self.poll(st, false);
        // Collapse expansions nobody listed for `collapse_after` (§2(d)10).
        let stale: Vec<u64> = st
            .last_listed
            .iter()
            .filter(|(_, t)| t.elapsed() >= self.cfg.collapse_after)
            .map(|(&id, _)| id)
            .collect();
        for id in stale {
            let _ = self.collapse(st, id);
        }
        if st.last_budget.elapsed() >= Duration::from_secs(60) && !st.polled_root {
            // Other processes' watch usage changes; refine off-thread (never under this lock).
            st.last_budget = Instant::now();
            self.watcher.compute_budget_async(self.cfg.max_watches);
        }
        if self.held.load(Ordering::Relaxed) && st.last_hold_try.elapsed() >= Duration::from_secs(1)
        {
            // Space may have come back with nothing changing since: publish what is held.
            st.last_hold_try = Instant::now();
            let _ = self.ensure_reserved(st);
        }
        let want_cp = st.store.wants_checkpoint()
            || st.cp_failures > 0
            || (st.journal_dirty && st.last_checkpoint.elapsed() >= Duration::from_secs(600));
        // A failing checkpoint (disk full, state dir not writable) is retried with exponential
        // backoff, never on every tick: each attempt encodes the whole index under this lock.
        let backoff = st.cp_retry_at.is_some_and(|t| Instant::now() < t);
        if want_cp && !backoff {
            self.checkpoint(st, false);
        }
    }

    pub fn checkpoint(&self, st: &mut State, clean: bool) {
        // A checkpoint carries no throttle state: publish everything held back first.
        self.publish_throttled(st, true);
        let Some(ix) = st.ix.as_mut() else { return };
        ix.ops.expire();
        ix.idx
            .gc_tombs(self.cfg.tomb_horizon_secs, self.cfg.tomb_max);
        let r = st.store.reserve(ix.idx.next_id, ix.idx.seq).and_then(|_| {
            st.store.checkpoint(
                ix.index_id,
                &ix.origin,
                &ix.idx,
                &ix.ops,
                &ix.suspect,
                ix.observed_ns,
                clean,
            )
        });
        match r {
            Ok(()) => {
                st.last_checkpoint = Instant::now();
                st.journal_dirty = false;
                if st.cp_failures > 0 {
                    crate::log!("checkpoint: saved again after {} failures", st.cp_failures);
                    st.warnings.retain(|w| !w.starts_with(WARN_CHECKPOINT));
                }
                st.cp_failures = 0;
                st.cp_retry_at = None;
                sys::trim_heap();
            }
            Err(e) => {
                st.cp_failures = st.cp_failures.saturating_add(1);
                let delay = CP_RETRY_MIN
                    .saturating_mul(1u32 << (st.cp_failures - 1).min(16))
                    .min(CP_RETRY_MAX);
                st.cp_retry_at = Some(Instant::now() + delay);
                crate::log!(
                    "checkpoint failed ({} in a row): {e}; next attempt in {delay:?}",
                    st.cp_failures
                );
                st.warnings.retain(|w| !w.starts_with(WARN_CHECKPOINT));
                st.warnings.push(format!(
                    "{WARN_CHECKPOINT} ({e}): free space in {}",
                    st.store.dir().display()
                ));
            }
        }
    }

    // ---- id/seq reservation (alloc.bin) ---------------------------------------------------

    /// A reservation failed: hold publication back (see [`State::held_since`]).
    fn hold(&self, st: &mut State, e: &io::Error, journaled: bool) {
        if st.held_since.is_none() {
            st.held_since = Some(st.published_seq);
            st.held_journal_ok = true;
            st.last_hold_try = Instant::now();
            crate::log!(
                "cannot reserve ids/seqs in {}: {e}; holding changes back until it can",
                st.store.dir().display()
            );
            st.warnings.retain(|w| !w.starts_with(WARN_HELD));
            st.warnings.push(format!(
                "{WARN_HELD}: the VM's unlatchd state dir {} is full or not writable ({e})",
                st.store.dir().display()
            ));
            self.held.store(true, Ordering::Relaxed);
        }
        st.held_journal_ok &= journaled;
    }

    /// Make every id/seq the index holds durably reserved (D4); after a hold, publish what was
    /// held back first. `Err`: still held — nothing that carries an id or seq may go out.
    pub fn ensure_reserved(&self, st: &mut State) -> io::Result<()> {
        let Some(ix) = st.ix.as_ref() else {
            return Ok(());
        };
        if let Err(e) = st.store.reserve(ix.idx.next_id, ix.idx.seq) {
            self.hold(st, &e, true);
            return Err(e);
        }
        match st.held_since {
            Some(since) => self.release_held(st, since),
            None => Ok(()),
        }
    }

    /// Reserve ahead of a client mutation ([`MUTATION_HEADROOM`]), before it touches the VM.
    pub fn reserve_for_mutation(&self) -> Result<(), ProtoError> {
        let mut st = self.plock()?;
        let st = &mut *st;
        self.ensure_reserved(st).map_err(|e| reserve_err(&e))?;
        let Some(ix) = st.ix.as_ref() else {
            return Ok(());
        };
        st.store
            .reserve(
                ix.idx.next_id.saturating_add(MUTATION_HEADROOM),
                ix.idx.seq.saturating_add(MUTATION_HEADROOM),
            )
            .map_err(|e| reserve_err(&e))
    }

    /// Lock-free unless held: may a reply carrying ids/seqs go out now?
    pub fn deliverable(&self) -> Result<(), ProtoError> {
        if !self.held.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut st = self.plock()?;
        self.ensure_reserved(&mut st).map_err(|e| reserve_err(&e))
    }

    /// The reservation succeeded again: publish every change since `since` (what a client
    /// resuming from there would get), journaling it first if an append failed meanwhile.
    fn release_held(&self, st: &mut State, since: u64) -> io::Result<()> {
        let Some(ix) = st.ix.as_ref() else {
            return Ok(());
        };
        let idx = &ix.idx;
        if !st.held_journal_ok {
            let mut recs: Vec<JRec> = idx
                .bfs()
                .into_iter()
                .filter(|&s| idx.node(s).seq > since)
                .map(|s| JRec::Node(idx.pnode(s)))
                .collect();
            for t in idx.tombs.iter().filter(|t| t.seq > since) {
                recs.push(JRec::Remove {
                    id: t.id,
                    seq: t.seq,
                    tomb: true,
                    time: t.time,
                });
            }
            recs.push(JRec::Seq(idx.seq));
            st.store.append(&recs, true)?;
        }
        let changes = changes_since(idx, since);
        let seq = idx.seq;
        st.held_since = None;
        st.held_journal_ok = true;
        self.held.store(false, Ordering::Relaxed);
        st.warnings.retain(|w| !w.starts_with(WARN_HELD));
        crate::log!(
            "ids/seqs reserved again: publishing {} held changes",
            changes.len()
        );
        if !changes.is_empty() {
            st.published_seq = seq;
            let frames = event_frames(seq, changes.clone());
            for s in &st.subs {
                s.publish(&frames, &changes);
            }
        }
        Ok(())
    }

    /// Adaptive debounce (DESIGN §4, `Config::settle`): return once the inotify queue has been
    /// quiet for one step, or after `debounce_max`. Steps are `settle` for the first `debounce`
    /// of a batch and `debounce` after that, so a lone change costs ~`settle` while a burst is
    /// coalesced as before. The first growth check is against what is queued on entry (it used
    /// to be against an empty queue, so every batch, even a single write, slept two full
    /// `debounce` steps).
    fn coalesce(&self) {
        let t0 = Instant::now();
        self.watcher.take_is_growing(); // baseline: the events that woke us
        loop {
            let step = if t0.elapsed() < self.cfg.debounce {
                self.cfg.settle
            } else {
                self.cfg.debounce
            };
            std::thread::sleep(step);
            if t0.elapsed() >= self.cfg.debounce_max || !self.watcher.take_is_growing() {
                return;
            }
        }
    }

    fn actor_loop(self: Arc<Self>) {
        let mut last_periodic = Instant::now();
        let mut timeout = Duration::from_millis(100);
        while !self.shutdown.load(Ordering::Relaxed) {
            let has_events = self.watcher.wait(timeout);
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            if has_events {
                self.coalesce();
            }
            let Ok(mut st) = self.st.lock() else { break };
            // `stop` (or a crash in tests) may have run while we waited for the lock: after
            // its final checkpoint nothing here may touch the index or its files again.
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            if st.verify_pending {
                st.verify_pending = false;
                self.verify(&mut st);
            }
            if st.checkpoint_due {
                st.checkpoint_due = false;
                self.checkpoint(&mut st, false);
                crate::log!(
                    "heap after first checkpoint (in use, free): {:?}",
                    sys::heap_stats()
                );
            }
            if self.mounts_changed.swap(false, Ordering::Relaxed) {
                self.remount(&mut st);
            }
            self.process_events(&mut st);
            if last_periodic.elapsed() >= Duration::from_millis(50) {
                last_periodic = Instant::now();
                self.periodic(&mut st);
            }
            self.mark_quiet(&mut st);
            timeout = st.quiet_due.map_or(Duration::from_millis(100), |d| {
                d.saturating_duration_since(Instant::now())
                    .clamp(Duration::from_millis(1), Duration::from_millis(100))
            });
        }
    }

    /// Racy-git rule (D3) after a crash: the next process bumps every entry whose mtime/ctime
    /// lies within `RACY_WINDOW` of the last journaled observation, because a same-tick rewrite
    /// after it would be invisible in the stat tuple. With observations only at commits, the
    /// last file written before *any* crash — however long the daemon kept watching after it —
    /// came back with a new content version and the Mac's next save of it became a conflict
    /// copy of itself. Once nothing has happened for longer than the window, journal "observed
    /// up to now": every change before this instant went through inotify — no directory is
    /// polled, and neither the reader's queue nor the kernel's holds an unprocessed event —
    /// so only changes after it, in the same tick as a recorded stat, can have been missed.
    /// Not fsync'd: a lost marker only means an earlier (more conservative) observation.
    fn mark_quiet(&self, st: &mut State) {
        let Some(due) = st.quiet_due else { return };
        if Instant::now() < due || st.verify_pending || self.shutdown.load(Ordering::Relaxed) {
            return;
        }
        st.quiet_due = None;
        if st.polled_root || !self.watcher.enabled() {
            return;
        }
        let t = sys::now_ns();
        if !self.watcher.quiet() {
            // Events arrived meanwhile: their commit schedules the next marker.
            return;
        }
        let Some(ix) = st.ix.as_mut() else { return };
        let idx = &ix.idx;
        if idx.bfs().into_iter().any(|s| idx.node(s).has(F_POLLED)) {
            return;
        }
        if t > ix.observed_ns {
            ix.observed_ns = t;
            if let Err(e) = st.store.append(&[JRec::Observed(t)], false) {
                crate::log!("journal quiet observation: {e}");
            }
        }
    }

    /// Persist and stop background work (clean shutdown: exact hwm, no journal replay needed).
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.watcher.nudge();
        if let Ok(mut st) = self.st.lock() {
            // Drain once more after a moment so writes that completed just before the stop are
            // in the index; then watching ends *now*.
            std::thread::sleep(self.cfg.debounce);
            // Old versions still open elsewhere are kept as conflict copies: the startup walk
            // removes leftover staging names.
            crate::ops::sweep_parked(&mut st, true);
            self.process_events(&mut st);
            self.audit_if_due(&mut st, true);
            // Racy-git rule (D3) at the next start is about changes after watching stopped:
            // everything before this instant was observed through inotify.
            let polled = st.polled_root;
            if let Some(ix) = st.ix.as_mut() {
                if !polled {
                    ix.observed_ns = sys::now_ns();
                }
            }
            self.checkpoint(&mut st, true);
            for s in &st.subs {
                s.close();
            }
        }
        self.watcher.shutdown();
        crate::log!(
            "stop: {} statx calls in this process",
            sys::STATX_CALLS.load(Ordering::Relaxed)
        );
    }

    /// Entry of an id, after a flush.
    pub fn stat(&self, id: ItemId) -> Result<Entry, ProtoError> {
        let mut st = self.plock()?;
        self.flush(&mut st, false);
        let ix = st
            .ix
            .as_ref()
            .ok_or_else(|| perr(ErrorCode::Offline, "no index"))?;
        let s = ix
            .idx
            .slot_of(id.0)
            .ok_or_else(|| perr(ErrorCode::NotFound, "no such item"))?;
        Ok(ix.idx.entry(s))
    }

    /// Ping barrier: flush everything (throttled entries too) and return the published seq.
    /// Directories without a watch (beyond the budget, or a network-fs root) are re-listed
    /// too: the Pong contract covers every change that completed before the Ping, and nothing
    /// but a listing sees a change in a polled dir. With the engine's 5 s liveness Ping this
    /// caps the effective poll interval at ~5 s while a client is connected.
    pub fn barrier(&self) -> Result<u64, ProtoError> {
        let mut st = self.plock()?;
        self.barrier_locked(&mut st)
    }

    fn barrier_locked(&self, st: &mut State) -> Result<u64, ProtoError> {
        self.flush(st, true);
        self.poll(st, true);
        self.publish_throttled(st, true);
        // Never "everything before the Ping is published" while changes are held back.
        self.ensure_reserved(st).map_err(|e| reserve_err(&e))?;
        Ok(st.published_seq)
    }

    /// `Unwatch` from one session: collapse once no other session still has it listed.
    pub fn unwatch(&self, sess_id: u64, id: ItemId) -> Result<(), ProtoError> {
        let mut st = self.plock()?;
        self.flush(&mut st, false);
        let others = match st.expanders.get_mut(&id.0) {
            Some(set) => {
                set.remove(&sess_id);
                !set.is_empty()
            }
            None => false,
        };
        if others {
            return Ok(());
        }
        self.collapse(&mut st, id.0)
    }

    /// Test/diagnostic hook: simulate IN_Q_OVERFLOW.
    pub fn inject_overflow(&self) {
        self.watcher.inject_overflow();
    }
}

/// Unescape a mountinfo path field (`\040` = space, …).
fn unescape_mount(s: &str) -> PathBuf {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') as u32 * 64
                + (b[i + 2] - b'0') as u32 * 8
                + (b[i + 3] - b'0') as u32;
            out.push((v & 0xff) as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    let os: std::ffi::OsString = std::os::unix::ffi::OsStringExt::from_vec(out);
    PathBuf::from(os)
}

/// Mount points strictly below `root` (from /proc/self/mountinfo).
pub fn mounts_below(root: &Path) -> HashSet<PathBuf> {
    let Ok(s) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return HashSet::new();
    };
    s.lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(unescape_mount)
        .filter(|p| p != root && p.starts_with(root))
        .collect()
}

/// POLLPRI on /proc/self/mountinfo fires on every mount-table change (§2(d)7).
fn watch_mountinfo(core: std::sync::Weak<Core>) {
    let Ok(f) = std::fs::File::open("/proc/self/mountinfo") else {
        return;
    };
    loop {
        let Some(c) = core.upgrade() else { return };
        if c.shutdown.load(Ordering::Relaxed) {
            return;
        }
        drop(c);
        match sys::poll_fds(&[(f.as_raw_fd(), libc::POLLPRI)], 1000) {
            Ok(r) if r[0] & (libc::POLLPRI | libc::POLLERR) != 0 => {
                // Re-arm: the kernel reports the change until the file is read again.
                let _ = std::io::Read::read(&mut &f, &mut [0u8; 4096]);
                use std::io::Seek;
                let _ = (&f).seek(std::io::SeekFrom::Start(0));
                let _ = std::io::Read::read_to_end(&mut &f, &mut Vec::new());
                let _ = (&f).seek(std::io::SeekFrom::Start(0));
                if let Some(c) = core.upgrade() {
                    c.mounts_changed.store(true, Ordering::Relaxed);
                    c.watcher.nudge();
                }
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_unescape() {
        assert_eq!(unescape_mount("/a\\040b/c"), PathBuf::from("/a b/c"));
        assert_eq!(unescape_mount("/plain"), PathBuf::from("/plain"));
        let m = mounts_below(Path::new("/"));
        assert!(!m.contains(Path::new("/")));
    }

    fn core_for(root: &Path, state: &Path) -> Arc<Core> {
        let c = Core::new(root, state, Config::from_env()).unwrap();
        let mut st = c.lock().unwrap();
        c.ensure_index(&mut st, &[]).unwrap();
        drop(st);
        c
    }

    fn id_at(c: &Core, rel: &str) -> Option<u64> {
        let st = c.lock().unwrap();
        let ix = st.ix.as_ref().unwrap();
        let mut cur = ix.idx.root;
        for comp in rel.split('/') {
            cur = ix.idx.lookup(cur, comp)?;
        }
        Some(ix.idx.node(cur).id)
    }

    /// IN_Q_OVERFLOW (D14): events are lost; the overflow path re-lists every scanned dir and
    /// identity matching keeps ids.
    #[test]
    fn injected_overflow_reconciles_and_keeps_ids() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(root.path().join("a/b/keep.txt"), b"k").unwrap();
        std::fs::write(root.path().join("a/gone.txt"), b"g").unwrap();
        std::fs::write(root.path().join("moved.txt"), b"m").unwrap();
        let c = core_for(root.path(), state.path());
        let keep = id_at(&c, "a/b/keep.txt").unwrap();
        let moved = id_at(&c, "moved.txt").unwrap();
        let gone = id_at(&c, "a/gone.txt").unwrap();
        {
            // Hold the lock so the actor cannot consume the events, then drop them on the
            // floor like a kernel queue overflow would.
            let st = c.lock().unwrap();
            std::fs::remove_file(root.path().join("a/gone.txt")).unwrap();
            std::fs::rename(
                root.path().join("moved.txt"),
                root.path().join("a/b/moved2.txt"),
            )
            .unwrap();
            std::fs::write(root.path().join("a/new.txt"), b"n").unwrap();
            std::thread::sleep(Duration::from_millis(20));
            let (lost, _) = c.watcher.take();
            assert!(!lost.is_empty());
            drop(st);
        }
        assert_eq!(
            id_at(&c, "a/gone.txt"),
            Some(gone),
            "the index missed the events"
        );
        c.inject_overflow();
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, true);
        }
        assert_eq!(id_at(&c, "a/gone.txt"), None);
        assert_eq!(
            id_at(&c, "a/b/moved2.txt"),
            Some(moved),
            "move recovered with the same id"
        );
        assert_eq!(id_at(&c, "a/b/keep.txt"), Some(keep));
        assert!(id_at(&c, "a/new.txt").is_some());
        c.stop();
    }

    /// A content change held back by the hot-file throttle has a seq below later published
    /// seqs. If the daemon dies before publishing it, a client resuming from the last Events
    /// seq would never get it: the journal records the deferral and load gives it a fresh seq.
    /// D3 after a crash, both ways: a file written just before the crash is bumped (a same-tick
    /// rewrite after the last observation would be invisible), one written long before is not
    /// (the daemon journaled that it kept watching quietly) — else the Mac's next save of the
    /// last file the agent wrote before any crash conflicts with itself (fuzz seed 475).
    #[test]
    fn crash_bumps_only_files_changed_within_the_racy_window() {
        for quiet_before_crash in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("old"), b"old").unwrap();
            let cfg = Config::from_env();
            let c = Core::new(root.path(), state.path(), cfg.clone()).unwrap();
            {
                let mut st = c.lock().unwrap();
                c.ensure_index(&mut st, &[]).unwrap();
                c.checkpoint(&mut st, false);
            }
            let content_seq = |c: &Core, name: &str| {
                let st = c.lock().unwrap();
                let ix = st.ix.as_ref().unwrap();
                let s = ix.idx.lookup(ix.idx.root, name).unwrap();
                ix.idx.node(s).content_seq
            };
            let f = root.path().join("f");
            // Latest of f's mtime/ctime: what the racy rule compares with the observation.
            let changed_ns = || {
                use std::os::unix::fs::MetadataExt;
                let m = std::fs::symlink_metadata(&f).unwrap();
                let ns = |s: i64, n: i64| s * 1_000_000_000 + n;
                ns(m.mtime(), m.mtime_nsec()).max(ns(m.ctime(), m.ctime_nsec()))
            };
            // The lock is held from the write through its batch, so the actor cannot take the
            // events first: under load its wake-up and coalescing alone can outlast the window.
            let write_and_flush = |attempt: u32| {
                let mut st = c.lock().unwrap();
                std::fs::write(&f, format!("agent bytes {attempt:04}")).unwrap();
                c.flush(&mut st, false);
                st
            };
            let st = if quiet_before_crash {
                drop(write_and_flush(0));
                // The actor journals a quiet observation once nothing happened for a while.
                let deadline = Instant::now() + Duration::from_secs(10);
                while c.lock().unwrap().quiet_due.is_some() {
                    assert!(Instant::now() < deadline, "no quiet observation");
                    std::thread::sleep(Duration::from_millis(10));
                }
                c.lock().unwrap()
            } else {
                // "Just before the crash" means: f's change lies within RACY_WINDOW of the last
                // journaled observation (the batch's commit). A thread descheduled between the
                // write and the commit for longer than the window stat'ed f in a later tick
                // than its change, so a same-tick rewrite cannot hide and D3 rightly leaves f
                // alone — that run never set up this case. Write again until the commit lands
                // within the window (the lock stays held from here to the crash: no later
                // observation can move it).
                let deadline = Instant::now() + Duration::from_secs(60);
                let mut attempt = 0;
                loop {
                    let st = write_and_flush(attempt);
                    let observed = st.ix.as_ref().unwrap().observed_ns;
                    if changed_ns() >= observed - RACY_WINDOW.as_nanos() as i64 {
                        break st;
                    }
                    drop(st);
                    attempt += 1;
                    assert!(
                        Instant::now() < deadline,
                        "no commit within {RACY_WINDOW:?} of its write in {attempt} attempts"
                    );
                }
            };
            let before = {
                let ix = st.ix.as_ref().unwrap();
                let s = ix.idx.lookup(ix.idx.root, "f").unwrap();
                ix.idx.node(s).content_seq
            };
            // Crash: no stop, no checkpoint (the lock is held until the actor is told to stop).
            c.shutdown.store(true, Ordering::Relaxed);
            c.watcher.shutdown();
            drop(st);
            let c2 = Core::new(root.path(), state.path(), cfg).unwrap();
            {
                let mut st = c2.lock().unwrap();
                c2.ensure_index(&mut st, &[]).unwrap();
                c2.flush(&mut st, false); // the startup verify walk
            }
            let after = content_seq(&c2, "f");
            if quiet_before_crash {
                assert_eq!(
                    after, before,
                    "f was bumped although the daemon saw it settle"
                );
            } else {
                assert!(
                    after > before,
                    "D3: f changed just before the crash must be bumped"
                );
            }
            c2.stop();
        }
    }

    #[test]
    fn throttled_change_survives_a_crash_for_resuming_clients() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("f"), b"v0").unwrap();
        let mut cfg = Config::from_env();
        // Nothing is ever due on its own in this test: deferral is deterministic.
        cfg.hot_interval = Duration::from_secs(3600);
        cfg.hot_quiet = Duration::from_secs(3600);
        let c = Core::new(root.path(), state.path(), cfg.clone()).unwrap();
        let slot_seq = |c: &Core, name: &str| {
            let st = c.lock().unwrap();
            let ix = st.ix.as_ref().unwrap();
            let s = ix.idx.lookup(ix.idx.root, name).unwrap();
            (ix.idx.node(s).seq, ix.idx.node(s).size)
        };
        {
            let mut st = c.lock().unwrap();
            c.ensure_index(&mut st, &[]).unwrap();
            c.checkpoint(&mut st, false);
        }
        use std::io::Write;
        let mut fh = std::fs::OpenOptions::new()
            .append(true)
            .open(root.path().join("f"))
            .unwrap();
        fh.write_all(b"1").unwrap(); // first change: published at once
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, false);
        }
        fh.write_all(b"22").unwrap(); // hot: held back
        std::fs::write(root.path().join("g"), b"g").unwrap();
        // Let g age past the racy-timestamp window (D3) before it is observed: an entry changed
        // within 20 ms of the last persisted observation is (correctly) re-versioned on restart,
        // which a fast machine hits when the whole test fits inside that window.
        std::thread::sleep(Duration::from_millis(100));
        let published = {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, false);
            assert!(
                st.throttle.values().any(|t| t.pending_since.is_some()),
                "f's second change is held back"
            );
            st.published_seq
        };
        let (fseq, fsize) = slot_seq(&c, "f");
        assert_eq!(fsize, 5);
        assert!(
            fseq < published,
            "the held-back seq sits below a published one"
        );
        // Crash: no stop, no checkpoint, nothing published after this point.
        c.shutdown.store(true, Ordering::Relaxed);
        c.watcher.shutdown();
        drop(fh);
        let c2 = Core::new(root.path(), state.path(), cfg).unwrap();
        {
            let mut st = c2.lock().unwrap();
            c2.ensure_index(&mut st, &[]).unwrap();
        }
        let (fseq2, fsize2) = slot_seq(&c2, "f");
        assert_eq!(fsize2, 5);
        assert!(
            fseq2 > published,
            "a client resuming from {published} must be sent f again (seq {fseq2})"
        );
        let (gseq, _) = slot_seq(&c2, "g");
        assert!(gseq <= published, "published entries keep their seq");
        c2.stop();
    }

    /// Published while no client is connected (link cut): a client that resumes from the last
    /// seq it applied must still get the held-back change, so it goes out under a fresh seq.
    #[test]
    fn throttled_change_published_offline_reaches_a_resuming_client() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("f"), b"v0").unwrap();
        let mut cfg = Config::from_env();
        cfg.hot_interval = Duration::from_secs(3600);
        cfg.hot_quiet = Duration::from_secs(3600);
        let c = Core::new(root.path(), state.path(), cfg).unwrap();
        {
            let mut st = c.lock().unwrap();
            c.ensure_index(&mut st, &[]).unwrap();
        }
        use std::io::Write;
        let mut fh = std::fs::OpenOptions::new()
            .append(true)
            .open(root.path().join("f"))
            .unwrap();
        fh.write_all(b"1").unwrap();
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, false);
        }
        fh.write_all(b"22").unwrap();
        std::fs::write(root.path().join("g"), b"g").unwrap();
        let mut st = c.lock().unwrap();
        c.flush(&mut st, false);
        let resume_from = st.published_seq; // the client's last applied Events seq
        c.publish_throttled(&mut st, true); // nobody connected
        let ix = st.ix.as_ref().unwrap();
        let f = ix.idx.lookup(ix.idx.root, "f").unwrap();
        assert_eq!(ix.idx.node(f).size, 5);
        assert!(
            ix.idx.node(f).seq > resume_from,
            "Resume from {resume_from} must replay f (seq {})",
            ix.idx.node(f).seq
        );
        drop(st);
        drop(fh);
        c.stop();
    }

    fn size_at(c: &Core, rel: &str) -> Option<u64> {
        let st = c.lock().unwrap();
        let ix = st.ix.as_ref().unwrap();
        let mut cur = ix.idx.root;
        for comp in rel.split('/') {
            cur = ix.idx.lookup(cur, comp)?;
        }
        Some(ix.idx.node(cur).size)
    }

    /// Stress seed 930 (every run): `ln a/n2/n3 a/n4; echo >> a/n4; mv a/n4 a/n1; mv
    /// a/n1.tmp a/n1` in one batch. The write changed n3's inode with no event naming n3; the
    /// created name n4 was moved away, so it did not count as a transient link — but its
    /// landing name n1 was renamed over in the same batch, so the inode was observed nowhere
    /// and n3 kept its old size.
    #[test]
    fn link_written_then_moved_onto_a_name_renamed_over_reaches_the_other_link() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir_all(r.join("a/n2")).unwrap();
        std::fs::write(r.join("a/n2/n3"), vec![b'x'; 49]).unwrap();
        let c = core_for(r, state.path());
        {
            // One batch: nothing is processed while the state lock is held.
            let mut st = c.lock().unwrap();
            c.flush(&mut st, true);
            std::fs::hard_link(r.join("a/n2/n3"), r.join("a/n4")).unwrap();
            use std::io::Write;
            let mut h = std::fs::OpenOptions::new()
                .append(true)
                .open(r.join("a/n4"))
                .unwrap();
            h.write_all(b"aa").unwrap();
            drop(h);
            std::fs::rename(r.join("a/n4"), r.join("a/n1")).unwrap();
            std::fs::write(r.join("a/n1.tmp~"), vec![b'r'; 17]).unwrap();
            std::fs::rename(r.join("a/n1.tmp~"), r.join("a/n1")).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        c.barrier().unwrap();
        assert_eq!(size_at(&c, "a/n1"), Some(17));
        assert_eq!(
            size_at(&c, "a/n2/n3"),
            Some(51),
            "the write through the link"
        );
        c.stop();
    }

    fn core_cfg(root: &Path, state: &Path, f: impl FnOnce(&mut Config)) -> Arc<Core> {
        let mut cfg = Config::from_env();
        f(&mut cfg);
        let c = Core::new(root, state, cfg).unwrap();
        let mut st = c.lock().unwrap();
        c.ensure_index(&mut st, &[]).unwrap();
        drop(st);
        c
    }

    fn size_in(st: &State, rel: &str) -> Option<u64> {
        let ix = st.ix.as_ref().unwrap();
        let mut cur = ix.idx.root;
        for comp in rel.split('/') {
            cur = ix.idx.lookup(cur, comp)?;
        }
        Some(ix.idx.node(cur).size)
    }

    /// `ln src x; echo >> x; mv x idx; mv idx.lock idx` — the shape of two atomic saves of
    /// `idx` in one batch, except that the first saved inode is a link to `src`.
    fn link_written_then_renamed_over(r: &Path, src: &str, dir: &str) {
        use std::io::Write;
        let x = r.join(format!("{dir}/x"));
        std::fs::hard_link(r.join(src), &x).unwrap();
        let mut h = std::fs::OpenOptions::new().append(true).open(&x).unwrap();
        h.write_all(b"aa").unwrap();
        drop(h);
        std::fs::rename(&x, r.join(format!("{dir}/idx"))).unwrap();
        std::fs::write(r.join(format!("{dir}/idx.lock")), b"lock").unwrap();
        std::fs::rename(
            r.join(format!("{dir}/idx.lock")),
            r.join(format!("{dir}/idx")),
        )
        .unwrap();
    }

    /// A created inode renamed over unseen (two atomic saves of one file in one batch, as `git`
    /// does with index.lock) re-stats only the files of its own directory at once — a link next
    /// to its file is caught there — and owes one full audit, which the barrier runs: the
    /// other link may be anywhere (here `far/sub/g`, nlink back to 1, in a dir with no event).
    #[test]
    fn renamed_over_created_inode_audits_its_dir_now_and_every_file_at_the_barrier() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir_all(r.join("near")).unwrap();
        std::fs::create_dir_all(r.join("far/sub")).unwrap();
        std::fs::write(r.join("near/f"), vec![b'f'; 10]).unwrap();
        std::fs::write(r.join("far/sub/g"), vec![b'g'; 20]).unwrap();
        let c = core_cfg(r, state.path(), |cfg| {
            cfg.audit_quiet = Duration::from_secs(3600);
            cfg.audit_max_delay = Duration::from_secs(3600);
        });
        let mut st = c.lock().unwrap();
        c.flush(&mut st, true);
        link_written_then_renamed_over(r, "near/f", "near");
        link_written_then_renamed_over(r, "far/sub/g", "near");
        std::thread::sleep(Duration::from_millis(20));
        c.flush(&mut st, false);
        assert_eq!(size_in(&st, "near/idx"), Some(4));
        assert_eq!(size_in(&st, "near/f"), Some(12), "same dir: scoped pass");
        assert_eq!(size_in(&st, "far/sub/g"), Some(20), "no full audit yet");
        assert!(st.audit_due.is_some());
        c.flush(&mut st, true);
        assert_eq!(
            size_in(&st, "far/sub/g"),
            Some(22),
            "the barrier's full audit"
        );
        assert!(st.audit_due.is_none());
        drop(st);
        c.stop();
    }

    /// Without a barrier, the owed full audit runs once events go quiet — once for any number
    /// of suspect batches before that.
    #[test]
    fn owed_audit_runs_when_events_go_quiet() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir_all(r.join("near")).unwrap();
        std::fs::create_dir_all(r.join("far")).unwrap();
        std::fs::write(r.join("far/g"), vec![b'g'; 20]).unwrap();
        let c = core_cfg(r, state.path(), |cfg| {
            cfg.audit_quiet = Duration::from_millis(100);
            cfg.audit_max_delay = Duration::from_secs(3600);
        });
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, true);
            link_written_then_renamed_over(r, "far/g", "near");
            std::thread::sleep(Duration::from_millis(20));
            c.flush(&mut st, false);
            assert_eq!(size_in(&st, "far/g"), Some(20));
            assert!(st.audit_due.is_some());
        }
        let t0 = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let st = c.lock().unwrap();
            if size_in(&st, "far/g") == Some(22) {
                assert!(st.audit_due.is_none());
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "audit never ran");
        }
        c.stop();
    }

    fn append(p: &Path, b: &[u8]) {
        use std::io::Write;
        let mut h = std::fs::OpenOptions::new().append(true).open(p).unwrap();
        h.write_all(b).unwrap();
    }

    /// `near/` and `far/sub/g` (20 bytes); a core whose owed full audit only a barrier runs.
    fn split_fixture(r: &Path, state: &Path) -> Arc<Core> {
        std::fs::create_dir_all(r.join("near")).unwrap();
        std::fs::create_dir_all(r.join("far/sub")).unwrap();
        std::fs::write(r.join("near/f"), vec![b'f'; 10]).unwrap();
        std::fs::write(r.join("far/sub/g"), vec![b'g'; 20]).unwrap();
        core_cfg(r, state, |cfg| {
            cfg.audit_quiet = Duration::from_secs(3600);
            cfg.audit_max_delay = Duration::from_secs(3600);
        })
    }

    fn event_names(evs: &[sys::InotifyEvent]) -> Vec<String> {
        let mut v: Vec<String> = evs
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name).into_owned())
            .collect();
        v.dedup();
        v
    }

    /// `ln far/sub/g near/x; echo aa >> x; mv x idx; mv idx.lock idx` with the batch boundary
    /// right after the write: batch 1 is `x` CREATE+MODIFY, but `x` is gone when it is stat'ed;
    /// batch 2 is `x → idx`, then `idx.lock → idx`. Neither batch alone sees a created inode
    /// lost — the first never saw the inode, the second never saw it created — so the write to
    /// `g` (another dir, no event) is only found by the full audit the split must owe.
    #[test]
    fn split_batch_vanished_created_link_owes_the_full_audit() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        let c = split_fixture(r, state.path());
        let mut st = c.lock().unwrap();
        c.flush(&mut st, true);
        std::fs::hard_link(r.join("far/sub/g"), r.join("near/x")).unwrap();
        append(&r.join("near/x"), b"aa");
        let (ev1, _) = c.watcher.take();
        assert_eq!(event_names(&ev1), ["x"], "batch 1: x CREATE+MODIFY only");
        // The rest of the sequence lands before batch 1 stats `x`.
        std::fs::rename(r.join("near/x"), r.join("near/idx")).unwrap();
        std::fs::write(r.join("near/idx.lock"), b"lock").unwrap();
        std::fs::rename(r.join("near/idx.lock"), r.join("near/idx")).unwrap();
        c.process_batch(&mut st, ev1, false);
        c.flush(&mut st, false);
        assert_eq!(size_in(&st, "near/idx"), Some(4));
        assert!(st.audit_due.is_some(), "the split must owe the full audit");
        c.flush(&mut st, true);
        assert_eq!(size_in(&st, "far/sub/g"), Some(22), "stale after a barrier");
        assert!(st.audit_due.is_none());
        drop(st);
        c.stop();
    }

    /// The same sequence split after `mv x idx`: batch 1 sees `x` created and landing at
    /// `idx` — but `idx` already holds the lock file's inode when it is stat'ed, and the
    /// rename over it is only in batch 2.
    #[test]
    fn split_batch_landing_name_replaced_before_its_stat_owes_the_full_audit() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        let c = split_fixture(r, state.path());
        let mut st = c.lock().unwrap();
        c.flush(&mut st, true);
        std::fs::hard_link(r.join("far/sub/g"), r.join("near/x")).unwrap();
        append(&r.join("near/x"), b"aa");
        std::fs::rename(r.join("near/x"), r.join("near/idx")).unwrap();
        let (ev1, _) = c.watcher.take();
        assert_eq!(event_names(&ev1), ["x", "idx"]);
        std::fs::write(r.join("near/idx.lock"), b"lock").unwrap();
        std::fs::rename(r.join("near/idx.lock"), r.join("near/idx")).unwrap();
        c.process_batch(&mut st, ev1, false);
        c.flush(&mut st, false);
        assert_eq!(size_in(&st, "near/idx"), Some(4));
        assert!(st.audit_due.is_some(), "the split must owe the full audit");
        c.flush(&mut st, true);
        assert_eq!(size_in(&st, "far/sub/g"), Some(22), "stale after a barrier");
        drop(st);
        c.stop();
    }

    /// Split right after the link(2), with `x` still there when batch 1 stats it: the inode is
    /// observed (nlink 2), so batch 2's events on `x` re-stat its other link at once — no
    /// audit is owed.
    #[test]
    fn split_batch_observed_created_link_needs_no_audit() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        let c = split_fixture(r, state.path());
        let mut st = c.lock().unwrap();
        c.flush(&mut st, true);
        std::fs::hard_link(r.join("far/sub/g"), r.join("near/x")).unwrap();
        c.flush(&mut st, false);
        append(&r.join("near/x"), b"aa");
        std::fs::rename(r.join("near/x"), r.join("near/idx")).unwrap();
        std::fs::write(r.join("near/idx.lock"), b"lock").unwrap();
        std::fs::rename(r.join("near/idx.lock"), r.join("near/idx")).unwrap();
        c.flush(&mut st, false);
        assert_eq!(size_in(&st, "far/sub/g"), Some(22));
        assert_eq!(size_in(&st, "near/idx"), Some(4));
        assert!(st.audit_due.is_none());
        drop(st);
        c.stop();
    }

    /// The cheap path survives a batch boundary: an atomic save (`tmp → f`) split between its
    /// write and its rename, and atomic saves of one file in consecutive batches, owe nothing.
    #[test]
    fn split_atomic_saves_owe_no_audit() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        let c = split_fixture(r, state.path());
        let mut st = c.lock().unwrap();
        c.flush(&mut st, true);
        std::fs::write(r.join("near/f.tmp"), b"one").unwrap();
        let (ev1, _) = c.watcher.take();
        std::fs::rename(r.join("near/f.tmp"), r.join("near/f")).unwrap();
        c.process_batch(&mut st, ev1, false);
        c.flush(&mut st, false);
        assert_eq!(size_in(&st, "near/f"), Some(3));
        assert_eq!(size_in(&st, "near/f.tmp"), None);
        for i in 0..5usize {
            std::fs::write(r.join("near/f.tmp"), vec![b'x'; i + 1]).unwrap();
            std::fs::rename(r.join("near/f.tmp"), r.join("near/f")).unwrap();
            c.flush(&mut st, false);
            assert_eq!(size_in(&st, "near/f"), Some(i as u64 + 1));
        }
        assert!(st.audit_due.is_none(), "an ordinary save owes no audit");
        drop(st);
        c.stop();
    }

    #[test]
    fn lost_created_inode_follows_renames() {
        let (c, mf, mt, d) = (
            sys::IN_CREATE,
            sys::IN_MOVED_FROM,
            sys::IN_MOVED_TO,
            sys::IN_DELETE,
        );
        // Atomic save `tmp → f`: the inode is at f.
        assert!(!lost_created_inode(&[(0, c, 0), (0, mf, 7), (1, mt, 7)]));
        // ... then moved on `f → g`: still observed (at g).
        assert!(!lost_created_inode(&[
            (0, c, 0),
            (0, mf, 7),
            (1, mt, 7),
            (1, mf, 8),
            (2, mt, 8)
        ]));
        // Seed 930: `x → f`, then `tmp → f` renames over it.
        assert!(lost_created_inode(&[
            (0, c, 0),
            (0, mf, 7),
            (1, mt, 7),
            (2, c, 0),
            (2, mf, 8),
            (1, mt, 8)
        ]));
        // Landed, then deleted.
        assert!(lost_created_inode(&[
            (0, c, 0),
            (0, mf, 7),
            (1, mt, 7),
            (1, d, 0)
        ]));
        // Moved out of the watched tree (no MOVED_TO).
        assert!(lost_created_inode(&[(0, c, 0), (0, mf, 7)]));
        // A directory is not a hard link.
        let dir = sys::IN_ISDIR;
        assert!(!lost_created_inode(&[(0, c | dir, 0), (0, mf | dir, 7)]));
        // The lost inode's names: created at 0, landed on 1 (renamed over by 2's inode).
        assert_eq!(
            lost_created_names(&[
                (0, c, 0),
                (0, mf, 7),
                (1, mt, 7),
                (2, c, 0),
                (2, mf, 8),
                (1, mt, 8)
            ]),
            vec![0, 1]
        );
        // f was renamed over *before* x landed on it.
        assert!(!lost_created_inode(&[
            (1, mt, 3),
            (0, c, 0),
            (0, mf, 7),
            (1, mt, 7)
        ]));
    }

    /// `held`: where each created file inode not lost is at the end of the batch — what a
    /// split batch carries into the next one when that name already has events queued.
    #[test]
    fn created_inode_fates_name_where_created_inodes_are_held() {
        let (c, mf, mt, d, m) = (
            sys::IN_CREATE,
            sys::IN_MOVED_FROM,
            sys::IN_MOVED_TO,
            sys::IN_DELETE,
            sys::IN_MODIFY,
        );
        // Created and written, nothing else yet: held at its own name.
        assert_eq!(
            created_inode_fates(&[(0, c, 0), (0, m, 0)]),
            (vec![], vec![0])
        );
        // Atomic save `tmp → f`: held at f.
        assert_eq!(
            created_inode_fates(&[(0, c, 0), (0, mf, 7), (1, mt, 7)]),
            (vec![], vec![1])
        );
        // ... moved on `f → g`: held at g.
        assert_eq!(
            created_inode_fates(&[(0, c, 0), (0, mf, 7), (1, mt, 7), (1, mf, 8), (2, mt, 8)]),
            (vec![], vec![2])
        );
        // Created then unlinked, or renamed over: not held (the CREATE+DELETE/MOVED_TO rule).
        assert_eq!(
            created_inode_fates(&[(0, c, 0), (0, d, 0)]),
            (vec![], vec![])
        );
        assert_eq!(
            created_inode_fates(&[(0, c, 0), (1, c, 0), (1, mf, 7), (0, mt, 7)]),
            (vec![], vec![0])
        );
        // Seed 930 shape: x lost (x, f), the second temp held at f.
        assert_eq!(
            created_inode_fates(&[
                (0, c, 0),
                (0, mf, 7),
                (1, mt, 7),
                (2, c, 0),
                (2, mf, 8),
                (1, mt, 8)
            ]),
            (vec![0, 1], vec![1])
        );
        // Dirs are not tracked.
        let dir = sys::IN_ISDIR;
        assert_eq!(created_inode_fates(&[(0, c | dir, 0)]), (vec![], vec![]));
    }

    fn scanned_at(c: &Core, rel: &str) -> bool {
        let st = c.lock().unwrap();
        let ix = st.ix.as_ref().unwrap();
        let mut cur = ix.idx.root;
        for comp in rel.split('/') {
            match ix.idx.lookup(cur, comp) {
                Some(s) => cur = s,
                None => return false,
            }
        }
        ix.idx.node(cur).scanned()
    }

    /// CI stress seed 41 (`dir a02/n3/n5 missing`): the startup verify walk read the root's
    /// listing (`a0`), then `mv a0 a02` ran before a walker opened `a0`. The open failed, so
    /// this process never watched a0 or anything below it, and the live batch that applied the
    /// MOVED_FROM/MOVED_TO pair kept the persisted `scanned` flags without re-listing: every
    /// later change below a02 was invisible (and a Ping barrier does not poll watched dirs).
    #[test]
    fn dir_moved_during_the_verify_walk_is_watched() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path().to_path_buf();
        std::fs::create_dir_all(r.join("a0/n3")).unwrap();
        let c = core_for(&r, state.path());
        c.stop();
        drop(c);
        let c = Core::new(&r, state.path(), Config::from_env()).unwrap();
        let fired = Arc::new(AtomicBool::new(false));
        let (r2, f2) = (r.clone(), fired.clone());
        *c.walk_hook.lock().unwrap() = Some(Box::new(move |rel| {
            if rel == b"a0" && !f2.swap(true, Ordering::SeqCst) {
                std::fs::rename(r2.join("a0"), r2.join("a02")).unwrap();
            }
        }));
        {
            let mut st = c.lock().unwrap();
            c.ensure_index(&mut st, &[]).unwrap();
        }
        c.barrier().unwrap();
        assert!(
            fired.load(Ordering::SeqCst),
            "the walk opened a0 after listing the root"
        );
        assert!(id_at(&c, "a02/n3").is_some());
        std::fs::create_dir(r.join("a02/n3/n5")).unwrap();
        std::fs::write(r.join("a02/f"), b"f").unwrap();
        c.barrier().unwrap();
        assert!(id_at(&c, "a02/f").is_some(), "a02 is watched");
        assert!(id_at(&c, "a02/n3/n5").is_some(), "a02/n3 is watched");
        c.stop();
    }

    /// Debounce 0 (seed 41 locally, ~50%): `mkdir d/n2; mv d/n2 d/n20` where the batch that
    /// saw IN_CREATE stat'ed n2 before the rename and tried to list it after. The listing
    /// failed, the MOVED pair came in the next batch and only moved the node: n20 stayed
    /// unscanned and unwatched for good (published `lazy`, children never indexed).
    #[test]
    fn new_dir_renamed_before_its_first_listing_is_scanned() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir(r.join("d")).unwrap();
        let c = core_for(r, state.path());
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, true);
            std::fs::create_dir(r.join("d/n2")).unwrap();
            // The batch that consumes the IN_CREATE event: n2 is observed (stat'ed and
            // indexed, its listing queued), then the rename lands, then the listing runs.
            let _ = c.watcher.take();
            let root_fd = st.root_fd.as_raw_fd();
            let ix = st.ix.as_mut().unwrap();
            let d = ix.idx.lookup(ix.idx.root, "d").unwrap();
            let mut env = CoreEnv::new(root_fd, &c.watcher, false);
            let mut batch = Batch::default();
            batch.observe(&mut ix.idx, &mut env, d, "n2", H_CONTENT);
            std::fs::rename(r.join("d/n2"), r.join("d/n20")).unwrap();
            batch.settle(&mut ix.idx, &mut env);
            let txn = std::mem::take(&mut batch.txn);
            drop(env);
            c.commit(&mut st, txn, CommitOpts::default()).unwrap();
        }
        c.barrier().unwrap();
        assert!(id_at(&c, "d/n20").is_some());
        assert!(scanned_at(&c, "d/n20"), "n20 is not lazy: it is listed");
        std::fs::create_dir(r.join("d/n20/n4")).unwrap();
        c.barrier().unwrap();
        assert!(id_at(&c, "d/n20/n4").is_some(), "n20 is watched");
        c.stop();
    }

    /// A name whose directory moved after the batch took the event but before it stat'ed the
    /// name (`touch d/x; mv d d2`, the MOVED pair read by the next batch): the name was dropped
    /// as "under a dir that is really gone", although the dir was only moved.
    #[test]
    fn name_blocked_by_a_move_reported_in_a_later_batch_is_applied() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir_all(r.join("d/sub")).unwrap();
        let c = core_for(r, state.path());
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, true);
            std::fs::write(r.join("d/sub/x"), b"xx").unwrap();
            let _ = c.watcher.take();
            std::fs::rename(r.join("d"), r.join("d2")).unwrap();
            let root_fd = st.root_fd.as_raw_fd();
            let ix = st.ix.as_mut().unwrap();
            let d = ix.idx.lookup(ix.idx.root, "d").unwrap();
            let sub = ix.idx.lookup(d, "sub").unwrap();
            let mut env = CoreEnv::new(root_fd, &c.watcher, false);
            let mut batch = Batch::default();
            batch.observe(&mut ix.idx, &mut env, sub, "x", H_CONTENT | H_CLOSE);
            batch.settle(&mut ix.idx, &mut env);
            let txn = std::mem::take(&mut batch.txn);
            drop(env);
            c.commit(&mut st, txn, CommitOpts::default()).unwrap();
        }
        c.barrier().unwrap();
        assert_eq!(size_at(&c, "d2/sub/x"), Some(2));
        c.stop();
    }

    /// `ln f g; truncate g; mv tmp g` inside one batch: the write through the short-lived link
    /// changed f's inode, but no event names f and the link is gone when the batch runs. The
    /// transient created-then-replaced name triggers a stat audit that catches f.
    #[test]
    fn write_through_a_transient_hardlink_reaches_the_other_link() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("c")).unwrap();
        std::fs::create_dir(root.path().join("d")).unwrap();
        std::fs::write(root.path().join("c/f"), vec![b'x'; 43]).unwrap();
        let c = core_for(root.path(), state.path());
        {
            // Hold the lock: the whole burst lands in one batch.
            let st = c.lock().unwrap();
            let r = root.path();
            std::fs::hard_link(r.join("c/f"), r.join("d/g")).unwrap();
            std::fs::write(r.join("d/g"), b"").unwrap();
            std::fs::write(r.join("d/g.tmp"), b"replacement").unwrap();
            std::fs::rename(r.join("d/g.tmp"), r.join("d/g")).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            drop(st);
        }
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, true);
        }
        assert_eq!(size_at(&c, "d/g"), Some(11));
        assert_eq!(
            size_at(&c, "c/f"),
            Some(0),
            "the write through the link reached f"
        );
        c.stop();
    }

    /// A file written just before its directory is renamed: its name is retried after the move
    /// in the same batch, and the links that retry dirties must be re-stat'ed too.
    #[test]
    fn links_dirtied_by_a_deferred_name_are_processed() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir(r.join("a")).unwrap();
        std::fs::create_dir(r.join("c")).unwrap();
        std::fs::write(r.join("a/n1"), b"one").unwrap();
        std::fs::hard_link(r.join("a/n1"), r.join("c/n3")).unwrap();
        let c = core_for(root.path(), state.path());
        {
            let st = c.lock().unwrap();
            use std::io::Write;
            let mut h = std::fs::OpenOptions::new()
                .append(true)
                .open(r.join("a/n1"))
                .unwrap();
            h.write_all(b" more").unwrap();
            drop(h);
            std::fs::rename(r.join("a"), r.join("a1")).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            drop(st);
        }
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, true);
        }
        assert_eq!(size_at(&c, "a1/n1"), Some(8));
        assert_eq!(size_at(&c, "c/n3"), Some(8), "the other link follows");
        c.stop();
    }

    /// `echo x >> d/l; rm -rf d` in one batch, `l` a hard link of `keep`: the name's events go
    /// with its directory, so the removal itself must re-stat the surviving link.
    #[test]
    fn removing_a_link_dir_restats_the_surviving_link() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir_all(r.join("d/sub")).unwrap();
        std::fs::write(r.join("keep"), b"one").unwrap();
        std::fs::hard_link(r.join("keep"), r.join("d/sub/l")).unwrap();
        let c = core_for(r, state.path());
        {
            let st = c.lock().unwrap();
            use std::io::Write;
            let mut h = std::fs::OpenOptions::new()
                .append(true)
                .open(r.join("d/sub/l"))
                .unwrap();
            h.write_all(b" two").unwrap();
            drop(h);
            std::fs::remove_dir_all(r.join("d")).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            drop(st);
        }
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, true);
        }
        assert_eq!(id_at(&c, "d"), None);
        assert_eq!(size_at(&c, "keep"), Some(7));
        c.stop();
    }

    /// `mv d/sub d/sub2; mv d d2` with a file replaced in sub, all in one batch: the file's name
    /// is blocked until *both* moves are applied, which takes two retry rounds.
    #[test]
    fn change_under_nested_moves_in_one_batch_is_applied() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir_all(r.join("d/sub")).unwrap();
        std::fs::write(r.join("d/sub/f"), vec![b'x'; 42]).unwrap();
        let c = core_for(r, state.path());
        {
            let st = c.lock().unwrap();
            std::fs::write(r.join("d/sub/f.tmp"), b"sixsix").unwrap();
            std::fs::rename(r.join("d/sub/f.tmp"), r.join("d/sub/f")).unwrap();
            std::fs::rename(r.join("d/sub"), r.join("d/sub2")).unwrap();
            std::fs::rename(r.join("d"), r.join("d2")).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            drop(st);
        }
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, true);
        }
        assert_eq!(size_at(&c, "d2/sub2/f"), Some(6));
        assert_eq!(id_at(&c, "d2/sub2/f.tmp"), None);
        c.stop();
    }

    /// Until the startup verify walk of an adopted index has run, nothing is watched: a Ping
    /// barrier (and any request) must run it first, or changes made meanwhile are missed.
    #[test]
    fn barrier_runs_a_pending_verify_walk() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("d")).unwrap();
        let c = core_for(root.path(), state.path());
        c.stop();
        drop(c);
        let c = Core::new(root.path(), state.path(), Config::from_env()).unwrap();
        {
            let mut st = c.lock().unwrap();
            c.ensure_index(&mut st, &[]).unwrap();
            assert!(st.verify_pending, "adopted: verify runs after Welcome");
            // The change lands before the walk has watched anything.
            std::fs::write(root.path().join("d/new"), b"n").unwrap();
            drop(st);
        }
        c.barrier().unwrap();
        assert!(
            id_at(&c, "d/new").is_some(),
            "barrier covered the verify walk"
        );
        std::fs::write(root.path().join("d/later"), b"l").unwrap();
        c.barrier().unwrap();
        assert!(
            id_at(&c, "d/later").is_some(),
            "d is watched after the walk"
        );
        c.stop();
    }

    #[test]
    fn root_replaced_gets_a_new_index_id() {
        let base = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let root = base.path().join("r");
        std::fs::create_dir(&root).unwrap();
        let c = core_for(&root, state.path());
        let id1 = c.lock().unwrap().ix.as_ref().unwrap().index_id;
        std::fs::rename(&root, base.path().join("r.old")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, false);
            assert!(
                st.ix.is_none() || st.root_replaced,
                "IN_MOVE_SELF on the root drops the index"
            );
            c.check_root(&mut st).unwrap();
            c.ensure_index(&mut st, &[]).unwrap();
            let id2 = st.ix.as_ref().unwrap().index_id;
            assert_ne!(id1, id2);
        }
        c.stop();
    }

    /// Review (c)7: a client still holds the ids of a lost index (and fileproviderd reconciles a
    /// reimport by identifier), so a rebuilt index must never hand out an id the previous one
    /// issued — not even to the same path.
    #[test]
    fn rebuilt_index_never_reuses_ids() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("d")).unwrap();
        for p in ["a.txt", "b.txt", "d/x.txt"] {
            std::fs::write(root.path().join(p), p).unwrap();
        }
        let ids = |c: &Core| -> (u128, Vec<u64>) {
            let st = c.lock().unwrap();
            let ix = st.ix.as_ref().unwrap();
            let v = ix
                .idx
                .bfs()
                .into_iter()
                .map(|s| ix.idx.node(s).id)
                .collect();
            (ix.index_id, v)
        };
        let c = core_for(root.path(), state.path());
        let (ix1, old) = ids(&c);
        assert_eq!(old.len(), 5);
        c.stop();
        drop(c);
        // The index is lost (its file and journal), the daemon state otherwise intact.
        for e in std::fs::read_dir(state.path()).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n == "index.bin" || n.starts_with("journal.") {
                std::fs::remove_file(e.path()).unwrap();
            }
        }
        let c = core_for(root.path(), state.path());
        let (ix2, new) = ids(&c);
        c.stop();
        assert_ne!(ix1, ix2, "a new index id");
        assert_eq!(new.len(), 5);
        for id in &new {
            assert!(
                *id == ItemId::ROOT.0 || !old.contains(id),
                "id {id} of the new index was issued by the old one ({old:?})"
            );
        }
    }

    #[test]
    fn polled_mode_detects_changes() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut cfg = Config::from_env();
        cfg.force_poll = true;
        let c = Core::new(root.path(), state.path(), cfg).unwrap();
        {
            let mut st = c.lock().unwrap();
            c.ensure_index(&mut st, &[]).unwrap();
            assert!(st.polled_root);
        }
        assert!(!c.watcher.enabled());
        std::fs::write(root.path().join("p.txt"), b"x").unwrap();
        // The barrier re-lists polled dirs (Pong contract) — no race with the poll schedule.
        c.barrier().unwrap();
        assert!(
            id_at(&c, "p.txt").is_some(),
            "polling picked up the new file"
        );
        // …and so does the periodic poll on its own.
        std::fs::write(root.path().join("q.txt"), b"x").unwrap();
        let t0 = Instant::now();
        while id_at(&c, "q.txt").is_none() && t0.elapsed() < Duration::from_secs(120) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(id_at(&c, "q.txt").is_some(), "periodic poll never ran");
        c.stop();
    }

    #[test]
    fn frames_stay_under_64k() {
        let e = |i: u64| {
            Change::Upsert(Entry {
                id: ItemId(i),
                parent: ItemId::ROOT,
                name: format!("{}{i}", "n".repeat(200)),
                kind: unlatch_proto::Kind::File,
                size: i,
                mtime_ns: -1,
                mode: 0o644,
                version: unlatch_proto::Version {
                    content: u64::MAX,
                    meta: u64::MAX,
                },
                symlink_target: Some("t".repeat(100)),
                lazy: false,
                seq: u64::MAX,
                access: 7,
            })
        };
        let changes: Vec<Change> = (0..5000).map(e).collect();
        let frames = event_frames(9, changes);
        assert!(frames.len() > 1);
        let mut total = 0;
        for (i, f) in frames.iter().enumerate() {
            let body = &f[4..];
            // uncompressed size bound: decode and re-encode without compression
            let m: ServerMsg = frame::decode_body(body).unwrap();
            let raw = postcard::to_stdvec(&m).unwrap();
            assert!(raw.len() <= 64 * 1024, "frame {i}: {} bytes", raw.len());
            if let ServerMsg::Events {
                seq,
                changes,
                batch_end,
            } = m
            {
                assert_eq!(seq, 9);
                assert_eq!(batch_end, i + 1 == frames.len());
                total += changes.len();
            }
        }
        assert_eq!(total, 5000);
    }

    fn set_mode(p: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A failing checkpoint (state dir not writable, disk full) is retried with exponential
    /// backoff — not on every ~100 ms tick, each of which encoded the whole index under the core
    /// lock (review: ~60% of a core, 10 retries/s) — and reported as a ServerInfo warning
    /// until it succeeds again.
    #[test]
    fn failed_checkpoint_backs_off_and_warns() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"a").unwrap();
        let c = core_for(root.path(), state.path());
        let mut st = c.lock().unwrap();
        c.checkpoint(&mut st, false);
        assert_eq!(st.cp_failures, 0);
        set_mode(state.path(), 0o500);
        st.journal_dirty = true;
        st.last_checkpoint = Instant::now() - Duration::from_secs(700);
        for _ in 0..20 {
            c.periodic(&mut st);
        }
        let failures = st.cp_failures;
        let warned = st.warnings.iter().any(|w| w.starts_with(WARN_CHECKPOINT));
        let retry_in = st
            .cp_retry_at
            .map(|t| t.saturating_duration_since(Instant::now()));
        // Due again: the next attempt doubles the delay.
        st.cp_retry_at = Some(Instant::now());
        c.periodic(&mut st);
        let failures2 = st.cp_failures;
        let retry_in2 = st
            .cp_retry_at
            .map(|t| t.saturating_duration_since(Instant::now()));
        set_mode(state.path(), 0o700);
        assert_eq!(failures, 1, "retried without backoff");
        assert!(warned, "no warning: {:?}", st.warnings);
        assert!(retry_in.unwrap() > Duration::from_secs(4));
        assert_eq!(failures2, 2);
        assert!(retry_in2.unwrap() > Duration::from_secs(9));
        // Writable again: the next due attempt succeeds and clears the warning.
        st.cp_retry_at = Some(Instant::now());
        c.periodic(&mut st);
        assert_eq!(st.cp_failures, 0);
        assert!(!st.warnings.iter().any(|w| w.starts_with(WARN_CHECKPOINT)));
        let leftovers: Vec<_> = std::fs::read_dir(state.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        drop(st);
        c.stop();
    }

    /// Held changes whose journal appends failed are journaled when the hold is released, so a
    /// crash right after their publication keeps every published id (a reload that lost them
    /// would give the same files new ids, and the Mac would keep the old ones).
    #[test]
    fn released_hold_journals_what_it_publishes() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let c = core_for(root.path(), state.path());
        {
            let mut st = c.lock().unwrap();
            c.checkpoint(&mut st, false);
            // The reserved block is used up: the next commit must rewrite alloc.bin.
            st.store.id_hwm = 0;
            st.store.seq_hwm = 0;
            // Disk full: the journal append fails too.
            st.store.fail_appends = true;
        }
        set_mode(state.path(), 0o500);
        std::fs::create_dir(root.path().join("d")).unwrap();
        std::fs::write(root.path().join("d/g"), b"g").unwrap();
        let published = {
            let mut st = c.lock().unwrap();
            c.flush(&mut st, false);
            assert!(st.held_since.is_some(), "reservation failure must hold");
            assert!(!st.held_journal_ok);
            assert!(
                c.barrier_locked(&mut st).is_err(),
                "Ping must fail while held"
            );
            st.store.fail_appends = false;
            st.published_seq
        };
        assert!(c.deliverable().is_err());
        let g = id_at(&c, "d/g").unwrap();
        set_mode(state.path(), 0o700);
        let seq = {
            let mut st = c.lock().unwrap();
            c.ensure_reserved(&mut st).unwrap();
            assert!(st.held_since.is_none());
            assert!(st.published_seq > published);
            st.published_seq
        };
        assert!(c.deliverable().is_ok());
        // Crash right after the release.
        c.shutdown.store(true, Ordering::Relaxed);
        c.watcher.shutdown();
        let c2 = Core::new(root.path(), state.path(), Config::from_env()).unwrap();
        {
            let mut st = c2.lock().unwrap();
            c2.ensure_index(&mut st, &[]).unwrap();
            assert!(st.ix.as_ref().unwrap().idx.seq >= seq);
        }
        assert_eq!(id_at(&c2, "d/g"), Some(g), "published id lost by a crash");
        c2.stop();
    }

    fn id_in(st: &State, rel: &str) -> Option<u64> {
        let ix = st.ix.as_ref().unwrap();
        let mut cur = ix.idx.root;
        for comp in rel.split('/') {
            cur = ix.idx.lookup(cur, comp)?;
        }
        Some(ix.idx.node(cur).id)
    }

    /// `mv a b; echo new > a` (identity.rs `mv_a_b_then_touch_a`, CI 2026-10-01) with the batch
    /// boundary right after the rename's IN_MOVED_FROM: rename(2) queues it and IN_MOVED_TO one
    /// after the other, and a batch taken in between (the renaming thread preempted) held the
    /// IN_MOVED_FROM alone. The moved inode must keep its id whether that batch stats the old
    /// name before the new file exists (the old node was removed, b came back as a new item)
    /// or after (the old id was merged into the new file). Same for a directory moved into
    /// another one, with a new directory taking its old name: its subtree keeps its ids.
    #[test]
    fn rename_cut_after_its_moved_from_keeps_the_id() {
        for dir in [false, true] {
            for replaced_first in [false, true] {
                let what = format!("dir={dir} new occupant before the cut batch={replaced_first}");
                let root = tempfile::tempdir().unwrap();
                let state = tempfile::tempdir().unwrap();
                let r = root.path();
                std::fs::create_dir_all(r.join("d/sub")).unwrap();
                std::fs::create_dir(r.join("e")).unwrap();
                std::fs::write(r.join("d/sub/f"), b"f").unwrap();
                std::fs::write(r.join("a"), b"A").unwrap();
                let (from, to) = if dir { ("d/sub", "e/sub") } else { ("a", "b") };
                let replace = || {
                    if dir {
                        std::fs::create_dir(r.join(from)).unwrap();
                    } else {
                        std::fs::write(r.join(from), b"new").unwrap();
                    }
                };
                let c = core_for(r, state.path());
                let mut st = c.lock().unwrap();
                c.flush(&mut st, true);
                let old = id_in(&st, from).unwrap();
                let leaf = id_in(&st, "d/sub/f").unwrap();
                std::fs::rename(r.join(from), r.join(to)).unwrap();
                if replaced_first {
                    replace();
                }
                let (mut ev, _) = c.watcher.take();
                assert!(ev[0].mask & sys::IN_MOVED_FROM != 0, "{what}");
                assert!(ev[1].mask & sys::IN_MOVED_TO != 0, "{what}");
                // The batch boundary: IN_MOVED_TO (and what follows) is not read yet.
                c.watcher.requeue(ev.split_off(1));
                c.process_taken(&mut st, ev, false);
                if !replaced_first {
                    replace();
                }
                c.flush(&mut st, true);
                assert_eq!(
                    id_in(&st, to),
                    Some(old),
                    "{what}: the id follows the inode"
                );
                let now = id_in(&st, from).expect("the new occupant is indexed");
                assert_ne!(now, old, "{what}: the new occupant is a new item");
                if dir {
                    assert_eq!(
                        id_in(&st, "e/sub/f"),
                        Some(leaf),
                        "{what}: subtree ids kept"
                    );
                }
                drop(st);
                c.stop();
            }
        }
    }

    #[test]
    fn unpaired_moves_names_moved_from_without_its_moved_to() {
        let ev = |mask, cookie| sys::InotifyEvent {
            wd: 1,
            mask,
            cookie,
            name: b"x".to_vec(),
        };
        let (mf, mt) = (sys::IN_MOVED_FROM, sys::IN_MOVED_TO);
        assert!(unpaired_moves(&[ev(mf, 7), ev(mt, 7)]).is_empty());
        assert_eq!(unpaired_moves(&[ev(mf, 7)]), [(1, 7)]);
        assert_eq!(
            unpaired_moves(&[ev(mt, 6), ev(mf, 7), ev(sys::IN_CREATE, 0)]),
            [(1, 7)]
        );
    }
}
