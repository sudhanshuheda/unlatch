//! Batch reconcile: turn fresh observations of `(dir, name)` pairs and full directory listings
//! into index changes with the identity rules of D4 (review §2(a)3, consistency findings):
//!
//! 1. every observed name is stat'ed fresh (the caller supplies stats through [`Env`]);
//! 2. an inode already known elsewhere *moves* (ids follow inodes; RENAME_EXCHANGE swaps);
//! 3. a node whose inode is gone is *detached* ("missing") until the end of the batch;
//! 4. at the end, a missing node is reused for the item now at its old name only if the kind is
//!    identical, neither is multi-link, and its old inode was not found anywhere in the batch
//!    (atomic save via an unobserved temp → merge; via an observed temp → rename-over keeps the
//!    destination id); otherwise it is removed with a tombstone. When unsure: split, never merge.

use crate::index::{
    Index, NKind, Slot, Txn, F_EXPANDED, F_MOUNT, F_MULTILINK, F_NOEXPAND, F_POLLED, F_SCANNED, NIL,
};
use crate::sys::{self, Stat};
use std::collections::{HashSet, VecDeque};
use std::io;
use std::os::fd::RawFd;

/// Content hint from inotify (IN_MODIFY/IN_CLOSE_WRITE/IN_CREATE/IN_MOVED_TO): bump the content
/// version even if the stat tuple looks unchanged (T17).
pub const H_CONTENT: u8 = 1;
pub const H_CLOSE: u8 = 2;
/// The name appeared by rename (lets identity follow the inode when btime is unavailable).
pub const H_MOVED_TO: u8 = 4;

#[derive(Clone, Debug)]
pub struct Observed {
    pub name: String,
    pub st: Stat,
    pub target: Option<String>,
}

pub enum StatResult {
    Present(Stat, Option<String>),
    Absent,
    /// The directory itself could not be opened at its indexed path (moved in this batch, or
    /// gone): retry after the rest of the batch.
    DirUnavailable,
}

/// What reconcile needs from the outside world.
pub trait Env {
    fn stat(&mut self, idx: &Index, dir: Slot, name: &str) -> StatResult;
    /// Watch (if budget allows) and read `dir` one level. `Some((entries, watched))`.
    fn scan(&mut self, idx: &Index, dir: Slot) -> Option<(Vec<Observed>, bool)>;
    fn dir_removed(&mut self, id: u64);
    fn dir_remapped(&mut self, from: u64, to: u64);
    /// Dir `id` was not at its indexed path when it had to be read (see
    /// [`crate::watch::Watcher::mark_stale`]): re-list it, and re-observe `name`, in a later
    /// batch. The record outlives this batch — the move that explains it may be reported by
    /// the next one.
    fn mark_stale(&mut self, id: u64, name: Option<(String, u8)>);
    /// Take every stale record (each batch retries them once, at its end).
    fn take_stale(&mut self) -> Vec<(u64, Vec<(String, u8)>)>;
    /// This process holds an inotify watch on dir `id` (or the whole root is polled).
    fn watching(&mut self, id: u64) -> bool;
}

/// Names unlatchd never shows: its own staging files (§2(d)2) and NFS silly-renames.
pub fn ignored_name(name: &[u8]) -> bool {
    if let Some(rest) = name.strip_prefix(b".unlatch-") {
        return rest.len() >= 32 && rest[..32].iter().all(|b| b.is_ascii_hexdigit());
    }
    name.starts_with(b".nfs")
}

/// Read one directory level through an open dir fd: `(name, statx, symlink target)` for every
/// exposable child (regular files, dirs, symlinks with UTF-8 names/targets).
pub fn read_listing(dirfd: RawFd) -> io::Result<Vec<Observed>> {
    read_listing_ex(dirfd, false)
}

/// Staging files older than this are leftovers of a crashed unlatchd (§2(d)2 "startup sweeps").
const STALE_STAGING_NS: i64 = 10 * 60 * 1_000_000_000;

/// [`read_listing`]; `sweep` also unlinks stale `.unlatch-<op>` staging files (startup walk).
pub fn read_listing_ex(dirfd: RawFd, sweep: bool) -> io::Result<Vec<Observed>> {
    let ents = sys::read_dir_fd(dirfd)?;
    let mut out = Vec::with_capacity(ents.len());
    for e in ents {
        if ignored_name(&e.name) {
            if sweep && e.name.starts_with(b".unlatch-") {
                if let Ok(st) = sys::statat(dirfd, &e.name) {
                    if st.is_file()
                        && sys::now_ns() - st.mtime_ns.max(st.ctime_ns) > STALE_STAGING_NS
                    {
                        let _ = sys::unlinkat(dirfd, &e.name, false);
                    }
                }
            }
            continue;
        }
        let Ok(name) = String::from_utf8(e.name) else {
            continue; // non-UTF-8 names are not exposed (DESIGN §8)
        };
        if !unlatch_proto::valid_name(&name) {
            continue;
        }
        match stat_child(dirfd, &name) {
            Ok(Some((st, target))) => out.push(Observed { name, st, target }),
            Ok(None) => {}
            Err(e) => crate::log!("statx {name}: {e}"),
        }
    }
    Ok(out)
}

/// statx + readlink of one child. `Ok(None)`: absent or not an exposable type.
pub fn stat_child(dirfd: RawFd, name: &str) -> io::Result<Option<(Stat, Option<String>)>> {
    let st = match sys::statat(dirfd, name.as_bytes()) {
        Ok(st) => st,
        Err(e) if sys::is_errno(&e, libc::ENOENT) || sys::is_errno(&e, libc::ENOTDIR) => {
            return Ok(None)
        }
        Err(e) => return Err(e),
    };
    if NKind::of(&st).is_none() {
        return Ok(None);
    }
    let target = if st.is_symlink() {
        match sys::readlinkat(dirfd, name.as_bytes()) {
            Ok(t) => match String::from_utf8(t) {
                Ok(t) => Some(t),
                Err(_) => return Ok(None),
            },
            Err(e) if sys::is_errno(&e, libc::ENOENT) => return Ok(None),
            Err(e) => return Err(e),
        }
    } else {
        None
    };
    Ok(Some((st, target)))
}

struct Missing {
    slot: Slot,
    parent: Slot,
    name: String,
}

/// One reconcile batch (may span a whole verify walk).
#[derive(Default)]
pub struct Batch {
    pub txn: Txn,
    missing: Vec<Missing>,
    new_nodes: HashSet<Slot>,
    moved: HashSet<Slot>,
    /// Dirs that need a one-level scan (new non-lazy dirs; with `rescan_existing`, every scanned
    /// child dir of a listed dir too — the verify walk).
    pub to_scan: VecDeque<Slot>,
    pub rescan_existing: bool,
    /// Racy-git rule (D3): force a content bump for entries whose mtime/ctime is at/after this.
    pub racy_after_ns: Option<i64>,
    /// Multi-link files whose other links must be re-stat'ed (each link at most once per
    /// batch, so links never ping-pong).
    link_dirty: Vec<(Slot, String)>,
    links_done: HashSet<Slot>,
    deferred: Vec<(Slot, String, u8)>,
    /// Scanned dirs observed by a batch that does not re-list existing dirs: re-listed if this
    /// process does not watch them (their listing in this process failed — e.g. the startup
    /// walk lost a race with a move — so nothing below them is watched either).
    check_watch: Vec<Slot>,
    /// Blocked names carried over from an earlier batch, re-observed after their dir's scan.
    retry_names: Vec<(Slot, String, u8)>,
    /// Dirs expanded (by ListDir) in this batch whose cold children may be restored.
    pub cold_dirs: HashSet<u64>,
    /// `(parent, name)` of a directory a client is creating: if it is lazy by rule (lazy name,
    /// or inside an expanded lazy dir), it is expanded at once — the client holds its (empty)
    /// listing, and fileproviderd never enumerates a folder it created itself.
    pub expand_new: Option<(Slot, String)>,
    pub new_dir_count: usize,
    /// Startup verify: without btime an inode match elsewhere is not trusted (D4).
    pub restart_mode: bool,
    dup_dirs: Vec<Slot>,
}

impl Batch {
    fn detach(&mut self, idx: &mut Index, s: Slot) {
        if idx.is_detached(s) {
            return;
        }
        if idx.node(s).has(F_MULTILINK) {
            // One link fewer: the remaining links' nlink changed (no event names them).
            self.dirty_links(idx, s);
        }
        let (parent, name) = {
            let n = idx.node(s);
            (n.parent, idx.name(s).to_string())
        };
        idx.detach(s);
        self.missing.push(Missing {
            slot: s,
            parent,
            name,
        });
    }

    /// Re-stat every other link of the inode behind `s` later in this batch (review (a)3: an
    /// event on the inode dirties all of its links). Each link at most once per batch.
    fn dirty_links(&mut self, idx: &Index, s: Slot) {
        self.links_done.insert(s);
        for l in idx.links_of(s) {
            if self.links_done.insert(l) && !idx.is_detached(l) {
                let (p, n) = (idx.node(l).parent, idx.name(l).to_string());
                self.link_dirty.push((p, n));
            }
        }
    }

    fn racy(&self, st: &Stat) -> bool {
        matches!(self.racy_after_ns, Some(t) if st.mtime_ns >= t || st.ctime_ns >= t)
    }

    /// Observe `(dir, name)` (inotify dirty name, or a listing entry).
    pub fn observe(&mut self, idx: &mut Index, env: &mut dyn Env, dir: Slot, name: &str, hint: u8) {
        if !idx.alive(dir) {
            return;
        }
        match env.stat(idx, dir, name) {
            StatResult::DirUnavailable => self.deferred.push((dir, name.to_string(), hint)),
            StatResult::Absent => {
                if let Some(c) = idx.lookup(dir, name) {
                    self.detach(idx, c);
                }
            }
            StatResult::Present(st, target) => self.apply(idx, dir, name, &st, target, hint),
        }
    }

    /// Apply one present observation.
    pub fn apply(
        &mut self,
        idx: &mut Index,
        dir: Slot,
        name: &str,
        st: &Stat,
        target: Option<String>,
        hint: u8,
    ) {
        let kind = NKind::of(st).filter(|_| !idx.is_excluded(st));
        let Some(kind) = kind else {
            if let Some(c) = idx.lookup(dir, name) {
                self.detach(idx, c);
            }
            return;
        };
        let force = hint & H_CONTENT != 0 || self.racy(st);
        if hint & H_CLOSE != 0 {
            // Recorded after the node is known below.
        }
        let cur = idx.lookup(dir, name);
        if let Some(c) = cur {
            if idx.same_identity(c, st) {
                let moved = self.moved.contains(&c);
                let before = idx.node(c).seq;
                idx.update_stat(c, st, target, force, !moved, &mut self.txn);
                if hint & H_CLOSE != 0 {
                    self.txn.closed.insert(c);
                }
                if idx.node(c).has(F_MULTILINK) && idx.node(c).seq != before {
                    self.dirty_links(idx, c);
                }
                if kind == NKind::Dir {
                    self.recheck_dir(idx, c);
                }
                return;
            }
            self.detach(idx, c);
        }
        // Inode known elsewhere → it moved here (ids follow inodes).
        let multi = kind == NKind::File && st.nlink > 1;
        let known = if multi { None } else { idx.ident_lookup(st) };
        let known = known.filter(|&k| {
            // Without btime an inode number alone is too weak across locations (inode reuse):
            // follow it only for a real rename (MOVED_TO) — D4 "when unsure, split".
            let n = idx.node(k);
            let strong = n.btime != 0 && st.btime_ns != 0;
            (strong || !self.restart_mode || hint & H_MOVED_TO != 0)
                && !idx.is_ancestor_or_self(k, dir)
        });
        // A file inode still indexed at its old name that appears here by IN_CREATE (no
        // MOVED_TO) was hard-linked, not moved: a rename always reports MOVED_TO. Its nlink
        // reads 1 when the stat races an unlink of either name (`ln f x; rm x`): following
        // the inode would move f's id to x, and `rm x` would then remove f. New node; f's
        // name is re-stat'ed (if it is gone, its own event removes it).
        let linked = known.filter(|&k| {
            kind == NKind::File
                && !idx.is_detached(k)
                && hint & H_CONTENT != 0
                && hint & H_MOVED_TO == 0
        });
        if let Some(k) = linked {
            if self.links_done.insert(k) {
                let (p, n) = (idx.node(k).parent, idx.name(k).to_string());
                self.link_dirty.push((p, n));
            }
        }
        let known = known.filter(|_| linked.is_none());
        if let Some(k) = known {
            if idx.is_detached(k) {
                let pos = self.missing.iter().position(|m| m.slot == k);
                let old = pos.map(|p| self.missing.remove(p));
                match old {
                    Some(m) => idx.attach(k, dir, name, Some((m.parent, &m.name)), &mut self.txn),
                    None => idx.attach(k, dir, name, None, &mut self.txn),
                }
            } else {
                idx.move_node(k, dir, name, &mut self.txn);
            }
            self.moved.insert(k);
            idx.update_stat(k, st, target, force, false, &mut self.txn);
            if hint & H_CLOSE != 0 {
                self.txn.closed.insert(k);
            }
            if kind == NKind::Dir {
                // Verify walk: a dir that moved while nobody watched is re-listed (and re-watched
                // by this process's inotify instance) like one that stayed in place.
                self.recheck_dir(idx, k);
            }
            return;
        }
        // New item (or a collapsed one coming back).
        let dir_id = idx.node(dir).id;
        if self.cold_dirs.contains(&dir_id) {
            let cold_match = idx.cold.get(&dir_id).and_then(|v| {
                v.iter().position(|c| {
                    c.name == name
                        && c.kind == kind
                        && c.dev == st.dev
                        && c.ino == st.ino
                        && (c.btime == 0
                            || st.btime_ns == 0
                            || c.btime == crate::index::btime_fp(st.btime_ns))
                })
            });
            if let Some(i) = cold_match {
                let c = idx.cold.get_mut(&dir_id).map(|v| v.swap_remove(i));
                if let Some(c) = c {
                    if idx.slot_of(c.id).is_none() {
                        let s = idx.create_from_cold(dir, &c, st, target, &mut self.txn);
                        if kind == NKind::Dir {
                            self.classify_dir(idx, s, dir, name, st);
                        }
                        return;
                    }
                }
            }
        }
        let dup_dir =
            kind == NKind::Dir && idx.ident_lookup(st).is_some_and(|k| !idx.is_detached(k));
        let s = idx.create(dir, name, st, kind, target, &mut self.txn);
        self.new_nodes.insert(s);
        if multi {
            // A new link to a known inode: the other links' nlink (and ctime) changed without
            // any event naming them (link(2) sends IN_ATTRIB to the inode's own watches only).
            self.dirty_links(idx, s);
        }
        if hint & H_CLOSE != 0 {
            self.txn.closed.insert(s);
        }
        if kind == NKind::Dir {
            self.new_dir_count += 1;
            if dup_dir {
                idx.set_flags(s, F_NOEXPAND, 0);
                self.dup_dirs.push(s);
            }
            self.classify_dir(idx, s, dir, name, st);
        }
    }

    /// An existing dir was observed (in place or moved here). The verify walk re-lists every
    /// scanned dir, and also lists a non-lazy dir an earlier process failed to (persisted
    /// unscanned); other batches re-list a scanned dir only if this process does not watch it.
    fn recheck_dir(&mut self, idx: &Index, s: Slot) {
        let n = idx.node(s);
        if n.scanned() {
            if self.rescan_existing {
                self.to_scan.push_back(s);
            } else if !n.has(F_POLLED) {
                self.check_watch.push(s);
            }
        } else if self.rescan_existing && wants_scan(idx, s) {
            self.to_scan.push_back(s);
        }
    }

    /// Decide whether a newly indexed dir is scanned now or stays lazy (D13, §2(d)7).
    fn classify_dir(&mut self, idx: &mut Index, s: Slot, parent: Slot, name: &str, st: &Stat) {
        let pdev = idx.dev_of(idx.node(parent));
        if pdev != st.dev {
            idx.set_flags(s, F_MOUNT, 0);
        }
        let n = idx.node(s);
        let blocked = n.has(F_MOUNT) || n.has(F_NOEXPAND);
        let lazy_rule = idx.node(parent).has(F_EXPANDED) || idx.is_lazy_name(name);
        if !blocked
            && lazy_rule
            && self
                .expand_new
                .as_ref()
                .is_some_and(|(p, nm)| *p == parent && nm == name)
        {
            idx.set_flags(s, F_EXPANDED, 0);
            self.to_scan.push_back(s);
            return;
        }
        if !(blocked || lazy_rule) {
            self.to_scan.push_back(s);
        }
    }

    /// A complete one-level listing of `dir`: children not listed are missing.
    pub fn apply_listing(&mut self, idx: &mut Index, dir: Slot, entries: &[Observed]) {
        if !idx.alive(dir) {
            return;
        }
        let names: HashSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        let gone: Vec<Slot> = idx
            .children(dir)
            .filter(|&c| !names.contains(idx.name(c)))
            .collect();
        for c in gone {
            self.detach(idx, c);
        }
        for e in entries {
            self.apply(idx, dir, &e.name, &e.st, e.target.clone(), 0);
        }
    }

    /// Scan queued dirs (serially, through `env`), re-stat dirty links, retry deferred names,
    /// (call [`Batch::finish`] afterwards, or use [`Batch::settle`]).
    pub fn run(&mut self, idx: &mut Index, env: &mut dyn Env) {
        let mut stale_retried = false;
        loop {
            let mut progressed = false;
            while let Some(d) = self.to_scan.pop_front() {
                progressed = true;
                self.scan_one(idx, env, d);
            }
            for s in std::mem::take(&mut self.check_watch) {
                if idx.alive(s)
                    && !idx.is_detached(s)
                    && idx.node(s).kind() == NKind::Dir
                    && idx.node(s).scanned()
                    && !idx.node(s).has(F_POLLED)
                    && !env.watching(idx.node(s).id)
                {
                    progressed = true;
                    self.to_scan.push_back(s);
                }
            }
            let links = std::mem::take(&mut self.link_dirty);
            for (p, n) in links {
                progressed = true;
                self.observe(idx, env, p, &n, H_CONTENT);
            }
            for (d, n, h) in std::mem::take(&mut self.retry_names) {
                progressed = true;
                self.observe(idx, env, d, &n, h);
            }
            if progressed {
                continue;
            }
            // Names under dirs that moved in this batch: retry now that (some) moves are
            // applied. A retry can itself apply a move that unblocks other names (`mv d/sub
            // d/sub2; mv d d2` with a change in sub), so rounds repeat while any name got
            // through; what they dirty (links, new dirs) runs in the next round of the loop.
            let deferred = std::mem::take(&mut self.deferred);
            if deferred.is_empty() {
                if !stale_retried && self.retry_stale(idx, env) {
                    stale_retried = true;
                    continue;
                }
                break;
            }
            let mut blocked = Vec::new();
            let mut through = false;
            for (d, n, h) in deferred {
                if !idx.alive(d) || idx.is_detached(d) {
                    blocked.push((d, n, h));
                    continue;
                }
                match env.stat(idx, d, &n) {
                    StatResult::Present(st, t) => {
                        through = true;
                        self.apply(idx, d, &n, &st, t, h);
                    }
                    StatResult::Absent => {
                        through = true;
                        if let Some(c) = idx.lookup(d, &n) {
                            self.detach(idx, c);
                        }
                    }
                    StatResult::DirUnavailable => blocked.push((d, n, h)),
                }
            }
            if !through {
                // Nothing moved in this batch: the rest are under dirs that are gone — or that
                // moved with the MOVED pair not read yet (it may be in the next batch). Never
                // drop them: the dir is re-listed and the names re-observed once it is
                // reachable again (`mark_stale`); a removed dir drops its record.
                for (d, n, h) in blocked {
                    if idx.alive(d) {
                        env.mark_stale(idx.node(d).id, Some((n, h)));
                    }
                }
                if !stale_retried && self.retry_stale(idx, env) {
                    stale_retried = true;
                    continue;
                }
                break;
            }
            self.deferred.extend(blocked);
        }
        self.deferred.clear();
    }

    /// Queue every stale dir (one this or an earlier batch could not read at its indexed path)
    /// that is attached now: re-list it, then re-observe its blocked names. A dir still
    /// unreachable is marked stale again by its scan / observation. False: nothing queued.
    fn retry_stale(&mut self, idx: &Index, env: &mut dyn Env) -> bool {
        let mut queued = false;
        for (id, names) in env.take_stale() {
            let Some(s) = idx.slot_of(id) else { continue };
            if !idx.alive(s) || idx.node(s).kind() != NKind::Dir {
                continue;
            }
            if !wants_scan(idx, s) {
                // Lazy (collapsed meanwhile, or never meant to be listed): nothing to redo.
                continue;
            }
            if idx.rel_path(s).is_none() {
                // Detached (or under a detached dir) right now: settled by `finish`, which
                // drops the record if it removes the dir.
                env.mark_stale(id, None);
                for (n, h) in names {
                    env.mark_stale(id, Some((n, h)));
                }
                continue;
            }
            self.to_scan.push_back(s);
            queued = true;
            for (n, h) in names {
                self.retry_names.push((s, n, h));
            }
        }
        queued
    }

    pub fn scan_one(&mut self, idx: &mut Index, env: &mut dyn Env, d: Slot) {
        if !idx.alive(d) || idx.is_detached(d) || idx.node(d).kind() != NKind::Dir {
            return;
        }
        // (A failed scan marks the dir stale in `env`: retried by a later batch.)
        if let Some((entries, watched)) = env.scan(idx, d) {
            let was_scanned = idx.node(d).scanned();
            let polled = if watched { 0 } else { F_POLLED };
            idx.set_flags(d, F_SCANNED | polled, if watched { F_POLLED } else { 0 });
            if !was_scanned {
                idx.touch_other(d, &mut self.txn);
            }
            self.apply_listing(idx, d, &entries);
        }
    }

    /// Run to completion: scans, finish, and re-evaluate dirs that looked like duplicates of a
    /// dir that turned out to be gone (a move seen as remove+add).
    pub fn settle(&mut self, idx: &mut Index, env: &mut dyn Env) {
        loop {
            self.run(idx, env);
            self.finish(idx, env);
            let dups = std::mem::take(&mut self.dup_dirs);
            let mut again = false;
            for d in dups {
                if !idx.alive(d) || !idx.node(d).has(F_NOEXPAND) {
                    continue;
                }
                let other = idx.ident_other(d).filter(|&k| !idx.is_detached(k));
                if other.is_none() {
                    idx.set_flags(d, 0, F_NOEXPAND);
                    let st = node_stat(idx, d);
                    let (p, name) = (idx.node(d).parent, idx.name(d).to_string());
                    self.classify_dir(idx, d, p, &name, &st);
                    again = true;
                } else {
                    self.dup_dirs.push(d);
                }
            }
            // Removals in `finish` can dirty surviving hard links: one more round for them.
            if !again && self.link_dirty.is_empty() {
                self.dup_dirs.clear();
                return;
            }
        }
    }

    /// Settle every node still detached (see module docs). Must run once per batch, last.
    pub fn finish(&mut self, idx: &mut Index, env: &mut dyn Env) {
        let missing = std::mem::take(&mut self.missing);
        for m in missing {
            let s = m.slot;
            if !idx.alive(s) || !idx.is_detached(s) {
                continue;
            }
            let occupant = if idx.alive(m.parent) && !idx.is_detached(m.parent) {
                idx.lookup(m.parent, &m.name)
            } else {
                None
            };
            let reuse = occupant.filter(|&o| {
                let (mn, on) = (idx.node(s), idx.node(o));
                mn.kind() == on.kind() && !mn.has(F_MULTILINK) && !on.has(F_MULTILINK)
            });
            match reuse {
                Some(o) if self.new_nodes.contains(&o) => {
                    // Atomic replace by an unobserved temp (or rm+recreate): the old id survives.
                    let (mid, oid) = (idx.node(s).id, idx.node(o).id);
                    let old_kids: Vec<Slot> = idx.children(s).collect();
                    for k in old_kids {
                        self.remove(idx, env, k);
                    }
                    idx.transplant(s, o, &mut self.txn);
                    self.new_nodes.remove(&o);
                    idx.detach(o);
                    let mut t = std::mem::take(&mut self.txn);
                    idx.remove_subtree(o, false, &mut t);
                    // o was never published: forget the drop record.
                    t.dropped.retain(|&d| d != oid);
                    self.txn = t;
                    env.dir_remapped(oid, mid);
                    idx.attach(
                        s,
                        m.parent,
                        &m.name,
                        Some((m.parent, &m.name)),
                        &mut self.txn,
                    );
                }
                Some(o) if self.moved.contains(&o) && idx.node(o).kind() != NKind::Dir => {
                    // Rename-over with an observed temp: the destination id survives (D4).
                    idx.transplant(s, o, &mut self.txn);
                    idx.detach(o);
                    let mut t = std::mem::take(&mut self.txn);
                    idx.remove_subtree(o, true, &mut t);
                    self.txn = t;
                    idx.attach(
                        s,
                        m.parent,
                        &m.name,
                        Some((m.parent, &m.name)),
                        &mut self.txn,
                    );
                }
                _ => self.remove(idx, env, s),
            }
        }
    }

    fn remove(&mut self, idx: &mut Index, env: &mut dyn Env, s: Slot) {
        let mut t = std::mem::take(&mut self.txn);
        let removed = idx.remove_subtree(s, true, &mut t);
        self.txn = t;
        for &r in &removed {
            let n = idx.node(r);
            if n.kind() == NKind::Dir {
                env.dir_removed(n.id);
            }
            self.new_nodes.remove(&r);
        }
        // A removed file's surviving links lost one nlink — and may have been written through
        // the removed name just before (`echo x >> d/l; rm -rf d`: the name's events are dropped
        // with its directory). Re-stat them.
        for &r in &removed {
            let n = idx.node(r);
            if n.kind() != NKind::File {
                continue;
            }
            for l in idx.links_of_ident(n.dev, n.ino, n.btime) {
                if idx.alive(l) && !idx.is_detached(l) && self.links_done.insert(l) {
                    let (p, nm) = (idx.node(l).parent, idx.name(l).to_string());
                    self.link_dirty.push((p, nm));
                }
            }
        }
    }

    pub fn is_new(&self, s: Slot) -> bool {
        self.new_nodes.contains(&s)
    }
}

/// Whether dir `s` should be listed and watched: it is scanned already (an expanded lazy dir
/// or mount point included), or it is neither blocked (mount point / duplicate) nor lazy by
/// rule (lazy name, or child of an expanded dir).
fn wants_scan(idx: &Index, s: Slot) -> bool {
    let n = idx.node(s);
    if n.kind() != NKind::Dir {
        return false;
    }
    if n.scanned() {
        return true;
    }
    if n.has(F_MOUNT) || n.has(F_NOEXPAND) {
        return false;
    }
    let p = n.parent;
    let lazy = (p != NIL && idx.node(p).has(F_EXPANDED)) || idx.is_lazy_name(idx.name(s));
    !lazy
}

/// Reconstruct the identity part of a node's stat.
pub fn node_stat(idx: &Index, s: Slot) -> Stat {
    let n = idx.node(s);
    let t = match n.kind() {
        NKind::Dir => libc::S_IFDIR,
        NKind::File => libc::S_IFREG,
        NKind::Symlink => libc::S_IFLNK,
    };
    Stat {
        dev: idx.dev_of(n),
        ino: n.ino,
        btime_ns: 0,
        mode: t | n.perm as u32,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: n.size,
        mtime_ns: n.mtime_ns,
        ctime_ns: n.ctime_ns,
    }
}

/// Index identity check of an open directory fd.
pub fn fd_matches(idx: &Index, s: Slot, fd: RawFd) -> bool {
    match sys::fstat(fd) {
        Ok(st) => idx.same_identity(s, &st),
        Err(_) => false,
    }
}

/// Keep NIL referenced for readers of this module's docs.
pub const _NIL: Slot = NIL;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Txn;
    use std::collections::HashMap;

    /// In-memory fake filesystem: dir id → name → stat.
    #[derive(Default)]
    struct Fake {
        dirs: HashMap<u64, HashMap<String, Stat>>,
        removed: Vec<u64>,
        stale: HashMap<u64, Vec<(String, u8)>>,
    }

    impl Env for Fake {
        fn stat(&mut self, idx: &Index, dir: Slot, name: &str) -> StatResult {
            match self.dirs.get(&idx.node(dir).id) {
                None => StatResult::DirUnavailable,
                Some(d) => match d.get(name) {
                    Some(st) => StatResult::Present(*st, None),
                    None => StatResult::Absent,
                },
            }
        }
        fn scan(&mut self, idx: &Index, dir: Slot) -> Option<(Vec<Observed>, bool)> {
            let Some(d) = self.dirs.get(&idx.node(dir).id) else {
                self.stale.entry(idx.node(dir).id).or_default();
                return None;
            };
            Some((
                d.iter()
                    .map(|(n, st)| Observed {
                        name: n.clone(),
                        st: *st,
                        target: None,
                    })
                    .collect(),
                true,
            ))
        }
        fn dir_removed(&mut self, id: u64) {
            self.removed.push(id);
        }
        fn dir_remapped(&mut self, _from: u64, _to: u64) {}
        fn mark_stale(&mut self, id: u64, name: Option<(String, u8)>) {
            let e = self.stale.entry(id).or_default();
            e.extend(name);
        }
        fn take_stale(&mut self) -> Vec<(u64, Vec<(String, u8)>)> {
            self.stale.drain().collect()
        }
        fn watching(&mut self, _id: u64) -> bool {
            true
        }
    }

    fn file(ino: u64) -> Stat {
        Stat {
            dev: 1,
            ino,
            btime_ns: ino as i64,
            mode: libc::S_IFREG | 0o644,
            nlink: 1,
            size: 1,
            ..Default::default()
        }
    }
    fn dir(ino: u64) -> Stat {
        Stat {
            dev: 1,
            ino,
            btime_ns: ino as i64,
            mode: libc::S_IFDIR | 0o755,
            nlink: 2,
            ..Default::default()
        }
    }

    fn setup(files: &[(&str, Stat)]) -> (Index, Fake, Slot) {
        let mut idx = Index::new(vec!["node_modules".into()]);
        let r = idx.create_root("root", &dir(1));
        let mut fake = Fake::default();
        fake.dirs
            .insert(1, files.iter().map(|(n, s)| (n.to_string(), *s)).collect());
        let mut b = Batch::default();
        b.to_scan.push_back(r);
        b.settle(&mut idx, &mut fake);
        (idx, fake, r)
    }

    fn batch(idx: &mut Index, fake: &mut Fake, r: Slot, names: &[(&str, u8)]) -> Batch {
        let mut b = Batch::default();
        for (n, h) in names {
            b.observe(idx, fake, r, n, *h);
        }
        b.settle(idx, fake);
        b
    }

    fn id_at(idx: &Index, r: Slot, n: &str) -> Option<u64> {
        idx.lookup(r, n).map(|s| idx.node(s).id)
    }

    #[test]
    fn mv_a_b_touch_a_in_one_batch() {
        let (mut idx, mut fake, r) = setup(&[("a", file(10))]);
        let a = id_at(&idx, r, "a").unwrap();
        let root = fake.dirs.get_mut(&1).unwrap();
        root.remove("a");
        root.insert("b".into(), file(10));
        root.insert("a".into(), file(11));
        batch(
            &mut idx,
            &mut fake,
            r,
            &[("a", H_CONTENT), ("b", H_MOVED_TO)],
        );
        assert_eq!(id_at(&idx, r, "b"), Some(a), "id follows the inode");
        let na = id_at(&idx, r, "a").unwrap();
        assert_ne!(na, a, "the new a is a new item");
        // reverse processing order gives the same answer
        let (mut idx, mut fake, r) = setup(&[("a", file(10))]);
        let a = id_at(&idx, r, "a").unwrap();
        let root = fake.dirs.get_mut(&1).unwrap();
        root.remove("a");
        root.insert("b".into(), file(10));
        root.insert("a".into(), file(11));
        batch(
            &mut idx,
            &mut fake,
            r,
            &[("b", H_MOVED_TO), ("a", H_CONTENT)],
        );
        assert_eq!(id_at(&idx, r, "b"), Some(a));
        assert_ne!(id_at(&idx, r, "a"), Some(a));
    }

    #[test]
    fn rename_over_keeps_destination_id() {
        // Observed temp: tmp (ino 20) was indexed, then `mv tmp b` over b (ino 10).
        let (mut idx, mut fake, r) = setup(&[("b", file(10)), ("tmp", file(20))]);
        let b = id_at(&idx, r, "b").unwrap();
        let tmp = id_at(&idx, r, "tmp").unwrap();
        let root = fake.dirs.get_mut(&1).unwrap();
        root.remove("tmp");
        root.insert("b".into(), file(20));
        let bt = batch(&mut idx, &mut fake, r, &[("tmp", 0), ("b", H_MOVED_TO)]);
        assert_eq!(id_at(&idx, r, "b"), Some(b), "destination id survives");
        assert!(idx.slot_of(tmp).is_none(), "temp id removed");
        assert!(bt.txn.removed.iter().any(|&(id, _)| id == tmp));
        let s = idx.lookup(r, "b").unwrap();
        assert_eq!(idx.node(s).ino, 20);
        // Unobserved temp: new inode appears at b directly.
        let c0 = idx.node(s).content_seq;
        fake.dirs.get_mut(&1).unwrap().insert("b".into(), file(30));
        batch(&mut idx, &mut fake, r, &[("b", H_MOVED_TO)]);
        assert_eq!(id_at(&idx, r, "b"), Some(b));
        let s = idx.lookup(r, "b").unwrap();
        assert!(idx.node(s).content_seq > c0);
    }

    #[test]
    fn exchange_swaps_ids() {
        let (mut idx, mut fake, r) = setup(&[("x", file(10)), ("y", file(20))]);
        let (x, y) = (id_at(&idx, r, "x").unwrap(), id_at(&idx, r, "y").unwrap());
        let root = fake.dirs.get_mut(&1).unwrap();
        root.insert("x".into(), file(20));
        root.insert("y".into(), file(10));
        batch(
            &mut idx,
            &mut fake,
            r,
            &[("x", H_MOVED_TO), ("y", H_MOVED_TO)],
        );
        assert_eq!(id_at(&idx, r, "x"), Some(y));
        assert_eq!(id_at(&idx, r, "y"), Some(x));
    }

    #[test]
    fn kind_change_never_reuses_id() {
        let (mut idx, mut fake, r) = setup(&[("f", file(10))]);
        let f = id_at(&idx, r, "f").unwrap();
        fake.dirs.get_mut(&1).unwrap().insert("f".into(), dir(11));
        fake.dirs.insert(99, HashMap::new());
        let b = batch(&mut idx, &mut fake, r, &[("f", H_CONTENT)]);
        let nf = id_at(&idx, r, "f").unwrap();
        assert_ne!(nf, f);
        assert!(b.txn.removed.iter().any(|&(id, _)| id == f));
    }

    #[test]
    fn rm_then_recreate_same_kind_reuses_in_batch() {
        let (mut idx, mut fake, r) = setup(&[("f", file(10))]);
        let f = id_at(&idx, r, "f").unwrap();
        fake.dirs.get_mut(&1).unwrap().insert("f".into(), file(12));
        batch(&mut idx, &mut fake, r, &[("f", H_CONTENT)]);
        assert_eq!(id_at(&idx, r, "f"), Some(f));
    }

    /// Stress seed 104 (`UNLATCHD_DEBOUNCE_MS=0`, ~2%): `ln d/f x; rm x` with the IN_CREATE batch
    /// stat'ing x while the unlink ran — x present, nlink already 1. The inode was "known" at
    /// f, so f's id followed it to x (a move), and `rm x` then removed f's item: f vanished
    /// from the replica while still on disk. A name that appears by IN_CREATE (no MOVED_TO)
    /// with an inode still indexed elsewhere is a hard link, never a move.
    #[test]
    fn created_name_of_an_indexed_inode_is_a_link_not_a_move() {
        let (mut idx, mut fake, r) = setup(&[("f", file(10))]);
        let f = id_at(&idx, r, "f").unwrap();
        // The raced stat: x is there with f's inode, nlink 1 (the unlink of x is under way).
        fake.dirs.get_mut(&1).unwrap().insert("x".into(), file(10));
        batch(&mut idx, &mut fake, r, &[("x", H_CONTENT)]);
        assert_eq!(id_at(&idx, r, "f"), Some(f), "f keeps its id and name");
        assert!(id_at(&idx, r, "x").is_some_and(|x| x != f));
        fake.dirs.get_mut(&1).unwrap().remove("x");
        batch(&mut idx, &mut fake, r, &[("x", 0)]);
        assert_eq!(id_at(&idx, r, "f"), Some(f), "rm x leaves f");
        assert_eq!(id_at(&idx, r, "x"), None);
        // A real rename (MOVED_TO) still moves the id.
        fake.dirs.get_mut(&1).unwrap().remove("f");
        fake.dirs.get_mut(&1).unwrap().insert("g".into(), file(10));
        batch(&mut idx, &mut fake, r, &[("g", H_MOVED_TO), ("f", 0)]);
        assert_eq!(id_at(&idx, r, "g"), Some(f));
    }

    #[test]
    fn hardlinks_get_separate_ids() {
        let mut l = file(10);
        l.nlink = 2;
        let (idx, _fake, r) = setup(&[("a", l), ("b", l)]);
        let (a, b) = (id_at(&idx, r, "a").unwrap(), id_at(&idx, r, "b").unwrap());
        assert_ne!(a, b);
    }

    #[test]
    fn delete_is_removed_with_tombstone() {
        let (mut idx, mut fake, r) = setup(&[("f", file(10))]);
        let f = id_at(&idx, r, "f").unwrap();
        fake.dirs.get_mut(&1).unwrap().remove("f");
        let b = batch(&mut idx, &mut fake, r, &[("f", 0)]);
        assert!(idx.slot_of(f).is_none());
        assert_eq!(b.txn.removed.len(), 1);
    }

    #[test]
    fn overflow_full_listing_keeps_ids() {
        let (mut idx, mut fake, r) = setup(&[("a", file(10)), ("b", file(11))]);
        let (a, b) = (id_at(&idx, r, "a").unwrap(), id_at(&idx, r, "b").unwrap());
        // Events were lost: a renamed to c, b deleted, d created.
        let root = fake.dirs.get_mut(&1).unwrap();
        root.remove("a");
        root.remove("b");
        root.insert("c".into(), file(10));
        root.insert("d".into(), file(12));
        let mut bt = Batch::default();
        bt.to_scan.push_back(r);
        bt.settle(&mut idx, &mut fake);
        assert_eq!(id_at(&idx, r, "c"), Some(a));
        assert!(idx.slot_of(b).is_none());
        assert!(id_at(&idx, r, "d").is_some());
    }

    #[test]
    fn racy_entries_get_a_content_bump_at_verify() {
        let (mut idx, mut fake, r) = setup(&[("old", file(10)), ("fresh", file(11))]);
        let (o, f) = (
            idx.lookup(r, "old").unwrap(),
            idx.lookup(r, "fresh").unwrap(),
        );
        let (co, cf) = (idx.node(o).content_seq, idx.node(f).content_seq);
        // "fresh" was modified within 2 ticks of the moment watching stopped.
        let mut st = file(11);
        st.mtime_ns = 1_000;
        st.ctime_ns = 1_000;
        fake.dirs.get_mut(&1).unwrap().insert("fresh".into(), st);
        idx.update_stat(f, &st, None, false, true, &mut Txn::default());
        let cf = cf.max(idx.node(f).content_seq);
        let mut b = Batch {
            racy_after_ns: Some(900),
            rescan_existing: true,
            restart_mode: true,
            ..Default::default()
        };
        b.to_scan.push_back(r);
        b.settle(&mut idx, &mut fake);
        assert_eq!(idx.node(o).content_seq, co, "old entries untouched");
        assert!(
            idx.node(f).content_seq > cf,
            "racy entry bumped although its stat is unchanged"
        );
    }

    #[test]
    fn lazy_names_are_not_scanned() {
        let (idx, fake, r) = setup(&[("node_modules", dir(50)), ("src", dir(51))]);
        let _ = fake;
        let nm = idx.lookup(r, "node_modules").unwrap();
        let src = idx.lookup(r, "src").unwrap();
        assert!(!idx.node(nm).scanned());
        // src had no fake dir → scan failed → stays lazy; give it one and it scans
        assert!(!idx.node(src).scanned());
    }

    /// A non-lazy dir an earlier process failed to list (its listing raced a move, so it was
    /// persisted unscanned and published `lazy`) is listed by the next verify walk; dirs lazy
    /// by rule stay lazy.
    #[test]
    fn verify_lists_a_non_lazy_dir_left_unscanned() {
        let (mut idx, mut fake, r) = setup(&[("node_modules", dir(50)), ("src", dir(51))]);
        let src = idx.lookup(r, "src").unwrap();
        let nm = idx.lookup(r, "node_modules").unwrap();
        assert!(!idx.node(src).scanned());
        fake.stale.clear();
        fake.dirs
            .insert(idx.node(src).id, [("f".to_string(), file(60))].into());
        fake.dirs.insert(idx.node(nm).id, HashMap::new());
        let mut b = Batch {
            rescan_existing: true,
            restart_mode: true,
            ..Default::default()
        };
        b.to_scan.push_back(r);
        b.settle(&mut idx, &mut fake);
        assert!(idx.node(src).scanned());
        assert!(id_at(&idx, src, "f").is_some());
        assert!(!idx.node(nm).scanned(), "lazy by name");
    }

    /// A dir whose listing failed (not at its indexed path) is listed by a later batch once
    /// it is reachable, without any event naming it.
    #[test]
    fn failed_listing_is_retried_by_a_later_batch() {
        let (mut idx, mut fake, r) = setup(&[("src", dir(51))]);
        let src = idx.lookup(r, "src").unwrap();
        assert!(!idx.node(src).scanned());
        assert!(fake.stale.contains_key(&idx.node(src).id), "marked stale");
        fake.dirs
            .insert(idx.node(src).id, [("f".to_string(), file(60))].into());
        let mut b = Batch::default();
        b.settle(&mut idx, &mut fake);
        assert!(idx.node(src).scanned());
        assert!(id_at(&idx, src, "f").is_some());
        assert!(fake.stale.is_empty());
    }
}
