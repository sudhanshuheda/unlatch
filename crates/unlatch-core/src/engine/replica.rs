//! Metadata replica: the in-memory index every hot path reads (item/list/lookup in µs), the
//! engine-local change journal (per-record `changed_seq`, tombstones, move records) that backs
//! the working-set anchor (D5), the materialized set M, and Mac-only metadata.
//!
//! Every mutation goes through [`State`] methods that record what changed in a [`Delta`];
//! [`Delta::write_set`] turns it into rows for [`Db::write`], which persists them in one SQLite
//! transaction (items, tombstones and move records together, review §2(a)5).

use super::names::{assign_group, fold_key};
use super::rules::{capabilities, exec_allowed, exec_relevant_dir_name, symlink_view};
use crate::{err, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use unlatch_proto::ipc::{IpcItem, LocalMeta};
use unlatch_proto::wire::ServerInfo;
use unlatch_proto::{Entry, ErrorCode, IndexId, ItemId, Kind, Version};

/// Tombstones / move records older than this are garbage-collected (review §2(a)4: ≥ 30 days).
pub(crate) const GC_AGE_SECS: i64 = 30 * 86_400;
/// Hard bound on journal rows kept (tombstones + moves).
pub(crate) const MAX_JOURNAL_ROWS: usize = 1_000_000;

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub entry: Entry,
    /// Local journal seq of this item's last change.
    pub changed_seq: u64,
    /// Dir: the one-level listing is known (snapshot `complete_dirs`, a finished `ListDir`, or a
    /// non-lazy dir created by an event).
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tomb {
    pub id: ItemId,
    pub old_parent: ItemId,
    pub server_seq: u64,
    pub ts: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Move {
    pub id: ItemId,
    pub old_parent: ItemId,
    pub ts: i64,
}

/// Children of one directory, with case/normalization collision mapping (rule 11).
#[derive(Default, Debug)]
pub(crate) struct Children {
    /// Display name → id, sorted (list order).
    pub by_display: BTreeMap<String, ItemId>,
    pub by_real: HashMap<String, ItemId>,
    /// Collision key → members (real names).
    groups: HashMap<String, Vec<ItemId>>,
    /// Only for members whose display differs from the real name.
    display_of: HashMap<ItemId, String>,
    /// Key of a generated/override display → its owner.
    generated: HashMap<String, ItemId>,
}

impl Children {
    pub fn len(&self) -> usize {
        self.by_real.len()
    }
}

/// Where an upserted entry came from (decides the initial `complete` flag of a new dir).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Live `Events`: a new non-lazy dir is created empty; its children arrive as events.
    Event,
    Snapshot,
    Listing,
    /// Result of our own mutation (`Mkdir` → new empty dir).
    LocalCreate,
    Other,
}

/// What one apply step changed (for SQL, events, signalling).
#[derive(Default, Debug)]
pub(crate) struct Delta {
    pub upserted: Vec<ItemId>,
    pub removed: Vec<(u64, Tomb)>,
    pub moved: Vec<(u64, Move)>,
    pub untombed: Vec<ItemId>,
    pub parents: BTreeSet<ItemId>,
    /// Some record touches the working set (signal after commit).
    pub ws: bool,
    /// (id, new content version): stale cache files can go.
    pub content_changed: Vec<(ItemId, u64)>,
    pub local_meta: Vec<ItemId>,
    pub overrides: Vec<ItemId>,
    pub delete_seen: Vec<ItemId>,
    pub materialized: Option<MatChange>,
    pub meta: bool,
    pub wipe: bool,
    pub gc: Option<u64>,
}

#[derive(Debug)]
pub(crate) enum MatChange {
    Full,
    Delta {
        added: Vec<ItemId>,
        removed: Vec<ItemId>,
    },
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.upserted.is_empty()
            && self.removed.is_empty()
            && self.untombed.is_empty()
            && self.local_meta.is_empty()
            && self.overrides.is_empty()
            && self.delete_seen.is_empty()
            && self.materialized.is_none()
            && !self.meta
            && !self.wipe
            && self.gc.is_none()
    }

    /// Changed ids (for `ReplicaChanged`).
    pub fn ids(&self) -> Vec<ItemId> {
        let mut v: Vec<ItemId> = self.upserted.clone();
        v.extend(self.removed.iter().map(|(_, t)| t.id));
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Snapshot the rows to persist (cheap clones under the state lock).
    pub fn write_set(&self, st: &State) -> WriteSet {
        let mut ws = WriteSet {
            wipe: self.wipe,
            ..Default::default()
        };
        let mut seen = HashSet::new();
        for id in &self.upserted {
            if !seen.insert(*id) {
                continue;
            }
            match st.nodes.get(id) {
                Some(n) => ws.items.push(n.clone()),
                None => ws.deleted_items.push(*id),
            }
        }
        for (s, t) in &self.removed {
            if seen.insert(t.id) || !st.nodes.contains_key(&t.id) {
                ws.deleted_items.push(t.id);
            }
            if st.tomb_of.get(&t.id) == Some(s) {
                ws.tombs.push((*s, *t));
            }
        }
        ws.untombed = self.untombed.clone();
        ws.moves = self
            .moved
            .iter()
            .filter(|(s, _)| st.moves.contains_key(s))
            .copied()
            .collect();
        for id in &self.local_meta {
            ws.local_meta.push((*id, st.local_meta.get(id).cloned()));
        }
        for id in &self.overrides {
            ws.overrides.push((*id, st.overrides.get(id).cloned()));
        }
        for id in &self.delete_seen {
            ws.delete_seen.push((*id, st.delete_seen.get(id).copied()));
        }
        ws.materialized = match &self.materialized {
            Some(MatChange::Full) => {
                Some(MatWrite::Full(st.materialized.iter().copied().collect()))
            }
            Some(MatChange::Delta { added, removed }) => Some(MatWrite::Delta {
                added: added.clone(),
                removed: removed.clone(),
            }),
            None => None,
        };
        ws.meta = Some(st.meta_rows());
        ws.gc = self.gc;
        ws
    }
}

#[derive(Default, Debug)]
pub(crate) struct WriteSet {
    pub items: Vec<Node>,
    pub deleted_items: Vec<ItemId>,
    pub tombs: Vec<(u64, Tomb)>,
    pub untombed: Vec<ItemId>,
    pub moves: Vec<(u64, Move)>,
    pub local_meta: Vec<(ItemId, Option<LocalMeta>)>,
    pub overrides: Vec<(ItemId, Option<String>)>,
    pub delete_seen: Vec<(ItemId, Option<DeleteSeen>)>,
    pub materialized: Option<MatWrite>,
    pub meta: Option<MetaRows>,
    pub anchor: Option<(u64, u64)>,
    pub wipe: bool,
    pub gc: Option<u64>,
}

#[derive(Debug)]
pub(crate) enum MatWrite {
    Full(Vec<ItemId>),
    Delta {
        added: Vec<ItemId>,
        removed: Vec<ItemId>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct MetaRows {
    pub replica_uuid: u128,
    pub index: Option<IndexId>,
    pub server_seq: u64,
    pub seq: u64,
    pub snapshot_complete: bool,
    pub gc_horizon: u64,
    pub consumed: u64,
    pub info: Option<ServerInfo>,
}

/// The `seen_seq` a delete of one item was first attempted with (rule 6), keyed by what the
/// system sent (`key` = hash of base version + recursive). A retry of the same call — its reply
/// lost, the engine restarted, the system's backoff elapsed — reuses it, even when the system
/// has consumed newer anchors meanwhile: changes consumed after the user deleted the item
/// locally were never shown under it. Released when the system enumerates the item again (it
/// re-learned its contents, e.g. after `DeletionRejected` restored it) or the item goes away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeleteSeen {
    pub key: [u8; 16],
    pub seen: u64,
}

/// The replica.
#[derive(Debug)]
pub(crate) struct State {
    pub nodes: HashMap<ItemId, Node>,
    pub dirs: HashMap<ItemId, Children>,
    /// changed_seq → id (one record per item: its latest change).
    pub by_changed: BTreeMap<u64, ItemId>,
    pub tombs: BTreeMap<u64, Tomb>,
    pub tomb_of: HashMap<ItemId, u64>,
    /// LWW guard: server seq at which an id was removed.
    pub tomb_server: HashMap<ItemId, u64>,
    pub moves: BTreeMap<u64, Move>,
    pub materialized: HashSet<ItemId>,
    pub local_meta: HashMap<ItemId, LocalMeta>,
    /// Display overrides from bounce-like renames kept local (rule 11).
    pub overrides: HashMap<ItemId, String>,
    /// Frozen `seen_seq` of deletes in flight (see [`DeleteSeen`]).
    pub delete_seen: HashMap<ItemId, DeleteSeen>,
    /// local seq (end of a committed batch) → server seq applied at that point (for `seen_seq`).
    pub anchors: BTreeMap<u64, u64>,
    pub seq: u64,
    pub replica_uuid: u128,
    pub index: Option<IndexId>,
    /// Highest server seq such that every event ≤ it has been applied (resume point).
    pub server_seq: u64,
    pub snapshot_complete: bool,
    pub gc_horizon: u64,
    /// Last anchor handed to / consumed by the system (maps to `seen_seq`).
    pub consumed: u64,
    pub info: Option<ServerInfo>,
    /// Transient (never persisted): members of a collision group whose display name changed
    /// as a side effect of a sibling joining or leaving the group (rule 11). The writer that
    /// caused it re-reports them through the working set (MQ-016).
    display_moved: Vec<ItemId>,
}

pub(crate) fn new_uuid() -> u128 {
    rand::random::<u128>() | 1
}

impl State {
    pub fn new() -> State {
        State {
            nodes: HashMap::new(),
            dirs: HashMap::new(),
            by_changed: BTreeMap::new(),
            tombs: BTreeMap::new(),
            tomb_of: HashMap::new(),
            tomb_server: HashMap::new(),
            moves: BTreeMap::new(),
            materialized: HashSet::new(),
            local_meta: HashMap::new(),
            overrides: HashMap::new(),
            delete_seen: HashMap::new(),
            anchors: BTreeMap::new(),
            seq: 0,
            replica_uuid: new_uuid(),
            index: None,
            server_seq: 0,
            snapshot_complete: false,
            gc_horizon: 0,
            consumed: 0,
            info: None,
            display_moved: Vec::new(),
        }
    }

    pub fn meta_rows(&self) -> MetaRows {
        MetaRows {
            replica_uuid: self.replica_uuid,
            index: self.index,
            server_seq: self.server_seq,
            seq: self.seq,
            snapshot_complete: self.snapshot_complete,
            gc_horizon: self.gc_horizon,
            consumed: self.consumed,
            info: self.info.clone(),
        }
    }

    /// M, with the root always included (the system enumerates it at `add(domain)`).
    #[inline]
    pub fn in_m(&self, id: ItemId) -> bool {
        id == ItemId::ROOT || self.materialized.contains(&id)
    }

    // ---- reads ------------------------------------------------------------------------------

    pub fn display_name(&self, id: ItemId) -> Option<&str> {
        let n = self.nodes.get(&id)?;
        if id == ItemId::ROOT {
            return Some(&n.entry.name);
        }
        Some(
            self.dirs
                .get(&n.entry.parent)
                .and_then(|c| c.display_of.get(&id))
                .map_or(n.entry.name.as_str(), |s| s.as_str()),
        )
    }

    /// Depth of `dir` below the root (root = 0). Bounded against corrupt cycles.
    pub fn depth(&self, mut dir: ItemId) -> usize {
        let mut d = 0;
        while dir != ItemId::ROOT && d < 4096 {
            match self.nodes.get(&dir) {
                Some(n) => {
                    dir = n.entry.parent;
                    d += 1;
                }
                None => break,
            }
        }
        d
    }

    fn ancestor_names(&self, mut dir: ItemId) -> Vec<&str> {
        let mut v = Vec::new();
        while dir != ItemId::ROOT && v.len() < 4096 {
            match self.nodes.get(&dir) {
                Some(n) => {
                    v.push(n.entry.name.as_str());
                    dir = n.entry.parent;
                }
                None => break,
            }
        }
        v
    }

    pub fn ipc_item(&self, id: ItemId, expose_exec: bool) -> Option<IpcItem> {
        let n = self.nodes.get(&id)?;
        let mut entry = n.entry.clone();
        let mut blocked = false;
        if entry.kind == Kind::Symlink {
            let target = entry.symlink_target.take().unwrap_or_default();
            let root = self.info.as_ref().map(|i| i.root_path.as_str());
            match symlink_view(&target, self.depth(entry.parent), root) {
                Some(t) => {
                    entry.size = t.len() as u64;
                    entry.symlink_target = Some(t);
                }
                None => {
                    // Rule 9 / D12: a read-only file whose content is the target text. The
                    // entry keeps `Kind::Symlink` (+ `symlink_blocked`), as the IPC contract
                    // says (mac/UnlatchShared/Fixtures/ipc_response_Item~blocked-symlink.json):
                    // the shim maps it to `.plainText` with no `symlinkTargetPath`. Keeping the
                    // kind also means a verdict flip (an ancestor moved) is not a kind change.
                    blocked = true;
                    entry.size = target.len() as u64;
                    entry.symlink_target = Some(target);
                }
            }
        }
        let is_root = id == ItemId::ROOT;
        let parent_access = if is_root {
            None
        } else {
            self.nodes.get(&entry.parent).map(|p| p.entry.access)
        };
        let caps = capabilities(entry.kind, entry.access, blocked, is_root, parent_access);
        let user_exec = expose_exec
            && entry.kind == Kind::File
            && !blocked
            && entry.mode & 0o100 != 0
            && exec_allowed(&entry.name, self.ancestor_names(entry.parent));
        let display_name = self.display_name(id).unwrap_or(&entry.name).to_string();
        let local = self.local_meta.get(&id).cloned().unwrap_or_default();
        Some(IpcItem {
            entry,
            display_name,
            caps,
            local,
            user_exec,
            symlink_blocked: blocked,
        })
    }

    /// One page of `dir` sorted by display name. `cursor` = last display name of the previous
    /// page.
    pub fn list_ids(
        &self,
        dir: ItemId,
        cursor: Option<&[u8]>,
        limit: usize,
    ) -> (Vec<ItemId>, Option<Vec<u8>>) {
        let Some(ch) = self.dirs.get(&dir) else {
            return (Vec::new(), None);
        };
        let limit = limit.max(1);
        let mut out = Vec::with_capacity(limit.min(ch.by_display.len()));
        let iter: Box<dyn Iterator<Item = (&String, &ItemId)>> =
            match cursor {
                Some(c) => {
                    let c = String::from_utf8_lossy(c).into_owned();
                    Box::new(ch.by_display.range::<String, _>((
                        std::ops::Bound::Excluded(c),
                        std::ops::Bound::Unbounded,
                    )))
                }
                None => Box::new(ch.by_display.iter()),
            };
        let mut last: Option<&String> = None;
        let mut more = false;
        for (name, id) in iter {
            if out.len() == limit {
                more = true;
                break;
            }
            out.push(*id);
            last = Some(name);
        }
        let next = if more {
            last.map(|s| s.as_bytes().to_vec())
        } else {
            None
        };
        (out, next)
    }

    pub fn lookup_id(&self, dir: ItemId, name: &str) -> Option<ItemId> {
        let ch = self.dirs.get(&dir)?;
        ch.by_display
            .get(name)
            .or_else(|| ch.by_real.get(name))
            .copied()
    }

    /// Does `id` share its collision key with a sibling?
    pub fn collides(&self, id: ItemId) -> bool {
        let Some(n) = self.nodes.get(&id) else {
            return false;
        };
        self.dirs
            .get(&n.entry.parent)
            .and_then(|c| c.groups.get(&fold_key(&n.entry.name)))
            .is_some_and(|g| g.len() > 1)
    }

    pub fn child_count(&self, dir: ItemId) -> usize {
        self.dirs.get(&dir).map_or(0, |c| c.len())
    }

    pub fn children_of(&self, dir: ItemId) -> Vec<ItemId> {
        self.dirs
            .get(&dir)
            .map(|c| c.by_real.values().copied().collect())
            .unwrap_or_default()
    }

    /// Descendants of `id` then `id` itself (children before parents). Each item is visited
    /// once, so a corrupt cycle cannot make this loop ([`State::upsert`] refuses to create one).
    pub fn subtree_post_order(&self, id: ItemId) -> Vec<ItemId> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![(id, false)];
        while let Some((x, expanded)) = stack.pop() {
            if expanded {
                out.push(x);
                continue;
            }
            if !seen.insert(x) {
                continue;
            }
            stack.push((x, true));
            if let Some(ch) = self.dirs.get(&x) {
                let mut kids: Vec<ItemId> = ch.by_real.values().copied().collect();
                kids.sort_unstable();
                for k in kids {
                    if !seen.contains(&k) {
                        stack.push((k, false));
                    }
                }
            }
        }
        out
    }

    /// Would making `parent` the parent of `id` create a cycle (`parent` is `id` or one of its
    /// descendants)? Bounded like [`State::depth`]; an over-deep or already-cyclic chain counts as
    /// a cycle too.
    fn would_cycle(&self, id: ItemId, parent: ItemId) -> bool {
        let mut dir = parent;
        for _ in 0..4096 {
            if dir == id {
                return true;
            }
            if dir == ItemId::ROOT {
                return false;
            }
            match self.nodes.get(&dir) {
                Some(n) => dir = n.entry.parent,
                None => return false,
            }
        }
        true
    }

    /// Current rule-10 verdict ([`exec_allowed`], without `expose_exec`) of every exec-mode file
    /// strictly below dir `id`.
    fn exec_verdicts_below(&self, id: ItemId) -> Vec<(ItemId, bool)> {
        self.subtree_post_order(id)
            .into_iter()
            .filter(|x| *x != id)
            .filter_map(|x| {
                let n = self.nodes.get(&x)?;
                (n.entry.kind == Kind::File && n.entry.mode & 0o100 != 0).then(|| {
                    (
                        x,
                        exec_allowed(&n.entry.name, self.ancestor_names(n.entry.parent)),
                    )
                })
            })
            .collect()
    }

    /// Materialized items (M, root excluded) that removing these subtrees would delete.
    pub fn materialized_in_subtrees(&self, roots: &[ItemId]) -> usize {
        let mut seen = HashSet::new();
        let mut count = 0;
        for r in roots {
            for x in self.subtree_post_order(*r) {
                if seen.insert(x)
                    && x != ItemId::ROOT
                    && self.materialized.contains(&x)
                    && self.nodes.contains_key(&x)
                {
                    count += 1;
                }
            }
        }
        count
    }

    /// `ids` plus all their ancestors (for "keep items with pending edits + ancestors").
    pub fn with_ancestors(&self, ids: impl IntoIterator<Item = ItemId>) -> HashSet<ItemId> {
        let mut keep = HashSet::new();
        for mut x in ids {
            let mut guard = 0;
            while keep.insert(x) && x != ItemId::ROOT && guard < 4096 {
                match self.nodes.get(&x) {
                    Some(n) => x = n.entry.parent,
                    None => break,
                }
                guard += 1;
            }
        }
        keep
    }

    /// Server seq covered by local anchor `a` (the last committed batch ending at or before it).
    pub fn server_seq_at(&self, a: u64) -> u64 {
        self.anchors.range(..=a).next_back().map_or(0, |(_, s)| *s)
    }

    /// Working-set changes in `(from, upto]` (review §2(a)5, D5): items with changed_seq in range
    /// where `id ∈ M || parent ∈ M || old_parent ∈ M` (old parents from move records in range),
    /// tombstones likewise, merged in seq order (tombstones were assigned children-first).
    /// Returns `(updated, removed, anchor_seq, more)`.
    pub fn changes(
        &self,
        from: u64,
        upto: u64,
        limit: usize,
        expose_exec: bool,
    ) -> (Vec<IpcItem>, Vec<ItemId>, u64, bool) {
        let limit = limit.max(1);
        let mut moved_out: HashSet<ItemId> = HashSet::new();
        if from < upto {
            for m in self.moves.range(from + 1..=upto).map(|(_, m)| m) {
                if self.in_m(m.old_parent) {
                    moved_out.insert(m.id);
                }
            }
        }
        let mut updated = Vec::new();
        let mut removed = Vec::new();
        if from >= upto {
            return (updated, removed, upto.max(from), false);
        }
        let mut tomb_memo: HashMap<u64, bool> = HashMap::new();
        let mut items = self.by_changed.range(from + 1..=upto).peekable();
        let mut tombs = self.tombs.range(from + 1..=upto).peekable();
        let mut last = from;
        loop {
            let next_item = items.peek().map(|(s, _)| **s);
            let next_tomb = tombs.peek().map(|(s, _)| **s);
            let take_item = match (next_item, next_tomb) {
                (None, None) => return (updated, removed, upto, false),
                (Some(a), Some(b)) => a < b,
                (Some(_), None) => true,
                (None, Some(_)) => false,
            };
            if updated.len() + removed.len() >= limit {
                return (updated, removed, last, true);
            }
            if take_item {
                let Some((s, id)) = items.next() else {
                    continue;
                };
                last = *s;
                if let Some(n) = self.nodes.get(id) {
                    if self.in_m(*id) || self.in_m(n.entry.parent) || moved_out.contains(id) {
                        if let Some(it) = self.ipc_item(*id, expose_exec) {
                            updated.push(it);
                        }
                    }
                }
            } else {
                let Some((s, t)) = tombs.next() else { continue };
                last = *s;
                if self.tomb_reported(*s, t, &moved_out, &mut tomb_memo) {
                    removed.push(t.id);
                }
            }
        }
    }

    /// Is tombstone `t` (journalled at `s`) reported? Directly when `id ∈ M || old_parent ∈ M`
    /// (or it moved out of M in the window); and also when its old parent was removed by the
    /// same subtree removal (a later tombstone, children first) and *that* tombstone is
    /// reported: the Mac can hold a child of a never-enumerated folder (moved in through the
    /// working set), and it keeps a folder until every child it holds is reported deleted
    /// (review (a)5). Reporting a removal the Mac never held is harmless.
    fn tomb_reported(
        &self,
        s: u64,
        t: &Tomb,
        moved_out: &HashSet<ItemId>,
        memo: &mut HashMap<u64, bool>,
    ) -> bool {
        // Walk up the chain of removed ancestors (iteratively: rm -rf can be deep).
        let mut chain: Vec<u64> = Vec::new();
        let (mut cs, mut ct) = (s, *t);
        let verdict = loop {
            if let Some(v) = memo.get(&cs) {
                break *v;
            }
            if self.in_m(ct.id) || self.in_m(ct.old_parent) || moved_out.contains(&ct.id) {
                memo.insert(cs, true);
                break true;
            }
            chain.push(cs);
            let parent_tomb = self
                .tomb_of
                .get(&ct.old_parent)
                .filter(|ps| **ps > cs)
                .and_then(|ps| self.tombs.get(ps).map(|pt| (*ps, *pt)));
            match parent_tomb {
                Some((ps, pt)) if chain.len() < 4096 => {
                    cs = ps;
                    ct = pt;
                }
                _ => break false,
            }
        };
        for c in chain {
            memo.insert(c, verdict);
        }
        verdict
    }

    // ---- children index (collision mapping) ---------------------------------------------------

    fn child_add(&mut self, parent: ItemId, id: ItemId, name: &str) {
        let key = fold_key(name);
        let ch = self.dirs.entry(parent).or_default();
        ch.by_real.insert(name.to_string(), id);
        let g = ch.groups.entry(key.clone()).or_default();
        g.push(id);
        let crowded = g.len() > 1;
        let clash_owner = ch.generated.get(&key).copied();
        if !crowded && clash_owner.is_none() && !self.overrides.contains_key(&id) {
            ch.by_display.insert(name.to_string(), id);
            return;
        }
        // A real name now claims the key of another item's generated display: move that one.
        if let Some(owner) = clash_owner {
            if owner != id {
                if let Some(oname) = self.nodes.get(&owner).map(|n| fold_key(&n.entry.name)) {
                    if let Some(ch) = self.dirs.get_mut(&parent) {
                        ch.generated.remove(&key);
                    }
                    self.recompute_group(parent, &oname);
                }
            }
        }
        self.recompute_group(parent, &key);
    }

    fn child_remove(&mut self, parent: ItemId, id: ItemId, name: &str) {
        let key = fold_key(name);
        let Some(ch) = self.dirs.get_mut(&parent) else {
            return;
        };
        if ch.by_real.get(name) == Some(&id) {
            ch.by_real.remove(name);
        }
        match ch.display_of.remove(&id) {
            Some(d) => {
                if ch.by_display.get(&d) == Some(&id) {
                    ch.by_display.remove(&d);
                }
                let dk = fold_key(&d);
                if ch.generated.get(&dk) == Some(&id) {
                    ch.generated.remove(&dk);
                }
            }
            None => {
                if ch.by_display.get(name) == Some(&id) {
                    ch.by_display.remove(name);
                }
            }
        }
        let mut regroup = false;
        if let Some(g) = ch.groups.get_mut(&key) {
            g.retain(|x| *x != id);
            if g.is_empty() {
                ch.groups.remove(&key);
            } else {
                regroup = true;
            }
        }
        if regroup {
            self.recompute_group(parent, &key);
        }
    }

    /// Reassign display names of one collision group (first real name in byte order keeps it).
    fn recompute_group(&mut self, parent: ItemId, key: &str) {
        let State {
            nodes,
            dirs,
            overrides,
            display_moved,
            ..
        } = self;
        let Some(ch) = dirs.get_mut(&parent) else {
            return;
        };
        let members: Vec<ItemId> = ch.groups.get(key).cloned().unwrap_or_default();
        // Drop the members' current displays (remembering them: a member whose display changes
        // must be re-reported, MQ-016).
        let mut before: HashMap<ItemId, String> = HashMap::new();
        for m in &members {
            let Some(n) = nodes.get(m) else { continue };
            match ch.display_of.remove(m) {
                Some(d) => {
                    if ch.by_display.get(&d) == Some(m) {
                        ch.by_display.remove(&d);
                    }
                    let dk = fold_key(&d);
                    if ch.generated.get(&dk) == Some(m) {
                        ch.generated.remove(&dk);
                    }
                    before.insert(*m, d);
                }
                None => {
                    if ch.by_display.get(&n.entry.name) == Some(m) {
                        ch.by_display.remove(&n.entry.name);
                        before.insert(*m, n.entry.name.clone());
                    }
                }
            }
        }
        let mut plain: Vec<(ItemId, &str)> = Vec::new();
        let mut assigned: Vec<(ItemId, String)> = Vec::new();
        for m in &members {
            let Some(n) = nodes.get(m) else { continue };
            match overrides.get(m) {
                Some(o)
                    if !ch.groups.contains_key(&fold_key(o))
                        && !ch.generated.contains_key(&fold_key(o)) =>
                {
                    ch.generated.insert(fold_key(o), *m);
                    assigned.push((*m, o.clone()));
                }
                _ => plain.push((*m, n.entry.name.as_str())),
            }
        }
        let names: Vec<&str> = plain.iter().map(|(_, n)| *n).collect();
        let taken =
            |k: &str| (k != key && ch.groups.contains_key(k)) || ch.generated.contains_key(k);
        let displays = assign_group(&names, &taken);
        for ((m, real), d) in plain.iter().zip(displays) {
            if d != *real {
                ch.generated.insert(fold_key(&d), *m);
            }
            assigned.push((*m, d));
        }
        for (m, d) in assigned {
            let real = nodes.get(&m).map(|n| n.entry.name.as_str()).unwrap_or("");
            if d != real {
                ch.display_of.insert(m, d.clone());
            }
            // A member newly joining the group has no previous display: its own writer reports it.
            if before.get(&m).is_some_and(|b| *b != d) {
                display_moved.push(m);
            }
            ch.by_display.insert(d, m);
        }
    }

    // ---- writes -------------------------------------------------------------------------------

    fn ws_touch(&self, id: ItemId, parent: ItemId, old_parent: Option<ItemId>) -> bool {
        self.in_m(id) || self.in_m(parent) || old_parent.is_some_and(|p| self.in_m(p))
    }

    /// Re-report an unchanged item through the working set: give it a fresh `changed_seq`.
    fn touch(&mut self, id: ItemId, d: &mut Delta) {
        let Some(n) = self.nodes.get_mut(&id) else {
            return;
        };
        self.by_changed.remove(&n.changed_seq);
        self.seq += 1;
        n.changed_seq = self.seq;
        let parent = n.entry.parent;
        self.by_changed.insert(self.seq, id);
        d.upserted.push(id);
        if id != ItemId::ROOT {
            d.parents.insert(parent);
        }
        d.ws |= self.ws_touch(id, parent, None);
    }

    /// Re-report each of `ids` (deduplicated, skipping `except` and items no longer known).
    fn touch_all(&mut self, ids: Vec<ItemId>, except: ItemId, d: &mut Delta) {
        let mut seen = HashSet::new();
        for x in ids {
            if x != except && seen.insert(x) {
                self.touch(x, d);
            }
        }
    }

    /// Symlinks below dir `id` whose rule-9 view (blocked or not, and the rewritten target of an
    /// absolute in-root link) changes because `id` moved `delta` levels deeper (D12): the link
    /// is untouched on the VM, but `../../x` at depth 2 is an escape at depth 1.
    fn symlink_flips(&self, id: ItemId, delta: i64) -> Vec<ItemId> {
        if delta == 0 || !self.dirs.contains_key(&id) {
            return Vec::new();
        }
        let root = self.info.as_ref().map(|i| i.root_path.as_str());
        let mut out = Vec::new();
        for x in self.subtree_post_order(id) {
            if x == id {
                continue;
            }
            let Some(n) = self.nodes.get(&x) else {
                continue;
            };
            if n.entry.kind != Kind::Symlink {
                continue;
            }
            let target = n.entry.symlink_target.as_deref().unwrap_or("");
            let now = self.depth(n.entry.parent) as i64;
            let was = (now - delta).max(0) as usize;
            if symlink_view(target, was, root) != symlink_view(target, now as usize, root) {
                out.push(x);
            }
        }
        out
    }

    /// Apply an upsert, last-writer-wins by `Entry.seq` (D15, rule 13). Returns whether it applied.
    ///
    /// Besides the item itself this re-reports items whose *presentation* changed as a side
    /// effect: collision-group siblings whose display name moved (rule 11, MQ-016) and symlinks
    /// below a moved directory whose rule-9 verdict changed (D12). Siblings displaced by the
    /// newcomer and flipped symlinks are journalled *before* the item (so the Mac never holds two
    /// names that fold equal, and never sees an escaping link at its new depth); siblings that
    /// regain a name the item vacated are journalled *after* it.
    pub fn upsert(&mut self, e: Entry, source: Source, d: &mut Delta) -> bool {
        if self.tomb_server.get(&e.id).is_some_and(|ts| *ts >= e.seq) {
            return false;
        }
        if self.nodes.get(&e.id).is_some_and(|n| n.entry.seq >= e.seq) {
            return false;
        }
        let reparented = self
            .nodes
            .get(&e.id)
            .is_none_or(|n| n.entry.parent != e.parent);
        if e.id != ItemId::ROOT && reparented && self.would_cycle(e.id, e.parent) {
            // Only a hostile or corrupt VM sends this (the VM is untrusted input): applying it
            // would detach a subtree into a loop that every traversal below would walk forever.
            tracing::warn!(
                id = e.id.0,
                parent = e.parent.0,
                seq = e.seq,
                "rejecting upsert: the new parent is the item itself or one of its descendants"
            );
            return false;
        }
        let id = e.id;
        let old = self.nodes.remove(&id);
        let is_root = id == ItemId::ROOT;
        let mut moved_from = None;
        let mut relink = old.is_none();
        let mut old_depth: Option<usize> = None;
        let mut exec_before: Vec<(ItemId, bool)> = Vec::new();
        let mut after: Vec<ItemId> = Vec::new();
        if let Some(o) = &old {
            if !is_root && (o.entry.parent != e.parent || o.entry.name != e.name) {
                self.nodes.insert(id, o.clone());
                if o.entry.parent != e.parent {
                    old_depth = Some(self.depth(o.entry.parent));
                }
                // Rule 10 depends on ancestor names: a move, or a rename into/out of the
                // `X.app/Contents/MacOS` pattern, can flip the exec verdict of files below.
                if o.entry.kind == Kind::Dir
                    && (o.entry.parent != e.parent
                        || exec_relevant_dir_name(&o.entry.name)
                        || exec_relevant_dir_name(&e.name))
                {
                    exec_before = self.exec_verdicts_below(id);
                }
                self.child_remove(o.entry.parent, id, &o.entry.name);
                after = std::mem::take(&mut self.display_moved);
                self.nodes.remove(&id);
                if self.overrides.remove(&id).is_some() {
                    d.overrides.push(id);
                }
                relink = true;
                if o.entry.parent != e.parent {
                    moved_from = Some(o.entry.parent);
                }
            }
            self.by_changed.remove(&o.changed_seq);
            if o.entry.kind == Kind::File && o.entry.version.content != e.version.content {
                d.content_changed.push((id, e.version.content));
            }
        }
        let complete = match &old {
            Some(o) if e.kind == Kind::Dir => o.complete && !(e.lazy && !o.entry.lazy),
            Some(_) => false,
            None => {
                e.kind == Kind::Dir
                    && !e.lazy
                    && matches!(source, Source::Event | Source::LocalCreate)
            }
        };
        let parent = e.parent;
        let name = e.name.clone();
        self.nodes.insert(
            id,
            Node {
                entry: e,
                changed_seq: 0,
                complete,
            },
        );
        if relink && !is_root {
            self.child_add(parent, id, &name);
        }
        let mut before = std::mem::take(&mut self.display_moved);
        if let Some(od) = old_depth {
            let delta = self.depth(parent) as i64 - od as i64;
            before.extend(self.symlink_flips(id, delta));
        }
        // Files whose exec verdict flipped are journalled before the dir, like flipped symlinks,
        // so the Mac never holds +x on a binary that is now inside a bundle.
        for (x, was) in exec_before {
            if let Some(n) = self.nodes.get(&x) {
                if exec_allowed(&n.entry.name, self.ancestor_names(n.entry.parent)) != was {
                    before.push(x);
                }
            }
        }
        self.touch_all(before, id, d);
        self.seq += 1;
        let seq = self.seq;
        self.by_changed.insert(seq, id);
        if let Some(n) = self.nodes.get_mut(&id) {
            n.changed_seq = seq;
        }
        if let Some(ts) = self.tomb_of.remove(&id) {
            self.tombs.remove(&ts);
            d.untombed.push(id);
        }
        if let Some(op) = moved_from {
            let m = Move {
                id,
                old_parent: op,
                ts: now_secs(),
            };
            self.moves.insert(seq, m);
            d.moved.push((seq, m));
        }
        d.ws |= self.ws_touch(id, parent, moved_from);
        if !is_root {
            d.parents.insert(parent);
        }
        if let Some(p) = moved_from {
            d.parents.insert(p);
        }
        d.upserted.push(id);
        self.touch_all(after, id, d);
        true
    }

    /// Mark a dir's listing complete / incomplete.
    pub fn set_complete(&mut self, dir: ItemId, complete: bool, d: &mut Delta) {
        if let Some(n) = self.nodes.get_mut(&dir) {
            if n.complete != complete {
                n.complete = complete;
                d.upserted.push(dir);
                d.parents.insert(dir);
            }
        }
    }

    fn remove_node(&mut self, x: ItemId, server_seq: u64, d: &mut Delta) {
        let Some(n) = self.nodes.get(&x).cloned() else {
            return;
        };
        self.child_remove(n.entry.parent, x, &n.entry.name);
        self.nodes.remove(&x);
        self.by_changed.remove(&n.changed_seq);
        if self.dirs.get(&x).is_some_and(|c| c.len() == 0) {
            self.dirs.remove(&x);
        }
        if self.overrides.remove(&x).is_some() {
            d.overrides.push(x);
        }
        if self.local_meta.remove(&x).is_some() {
            d.local_meta.push(x);
        }
        if self.delete_seen.remove(&x).is_some() {
            d.delete_seen.push(x);
        }
        self.seq += 1;
        let seq = self.seq;
        let t = Tomb {
            id: x,
            old_parent: n.entry.parent,
            server_seq,
            ts: now_secs(),
        };
        self.tombs.insert(seq, t);
        if let Some(old) = self.tomb_of.insert(x, seq) {
            self.tombs.remove(&old);
        }
        let ts = self.tomb_server.entry(x).or_insert(0);
        *ts = (*ts).max(server_seq);
        d.ws |= self.in_m(x) || self.in_m(n.entry.parent);
        d.parents.insert(n.entry.parent);
        d.removed.push((seq, t));
    }

    /// Apply `Remove { id, seq }` (the whole subtree), LWW, children first. Items in `keep`
    /// (pending local edits and their ancestors) survive.
    pub fn remove(
        &mut self,
        id: ItemId,
        server_seq: u64,
        keep: &HashSet<ItemId>,
        d: &mut Delta,
    ) -> bool {
        let ts = self.tomb_server.entry(id).or_insert(0);
        *ts = (*ts).max(server_seq);
        let Some(n) = self.nodes.get(&id) else {
            return false;
        };
        if n.entry.seq > server_seq || id == ItemId::ROOT {
            return false;
        }
        for x in self.subtree_post_order(id) {
            if !keep.contains(&x) {
                self.remove_node(x, server_seq, d);
            }
        }
        // Siblings that regain a plain name (rule 11) are re-reported after the tombstones.
        let moved = std::mem::take(&mut self.display_moved);
        self.touch_all(moved, ItemId::ROOT, d);
        true
    }

    /// Remove every item for which `drop` holds (snapshot drop / listing drop), children first,
    /// keeping `keep`. Tombstone server seq = the item's own seq (a newer upsert restores it).
    pub fn drop_where(
        &mut self,
        candidates: &HashSet<ItemId>,
        keep: &HashSet<ItemId>,
        d: &mut Delta,
    ) -> usize {
        let mut order: Vec<(usize, ItemId)> = candidates
            .iter()
            .filter(|x| **x != ItemId::ROOT && !keep.contains(x))
            .map(|x| (0, *x))
            .collect();
        for o in order.iter_mut() {
            o.0 = self
                .nodes
                .get(&o.1)
                .map_or(0, |n| self.depth(n.entry.parent));
        }
        order.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut n = 0;
        for (_, x) in order {
            let Some(s) = self.nodes.get(&x).map(|n| n.entry.seq) else {
                continue;
            };
            // Dropping a dir drops what's left under it (children-first order).
            for y in self.subtree_post_order(x) {
                if !keep.contains(&y) {
                    self.remove_node(y, s, d);
                    n += 1;
                }
            }
        }
        let moved = std::mem::take(&mut self.display_moved);
        self.touch_all(moved, ItemId::ROOT, d);
        n
    }

    /// Forget everything (index changed): new replica uuid so every old anchor expires.
    pub fn wipe(&mut self, d: &mut Delta) {
        let seq = self.seq;
        *self = State::new();
        // Keep the counter monotonic anyway.
        self.seq = seq;
        d.wipe = true;
        d.meta = true;
        d.ws = true;
    }

    pub fn set_materialized(
        &mut self,
        added: &[ItemId],
        removed: &[ItemId],
        full: bool,
        d: &mut Delta,
    ) {
        if full {
            self.materialized = added.iter().copied().collect();
            d.materialized = Some(MatChange::Full);
        } else {
            for r in removed {
                self.materialized.remove(r);
            }
            for a in added {
                self.materialized.insert(*a);
            }
            // Two changes in one batch (or a batch kept pending after a failed commit) cannot
            // be replayed as one add/remove pair in order: rewrite the whole set instead.
            d.materialized = match d.materialized.take() {
                None => Some(MatChange::Delta {
                    added: added.to_vec(),
                    removed: removed.to_vec(),
                }),
                Some(_) => Some(MatChange::Full),
            };
        }
    }

    /// Record (or with `None` release) the frozen `seen_seq` of a delete of `id`.
    pub fn set_delete_seen(&mut self, id: ItemId, e: Option<DeleteSeen>, d: &mut Delta) {
        let changed = match e {
            Some(e) if self.nodes.contains_key(&id) => self.delete_seen.insert(id, e) != Some(e),
            Some(_) => false,
            None => self.delete_seen.remove(&id).is_some(),
        };
        if changed {
            d.delete_seen.push(id);
        }
    }

    pub fn set_override(&mut self, id: ItemId, display: Option<String>, d: &mut Delta) {
        let Some((parent, name)) = self
            .nodes
            .get(&id)
            .map(|n| (n.entry.parent, n.entry.name.clone()))
        else {
            return;
        };
        self.child_remove(parent, id, &name);
        let after = std::mem::take(&mut self.display_moved);
        match display {
            Some(s) => self.overrides.insert(id, s),
            None => self.overrides.remove(&id),
        };
        self.child_add(parent, id, &name);
        let before = std::mem::take(&mut self.display_moved);
        self.touch_all(before, id, d);
        // The system must learn the new display name through the working set too.
        self.touch(id, d);
        d.overrides.push(id);
        self.touch_all(after, id, d);
    }

    /// Drop journal rows older than the GC age (or beyond the row bound); advance the horizon.
    pub fn gc(&mut self, now: i64, d: &mut Delta) {
        let cutoff = now - GC_AGE_SECS;
        let mut horizon = self.gc_horizon;
        let excess = (self.tombs.len() + self.moves.len()).saturating_sub(MAX_JOURNAL_ROWS);
        let mut dropped = 0usize;
        while let Some((&s, &t)) = self.tombs.iter().next() {
            if t.ts >= cutoff && dropped >= excess {
                break;
            }
            self.tombs.remove(&s);
            if self.tomb_of.get(&t.id) == Some(&s) {
                self.tomb_of.remove(&t.id);
            }
            if !self.nodes.contains_key(&t.id) {
                self.materialized.remove(&t.id);
            }
            horizon = horizon.max(s);
            dropped += 1;
        }
        while let Some((&s, &m)) = self.moves.iter().next() {
            if m.ts >= cutoff && dropped >= excess {
                break;
            }
            self.moves.remove(&s);
            horizon = horizon.max(s);
            dropped += 1;
        }
        // Keep the anchor map small: everything above the horizon plus the last entry below.
        if let Some((&keep_from, _)) = self.anchors.range(..=horizon).next_back() {
            self.anchors = self.anchors.split_off(&keep_from);
        }
        while self.anchors.len() > 100_000 {
            let Some((&k, _)) = self.anchors.iter().next() else {
                break;
            };
            self.anchors.remove(&k);
        }
        // The LWW guard for ids we no longer track (removed long ago, or never known) goes too.
        let State {
            tomb_server,
            tomb_of,
            nodes,
            ..
        } = self;
        tomb_server.retain(|id, _| tomb_of.contains_key(id) || nodes.contains_key(id));
        if horizon != self.gc_horizon {
            self.gc_horizon = horizon;
            d.gc = Some(horizon);
            d.meta = true;
            d.materialized = Some(MatChange::Full);
        }
    }
}

// ---- SQLite --------------------------------------------------------------------------------

pub(crate) struct Db {
    conn: Connection,
}

fn sql_err(e: rusqlite::Error) -> unlatch_proto::ProtoError {
    err(ErrorCode::Io, format!("replica db: {e}"))
}

fn kind_to_i(k: Kind) -> i64 {
    match k {
        Kind::File => 0,
        Kind::Dir => 1,
        Kind::Symlink => 2,
    }
}

fn kind_from_i(i: i64) -> Kind {
    match i {
        1 => Kind::Dir,
        2 => Kind::Symlink,
        _ => Kind::File,
    }
}

// SQLite INTEGER is i64: u64 values are stored bit-cast.
#[inline]
fn i(v: u64) -> i64 {
    v as i64
}
#[inline]
fn u(v: i64) -> u64 {
    v as u64
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS items (
  id INTEGER PRIMARY KEY, parent INTEGER NOT NULL, name TEXT NOT NULL, kind INTEGER NOT NULL,
  size INTEGER NOT NULL, mtime INTEGER NOT NULL, mode INTEGER NOT NULL, vc INTEGER NOT NULL,
  vm INTEGER NOT NULL, target TEXT, lazy INTEGER NOT NULL, seq INTEGER NOT NULL,
  access INTEGER NOT NULL, changed_seq INTEGER NOT NULL, complete INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS items_changed ON items(changed_seq);
CREATE TABLE IF NOT EXISTS tombstones (seq INTEGER PRIMARY KEY, id INTEGER NOT NULL UNIQUE,
  old_parent INTEGER NOT NULL, server_seq INTEGER NOT NULL, ts INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS moves (seq INTEGER PRIMARY KEY, id INTEGER NOT NULL,
  old_parent INTEGER NOT NULL, ts INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS materialized (id INTEGER PRIMARY KEY);
CREATE TABLE IF NOT EXISTS local_meta (id INTEGER PRIMARY KEY, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS display_override (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS anchors (local_seq INTEGER PRIMARY KEY, server_seq INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS delete_seen (id INTEGER PRIMARY KEY, key BLOB NOT NULL,
  seen INTEGER NOT NULL);
";

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).map_err(sql_err)?;
        // FULL: an anchor handed to the system must never be lost by a power cut (it would
        // otherwise be "from the future" after restart).
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=OFF;",
        )
        .map_err(sql_err)?;
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
        Ok(Db { conn })
    }

    fn meta_get(&self, k: &str) -> Result<Option<Vec<u8>>> {
        self.conn
            .query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .optional()
            .map_err(sql_err)
    }

    fn meta_u64(&self, k: &str) -> Result<u64> {
        Ok(self
            .meta_get(k)?
            .and_then(|v| <[u8; 8]>::try_from(v.as_slice()).ok())
            .map_or(0, u64::from_le_bytes))
    }

    /// Load the persisted replica (engine start; works offline).
    pub fn load(&self) -> Result<State> {
        let mut st = State::new();
        match self
            .meta_get("replica_uuid")?
            .and_then(|v| <[u8; 16]>::try_from(v.as_slice()).ok())
        {
            Some(b) => st.replica_uuid = u128::from_le_bytes(b),
            None => {
                // Fresh db: persist the new uuid right away.
                self.conn
                    .execute(
                        "INSERT OR REPLACE INTO meta(k, v) VALUES ('replica_uuid', ?1)",
                        [st.replica_uuid.to_le_bytes().to_vec()],
                    )
                    .map_err(sql_err)?;
            }
        }
        st.index = self
            .meta_get("index_id")?
            .and_then(|v| <[u8; 16]>::try_from(v.as_slice()).ok())
            .map(|b| IndexId(u128::from_le_bytes(b)));
        st.server_seq = self.meta_u64("server_seq")?;
        st.seq = self.meta_u64("seq")?;
        st.snapshot_complete = self.meta_u64("snapshot_complete")? == 1;
        st.gc_horizon = self.meta_u64("gc_horizon")?;
        st.consumed = self.meta_u64("consumed")?;
        st.info = self
            .meta_get("info")?
            .and_then(|v| postcard::from_bytes(&v).ok());

        {
            let mut q = self
                .conn
                .prepare("SELECT id, name FROM display_override")
                .map_err(sql_err)?;
            let rows = q
                .query_map([], |r| Ok((ItemId(u(r.get(0)?)), r.get::<_, String>(1)?)))
                .map_err(sql_err)?;
            for row in rows {
                let (id, n) = row.map_err(sql_err)?;
                st.overrides.insert(id, n);
            }
        }
        let mut nodes = Vec::new();
        {
            let mut q = self
                .conn
                .prepare(
                    "SELECT id, parent, name, kind, size, mtime, mode, vc, vm, target, lazy, seq, access, \
                     changed_seq, complete FROM items",
                )
                .map_err(sql_err)?;
            let rows = q
                .query_map([], |r| {
                    Ok(Node {
                        entry: Entry {
                            id: ItemId(u(r.get(0)?)),
                            parent: ItemId(u(r.get(1)?)),
                            name: r.get(2)?,
                            kind: kind_from_i(r.get(3)?),
                            size: u(r.get(4)?),
                            mtime_ns: r.get(5)?,
                            mode: r.get::<_, i64>(6)? as u32,
                            version: Version {
                                content: u(r.get(7)?),
                                meta: u(r.get(8)?),
                            },
                            symlink_target: r.get(9)?,
                            lazy: r.get::<_, i64>(10)? != 0,
                            seq: u(r.get(11)?),
                            access: r.get::<_, i64>(12)? as u8,
                        },
                        changed_seq: u(r.get(13)?),
                        complete: r.get::<_, i64>(14)? != 0,
                    })
                })
                .map_err(sql_err)?;
            for row in rows {
                nodes.push(row.map_err(sql_err)?);
            }
        }
        for n in &nodes {
            st.by_changed.insert(n.changed_seq, n.entry.id);
            st.nodes.insert(n.entry.id, n.clone());
        }
        for n in &nodes {
            if n.entry.id != ItemId::ROOT {
                st.child_add(n.entry.parent, n.entry.id, &n.entry.name);
            }
        }
        // Rebuilding the index is not a change: the system already holds these displays.
        st.display_moved.clear();
        {
            let mut q = self
                .conn
                .prepare("SELECT seq, id, old_parent, server_seq, ts FROM tombstones")
                .map_err(sql_err)?;
            let rows = q
                .query_map([], |r| {
                    Ok((
                        u(r.get(0)?),
                        Tomb {
                            id: ItemId(u(r.get(1)?)),
                            old_parent: ItemId(u(r.get(2)?)),
                            server_seq: u(r.get(3)?),
                            ts: r.get(4)?,
                        },
                    ))
                })
                .map_err(sql_err)?;
            for row in rows {
                let (s, t) = row.map_err(sql_err)?;
                st.tombs.insert(s, t);
                st.tomb_of.insert(t.id, s);
                st.tomb_server.insert(t.id, t.server_seq);
            }
        }
        {
            let mut q = self
                .conn
                .prepare("SELECT seq, id, old_parent, ts FROM moves")
                .map_err(sql_err)?;
            let rows = q
                .query_map([], |r| {
                    Ok((
                        u(r.get(0)?),
                        Move {
                            id: ItemId(u(r.get(1)?)),
                            old_parent: ItemId(u(r.get(2)?)),
                            ts: r.get(3)?,
                        },
                    ))
                })
                .map_err(sql_err)?;
            for row in rows {
                let (s, m) = row.map_err(sql_err)?;
                st.moves.insert(s, m);
            }
        }
        {
            let mut q = self
                .conn
                .prepare("SELECT id FROM materialized")
                .map_err(sql_err)?;
            let rows = q
                .query_map([], |r| Ok(ItemId(u(r.get(0)?))))
                .map_err(sql_err)?;
            for row in rows {
                st.materialized.insert(row.map_err(sql_err)?);
            }
        }
        {
            let mut q = self
                .conn
                .prepare("SELECT id, data FROM local_meta")
                .map_err(sql_err)?;
            let rows = q
                .query_map([], |r| Ok((ItemId(u(r.get(0)?)), r.get::<_, Vec<u8>>(1)?)))
                .map_err(sql_err)?;
            for row in rows {
                let (id, data) = row.map_err(sql_err)?;
                if let Ok(m) = postcard::from_bytes::<LocalMeta>(&data) {
                    st.local_meta.insert(id, m);
                }
            }
        }
        {
            let mut q = self
                .conn
                .prepare("SELECT id, key, seen FROM delete_seen")
                .map_err(sql_err)?;
            let rows = q
                .query_map([], |r| {
                    Ok((ItemId(u(r.get(0)?)), r.get::<_, Vec<u8>>(1)?, u(r.get(2)?)))
                })
                .map_err(sql_err)?;
            for row in rows {
                let (id, key, seen) = row.map_err(sql_err)?;
                if let Ok(key) = <[u8; 16]>::try_from(key.as_slice()) {
                    st.delete_seen.insert(id, DeleteSeen { key, seen });
                }
            }
        }
        {
            let mut q = self
                .conn
                .prepare("SELECT local_seq, server_seq FROM anchors")
                .map_err(sql_err)?;
            let rows = q
                .query_map([], |r| Ok((u(r.get(0)?), u(r.get(1)?))))
                .map_err(sql_err)?;
            for row in rows {
                let (a, s) = row.map_err(sql_err)?;
                st.anchors.insert(a, s);
            }
        }
        // The journal seq must be ≥ every persisted record (defensive against a torn meta row).
        let max_rec = [
            st.by_changed.keys().next_back(),
            st.tombs.keys().next_back(),
            st.moves.keys().next_back(),
        ]
        .into_iter()
        .flatten()
        .max()
        .copied()
        .unwrap_or(0);
        st.seq = st.seq.max(max_rec);
        Ok(st)
    }

    /// Persist one batch in a single transaction.
    pub fn write(&mut self, w: &WriteSet) -> Result<()> {
        let tx = self.conn.transaction().map_err(sql_err)?;
        if w.wipe {
            tx.execute_batch(
                "DELETE FROM items; DELETE FROM tombstones; DELETE FROM moves; DELETE FROM materialized; \
                 DELETE FROM local_meta; DELETE FROM display_override; DELETE FROM anchors; \
                 DELETE FROM delete_seen;",
            )
            .map_err(sql_err)?;
        }
        {
            let mut del = tx
                .prepare_cached("DELETE FROM items WHERE id = ?1")
                .map_err(sql_err)?;
            for id in &w.deleted_items {
                del.execute([i(id.0)]).map_err(sql_err)?;
            }
            let mut ins = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO items (id, parent, name, kind, size, mtime, mode, vc, vm, target, \
                     lazy, seq, access, changed_seq, complete) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                )
                .map_err(sql_err)?;
            for n in &w.items {
                let e = &n.entry;
                ins.execute(params![
                    i(e.id.0),
                    i(e.parent.0),
                    e.name,
                    kind_to_i(e.kind),
                    i(e.size),
                    e.mtime_ns,
                    e.mode as i64,
                    i(e.version.content),
                    i(e.version.meta),
                    e.symlink_target,
                    e.lazy as i64,
                    i(e.seq),
                    e.access as i64,
                    i(n.changed_seq),
                    n.complete as i64
                ])
                .map_err(sql_err)?;
            }
            let mut unt = tx
                .prepare_cached("DELETE FROM tombstones WHERE id = ?1")
                .map_err(sql_err)?;
            for id in &w.untombed {
                unt.execute([i(id.0)]).map_err(sql_err)?;
            }
            let mut tb = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO tombstones (seq, id, old_parent, server_seq, ts) VALUES (?1,?2,?3,?4,?5)",
                )
                .map_err(sql_err)?;
            for (s, t) in &w.tombs {
                tb.execute(params![
                    i(*s),
                    i(t.id.0),
                    i(t.old_parent.0),
                    i(t.server_seq),
                    t.ts
                ])
                .map_err(sql_err)?;
            }
            let mut mv = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO moves (seq, id, old_parent, ts) VALUES (?1,?2,?3,?4)",
                )
                .map_err(sql_err)?;
            for (s, m) in &w.moves {
                mv.execute(params![i(*s), i(m.id.0), i(m.old_parent.0), m.ts])
                    .map_err(sql_err)?;
            }
            let mut lm_del = tx
                .prepare_cached("DELETE FROM local_meta WHERE id = ?1")
                .map_err(sql_err)?;
            let mut lm_ins = tx
                .prepare_cached("INSERT OR REPLACE INTO local_meta (id, data) VALUES (?1, ?2)")
                .map_err(sql_err)?;
            for (id, m) in &w.local_meta {
                match m {
                    Some(m) => {
                        let data = postcard::to_stdvec(m)
                            .map_err(|e| err(ErrorCode::Io, e.to_string()))?;
                        lm_ins.execute(params![i(id.0), data]).map_err(sql_err)?;
                    }
                    None => {
                        lm_del.execute([i(id.0)]).map_err(sql_err)?;
                    }
                }
            }
            let mut ov_del = tx
                .prepare_cached("DELETE FROM display_override WHERE id = ?1")
                .map_err(sql_err)?;
            let mut ov_ins = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO display_override (id, name) VALUES (?1, ?2)",
                )
                .map_err(sql_err)?;
            for (id, o) in &w.overrides {
                match o {
                    Some(n) => ov_ins.execute(params![i(id.0), n]).map_err(sql_err)?,
                    None => ov_del.execute([i(id.0)]).map_err(sql_err)?,
                };
            }
            let mut ds_del = tx
                .prepare_cached("DELETE FROM delete_seen WHERE id = ?1")
                .map_err(sql_err)?;
            let mut ds_ins = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO delete_seen (id, key, seen) VALUES (?1,?2,?3)",
                )
                .map_err(sql_err)?;
            for (id, e) in &w.delete_seen {
                match e {
                    Some(e) => ds_ins
                        .execute(params![i(id.0), e.key.to_vec(), i(e.seen)])
                        .map_err(sql_err)?,
                    None => ds_del.execute([i(id.0)]).map_err(sql_err)?,
                };
            }
            match &w.materialized {
                Some(MatWrite::Full(ids)) => {
                    tx.execute("DELETE FROM materialized", [])
                        .map_err(sql_err)?;
                    let mut m = tx
                        .prepare_cached("INSERT OR IGNORE INTO materialized (id) VALUES (?1)")
                        .map_err(sql_err)?;
                    for id in ids {
                        m.execute([i(id.0)]).map_err(sql_err)?;
                    }
                }
                Some(MatWrite::Delta { added, removed }) => {
                    let mut rm = tx
                        .prepare_cached("DELETE FROM materialized WHERE id = ?1")
                        .map_err(sql_err)?;
                    for id in removed {
                        rm.execute([i(id.0)]).map_err(sql_err)?;
                    }
                    let mut m = tx
                        .prepare_cached("INSERT OR IGNORE INTO materialized (id) VALUES (?1)")
                        .map_err(sql_err)?;
                    for id in added {
                        m.execute([i(id.0)]).map_err(sql_err)?;
                    }
                }
                None => {}
            }
            if let Some(h) = w.gc {
                tx.execute("DELETE FROM tombstones WHERE seq <= ?1", [i(h)])
                    .map_err(sql_err)?;
                tx.execute("DELETE FROM moves WHERE seq <= ?1", [i(h)])
                    .map_err(sql_err)?;
                tx.execute(
                    "DELETE FROM anchors WHERE local_seq < (SELECT COALESCE(MAX(local_seq), 0) FROM anchors WHERE local_seq <= ?1)",
                    [i(h)],
                )
                .map_err(sql_err)?;
            }
            if let Some((a, s)) = w.anchor {
                tx.execute(
                    "INSERT OR REPLACE INTO anchors (local_seq, server_seq) VALUES (?1, ?2)",
                    params![i(a), i(s)],
                )
                .map_err(sql_err)?;
            }
            if let Some(m) = &w.meta {
                let mut put = tx
                    .prepare_cached("INSERT OR REPLACE INTO meta (k, v) VALUES (?1, ?2)")
                    .map_err(sql_err)?;
                put.execute(params![
                    "replica_uuid",
                    m.replica_uuid.to_le_bytes().to_vec()
                ])
                .map_err(sql_err)?;
                match m.index {
                    Some(ix) => put
                        .execute(params!["index_id", ix.0.to_le_bytes().to_vec()])
                        .map_err(sql_err)?,
                    None => tx
                        .execute("DELETE FROM meta WHERE k = 'index_id'", [])
                        .map_err(sql_err)?,
                };
                put.execute(params!["server_seq", m.server_seq.to_le_bytes().to_vec()])
                    .map_err(sql_err)?;
                put.execute(params!["seq", m.seq.to_le_bytes().to_vec()])
                    .map_err(sql_err)?;
                put.execute(params![
                    "snapshot_complete",
                    (m.snapshot_complete as u64).to_le_bytes().to_vec()
                ])
                .map_err(sql_err)?;
                put.execute(params!["gc_horizon", m.gc_horizon.to_le_bytes().to_vec()])
                    .map_err(sql_err)?;
                put.execute(params!["consumed", m.consumed.to_le_bytes().to_vec()])
                    .map_err(sql_err)?;
                if let Some(info) = &m.info {
                    let v =
                        postcard::to_stdvec(info).map_err(|e| err(ErrorCode::Io, e.to_string()))?;
                    put.execute(params!["info", v]).map_err(sql_err)?;
                }
            }
        }
        tx.commit().map_err(sql_err)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use unlatch_proto::{ACCESS_R, ACCESS_W, ACCESS_X};

    pub fn ent(id: u64, parent: u64, name: &str, kind: Kind, seq: u64) -> Entry {
        Entry {
            id: ItemId(id),
            parent: ItemId(parent),
            name: name.into(),
            kind,
            size: 0,
            mtime_ns: 0,
            mode: 0o644,
            version: Version {
                content: seq,
                meta: seq,
            },
            symlink_target: None,
            lazy: false,
            seq,
            access: ACCESS_R | ACCESS_W | ACCESS_X,
        }
    }

    fn base() -> State {
        let mut st = State::new();
        let mut d = Delta::default();
        st.upsert(ent(1, 1, "root", Kind::Dir, 1), Source::Snapshot, &mut d);
        st
    }

    fn up(st: &mut State, e: Entry) -> Delta {
        let mut d = Delta::default();
        st.upsert(e, Source::Event, &mut d);
        d
    }

    fn rm(st: &mut State, id: u64, seq: u64) -> Delta {
        let mut d = Delta::default();
        st.remove(ItemId(id), seq, &HashSet::new(), &mut d);
        d
    }

    fn names(st: &State, dir: u64) -> Vec<String> {
        let (ids, _) = st.list_ids(ItemId(dir), None, 1000);
        ids.iter()
            .map(|i| st.display_name(*i).unwrap_or("?").to_string())
            .collect()
    }

    #[test]
    fn lww_by_seq() {
        let mut st = base();
        up(&mut st, ent(2, 1, "a", Kind::File, 10));
        // Older listing data overtaking events must not win.
        let d = up(&mut st, ent(2, 1, "old-name", Kind::File, 5));
        assert!(d.upserted.is_empty());
        assert_eq!(st.nodes[&ItemId(2)].entry.name, "a");
        // Remove at seq 8 < entry seq 10: ignored.
        assert!(!st.remove(ItemId(2), 8, &HashSet::new(), &mut Delta::default()));
        rm(&mut st, 2, 12);
        assert!(!st.nodes.contains_key(&ItemId(2)));
        // A stale upsert (seq ≤ tombstone) after removal is dropped; a newer one restores.
        up(&mut st, ent(2, 1, "a", Kind::File, 11));
        assert!(!st.nodes.contains_key(&ItemId(2)));
        let d = up(&mut st, ent(2, 1, "a", Kind::File, 13));
        assert_eq!(d.untombed, vec![ItemId(2)]);
        assert!(st.nodes.contains_key(&ItemId(2)));
    }

    #[test]
    fn collisions_mapped_and_restored() {
        let mut st = base();
        up(&mut st, ent(2, 1, "foo.txt", Kind::File, 2));
        up(&mut st, ent(3, 1, "Foo.txt", Kind::File, 3));
        up(&mut st, ent(4, 1, "bar", Kind::File, 4));
        assert_eq!(names(&st, 1), vec!["Foo.txt", "bar", "foo (Unlatch 2).txt"]);
        assert_eq!(
            st.lookup_id(ItemId(1), "foo (Unlatch 2).txt"),
            Some(ItemId(2))
        );
        assert_eq!(
            st.lookup_id(ItemId(1), "foo.txt"),
            Some(ItemId(2)),
            "real name lookup works too"
        );
        rm(&mut st, 3, 5);
        assert_eq!(names(&st, 1), vec!["bar", "foo.txt"]);
        // Rename into a collision.
        up(&mut st, ent(4, 1, "FOO.TXT", Kind::File, 6));
        assert_eq!(names(&st, 1), vec!["FOO.TXT", "foo (Unlatch 2).txt"]);
        let it = st.ipc_item(ItemId(2), false).expect("item");
        assert_eq!(it.entry.name, "foo.txt");
        assert_eq!(it.display_name, "foo (Unlatch 2).txt");
    }

    #[test]
    fn generated_name_clash_with_later_real_name() {
        let mut st = base();
        up(&mut st, ent(2, 1, "a.txt", Kind::File, 2));
        up(&mut st, ent(3, 1, "A.txt", Kind::File, 3));
        assert_eq!(names(&st, 1), vec!["A.txt", "a (Unlatch 2).txt"]);
        up(&mut st, ent(4, 1, "a (unlatch 2).txt", Kind::File, 4));
        let n = names(&st, 1);
        assert_eq!(n.len(), 3);
        let keys: HashSet<String> = n.iter().map(|s| fold_key(s)).collect();
        assert_eq!(keys.len(), 3, "no two displays collide: {n:?}");
        assert!(n.contains(&"a (unlatch 2).txt".to_string()));
    }

    #[test]
    fn override_display() {
        let mut st = base();
        up(&mut st, ent(2, 1, "x", Kind::File, 2));
        let mut d = Delta::default();
        st.set_override(ItemId(2), Some("x 2".into()), &mut d);
        assert_eq!(names(&st, 1), vec!["x 2"]);
        assert_eq!(
            st.ipc_item(ItemId(2), false).map(|i| i.entry.name),
            Some("x".into())
        );
        // A server rename clears the override.
        up(&mut st, ent(2, 1, "y", Kind::File, 3));
        assert_eq!(names(&st, 1), vec!["y"]);
    }

    fn anchor_changes(st: &State, from: u64) -> (Vec<u64>, Vec<u64>, u64, bool) {
        let (u, r, a, more) = st.changes(from, st.seq, 1000, false);
        (
            u.iter().map(|i| i.entry.id.0).collect(),
            r.iter().map(|i| i.0).collect(),
            a,
            more,
        )
    }

    #[test]
    fn working_set_filter_and_moves() {
        let mut st = base();
        up(&mut st, ent(2, 1, "m", Kind::Dir, 2)); // materialized dir
        up(&mut st, ent(3, 1, "n", Kind::Dir, 3)); // not materialized (but root child)
        up(&mut st, ent(4, 3, "deep", Kind::Dir, 4));
        up(&mut st, ent(5, 2, "f", Kind::File, 5));
        st.materialized.insert(ItemId(2));
        let a0 = st.seq;
        // Change inside a non-materialized dir: not reported.
        up(&mut st, ent(6, 4, "g", Kind::File, 6));
        let (u, r, a, _) = anchor_changes(&st, a0);
        assert!(u.is_empty() && r.is_empty());
        assert_eq!(a, st.seq, "anchor advances past filtered records");
        // Reparent out of M: reported via the move record (old_parent ∈ M).
        let a1 = st.seq;
        up(&mut st, ent(5, 4, "f", Kind::File, 7));
        let (u, _, _, _) = anchor_changes(&st, a1);
        assert_eq!(u, vec![5]);
        // A second move (between two non-M dirs) keeps the first move visible from an older anchor.
        up(&mut st, ent(5, 3, "f", Kind::File, 8));
        let (u, _, _, _) = anchor_changes(&st, a1);
        assert_eq!(u, vec![5]);
        // From the newest anchor the last move is not reported (3 and 4 ∉ M... but 3 is a root
        // child only; its parent is root ∈ M, the item's parent 3 is not).
        let a2 = st.seq;
        up(&mut st, ent(5, 4, "f", Kind::File, 9));
        let (u, _, _, _) = anchor_changes(&st, a2);
        assert!(u.is_empty());
        // Deleted after moving out of M: tombstone still reported from a1.
        rm(&mut st, 5, 10);
        let (_, r, _, _) = anchor_changes(&st, a1);
        assert_eq!(r, vec![5]);
    }

    #[test]
    fn dir_removal_tombstones_children_first() {
        let mut st = base();
        up(&mut st, ent(2, 1, "d", Kind::Dir, 2));
        up(&mut st, ent(3, 2, "e", Kind::Dir, 3));
        up(&mut st, ent(4, 3, "f", Kind::File, 4));
        up(&mut st, ent(5, 2, "g", Kind::File, 5));
        st.materialized.extend([ItemId(2), ItemId(3)]);
        let a = st.seq;
        rm(&mut st, 2, 6);
        let (_, r, _, _) = anchor_changes(&st, a);
        assert_eq!(r.len(), 4);
        let pos = |x: u64| r.iter().position(|y| *y == x).unwrap_or(usize::MAX);
        assert!(
            pos(4) < pos(3) && pos(3) < pos(2) && pos(5) < pos(2),
            "{r:?}"
        );
        assert!(st.nodes.len() == 1);
    }

    #[test]
    fn displaced_twin_is_reported_before_the_newcomer() {
        // MQ-016: `README.md` sorts first and takes the plain name from `readme.md`; the old
        // twin's display changes, so it must be journalled too, and before the newcomer.
        let mut st = base();
        up(&mut st, ent(2, 1, "readme.md", Kind::File, 2));
        let a = st.seq;
        up(&mut st, ent(3, 1, "README.md", Kind::File, 3));
        let (u, _, _, _) = anchor_changes(&st, a);
        assert_eq!(u, vec![2, 3]);
        assert_eq!(st.display_name(ItemId(2)), Some("readme (Unlatch 2).md"));
        // The newcomer leaves: the twin regains its name, journalled after the tombstone.
        let a = st.seq;
        rm(&mut st, 3, 4);
        let (u, r, _, _) = anchor_changes(&st, a);
        assert_eq!((u, r), (vec![2], vec![3]));
        assert_eq!(st.display_name(ItemId(2)), Some("readme.md"));
        assert!(st.nodes[&ItemId(2)].changed_seq > st.tomb_of[&ItemId(3)]);
    }

    #[test]
    fn descendant_tombstones_follow_a_reported_dir() {
        // (a)5: `f` moved from the materialized root into never-enumerated `d`, so the Mac
        // holds it under `d`; `rm -rf d` must report `f`'s tombstone with `d`'s.
        let mut st = base();
        up(&mut st, ent(2, 1, "d", Kind::Dir, 2));
        up(&mut st, ent(3, 2, "inner", Kind::File, 3));
        up(&mut st, ent(4, 1, "f", Kind::File, 4));
        up(&mut st, ent(4, 2, "f", Kind::File, 5));
        let a = st.seq;
        rm(&mut st, 2, 6);
        let (_, r, _, _) = anchor_changes(&st, a);
        let mut r2 = r.clone();
        r2.sort_unstable();
        assert_eq!(r2, vec![2, 3, 4], "{r:?}");
        // A tombstone inside a dir that itself survives stays filtered.
        up(&mut st, ent(5, 1, "e", Kind::Dir, 7));
        up(&mut st, ent(6, 5, "x", Kind::File, 8));
        let a = st.seq;
        rm(&mut st, 6, 9);
        assert_eq!(anchor_changes(&st, a).1, Vec::<u64>::new());
    }

    #[test]
    fn symlinks_below_a_moved_dir_are_reevaluated() {
        // D12: `a/b/link -> ../../x` is in the root at depth 2 and escapes at depth 1.
        let mut st = base();
        up(&mut st, ent(2, 1, "a", Kind::Dir, 2));
        up(&mut st, ent(3, 2, "b", Kind::Dir, 3));
        let mut l = ent(4, 3, "link", Kind::Symlink, 4);
        l.symlink_target = Some("../../x".into());
        up(&mut st, l);
        st.materialized.extend([ItemId(2), ItemId(3)]);
        assert!(!st.ipc_item(ItemId(4), false).expect("item").symlink_blocked);
        let a = st.seq;
        up(&mut st, ent(3, 1, "b", Kind::Dir, 5));
        let (u, _, _, _) = anchor_changes(&st, a);
        assert_eq!(
            u,
            vec![4, 3],
            "the flipped link is journalled before the move"
        );
        let it = st.ipc_item(ItemId(4), false).expect("item");
        assert!(it.symlink_blocked);
        assert_eq!(it.entry.kind, Kind::Symlink);
        assert_eq!(it.entry.symlink_target.as_deref(), Some("../../x"));
        // Moving it back deeper un-blocks it, and is reported again.
        let a = st.seq;
        up(&mut st, ent(3, 2, "b", Kind::Dir, 6));
        assert_eq!(anchor_changes(&st, a).0, vec![4, 3]);
        assert!(!st.ipc_item(ItemId(4), false).expect("item").symlink_blocked);
    }

    /// Run `f` on a thread; fail (instead of hanging the suite) if it does not finish in 5 s.
    fn within_5s<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("replica traversal did not terminate (cycle)")
    }

    #[test]
    fn move_into_own_descendant_is_rejected() {
        // A hostile/corrupt VM moves a dir under its own descendant: root/a(2)/b(3), then
        // Upsert{2, parent 3}. Must be rejected (no cycle, no hang), for every source.
        for source in [Source::Event, Source::Listing, Source::Snapshot] {
            let st = within_5s(move || {
                let mut st = base();
                up(&mut st, ent(2, 1, "a", Kind::Dir, 2));
                up(&mut st, ent(3, 2, "b", Kind::Dir, 3));
                st.materialized.extend([ItemId(2), ItemId(3)]);
                let mut d = Delta::default();
                assert!(!st.upsert(ent(2, 3, "a", Kind::Dir, 5), source, &mut d));
                assert!(d.upserted.is_empty());
                // Self-parenting is a cycle too.
                assert!(!st.upsert(ent(3, 3, "b", Kind::Dir, 6), source, &mut d));
                // A later delete of the subtree still terminates and removes both.
                let d = rm(&mut st, 2, 7);
                assert_eq!(d.removed.len(), 2);
                st
            });
            assert!(!st.nodes.contains_key(&ItemId(2)));
            assert!(!st.nodes.contains_key(&ItemId(3)));
        }
        // A new (unknown) dir whose would-be parent already hangs below it is a cycle too.
        let st = within_5s(|| {
            let mut st = base();
            up(&mut st, ent(3, 2, "orphan", Kind::Dir, 3));
            let mut d = Delta::default();
            assert!(!st.upsert(ent(2, 3, "a", Kind::Dir, 4), Source::Listing, &mut d));
            st
        });
        assert!(!st.nodes.contains_key(&ItemId(2)));
    }

    #[test]
    fn traversals_terminate_on_a_corrupt_cycle() {
        // Defence in depth: even if a cycle got into the replica, subtree walks terminate.
        let n = within_5s(|| {
            let mut st = base();
            up(&mut st, ent(2, 1, "a", Kind::Dir, 2));
            up(&mut st, ent(3, 2, "b", Kind::Dir, 3));
            // Corrupt: 2 is also listed as a child of 3.
            st.child_add(ItemId(3), ItemId(2), "a");
            let order = st.subtree_post_order(ItemId(2));
            st.materialized.extend([ItemId(2), ItemId(3)]);
            (order, st.materialized_in_subtrees(&[ItemId(2)]))
        });
        assert_eq!(n.0, vec![ItemId(3), ItemId(2)]);
        assert_eq!(n.1, 2);
    }

    #[test]
    fn exec_verdict_flips_below_a_renamed_or_moved_dir_are_rereported() {
        // Rule 10: renaming x -> x.app puts x/Contents/MacOS/x inside a bundle; the binary's
        // user_exec flips to false and must be re-journalled (before the dir).
        let mut st = base();
        up(&mut st, ent(2, 1, "x", Kind::Dir, 2));
        up(&mut st, ent(3, 2, "Contents", Kind::Dir, 3));
        up(&mut st, ent(4, 3, "MacOS", Kind::Dir, 4));
        let mut b = ent(5, 4, "x", Kind::File, 5);
        b.mode = 0o755;
        up(&mut st, b);
        let mut plain = ent(6, 4, "data", Kind::File, 6);
        plain.mode = 0o644;
        up(&mut st, plain);
        st.materialized
            .extend([ItemId(2), ItemId(3), ItemId(4), ItemId(5)]);
        assert!(st.ipc_item(ItemId(5), true).expect("item").user_exec);
        let a = st.seq;
        up(&mut st, ent(2, 1, "x.app", Kind::Dir, 7));
        assert!(!st.ipc_item(ItemId(5), true).expect("item").user_exec);
        assert_eq!(anchor_changes(&st, a).0, vec![5, 2]);
        // And back: the bit is allowed again and re-reported.
        let a = st.seq;
        up(&mut st, ent(2, 1, "x", Kind::Dir, 8));
        assert!(st.ipc_item(ItemId(5), true).expect("item").user_exec);
        assert_eq!(anchor_changes(&st, a).0, vec![5, 2]);
        // An unrelated rename re-reports nothing below.
        let a = st.seq;
        up(&mut st, ent(2, 1, "y", Kind::Dir, 9));
        assert_eq!(anchor_changes(&st, a).0, vec![2]);

        // Move case: MacOS/ (holding a 755 file) moved into Y.app/Contents.
        let mut st = base();
        up(&mut st, ent(10, 1, "Y.app", Kind::Dir, 2));
        up(&mut st, ent(11, 10, "Contents", Kind::Dir, 3));
        up(&mut st, ent(12, 1, "MacOS", Kind::Dir, 4));
        let mut b = ent(13, 12, "y", Kind::File, 5);
        b.mode = 0o755;
        up(&mut st, b);
        st.materialized
            .extend([ItemId(10), ItemId(11), ItemId(12), ItemId(13)]);
        assert!(st.ipc_item(ItemId(13), true).expect("item").user_exec);
        let a = st.seq;
        up(&mut st, ent(12, 11, "MacOS", Kind::Dir, 6));
        assert!(!st.ipc_item(ItemId(13), true).expect("item").user_exec);
        assert_eq!(anchor_changes(&st, a).0, vec![13, 12]);
    }

    #[test]
    fn paging_never_empty_while_behind() {
        let mut st = base();
        for k in 0..10u64 {
            up(
                &mut st,
                ent(10 + k, 1, &format!("f{k}"), Kind::File, 10 + k),
            );
        }
        let mut a = 0;
        let mut all = Vec::new();
        loop {
            let (u, r, na, more) = st.changes(a, st.seq, 3, false);
            assert!(
                !(u.is_empty() && r.is_empty() && na <= a && a < st.seq),
                "MQ-004: empty page while behind"
            );
            all.extend(u.iter().map(|i| i.entry.id.0));
            assert!(na > a || !more);
            a = na;
            if !more {
                break;
            }
        }
        assert_eq!(all.len(), 11); // root + 10
        assert_eq!(a, st.seq);
    }

    #[test]
    fn readd_after_remove_reports_update_not_removal() {
        let mut st = base();
        up(&mut st, ent(2, 1, "a", Kind::File, 2));
        let a = st.seq;
        rm(&mut st, 2, 3);
        up(&mut st, ent(2, 1, "a", Kind::File, 4));
        let (u, r, _, _) = anchor_changes(&st, a);
        assert_eq!(u, vec![2]);
        assert!(r.is_empty());
    }

    #[test]
    fn keep_protected_and_ancestors() {
        let mut st = base();
        up(&mut st, ent(2, 1, "d", Kind::Dir, 2));
        up(&mut st, ent(3, 2, "e", Kind::Dir, 3));
        up(&mut st, ent(4, 3, "edited", Kind::File, 4));
        up(&mut st, ent(5, 3, "other", Kind::File, 5));
        let keep = st.with_ancestors([ItemId(4)]);
        let mut d = Delta::default();
        st.remove(ItemId(2), 9, &keep, &mut d);
        assert!(st.nodes.contains_key(&ItemId(4)));
        assert!(st.nodes.contains_key(&ItemId(3)));
        assert!(st.nodes.contains_key(&ItemId(2)));
        assert!(!st.nodes.contains_key(&ItemId(5)));
    }

    #[test]
    fn gc_horizon() {
        let mut st = base();
        up(&mut st, ent(2, 1, "a", Kind::File, 2));
        rm(&mut st, 2, 3);
        let s = st.seq;
        let mut d = Delta::default();
        st.gc(now_secs() + GC_AGE_SECS + 10, &mut d);
        assert_eq!(st.gc_horizon, s);
        assert!(st.tombs.is_empty());
    }

    #[test]
    fn two_materialized_changes_in_one_batch_both_persist() {
        // One batch (several queued messages, or a batch kept after a failed commit) can carry
        // more than one M change; the first must not be overwritten by the second.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("r.db");
        let mut db = Db::open(&path).expect("open");
        let mut st = db.load().expect("load");
        let mut d = Delta::default();
        st.upsert(ent(1, 1, "root", Kind::Dir, 1), Source::Snapshot, &mut d);
        for i in 2..6 {
            st.upsert(
                ent(i, 1, &format!("f{i}"), Kind::File, i),
                Source::Snapshot,
                &mut d,
            );
        }
        st.set_materialized(&[ItemId(2), ItemId(3)], &[], false, &mut d);
        st.set_materialized(&[ItemId(4)], &[ItemId(3)], false, &mut d);
        st.set_materialized(&[ItemId(3)], &[ItemId(4)], false, &mut d);
        db.write(&d.write_set(&st)).expect("write");
        drop(db);
        let st2 = Db::open(&path).expect("reopen").load().expect("load2");
        assert_eq!(st2.materialized, st.materialized);
        assert_eq!(st2.materialized, HashSet::from([ItemId(2), ItemId(3)]));
    }

    #[test]
    fn db_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("r.db");
        let mut db = Db::open(&path).expect("open");
        let mut st = db.load().expect("load");
        let uuid = st.replica_uuid;
        let mut d = Delta::default();
        st.upsert(ent(1, 1, "root", Kind::Dir, 1), Source::Snapshot, &mut d);
        st.upsert(ent(2, 1, "a", Kind::File, 2), Source::Snapshot, &mut d);
        st.upsert(ent(3, 1, "A", Kind::File, 3), Source::Snapshot, &mut d);
        st.upsert(ent(4, 1, "gone", Kind::File, 4), Source::Snapshot, &mut d);
        st.remove(ItemId(4), 5, &HashSet::new(), &mut d);
        st.set_materialized(&[ItemId(2)], &[], false, &mut d);
        st.local_meta.insert(
            ItemId(2),
            LocalMeta {
                tag_data: Some(vec![1, 2]),
                ..Default::default()
            },
        );
        d.local_meta.push(ItemId(2));
        st.index = Some(IndexId(77));
        st.server_seq = 5;
        let mut w = d.write_set(&st);
        w.anchor = Some((st.seq, 5));
        db.write(&w).expect("write");
        drop(db);
        let db = Db::open(&path).expect("reopen");
        let st2 = db.load().expect("load2");
        assert_eq!(st2.replica_uuid, uuid);
        assert_eq!(st2.nodes.len(), 3);
        assert_eq!(st2.index, Some(IndexId(77)));
        assert_eq!(st2.seq, st.seq);
        assert_eq!(st2.tombs.len(), 1);
        assert!(st2.materialized.contains(&ItemId(2)));
        assert_eq!(st2.local_meta[&ItemId(2)].tag_data, Some(vec![1, 2]));
        assert_eq!(st2.display_name(ItemId(2)), Some("a (Unlatch 2)"));
        assert_eq!(st2.server_seq_at(st.seq), 5);
        let (u, r, _, _) = st2.changes(0, st2.seq, 100, false);
        assert_eq!(u.len(), 3);
        assert_eq!(r, vec![ItemId(4)]);
    }
}
