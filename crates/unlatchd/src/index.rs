//! The daemon's in-memory index of the root: one [`Node`] per visible item, stored in a slab and
//! linked into per-directory sibling lists (no per-directory allocations).
//!
//! Identity (D4): `(st_dev, st_ino, btime)` for dirs and single-link files; files with
//! `nlink > 1` are keyed by location (every link gets its own id). Versions (D3) are
//! daemon-assigned sequence numbers: `content_seq`, `meta_seq`, and `seq` (last change of any
//! field, used for LWW on the client and for Resume).
//!
//! Memory (T12, ≤ 250 B/entry): nodes are 96 bytes, names live in one arena (no per-name
//! allocation), ids map to slots through a paged table (≈4 B/entry), and the name/identity
//! maps store 32-bit hashes (8-byte buckets) with an exact-key overflow map for collisions.

use crate::sys::Stat;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
use unlatch_proto::wire::Change;
use unlatch_proto::{Entry, ItemId, Kind, Version, ACCESS_R, ACCESS_W, ACCESS_X};

pub type Slot = u32;
pub const NIL: Slot = u32::MAX;

/// Children have been scanned and are kept current (watched or polled).
pub const F_SCANNED: u16 = 1 << 0;
/// Lazy-by-rule directory that a `ListDir` expanded: its child dirs are lazy (D13 inheritance).
pub const F_EXPANDED: u16 = 1 << 1;
/// Regular file with `nlink > 1` → keyed by location, never matched by inode elsewhere.
pub const F_MULTILINK: u16 = 1 << 2;
/// Mount point below the root (different st_dev than its parent).
pub const F_MOUNT: u16 = 1 << 3;
/// Second occurrence of a directory `(dev, ino)` (bind mount): can never be expanded.
pub const F_NOEXPAND: u16 = 1 << 4;
/// Scanned but polled instead of watched (watch budget / network fs).
pub const F_POLLED: u16 = 1 << 5;
/// Transient (reconcile): unlinked from its parent while the batch decides its fate.
pub const F_DETACHED: u16 = 1 << 6;
/// Symlink target stored in the side table.
const F_HAS_TARGET: u16 = 1 << 7;
const KIND_SHIFT: u16 = 13;
const KIND_MASK: u16 = 0b11 << KIND_SHIFT;

const PERSISTED_FLAGS: u16 = F_SCANNED | F_EXPANDED | F_MULTILINK | F_MOUNT | F_NOEXPAND;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NKind {
    File,
    Dir,
    Symlink,
}

impl NKind {
    pub fn of(st: &Stat) -> Option<NKind> {
        if st.is_file() {
            Some(NKind::File)
        } else if st.is_dir() {
            Some(NKind::Dir)
        } else if st.is_symlink() {
            Some(NKind::Symlink)
        } else {
            None // sockets, FIFOs, devices are not exposed (DESIGN §8)
        }
    }
    pub fn wire(self) -> Kind {
        match self {
            NKind::File => Kind::File,
            NKind::Dir => Kind::Dir,
            NKind::Symlink => Kind::Symlink,
        }
    }
    fn bits(self) -> u16 {
        (match self {
            NKind::File => 0,
            NKind::Dir => 1,
            NKind::Symlink => 2,
        }) << KIND_SHIFT
    }
    fn from_bits(f: u16) -> NKind {
        match (f & KIND_MASK) >> KIND_SHIFT {
            1 => NKind::Dir,
            2 => NKind::Symlink,
            _ => NKind::File,
        }
    }
}

/// 32-bit fingerprint of a birth time; 0 = unknown. Identity only needs equality.
pub fn btime_fp(ns: i64) -> u32 {
    if ns == 0 {
        return 0;
    }
    let x = (ns as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    ((x >> 32) as u32) | 1
}

/// 96 bytes (T12).
#[derive(Clone, Debug)]
pub struct Node {
    pub id: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: u64,
    pub content_seq: u64,
    pub meta_seq: u64,
    pub seq: u64,
    pub parent: Slot,
    pub first_child: Slot,
    pub next: Slot,
    pub prev: Slot,
    name_off: u32,
    /// [`btime_fp`] of the birth time (0 = unknown).
    pub btime: u32,
    pub dev: u16,
    pub perm: u16,
    pub flags: u16,
    name_len: u8,
    pub access: u8,
}

const _: () = assert!(std::mem::size_of::<Node>() == 96);

impl Node {
    pub fn kind(&self) -> NKind {
        NKind::from_bits(self.flags)
    }
    pub fn is_dir(&self) -> bool {
        self.kind() == NKind::Dir
    }
    pub fn scanned(&self) -> bool {
        self.flags & F_SCANNED != 0
    }
    pub fn has(&self, f: u16) -> bool {
        self.flags & f != 0
    }
}

/// Identity hasher for keys that are already well-mixed hashes.
#[derive(Default)]
pub struct PreHashed(u64);
impl Hasher for PreHashed {
    fn finish(&self) -> u64 {
        // Spread the 32-bit key over 64 bits for hashbrown's control bytes.
        self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8)) ^ b as u64;
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.0 = i as u64;
    }
    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }
}
type PreMap32 = HashMap<u32, Slot, BuildHasherDefault<PreHashed>>;

/// Mixing hasher for sequential integer keys (ItemIds).
#[derive(Default)]
pub struct MixHasher(u64);
impl Hasher for MixHasher {
    fn finish(&self) -> u64 {
        let mut z = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ b as u64;
        }
    }
    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }
}
pub type IdMap<V> = HashMap<u64, V, BuildHasherDefault<MixHasher>>;

/// ItemId → slot. Ids are allocated sequentially (hi/lo), so a paged array is dense: ≈4 B per
/// entry instead of a 16-byte hash bucket.
#[derive(Default)]
struct IdTable {
    pages: IdMap<(Box<[Slot]>, u32)>,
}

const PAGE_BITS: u32 = 10;
const PAGE: usize = 1 << PAGE_BITS;

impl IdTable {
    fn get(&self, id: u64) -> Option<Slot> {
        let (p, _) = self.pages.get(&(id >> PAGE_BITS))?;
        let s = p[(id as usize) & (PAGE - 1)];
        (s != NIL).then_some(s)
    }
    fn insert(&mut self, id: u64, s: Slot) {
        let e = self
            .pages
            .entry(id >> PAGE_BITS)
            .or_insert_with(|| (vec![NIL; PAGE].into_boxed_slice(), 0));
        let cell = &mut e.0[(id as usize) & (PAGE - 1)];
        if *cell == NIL {
            e.1 += 1;
        }
        *cell = s;
    }
    fn remove(&mut self, id: u64) {
        let key = id >> PAGE_BITS;
        let empty = match self.pages.get_mut(&key) {
            Some((p, n)) => {
                let cell = &mut p[(id as usize) & (PAGE - 1)];
                if *cell != NIL {
                    *cell = NIL;
                    *n -= 1;
                }
                *n == 0
            }
            None => false,
        };
        if empty {
            self.pages.remove(&key);
        }
    }
    fn pages(&self) -> usize {
        self.pages.len()
    }
}

/// Per-transaction change set: what to journal and what to publish.
#[derive(Default, Debug)]
pub struct Txn {
    order: Vec<Slot>,
    kinds: HashMap<Slot, u8>,
    pub removed: Vec<(u64, u64)>,
    /// Removed without tombstone (collapse): journaled, not published.
    pub dropped: Vec<u64>,
    /// Tombstones of the descendants of a removed subtree: journaled and replayed to resuming
    /// clients, not published live (a live session knows the subtree; the root's Remove
    /// covers it).
    pub implicit_tombs: Vec<(u64, u64)>,
    /// Slots freed in this txn; returned to the free list at commit (never reused mid-txn).
    freed: Vec<Slot>,
    /// Content hints for the hot-file throttle: CLOSE_WRITE seen for these slots.
    pub closed: HashSet<Slot>,
}

pub const CH_NEW: u8 = 1;
pub const CH_META: u8 = 2;
pub const CH_CONTENT: u8 = 4;
pub const CH_OTHER: u8 = 8;

impl Txn {
    pub fn touch(&mut self, s: Slot, kind: u8) {
        let e = self.kinds.entry(s).or_insert(0);
        if *e == 0 {
            self.order.push(s);
        }
        *e |= kind;
    }
    pub fn is_empty(&self) -> bool {
        self.order.is_empty() && self.removed.is_empty() && self.dropped.is_empty()
    }
    pub fn kind_of(&self, s: Slot) -> u8 {
        self.kinds.get(&s).copied().unwrap_or(0)
    }
    pub fn slots(&self) -> &[Slot] {
        &self.order
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tomb {
    pub id: u64,
    pub seq: u64,
    pub time: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ColdChild {
    pub id: u64,
    pub name: String,
    pub kind: NKind,
    pub dev: u64,
    pub ino: u64,
    pub btime: u32,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub perm: u16,
    pub content_seq: u64,
    pub meta_seq: u64,
    pub seq: u64,
}

/// Persisted form of a node (index.bin + journal).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PNode {
    pub id: u64,
    pub parent: u64,
    pub name: String,
    pub target: Option<String>,
    pub kind: NKind,
    pub flags: u16,
    pub perm: u16,
    pub access: u8,
    pub dev: u64,
    pub ino: u64,
    pub btime: u32,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub content_seq: u64,
    pub meta_seq: u64,
    pub seq: u64,
}

/// Borrowed twin of [`PNode`] with the identical serialized form (checkpoint writer).
#[derive(Serialize)]
pub struct PNodeRef<'a> {
    pub id: u64,
    pub parent: u64,
    pub name: &'a str,
    pub target: Option<&'a str>,
    pub kind: NKind,
    pub flags: u16,
    pub perm: u16,
    pub access: u8,
    pub dev: u64,
    pub ino: u64,
    pub btime: u32,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub content_seq: u64,
    pub meta_seq: u64,
    pub seq: u64,
}

pub struct Creds {
    pub euid: u32,
    pub egid: u32,
    pub groups: Vec<u32>,
}

impl Creds {
    pub fn current() -> Creds {
        Creds {
            euid: crate::sys::geteuid(),
            egid: crate::sys::getegid(),
            groups: crate::sys::getgroups(),
        }
    }
    /// faccessat(AT_EACCESS) semantics computed from the stat (no syscall per entry).
    pub fn access(&self, st: &Stat) -> u8 {
        let m = st.mode;
        if self.euid == 0 {
            let x = st.is_dir() || m & 0o111 != 0;
            return ACCESS_R | ACCESS_W | if x { ACCESS_X } else { 0 };
        }
        let bits = if st.uid == self.euid {
            (m >> 6) & 7
        } else if st.gid == self.egid || self.groups.contains(&st.gid) {
            (m >> 3) & 7
        } else {
            m & 7
        };
        let mut a = 0;
        if bits & 4 != 0 {
            a |= ACCESS_R;
        }
        if bits & 2 != 0 {
            a |= ACCESS_W;
        }
        if bits & 1 != 0 {
            a |= ACCESS_X;
        }
        a
    }
}

pub struct Index {
    nodes: Vec<Node>,
    free: Vec<Slot>,
    /// Name bytes of every node (see `Node::name_off`); garbage accumulates on rename/remove and
    /// is compacted when it outweighs live bytes.
    arena: Vec<u8>,
    arena_garbage: usize,
    targets: HashMap<Slot, Box<str>>,
    ids: IdTable,
    names: PreMap32,
    names_overflow: HashMap<(Slot, Box<str>), Slot>,
    idents: PreMap32,
    idents_overflow: HashMap<(u16, u64), Slot>,
    /// Further live single-link nodes of an inode whose holder (above) is another live node
    /// (a bind-mount duplicate, or a hard link whose stat raced an unlink): one is promoted
    /// when the holder goes, so the inode never drops out of the identity map while indexed.
    ident_dups: HashMap<(u16, u64), Vec<Slot>>,
    multi: HashMap<(u16, u64), Vec<Slot>>,
    devs: Vec<u64>,
    dev_idx: HashMap<u64, u16>,
    hasher: RandomState,
    pub root: Slot,
    pub seq: u64,
    pub next_id: u64,
    pub tombs: Vec<Tomb>,
    /// Highest seq of garbage-collected tombstones: resume below it → Snapshot.
    pub gc_seq: u64,
    pub cold: IdMap<Vec<ColdChild>>,
    pub creds: Creds,
    pub lazy_names: Vec<String>,
    lazy_set: HashSet<String>,
    /// (seq, wall ns) samples, for Remove's unindexed-entry rule.
    pub seq_times: Vec<(u64, i64)>,
    /// `(dev, ino)` of unlatchd's own install and state dirs (config, not persisted): never
    /// indexed or watched, wherever they sit in the root (see [`Index::is_excluded`]).
    pub excluded: Vec<(u64, u64)>,
    /// Wall minus monotonic clock (ns) at the last [`Index::check_clock`]: a drop is a step of
    /// the wall clock backwards. Not persisted.
    clock_off: Option<i64>,
    live: usize,
    /// Some node got [`F_POLLED`] since this index was built (never cleared; the flag is not
    /// persisted). While false, no directory is polled and [`Index::bfs`] need not run to find
    /// out — on every Ping barrier that walk cost milliseconds on a 100k-entry tree.
    any_polled: bool,
}

impl Index {
    pub fn new(lazy_names: Vec<String>) -> Index {
        let lazy_set = lazy_names.iter().cloned().collect();
        Index {
            nodes: Vec::new(),
            free: Vec::new(),
            arena: Vec::new(),
            arena_garbage: 0,
            targets: HashMap::new(),
            ids: IdTable::default(),
            names: PreMap32::default(),
            names_overflow: HashMap::new(),
            idents: PreMap32::default(),
            idents_overflow: HashMap::new(),
            ident_dups: HashMap::new(),
            multi: HashMap::new(),
            devs: Vec::new(),
            dev_idx: HashMap::new(),
            hasher: RandomState::new(),
            root: NIL,
            seq: 0,
            next_id: 2,
            tombs: Vec::new(),
            gc_seq: 0,
            cold: IdMap::default(),
            creds: Creds::current(),
            lazy_names,
            lazy_set,
            seq_times: Vec::new(),
            excluded: Vec::new(),
            clock_off: None,
            live: 0,
            any_polled: false,
        }
    }

    pub fn len(&self) -> usize {
        self.live
    }
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn is_lazy_name(&self, name: &str) -> bool {
        self.lazy_set.contains(name)
    }

    pub fn node(&self, s: Slot) -> &Node {
        &self.nodes[s as usize]
    }
    pub fn node_mut(&mut self, s: Slot) -> &mut Node {
        &mut self.nodes[s as usize]
    }
    pub fn slot_of(&self, id: u64) -> Option<Slot> {
        self.ids.get(id)
    }
    /// False only if no node has ever carried [`F_POLLED`] (see `any_polled`).
    pub fn may_have_polled(&self) -> bool {
        self.any_polled
    }

    pub fn alive(&self, s: Slot) -> bool {
        (s as usize) < self.nodes.len() && self.ids.get(self.nodes[s as usize].id) == Some(s)
    }

    /// Name of a node (arena slice; always valid UTF-8 because only `&str`s are stored).
    pub fn name(&self, s: Slot) -> &str {
        let n = &self.nodes[s as usize];
        let b = &self.arena[n.name_off as usize..n.name_off as usize + n.name_len as usize];
        std::str::from_utf8(b).unwrap_or("")
    }

    pub fn target(&self, s: Slot) -> Option<&str> {
        if self.nodes[s as usize].flags & F_HAS_TARGET == 0 {
            return None;
        }
        self.targets.get(&s).map(|t| &**t)
    }

    fn set_name(&mut self, s: Slot, name: &str) {
        let old_len = self.nodes[s as usize].name_len as usize;
        self.arena_garbage += old_len;
        let off = self.arena.len();
        // Names are ≤ 255 bytes (valid_name); a longer one would be cut at a char boundary.
        let mut end = name.len().min(255);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        self.arena.extend_from_slice(&name.as_bytes()[..end]);
        let n = &mut self.nodes[s as usize];
        n.name_off = off as u32;
        n.name_len = end as u8;
    }

    fn set_target(&mut self, s: Slot, t: Option<String>) {
        match t {
            Some(t) => {
                self.targets.insert(s, t.into_boxed_str());
                self.nodes[s as usize].flags |= F_HAS_TARGET;
            }
            None => {
                if self.nodes[s as usize].flags & F_HAS_TARGET != 0 {
                    self.targets.remove(&s);
                    self.nodes[s as usize].flags &= !F_HAS_TARGET;
                }
            }
        }
    }

    /// Rewrite the name arena without garbage (when garbage dominates).
    pub fn compact_names(&mut self) {
        if self.arena_garbage < 1 << 20 || self.arena_garbage < self.arena.len() / 2 {
            return;
        }
        self.force_compact_names();
    }

    fn force_compact_names(&mut self) {
        let mut arena = Vec::with_capacity(self.arena.len().saturating_sub(self.arena_garbage));
        for i in 0..self.nodes.len() {
            if !self.alive(i as Slot) {
                continue;
            }
            let n = &self.nodes[i];
            let (off, len) = (n.name_off as usize, n.name_len as usize);
            let new_off = arena.len() as u32;
            arena.extend_from_slice(&self.arena[off..off + len]);
            self.nodes[i].name_off = new_off;
        }
        self.arena = arena;
        self.arena_garbage = 0;
    }

    pub fn dev_of(&self, n: &Node) -> u64 {
        self.devs[n.dev as usize]
    }
    pub fn intern_dev(&mut self, dev: u64) -> u16 {
        if let Some(&i) = self.dev_idx.get(&dev) {
            return i;
        }
        let i = self.devs.len() as u16;
        self.devs.push(dev);
        self.dev_idx.insert(dev, i);
        i
    }

    pub fn bump(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn name_key(&self, parent: Slot, name: &str) -> u32 {
        let mut h = self.hasher.build_hasher();
        h.write_u32(parent);
        h.write(name.as_bytes());
        h.finish() as u32
    }
    fn ident_key(&self, dev: u16, ino: u64) -> u32 {
        let mut h = self.hasher.build_hasher();
        h.write_u16(dev);
        h.write_u64(ino);
        h.finish() as u32
    }

    // ---- lookups -------------------------------------------------------------------------

    pub fn lookup(&self, parent: Slot, name: &str) -> Option<Slot> {
        let k = self.name_key(parent, name);
        if let Some(&s) = self.names.get(&k) {
            let n = &self.nodes[s as usize];
            if n.parent == parent && n.flags & F_DETACHED == 0 && self.name(s) == name {
                return Some(s);
            }
        }
        if self.names_overflow.is_empty() {
            return None;
        }
        self.names_overflow.get(&(parent, Box::from(name))).copied()
    }

    /// Location-independent identity lookup (dirs and single-link files only).
    pub fn ident_lookup(&self, st: &Stat) -> Option<Slot> {
        let kind = NKind::of(st)?;
        let dev = *self.dev_idx.get(&st.dev)?;
        self.ident_lookup_raw(dev, st.ino, kind, btime_fp(st.btime_ns))
    }

    /// Identity lookup by raw key; `btime` is a fingerprint (0 = unknown).
    pub fn ident_lookup_raw(&self, dev: u16, ino: u64, kind: NKind, btime: u32) -> Option<Slot> {
        let k = self.ident_key(dev, ino);
        let s = match self.idents.get(&k) {
            Some(&s) if self.nodes[s as usize].dev == dev && self.nodes[s as usize].ino == ino => s,
            _ => *self.idents_overflow.get(&(dev, ino))?,
        };
        let n = &self.nodes[s as usize];
        if n.kind() != kind || n.flags & F_MULTILINK != 0 {
            return None;
        }
        if n.btime != 0 && btime != 0 && n.btime != btime {
            return None; // inode number reused by a different file (D4: split, never merge)
        }
        Some(s)
    }

    /// Another live node with the identity of `s` (bind-mount duplicate check).
    pub fn ident_other(&self, s: Slot) -> Option<Slot> {
        let n = &self.nodes[s as usize];
        self.ident_lookup_raw(n.dev, n.ino, n.kind(), n.btime)
            .filter(|&o| o != s)
    }

    /// Same item at the same location (identity unchanged).
    pub fn same_identity(&self, s: Slot, st: &Stat) -> bool {
        let n = &self.nodes[s as usize];
        let fp = btime_fp(st.btime_ns);
        self.devs[n.dev as usize] == st.dev
            && n.ino == st.ino
            && Some(n.kind()) == NKind::of(st)
            && (n.btime == 0 || fp == 0 || n.btime == fp)
    }

    pub fn children(&self, dir: Slot) -> ChildIter<'_> {
        ChildIter {
            idx: self,
            cur: self.nodes[dir as usize].first_child,
        }
    }

    /// Path components from the root (excluding the root), or None if detached.
    pub fn rel_path(&self, s: Slot) -> Option<Vec<u8>> {
        let mut parts: Vec<Slot> = Vec::new();
        let mut cur = s;
        while cur != self.root {
            let n = &self.nodes[cur as usize];
            if n.flags & F_DETACHED != 0 || n.parent == NIL || parts.len() > 4096 {
                return None;
            }
            parts.push(cur);
            cur = n.parent;
        }
        let mut out = Vec::new();
        for (i, &p) in parts.iter().rev().enumerate() {
            if i > 0 {
                out.push(b'/');
            }
            out.extend_from_slice(self.name(p).as_bytes());
        }
        Some(out)
    }

    pub fn depth(&self, s: Slot) -> usize {
        let mut d = 0;
        let mut cur = s;
        while cur != self.root && cur != NIL && d < 4096 {
            cur = self.nodes[cur as usize].parent;
            d += 1;
        }
        d
    }

    /// `a` is `b` or an ancestor of `b`.
    pub fn is_ancestor_or_self(&self, a: Slot, b: Slot) -> bool {
        let mut cur = b;
        let mut guard = 0;
        loop {
            if cur == a {
                return true;
            }
            if cur == self.root || cur == NIL || guard > 4096 {
                return false;
            }
            cur = self.nodes[cur as usize].parent;
            guard += 1;
        }
    }

    // ---- entries ---------------------------------------------------------------------------

    pub fn entry(&self, s: Slot) -> Entry {
        let n = &self.nodes[s as usize];
        let parent = if s == self.root || n.parent == NIL {
            ItemId::ROOT
        } else {
            ItemId(self.nodes[n.parent as usize].id)
        };
        let kind = n.kind();
        let target = self.target(s);
        let size = match kind {
            NKind::File => n.size,
            NKind::Dir => 0,
            NKind::Symlink => target.map(|t| t.len() as u64).unwrap_or(0),
        };
        Entry {
            id: ItemId(n.id),
            parent,
            name: self.name(s).to_string(),
            kind: kind.wire(),
            size,
            mtime_ns: n.mtime_ns,
            mode: n.perm as u32,
            version: Version {
                content: n.content_seq,
                meta: n.meta_seq,
            },
            symlink_target: target.map(|t| t.to_string()),
            lazy: kind == NKind::Dir && n.flags & F_SCANNED == 0,
            seq: n.seq,
            access: n.access,
        }
    }

    pub fn pnode(&self, s: Slot) -> PNode {
        let n = &self.nodes[s as usize];
        let parent = if s == self.root {
            n.id
        } else {
            self.nodes[n.parent as usize].id
        };
        PNode {
            id: n.id,
            parent,
            name: self.name(s).to_string(),
            target: self.target(s).map(|t| t.to_string()),
            kind: n.kind(),
            flags: n.flags & PERSISTED_FLAGS,
            perm: n.perm,
            access: n.access,
            dev: self.devs[n.dev as usize],
            ino: n.ino,
            btime: n.btime,
            size: n.size,
            mtime_ns: n.mtime_ns,
            ctime_ns: n.ctime_ns,
            content_seq: n.content_seq,
            meta_seq: n.meta_seq,
            seq: n.seq,
        }
    }

    pub fn pnode_ref(&self, s: Slot) -> PNodeRef<'_> {
        let n = &self.nodes[s as usize];
        let parent = if s == self.root {
            n.id
        } else {
            self.nodes[n.parent as usize].id
        };
        PNodeRef {
            id: n.id,
            parent,
            name: self.name(s),
            target: self.target(s),
            kind: n.kind(),
            flags: n.flags & PERSISTED_FLAGS,
            perm: n.perm,
            access: n.access,
            dev: self.devs[n.dev as usize],
            ino: n.ino,
            btime: n.btime,
            size: n.size,
            mtime_ns: n.mtime_ns,
            ctime_ns: n.ctime_ns,
            content_seq: n.content_seq,
            meta_seq: n.meta_seq,
            seq: n.seq,
        }
    }

    // ---- structural edits ------------------------------------------------------------------

    fn new_node(
        id: u64,
        parent: Slot,
        kind: NKind,
        st: &Stat,
        dev: u16,
        access: u8,
        seqs: (u64, u64, u64),
    ) -> Node {
        Node {
            id,
            size: st.size,
            mtime_ns: st.mtime_ns,
            ctime_ns: st.ctime_ns,
            ino: st.ino,
            content_seq: seqs.0,
            meta_seq: seqs.1,
            seq: seqs.2,
            parent,
            first_child: NIL,
            next: NIL,
            prev: NIL,
            name_off: 0,
            btime: btime_fp(st.btime_ns),
            dev,
            perm: st.perm() as u16,
            flags: kind.bits()
                | if kind == NKind::File && st.nlink > 1 {
                    F_MULTILINK
                } else {
                    0
                },
            name_len: 0,
            access,
        }
    }

    fn alloc_slot(&mut self, node: Node, name: &str) -> Slot {
        self.live += 1;
        let s = if let Some(s) = self.free.pop() {
            self.nodes[s as usize] = node;
            s
        } else {
            self.nodes.push(node);
            (self.nodes.len() - 1) as Slot
        };
        self.nodes[s as usize].name_len = 0;
        self.set_name(s, name);
        s
    }

    fn link_child(&mut self, parent: Slot, s: Slot) {
        let first = self.nodes[parent as usize].first_child;
        {
            let n = &mut self.nodes[s as usize];
            n.parent = parent;
            n.prev = NIL;
            n.next = first;
        }
        if first != NIL {
            self.nodes[first as usize].prev = s;
        }
        self.nodes[parent as usize].first_child = s;
    }

    fn unlink_child(&mut self, s: Slot) {
        let (parent, prev, next) = {
            let n = &self.nodes[s as usize];
            (n.parent, n.prev, n.next)
        };
        if parent == NIL {
            return;
        }
        if prev != NIL {
            self.nodes[prev as usize].next = next;
        } else if self.nodes[parent as usize].first_child == s {
            self.nodes[parent as usize].first_child = next;
        }
        if next != NIL {
            self.nodes[next as usize].prev = prev;
        }
        let n = &mut self.nodes[s as usize];
        n.prev = NIL;
        n.next = NIL;
    }

    fn names_insert(&mut self, parent: Slot, s: Slot) {
        let k = self.name_key(parent, self.name(s));
        match self.names.get(&k) {
            Some(&o) if o != s && self.alive(o) => {
                let on = &self.nodes[o as usize];
                if on.parent == parent && on.flags & F_DETACHED == 0 && self.name(o) == self.name(s)
                {
                    // Same (parent, name) twice would be a reconcile bug; keep the newest.
                    self.names.insert(k, s);
                } else {
                    self.names_overflow
                        .insert((parent, Box::from(self.name(s))), s);
                }
            }
            _ => {
                self.names.insert(k, s);
            }
        }
    }

    fn names_remove(&mut self, parent: Slot, s: Slot) {
        let k = self.name_key(parent, self.name(s));
        if self.names.get(&k) == Some(&s) {
            self.names.remove(&k);
            // Promote an overflow entry with the same hash, if any.
            if !self.names_overflow.is_empty() {
                let promote: Option<((Slot, Box<str>), Slot)> = self
                    .names_overflow
                    .iter()
                    .find(|((p, n), _)| self.name_key(*p, n) == k)
                    .map(|(key, &v)| (key.clone(), v));
                if let Some((key, v)) = promote {
                    self.names_overflow.remove(&key);
                    self.names.insert(k, v);
                }
            }
        } else if !self.names_overflow.is_empty() {
            let key = (parent, Box::from(self.name(s)));
            if self.names_overflow.get(&key) == Some(&s) {
                self.names_overflow.remove(&key);
            }
        }
    }

    fn idents_insert(&mut self, s: Slot) {
        let n = &self.nodes[s as usize];
        let (dev, ino, btime) = (n.dev, n.ino, n.btime);
        if n.flags & F_MULTILINK != 0 {
            self.multi.entry((dev, ino)).or_default().push(s);
            return;
        }
        let k = self.ident_key(dev, ino);
        match self.idents.get(&k) {
            Some(&o) if o != s && self.alive(o) => {
                let on = &self.nodes[o as usize];
                if on.dev == dev && on.ino == ino {
                    // The inode number was reused: the holder is a node being settled (deleted,
                    // its number already taken by a new file in the same batch) or a different
                    // file by birth time. The new node is the inode's live holder — otherwise it
                    // is never found by identity again (moves split, its hard links are
                    // never dirtied; fuzz seed 157).
                    let stale = on.flags & F_DETACHED != 0
                        || (on.btime != 0 && btime != 0 && on.btime != btime);
                    if stale {
                        self.idents.insert(k, s);
                    } else {
                        // Two live nodes with one inode (bind mount / race): keep the first,
                        // remember the second for when the first goes.
                        self.ident_dups.entry((dev, ino)).or_default().push(s);
                    }
                    return;
                }
                self.idents_overflow.insert((dev, ino), s);
            }
            _ => {
                self.idents.insert(k, s);
            }
        }
    }

    /// Rebuild the identity maps from the nodes. Journal replay applies a txn's upserts before
    /// its removals, so record by record an inode can pass through two live holders (a
    /// rename-over: the destination's upsert while the moved node still holds the inode, which
    /// [`Index::idents_insert`] keeps) and lose both when the second is removed; the replayed
    /// state is complete only at the end.
    pub fn rebuild_idents(&mut self) {
        self.idents.clear();
        self.idents_overflow.clear();
        self.ident_dups.clear();
        self.multi.clear();
        for s in self.bfs() {
            if self.nodes[s as usize].flags & F_DETACHED == 0 {
                self.idents_insert(s);
            }
        }
    }

    fn idents_remove(&mut self, s: Slot) {
        let n = &self.nodes[s as usize];
        let (dev, ino) = (n.dev, n.ino);
        if n.flags & F_MULTILINK != 0 {
            if let Some(v) = self.multi.get_mut(&(dev, ino)) {
                v.retain(|&x| x != s);
                if v.is_empty() {
                    self.multi.remove(&(dev, ino));
                }
            }
            return;
        }
        let k = self.ident_key(dev, ino);
        let was_holder =
            self.idents.get(&k) == Some(&s) || self.idents_overflow.get(&(dev, ino)) == Some(&s);
        self.idents_remove_holder(s, dev, ino, k);
        if let Some(v) = self.ident_dups.get_mut(&(dev, ino)) {
            v.retain(|&x| x != s);
            let mut cands = if was_holder {
                std::mem::take(v)
            } else {
                Vec::new()
            };
            if v.is_empty() {
                self.ident_dups.remove(&(dev, ino));
            }
            // Promote the next live node still standing for this inode.
            while let Some(c) = cands.pop() {
                let n = &self.nodes[c as usize];
                if self.alive(c) && n.dev == dev && n.ino == ino && n.flags & F_MULTILINK == 0 {
                    self.idents_insert(c);
                    if !cands.is_empty() {
                        self.ident_dups.entry((dev, ino)).or_default().extend(cands);
                    }
                    break;
                }
            }
        }
    }

    fn idents_remove_holder(&mut self, s: Slot, dev: u16, ino: u64, k: u32) {
        if self.idents.get(&k) == Some(&s) {
            self.idents.remove(&k);
            if !self.idents_overflow.is_empty() {
                let promote = self
                    .idents_overflow
                    .iter()
                    .find(|((d, i), _)| self.ident_key(*d, *i) == k)
                    .map(|(key, &v)| (*key, v));
                if let Some((key, v)) = promote {
                    self.idents_overflow.remove(&key);
                    self.idents.insert(k, v);
                }
            }
        } else if self.idents_overflow.get(&(dev, ino)) == Some(&s) {
            self.idents_overflow.remove(&(dev, ino));
        }
    }

    /// Other links of a file's inode (to dirty all of them on an event, D4): every multi-link
    /// node with the same `(dev, ino)`, plus a node still indexed as its single link (its
    /// nlink went up without an event naming it).
    pub fn links_of(&self, s: Slot) -> Vec<Slot> {
        let n = &self.nodes[s as usize];
        if n.kind() != NKind::File {
            return Vec::new();
        }
        let mut v = self.links_of_ident(n.dev, n.ino, n.btime);
        v.retain(|&x| x != s);
        v
    }

    /// Every indexed link of the file inode `(dev, ino)` (see [`Index::links_of`]); also usable
    /// for a node just removed (its dev/ino stay readable until the slot is reused).
    pub fn links_of_ident(&self, dev: u16, ino: u64, btime: u32) -> Vec<Slot> {
        let mut v: Vec<Slot> = self.multi.get(&(dev, ino)).cloned().unwrap_or_default();
        if let Some(o) = self.ident_lookup_raw(dev, ino, NKind::File, btime) {
            if !v.contains(&o) {
                v.push(o);
            }
        }
        v
    }

    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Create the root node (fresh index).
    pub fn create_root(&mut self, name: &str, st: &Stat) -> Slot {
        let seq = self.bump();
        let dev = self.intern_dev(st.dev);
        let access = self.creds.access(st);
        let mut node = Self::new_node(
            ItemId::ROOT.0,
            NIL,
            NKind::Dir,
            st,
            dev,
            access,
            (seq, seq, seq),
        );
        node.flags |= F_SCANNED;
        let s = self.alloc_slot(node, name);
        self.nodes[s as usize].parent = s;
        self.ids.insert(ItemId::ROOT.0, s);
        self.idents_insert(s);
        self.root = s;
        s
    }

    /// New node for a newly observed item.
    pub fn create(
        &mut self,
        parent: Slot,
        name: &str,
        st: &Stat,
        kind: NKind,
        target: Option<String>,
        txn: &mut Txn,
    ) -> Slot {
        let id = self.alloc_id();
        let seq = self.bump();
        let dev = self.intern_dev(st.dev);
        let access = self.creds.access(st);
        let node = Self::new_node(id, parent, kind, st, dev, access, (seq, seq, seq));
        let s = self.alloc_slot(node, name);
        self.set_target(s, target);
        self.ids.insert(id, s);
        self.link_child(parent, s);
        self.names_insert(parent, s);
        self.idents_insert(s);
        txn.touch(s, CH_NEW);
        s
    }

    /// Re-create a node collapsed earlier (Unwatch) with its old id and versions, so a re-listed
    /// lazy dir keeps stable ids. Bumps versions if the stat changed meanwhile.
    pub fn create_from_cold(
        &mut self,
        parent: Slot,
        c: &ColdChild,
        st: &Stat,
        target: Option<String>,
        txn: &mut Txn,
    ) -> Slot {
        let dev = self.intern_dev(st.dev);
        let access = self.creds.access(st);
        let old = Stat {
            size: c.size,
            mtime_ns: c.mtime_ns,
            ctime_ns: c.ctime_ns,
            mode: c.perm as u32 | (st.mode & libc::S_IFMT),
            ..*st
        };
        let mut node = Self::new_node(
            c.id,
            parent,
            c.kind,
            &old,
            dev,
            access,
            (c.content_seq, c.meta_seq, c.seq),
        );
        node.btime = c.btime;
        let s = self.alloc_slot(node, &c.name);
        self.set_target(s, target.clone());
        self.ids.insert(c.id, s);
        self.link_child(parent, s);
        self.names_insert(parent, s);
        self.idents_insert(s);
        txn.touch(s, CH_OTHER);
        self.update_stat(s, st, target, false, true, txn);
        s
    }

    /// Snapshot of a node for the cold cache (collapse).
    pub fn cold_child(&self, s: Slot) -> ColdChild {
        let n = &self.nodes[s as usize];
        ColdChild {
            id: n.id,
            name: self.name(s).to_string(),
            kind: n.kind(),
            dev: self.devs[n.dev as usize],
            ino: n.ino,
            btime: n.btime,
            size: n.size,
            mtime_ns: n.mtime_ns,
            ctime_ns: n.ctime_ns,
            perm: n.perm,
            content_seq: n.content_seq,
            meta_seq: n.meta_seq,
            seq: n.seq,
        }
    }

    /// `m` (detached, published) takes over the identity and stat of `o` (reconcile: atomic
    /// replace / rename-over keeps the destination id, D4). For dirs, `o`'s children move under
    /// `m`. The caller disposes of `o` afterwards.
    pub fn transplant(&mut self, m: Slot, o: Slot, txn: &mut Txn) {
        self.idents_remove(m);
        self.idents_remove(o);
        let src = self.nodes[o as usize].clone();
        let src_target = self.target(o).map(|t| t.to_string());
        let kids: Vec<Slot> = self.children(o).collect();
        for c in kids {
            self.names_remove(o, c);
            self.unlink_child(c);
            self.link_child(m, c);
            self.names_insert(m, c);
        }
        let seq = self.bump();
        let target_changed = self.target(m).map(|t| t.to_string()) != src_target;
        self.set_target(m, src_target);
        self.any_polled |= src.flags & F_POLLED != 0;
        let n = &mut self.nodes[m as usize];
        let perm_changed = n.perm != src.perm;
        n.dev = src.dev;
        n.ino = src.ino;
        n.btime = src.btime;
        n.size = src.size;
        n.mtime_ns = src.mtime_ns;
        n.ctime_ns = src.ctime_ns;
        n.perm = src.perm;
        n.access = src.access;
        let keep = F_DETACHED | F_HAS_TARGET;
        n.flags = (n.flags & keep) | (src.flags & !keep);
        n.content_seq = seq;
        if perm_changed || target_changed {
            n.meta_seq = seq;
        }
        n.seq = seq;
        // o no longer owns the inode.
        self.nodes[o as usize].ino = u64::MAX;
        self.idents_insert(m);
        txn.touch(m, CH_CONTENT | CH_META);
    }

    /// Temporarily unlink a node from its parent (reconcile). Its subtree stays attached to it.
    pub fn detach(&mut self, s: Slot) {
        if self.nodes[s as usize].flags & F_DETACHED != 0 || s == self.root {
            return;
        }
        let parent = self.nodes[s as usize].parent;
        self.names_remove(parent, s);
        self.unlink_child(s);
        self.nodes[s as usize].flags |= F_DETACHED;
    }

    pub fn is_detached(&self, s: Slot) -> bool {
        self.nodes[s as usize].flags & F_DETACHED != 0
    }

    /// Attach a detached node at (parent, name). Bumps meta if the location changed.
    pub fn attach(
        &mut self,
        s: Slot,
        parent: Slot,
        name: &str,
        old: Option<(Slot, &str)>,
        txn: &mut Txn,
    ) {
        let moved = match old {
            Some((op, on)) => op != parent || on != name,
            None => true,
        };
        self.nodes[s as usize].flags &= !F_DETACHED;
        if self.name(s) != name {
            self.set_name(s, name);
        }
        self.link_child(parent, s);
        self.names_insert(parent, s);
        if moved {
            let seq = self.bump();
            let n = &mut self.nodes[s as usize];
            n.meta_seq = seq;
            n.seq = seq;
            txn.touch(s, CH_META);
        }
    }

    /// Move an attached node to (parent, name) (detach + attach).
    pub fn move_node(&mut self, s: Slot, parent: Slot, name: &str, txn: &mut Txn) {
        let (op, on) = (self.nodes[s as usize].parent, self.name(s).to_string());
        self.detach(s);
        self.attach(s, parent, name, Some((op, &on)), txn);
    }

    /// Apply a fresh stat of the same item. `force_content`: an inotify content event was seen
    /// (T17: same-size rewrites within one tick leave the stat tuple unchanged, D3).
    /// `ctime_is_content`: false when the item was moved/renamed in this batch (rename bumps
    /// ctime without touching content).
    pub fn update_stat(
        &mut self,
        s: Slot,
        st: &Stat,
        target: Option<String>,
        force_content: bool,
        ctime_is_content: bool,
        txn: &mut Txn,
    ) {
        let dev = self.intern_dev(st.dev);
        let access = self.creds.access(st);
        let fp = btime_fp(st.btime_ns);
        let n = &self.nodes[s as usize];
        let kind = n.kind();
        let was_multi = n.flags & F_MULTILINK != 0;
        let is_multi = kind == NKind::File && st.nlink > 1;
        let content_diff = n.size != st.size
            || n.mtime_ns != st.mtime_ns
            || n.ino != st.ino
            || n.dev != dev
            || (n.btime != fp && n.btime != 0 && fp != 0);
        let perm_diff = n.perm != st.perm() as u16;
        let target_diff = kind == NKind::Symlink && self.target(s) != target.as_deref();
        let meta_diff = perm_diff || target_diff;
        // ctime alone moves on chmod/rename/link-count too: only treat it as a content change
        // when no metadata change explains it (D3 lists ctime in the content tuple).
        let ctime_only =
            ctime_is_content && n.ctime_ns != st.ctime_ns && !meta_diff && kind == NKind::File;
        let access_diff = n.access != access;
        let content = kind == NKind::File && (force_content || content_diff || ctime_only)
            || (kind == NKind::Symlink && target_diff);
        let dir_diff = kind == NKind::Dir && n.mtime_ns != st.mtime_ns;
        if was_multi != is_multi || n.ino != st.ino || n.dev != dev {
            self.idents_remove(s);
            let n = &mut self.nodes[s as usize];
            n.ino = st.ino;
            n.dev = dev;
            n.btime = fp;
            if is_multi {
                n.flags |= F_MULTILINK;
            } else {
                n.flags &= !F_MULTILINK;
            }
            self.idents_insert(s);
        }
        if kind == NKind::Symlink && target_diff {
            self.set_target(s, target);
        }
        let n = &mut self.nodes[s as usize];
        n.size = st.size;
        n.mtime_ns = st.mtime_ns;
        n.ctime_ns = st.ctime_ns;
        if fp != 0 {
            n.btime = fp;
        }
        n.perm = st.perm() as u16;
        n.access = access;
        if content || meta_diff || access_diff || dir_diff {
            let seq = self.seq + 1;
            self.seq = seq;
            let n = &mut self.nodes[s as usize];
            let mut k = 0;
            if content {
                n.content_seq = seq;
                k |= CH_CONTENT;
            }
            if meta_diff {
                n.meta_seq = seq;
                k |= CH_META;
            }
            if access_diff || dir_diff {
                k |= CH_OTHER;
            }
            n.seq = seq;
            txn.touch(s, k);
        }
    }

    /// Bump `seq` for a non-version change (lazy flag flip etc.).
    pub fn touch_other(&mut self, s: Slot, txn: &mut Txn) {
        let seq = self.bump();
        self.nodes[s as usize].seq = seq;
        txn.touch(s, CH_OTHER);
    }

    pub fn set_flags(&mut self, s: Slot, set: u16, clear: u16) {
        self.any_polled |= set & F_POLLED != 0;
        let n = &mut self.nodes[s as usize];
        n.flags = (n.flags | set) & !clear;
    }

    /// Remove `s` and its subtree. `tomb`: publish a tombstone (false = collapse/drop).
    /// Returns every removed slot (their `id` and kind stay readable until reuse).
    pub fn remove_subtree(&mut self, s: Slot, tomb: bool, txn: &mut Txn) -> Vec<Slot> {
        if s == self.root || !self.alive(s) {
            return Vec::new();
        }
        let id = self.nodes[s as usize].id;
        if !self.is_detached(s) {
            self.detach(s);
        }
        let mut removed = Vec::new();
        let mut stack = vec![s];
        while let Some(x) = stack.pop() {
            let mut c = self.nodes[x as usize].first_child;
            while c != NIL {
                stack.push(c);
                c = self.nodes[c as usize].next;
            }
            removed.push(x);
        }
        for &x in &removed {
            if x != s {
                let p = self.nodes[x as usize].parent;
                self.names_remove(p, x);
            }
            self.idents_remove(x);
            let xid = self.nodes[x as usize].id;
            self.ids.remove(xid);
            self.cold.remove(&xid);
            self.set_target(x, None);
            self.arena_garbage += self.nodes[x as usize].name_len as usize;
            let n = &mut self.nodes[x as usize];
            n.first_child = NIL;
            n.next = NIL;
            n.prev = NIL;
            n.parent = NIL;
            n.name_len = 0;
            self.live -= 1;
            txn.freed.push(x);
        }
        if tomb {
            let seq = self.bump();
            let time = crate::sys::now_secs();
            txn.removed.push((id, seq));
            self.tombs.push(Tomb { id, seq, time });
            // Every descendant gets its own tombstone (same seq) for Resume: a client that last
            // synced before a descendant was moved *into* this subtree still has it elsewhere,
            // and the root's tombstone alone would never remove it there.
            for &x in &removed {
                if x != s {
                    let xid = self.nodes[x as usize].id;
                    txn.implicit_tombs.push((xid, seq));
                    self.tombs.push(Tomb { id: xid, seq, time });
                }
            }
        } else {
            txn.dropped.push(id);
        }
        removed
    }

    /// Return freed slots to the free list (call once the txn has been journaled/published).
    pub fn finish_txn(&mut self, txn: &mut Txn) {
        self.free.append(&mut txn.freed);
    }

    fn ordered(&self, txn: &Txn, filter: impl Fn(Slot, u8) -> bool) -> Vec<Slot> {
        let mut ups: Vec<(usize, usize, Slot)> = txn
            .order
            .iter()
            .enumerate()
            .filter(|(_, &s)| self.alive(s) && !self.is_detached(s))
            .filter(|(_, &s)| filter(s, txn.kind_of(s)))
            .map(|(i, &s)| (self.depth(s), i, s))
            .collect();
        ups.sort_unstable();
        ups.into_iter().map(|(_, _, s)| s).collect()
    }

    /// Changes for publication, parents before children, then removals.
    pub fn changes(&self, txn: &Txn, filter: impl Fn(Slot, u8) -> bool) -> Vec<Change> {
        let mut out: Vec<Change> = self
            .ordered(txn, filter)
            .into_iter()
            .map(|s| Change::Upsert(self.entry(s)))
            .collect();
        for &(id, seq) in &txn.removed {
            out.push(Change::Remove {
                id: ItemId(id),
                seq,
            });
        }
        out
    }

    /// Journal records for a txn.
    pub fn pnodes(&self, txn: &Txn) -> Vec<PNode> {
        self.ordered(txn, |_, _| true)
            .into_iter()
            .map(|s| self.pnode(s))
            .collect()
    }

    // ---- persistence ------------------------------------------------------------------------

    /// All live nodes, parents before children (BFS from the root).
    pub fn bfs(&self) -> Vec<Slot> {
        let mut out = Vec::with_capacity(self.live);
        if self.root == NIL {
            return out;
        }
        out.push(self.root);
        let mut i = 0;
        while i < out.len() {
            let s = out[i];
            let mut c = self.nodes[s as usize].first_child;
            while c != NIL {
                out.push(c);
                c = self.nodes[c as usize].next;
            }
            i += 1;
        }
        out
    }

    /// Insert or update a node from its persisted form (load / journal replay). Parent must exist.
    pub fn apply_pnode(&mut self, p: &PNode) -> bool {
        let dev = self.intern_dev(p.dev);
        let flags = (p.flags & PERSISTED_FLAGS) | p.kind.bits();
        if let Some(s) = self.slot_of(p.id) {
            if s != self.root {
                let Some(ps) = self.slot_of(p.parent) else {
                    return false;
                };
                if self.nodes[s as usize].parent != ps || self.name(s) != p.name {
                    self.detach(s);
                    self.nodes[s as usize].flags &= !F_DETACHED;
                    self.set_name(s, &p.name);
                    self.link_child(ps, s);
                    self.names_insert(ps, s);
                }
            }
            self.idents_remove(s);
            let n = &mut self.nodes[s as usize];
            n.flags = flags | (n.flags & (F_POLLED | F_HAS_TARGET));
            n.perm = p.perm;
            n.access = p.access;
            n.dev = dev;
            n.ino = p.ino;
            n.btime = p.btime;
            n.size = p.size;
            n.mtime_ns = p.mtime_ns;
            n.ctime_ns = p.ctime_ns;
            n.content_seq = p.content_seq;
            n.meta_seq = p.meta_seq;
            n.seq = p.seq;
            self.set_target(s, p.target.clone());
            self.idents_insert(s);
        } else {
            let is_root = p.id == ItemId::ROOT.0;
            let parent = if is_root {
                NIL
            } else {
                match self.slot_of(p.parent) {
                    Some(ps) => ps,
                    None => return false,
                }
            };
            let node = Node {
                id: p.id,
                size: p.size,
                mtime_ns: p.mtime_ns,
                ctime_ns: p.ctime_ns,
                ino: p.ino,
                content_seq: p.content_seq,
                meta_seq: p.meta_seq,
                seq: p.seq,
                parent,
                first_child: NIL,
                next: NIL,
                prev: NIL,
                name_off: 0,
                btime: p.btime,
                dev,
                perm: p.perm,
                flags,
                name_len: 0,
                access: p.access,
            };
            let s = self.alloc_slot(node, &p.name);
            self.set_target(s, p.target.clone());
            self.ids.insert(p.id, s);
            if is_root {
                self.nodes[s as usize].parent = s;
                self.root = s;
            } else {
                self.link_child(parent, s);
                self.names_insert(parent, s);
            }
            self.idents_insert(s);
        }
        self.seq = self.seq.max(p.seq);
        self.next_id = self.next_id.max(p.id + 1);
        true
    }

    /// Journal replay of a removal.
    pub fn apply_remove(&mut self, id: u64, seq: u64, tomb: bool, time: u64) {
        if let Some(s) = self.slot_of(id) {
            let mut t = Txn::default();
            self.remove_subtree(s, false, &mut t);
            self.finish_txn(&mut t);
        }
        if tomb {
            self.tombs.push(Tomb { id, seq, time });
        }
        self.seq = self.seq.max(seq);
    }

    /// Drop tombstones older than the horizon (D17: 30 days), and the oldest beyond `max_count`
    /// (a directory churned under the root leaves one per removed item). `gc_seq` rises to the
    /// newest dropped seq: a client resuming from below it gets a Snapshot instead.
    pub fn gc_tombs(&mut self, horizon_secs: u64, max_count: usize) {
        let now = crate::sys::now_secs();
        let old = self
            .tombs
            .iter()
            .take_while(|t| now.saturating_sub(t.time) > horizon_secs)
            .count();
        let cut = old.max(self.tombs.len().saturating_sub(max_count));
        if cut > 0 {
            self.gc_seq = self.gc_seq.max(self.tombs[cut - 1].seq);
            self.tombs.drain(..cut);
        }
    }

    /// One of unlatchd's own dirs (install dir, state dir): treated as absent everywhere — no
    /// self-feeding event loop from its journal writes, nothing of unlatchd shown to the Mac.
    pub fn is_excluded(&self, st: &crate::sys::Stat) -> bool {
        !self.excluded.is_empty() && self.excluded.contains(&(st.dev, st.ino))
    }

    /// Tombstones a Resume from `since` would send.
    pub fn tombs_after(&self, since: u64) -> usize {
        self.tombs.len() - self.tombs.partition_point(|t| t.seq <= since)
    }

    pub fn note_seq_time(&mut self) {
        let now = self.check_clock();
        if self
            .seq_times
            .last()
            .map(|&(s, t)| s != self.seq && now - t > 1_000_000_000)
            .unwrap_or(true)
        {
            self.seq_times.push((self.seq, now));
            if self.seq_times.len() > 20_000 {
                // Thin out: keep every other sample.
                let keep: Vec<_> = self.seq_times.iter().copied().step_by(2).collect();
                self.seq_times = keep;
            }
        }
    }

    /// Keep the seq→time samples on the current wall clock; returns the wall time now.
    /// A step of the wall clock backwards (NTP correction after a resume or restore, a leap
    /// second, `date -s`) would leave samples later than changes made after the step, and a
    /// recursive Remove would take those changes for older than the client's seen point. The
    /// wall clock is compared with the monotonic one, which steps never move: when it fell
    /// behind by Δ since the last check, every sample moves back by Δ. Samples later than now
    /// (taken before a restart, where a step cannot be measured) are dropped: their seqs fall
    /// back to an earlier sample or to "unknown", and both keep more, never less.
    pub fn check_clock(&mut self) -> i64 {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let mono = START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_nanos() as i64;
        let now = crate::sys::now_ns();
        self.check_clock_at(now, mono);
        now
    }

    fn check_clock_at(&mut self, now: i64, mono: i64) {
        // Reading the two clocks is not atomic: ignore sub-millisecond jitter.
        const JITTER: i64 = 1_000_000;
        let off = now - mono;
        let first = self.clock_off.is_none();
        match self.clock_off {
            Some(prev) if off < prev - JITTER => {
                let d = prev - off;
                for s in &mut self.seq_times {
                    s.1 -= d;
                }
                self.clock_off = Some(off);
            }
            Some(prev) if off <= prev => {}
            _ => self.clock_off = Some(off),
        }
        // In-process samples are in time order; a loaded list is checked whole once.
        if first || self.seq_times.last().is_some_and(|&(_, t)| t > now) {
            self.seq_times.retain(|&(_, t)| t <= now);
        }
    }

    /// Wall time (ns) at which `seq` was current, if known (the latest sample ≤ seq, so
    /// anything changed after it is treated as newer).
    pub fn time_of_seq(&self, seq: u64) -> Option<i64> {
        let i = self.seq_times.partition_point(|&(s, _)| s <= seq);
        if i == 0 {
            None
        } else {
            Some(self.seq_times[i - 1].1)
        }
    }

    /// Approximate heap bytes held by the index (diagnostics / T12).
    pub fn mem_report(&self) -> String {
        let node = std::mem::size_of::<Node>();
        let bytes = self.nodes.capacity() * node
            + self.arena.capacity()
            + self.ids.pages() * (PAGE * 4 + 32)
            + self.names.capacity() * 9
            + self.idents.capacity() * 9
            + self.free.capacity() * 4;
        format!(
            "{} entries, ≈{} B/entry index heap (nodes {}×{node}B cap {}, arena {}B ({} garbage), id pages {}, names cap {}, idents cap {}, tombs {})",
            self.live,
            bytes / self.live.max(1),
            self.live,
            self.nodes.capacity(),
            self.arena.len(),
            self.arena_garbage,
            self.ids.pages(),
            self.names.capacity(),
            self.idents.capacity(),
            self.tombs.len(),
        )
    }

    pub fn shrink(&mut self) {
        self.nodes.shrink_to_fit();
        self.arena.shrink_to_fit();
        self.names.shrink_to_fit();
        self.idents.shrink_to_fit();
        self.free.shrink_to_fit();
    }
}

pub struct ChildIter<'a> {
    idx: &'a Index,
    cur: Slot,
}

impl Iterator for ChildIter<'_> {
    type Item = Slot;
    fn next(&mut self) -> Option<Slot> {
        if self.cur == NIL {
            return None;
        }
        let s = self.cur;
        self.cur = self.idx.nodes[s as usize].next;
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wall-clock step back measured against the monotonic clock moves every seq→time sample
    /// back by the step (they are then on the new clock); forward steps and jitter move
    /// nothing; samples later than now are dropped.
    #[test]
    fn seq_time_samples_follow_a_wall_clock_step_back() {
        const S: i64 = 1_000_000_000;
        let mut ix = Index::new(vec![]);
        let t0 = 1_800_000_000 * S;
        ix.check_clock_at(t0, 5 * S);
        ix.seq_times = vec![(10, t0 - 60 * S), (20, t0)];
        // Two seconds later, the wall clock stepped back ten minutes.
        ix.check_clock_at(t0 + 2 * S - 600 * S, 7 * S);
        assert_eq!(ix.time_of_seq(20), Some(t0 - 600 * S));
        assert_eq!(ix.time_of_seq(15), Some(t0 - 660 * S));
        // A change made right after the step is later than the client's seen point.
        assert!(t0 + 3 * S - 600 * S > ix.time_of_seq(20).unwrap());
        // Jitter and a forward step leave the samples alone.
        ix.check_clock_at(t0 + 3 * S - 600 * S - 1000, 8 * S);
        ix.check_clock_at(t0 + 3600 * S, 9 * S);
        assert_eq!(ix.time_of_seq(20), Some(t0 - 600 * S));
        // A sample later than now (a step not measured, e.g. across a restart) is dropped.
        let mut ix = Index::new(vec![]);
        ix.seq_times = vec![(10, t0 - 60 * S), (20, t0 + 600 * S)];
        ix.check_clock_at(t0, 5 * S);
        assert_eq!(ix.time_of_seq(20), Some(t0 - 60 * S));
        assert_eq!(ix.time_of_seq(5), None);
    }

    fn st(ino: u64, dir: bool) -> Stat {
        Stat {
            dev: 1,
            ino,
            btime_ns: ino as i64 * 10,
            mode: if dir {
                libc::S_IFDIR | 0o755
            } else {
                libc::S_IFREG | 0o644
            },
            nlink: 1,
            uid: 0,
            gid: 0,
            size: 3,
            mtime_ns: 5,
            ctime_ns: 5,
        }
    }

    #[test]
    fn create_lookup_move_remove() {
        let mut idx = Index::new(vec!["node_modules".into()]);
        let r = idx.create_root("root", &st(1, true));
        let mut t = Txn::default();
        let a = idx.create(r, "a", &st(2, true), NKind::Dir, None, &mut t);
        let f = idx.create(a, "f", &st(3, false), NKind::File, None, &mut t);
        assert_eq!(idx.lookup(a, "f"), Some(f));
        assert_eq!(idx.ident_lookup(&st(3, false)), Some(f));
        assert_eq!(idx.rel_path(f).unwrap(), b"a/f");
        let m0 = idx.node(f).meta_seq;
        idx.move_node(f, r, "g", &mut t);
        assert_eq!(idx.lookup(a, "f"), None);
        assert_eq!(idx.lookup(r, "g"), Some(f));
        assert_eq!(idx.name(f), "g");
        assert!(idx.node(f).meta_seq > m0);
        let c0 = idx.node(f).content_seq;
        idx.update_stat(f, &st(3, false), None, true, true, &mut t);
        assert!(idx.node(f).content_seq > c0, "forced content bump");
        let c1 = idx.node(f).content_seq;
        idx.update_stat(f, &st(3, false), None, false, true, &mut t);
        assert_eq!(idx.node(f).content_seq, c1, "no change, no bump");
        let removed = idx.remove_subtree(a, true, &mut t);
        assert_eq!(removed.len(), 1);
        assert_eq!(idx.len(), 2);
        assert_eq!(t.removed.len(), 1);
        idx.finish_txn(&mut t);
        // entries of changes come parents first
        let mut t2 = Txn::default();
        let d = idx.create(r, "d", &st(10, true), NKind::Dir, None, &mut t2);
        let e = idx.create(d, "e", &st(11, false), NKind::File, None, &mut t2);
        idx.touch_other(d, &mut t2);
        let ch = idx.changes(&t2, |_, _| true);
        let ids: Vec<u64> = ch
            .iter()
            .filter_map(|c| match c {
                Change::Upsert(e) => Some(e.id.0),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![idx.node(d).id, idx.node(e).id]);
    }

    #[test]
    fn btime_mismatch_splits() {
        let mut idx = Index::new(vec![]);
        let r = idx.create_root("root", &st(1, true));
        let mut t = Txn::default();
        idx.create(r, "a", &st(5, false), NKind::File, None, &mut t);
        let mut other = st(5, false);
        other.btime_ns = 999;
        assert_eq!(idx.ident_lookup(&other), None);
        let mut d = st(5, true);
        d.btime_ns = 50;
        assert_eq!(idx.ident_lookup(&d), None, "kind change never matches");
    }

    /// A file deleted and its inode number reused by a new file in the same batch: the new
    /// node must own the identity once the old one is gone (moves and hard links find it).
    #[test]
    fn reused_inode_number_goes_to_the_new_node() {
        let mut idx = Index::new(vec![]);
        let r = idx.create_root("root", &st(1, true));
        let mut t = Txn::default();
        let old = idx.create(r, "old", &st(5, false), NKind::File, None, &mut t);
        idx.detach(old); // deleted, fate decided at the end of the batch
        let mut reused = st(5, false);
        reused.btime_ns = 777;
        assert_eq!(
            idx.ident_lookup(&reused),
            None,
            "different birth: not a move"
        );
        let new = idx.create(r, "new", &reused, NKind::File, None, &mut t);
        idx.remove_subtree(old, true, &mut t);
        assert_eq!(idx.ident_lookup(&reused), Some(new));
    }

    /// Two live single-link nodes of one inode (a hard link whose stat raced an unlink, or a
    /// bind-mount duplicate): when the identity holder goes, the other must take over —
    /// otherwise the inode is in no identity map while still indexed (its hard links are never
    /// dirtied, its moves split).
    #[test]
    fn second_node_of_an_inode_takes_over_its_identity() {
        let mut idx = Index::new(vec![]);
        let r = idx.create_root("root", &st(1, true));
        let mut t = Txn::default();
        let f = idx.create(r, "f", &st(5, false), NKind::File, None, &mut t);
        let x = idx.create(r, "x", &st(5, false), NKind::File, None, &mut t);
        assert_eq!(
            idx.ident_lookup(&st(5, false)),
            Some(f),
            "the first keeps it"
        );
        idx.remove_subtree(f, true, &mut t);
        assert_eq!(idx.ident_lookup(&st(5, false)), Some(x));
        idx.remove_subtree(x, true, &mut t);
        assert_eq!(idx.ident_lookup(&st(5, false)), None);
    }

    #[test]
    fn persist_roundtrip_pnodes() {
        let mut idx = Index::new(vec![]);
        let r = idx.create_root("root", &st(1, true));
        let mut t = Txn::default();
        let a = idx.create(r, "a", &st(2, true), NKind::Dir, None, &mut t);
        idx.create(a, "f", &st(3, false), NKind::File, None, &mut t);
        let mut ls = st(4, false);
        ls.mode = libc::S_IFLNK | 0o777;
        idx.create(a, "l", &ls, NKind::Symlink, Some("f".into()), &mut t);
        let nodes: Vec<PNode> = idx.bfs().into_iter().map(|s| idx.pnode(s)).collect();
        for &s in &idx.bfs() {
            let a = postcard::to_stdvec(&idx.pnode(s)).unwrap();
            let b = postcard::to_stdvec(&idx.pnode_ref(s)).unwrap();
            assert_eq!(a, b, "PNodeRef encodes exactly like PNode");
        }
        let mut idx2 = Index::new(vec![]);
        for p in &nodes {
            assert!(idx2.apply_pnode(p));
        }
        assert_eq!(idx2.len(), 4);
        let a2 = idx2.lookup(idx2.root, "a").unwrap();
        assert!(idx2.lookup(a2, "f").is_some());
        let l2 = idx2.lookup(a2, "l").unwrap();
        assert_eq!(idx2.target(l2), Some("f"));
        assert_eq!(idx2.node(l2).kind(), NKind::Symlink);
        assert_eq!(idx2.seq, idx.seq);
        // replay a move
        let mut p = idx2.pnode(idx2.lookup(a2, "f").unwrap());
        p.parent = 1;
        p.name = "g".into();
        assert!(idx2.apply_pnode(&p));
        assert!(idx2.lookup(idx2.root, "g").is_some());
        assert!(idx2.lookup(a2, "f").is_none());
    }

    #[test]
    fn id_table_pages_and_many_names() {
        let mut t = IdTable::default();
        for id in 0..5000u64 {
            t.insert(id * 3, id as Slot);
        }
        assert_eq!(t.get(300), Some(100));
        assert_eq!(t.get(301), None);
        for id in 0..5000u64 {
            t.remove(id * 3);
        }
        assert_eq!(t.pages(), 0, "empty pages are freed");
        // Many names in one dir (32-bit hash collisions are expected at this size), renames and
        // removals keep lookups exact.
        let mut idx = Index::new(vec![]);
        let r = idx.create_root("root", &st(1, true));
        let mut tx = Txn::default();
        let mut slots = Vec::new();
        for i in 0..100_000u64 {
            slots.push(idx.create(
                r,
                &format!("n{i}"),
                &st(100 + i, false),
                NKind::File,
                None,
                &mut tx,
            ));
        }
        for (i, &s) in slots.iter().enumerate() {
            assert_eq!(idx.lookup(r, &format!("n{i}")), Some(s));
            assert_eq!(idx.ident_lookup(&st(100 + i as u64, false)), Some(s));
        }
        for (i, &s) in slots.iter().enumerate().step_by(2) {
            idx.move_node(s, r, &format!("m{i}"), &mut tx);
        }
        for (i, &s) in slots.iter().enumerate() {
            let want = if i % 2 == 0 {
                format!("m{i}")
            } else {
                format!("n{i}")
            };
            assert_eq!(idx.lookup(r, &want), Some(s));
            assert_eq!(idx.name(s), want);
        }
        for &s in slots.iter().skip(1).step_by(2) {
            idx.remove_subtree(s, true, &mut tx);
        }
        idx.finish_txn(&mut tx);
        idx.force_compact_names();
        for (i, &s) in slots.iter().enumerate().step_by(2) {
            assert_eq!(idx.name(s), format!("m{i}"));
            assert_eq!(idx.lookup(r, &format!("m{i}")), Some(s));
        }
        for i in (1..100_000usize).step_by(2) {
            assert_eq!(idx.lookup(r, &format!("n{i}")), None);
        }
    }
}
