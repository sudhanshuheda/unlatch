//! A scripted stand-in for the engine + VM, used to test fpsim itself (and the fuzzer's
//! machinery) without `unlatchd`.
//!
//! It implements the IPC semantics the design requires (engine rules 1–11 of review (c), the
//! working-set rule of (a)5, the ops table of (d)1) on an in-memory VM tree, and every rule can be
//! *broken* on purpose through [`Flaws`]. A scenario that passes against the correct fake and
//! fails against the flawed one is the "failing-first" evidence that fpsim models the measured
//! fileproviderd behaviour the rule exists for.
//!
//! Model: `vm` is the truth on the VM; `replica` is what the engine has committed. VM-side
//! changes reach the replica on [`ScriptedEngine::settle`] (≈ daemon event + engine commit +
//! `server_barrier`), which assigns seq versions, writes the journal and signals.

use super::backend::Backend;
use super::check::{symlink_view, SymlinkView};
use super::names::{display_names, split_ext, strip_unlatch_marker};
use super::vmfs::{VmFs, VmKind, VmNode, VmTree};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use unlatch_core::EngineEvent;
use unlatch_proto::ipc::{
    caps, fields, ConnState, CreateKind, EngineStatus, IpcItem, IpcRequest, IpcResponse, LocalMeta,
};
use unlatch_proto::{
    is_mac_local_name, BaseVersion, Entry, ErrorCode, ItemId, Kind, ProtoError, Version,
    PROTO_VERSION,
};

/// Deliberate engine bugs, one per rule fpsim defends. All `false` = a correct engine.
#[derive(Clone, Debug, Default)]
pub struct Flaws {
    /// Signal the working set before the change is committed and not again after (MQ-004).
    pub signal_before_commit: bool,
    /// Report only items that are themselves in M, not children of materialized dirs (MQ-001).
    pub ws_only_materialized_ids: bool,
    /// `ChangesSince` fails while offline instead of answering from the replica (MQ-005 setup).
    pub changes_fail_offline: bool,
    /// Never emit `ErrorResolved` on reconnect (MQ-005, MQ-037).
    pub no_error_resolved: bool,
    /// In-memory journal: every anchor expires across an engine restart (MQ-006).
    pub expire_on_restart: bool,
    /// `item(for:)` answers `NotFound` while offline (MQ-011).
    pub item_not_found_offline: bool,
    /// A content conflict returns the server version without `should_fetch_content` (MQ-013).
    pub conflict_without_fetch: bool,
    /// A create whose name is taken answers `Exists` (MQ-014).
    pub create_returns_exists: bool,
    /// No display-name mapping for case collisions (MQ-016).
    pub no_display_mapping: bool,
    /// No ops table and no equal-hash shortcut: replays execute twice (D1, MQ-035).
    pub non_idempotent: bool,
    /// An mtime-only modify of a directory fails `Unsupported` (MQ-049).
    pub dir_modify_unsupported: bool,
    /// A content modify of an item deleted on the VM answers success and drops the bytes (MQ-080).
    pub modify_missing_ok: bool,
    /// A directory removal emits one tombstone, not one per descendant.
    pub dir_tombstone_only: bool,
    /// A same-size rewrite keeps the content version (stat-tuple versions; evict-on-update).
    pub stale_content_version: bool,
    /// When a new name displaces an existing sibling's display name (rule 11), do not report
    /// the sibling (MQ-016: the system then renames it on the Mac only).
    pub no_display_rename_report: bool,
    /// A metadata-only modify (rename) replies with a newer content version but without
    /// `should_fetch_content` (MQ-013: the Mac keeps stale bytes for ever).
    pub metadata_reply_hides_content_change: bool,
    /// Filter tombstones strictly by `id ∈ M || old_parent ∈ M`, dropping descendants of a
    /// reported directory tombstone that the Mac may still hold.
    pub tombstone_filter_strict: bool,
    /// Do not re-evaluate/report symlinks whose rule-9 verdict changed because an ancestor
    /// moved (the link itself is untouched but its depth changed).
    pub symlink_depth_not_reevaluated: bool,
    /// Answer a replayed op with the stored reply verbatim, even when the item changed or was
    /// deleted since.
    pub stale_replay_reply: bool,
    /// A rename whose base no longer matches (the VM renamed/moved the item too) answers an
    /// error instead of the server's state (rule 4: metadata never errors).
    pub rename_conflict_errors: bool,
    /// A recursive delete also removes descendants the system has not seen yet (rule 6:
    /// `seen_seq`; the agent's new file is lost).
    pub delete_ignores_seen_seq: bool,
    /// A delete retried by the system (reply lost) recomputes `seen_seq` from the anchors it
    /// consumed since — while the folder was already gone on the Mac — and deletes what the
    /// first attempt kept (rule 6; fuzz seed 186).
    pub delete_retry_widens_seen: bool,
    /// Outgoing ops carry the generated display name of a mapped twin instead of its real name
    /// (rule 11: `readme (Unlatch 2).md` appears on the VM).
    pub display_name_leaks_to_vm: bool,
    /// Mac-only fields (tags, …) are stored but not merged into the items the engine reports
    /// (rule 5; MQ-043: the tag vanishes at the next update).
    pub local_meta_not_merged: bool,
    /// No mass-deletion guard: an agent `rm -rf` of most of what the Mac downloaded is applied
    /// (and reported) at once, with no pause for the user (rule 8).
    pub no_mass_delete_guard: bool,
    /// The VM's owner-exec bit is shown to the Mac (rule 10: hidden by default; an agent can
    /// otherwise drop a double-clickable `.command`).
    pub exec_bit_exposed: bool,
    /// Treat a base mismatch caused by this client's own unacknowledged write as a conflict.
    pub no_self_fastforward: bool,
    /// A replayed create (same template id) whose bytes changed since the first attempt is
    /// answered from the ops table and the new bytes are dropped (review (c)1 taken literally).
    pub create_replay_ignores_new_content: bool,
}

impl Flaws {
    /// Every flaw by name (for mutation testing of fpsim and the fuzzer).
    pub const NAMES: &'static [&'static str] = &[
        "signal_before_commit",
        "ws_only_materialized_ids",
        "changes_fail_offline",
        "no_error_resolved",
        "expire_on_restart",
        "item_not_found_offline",
        "conflict_without_fetch",
        "create_returns_exists",
        "no_display_mapping",
        "non_idempotent",
        "dir_modify_unsupported",
        "modify_missing_ok",
        "dir_tombstone_only",
        "stale_content_version",
        "no_display_rename_report",
        "metadata_reply_hides_content_change",
        "tombstone_filter_strict",
        "symlink_depth_not_reevaluated",
        "stale_replay_reply",
        "no_self_fastforward",
        "create_replay_ignores_new_content",
        "rename_conflict_errors",
        "delete_ignores_seen_seq",
        "delete_retry_widens_seen",
        "display_name_leaks_to_vm",
        "local_meta_not_merged",
        "no_mass_delete_guard",
        "exec_bit_exposed",
    ];

    /// A correct engine with exactly one flaw, by name.
    pub fn only(name: &str) -> Option<Flaws> {
        let mut f = Flaws::default();
        let slot = match name {
            "signal_before_commit" => &mut f.signal_before_commit,
            "ws_only_materialized_ids" => &mut f.ws_only_materialized_ids,
            "changes_fail_offline" => &mut f.changes_fail_offline,
            "no_error_resolved" => &mut f.no_error_resolved,
            "expire_on_restart" => &mut f.expire_on_restart,
            "item_not_found_offline" => &mut f.item_not_found_offline,
            "conflict_without_fetch" => &mut f.conflict_without_fetch,
            "create_returns_exists" => &mut f.create_returns_exists,
            "no_display_mapping" => &mut f.no_display_mapping,
            "non_idempotent" => &mut f.non_idempotent,
            "dir_modify_unsupported" => &mut f.dir_modify_unsupported,
            "modify_missing_ok" => &mut f.modify_missing_ok,
            "dir_tombstone_only" => &mut f.dir_tombstone_only,
            "stale_content_version" => &mut f.stale_content_version,
            "no_display_rename_report" => &mut f.no_display_rename_report,
            "metadata_reply_hides_content_change" => &mut f.metadata_reply_hides_content_change,
            "tombstone_filter_strict" => &mut f.tombstone_filter_strict,
            "symlink_depth_not_reevaluated" => &mut f.symlink_depth_not_reevaluated,
            "stale_replay_reply" => &mut f.stale_replay_reply,
            "no_self_fastforward" => &mut f.no_self_fastforward,
            "create_replay_ignores_new_content" => &mut f.create_replay_ignores_new_content,
            "rename_conflict_errors" => &mut f.rename_conflict_errors,
            "delete_ignores_seen_seq" => &mut f.delete_ignores_seen_seq,
            "delete_retry_widens_seen" => &mut f.delete_retry_widens_seen,
            "display_name_leaks_to_vm" => &mut f.display_name_leaks_to_vm,
            "local_meta_not_merged" => &mut f.local_meta_not_merged,
            "no_mass_delete_guard" => &mut f.no_mass_delete_guard,
            "exec_bit_exposed" => &mut f.exec_bit_exposed,
            _ => return None,
        };
        *slot = true;
        Some(f)
    }
}

/// One-shot IPC-hop faults: perform the op, then lose the reply (`die_before_ipc_reply:<kind>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReplyFault {
    Create,
    Modify,
    Delete,
    Fetch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VItem {
    parent: ItemId,
    name: String,
    kind: Kind,
    content: Vec<u8>,
    target: Option<String>,
    mode: u32,
    mtime_ns: i64,
}

#[derive(Clone, Debug)]
struct RItem {
    v: VItem,
    version: Version,
    changed_seq: u64,
    prev_parent: Option<ItemId>,
}

struct State {
    flaws: Flaws,
    vm: BTreeMap<ItemId, VItem>,
    replica: BTreeMap<ItemId, RItem>,
    tombstones: Vec<(ItemId, ItemId, u64)>,
    next_id: u64,
    seq: u64,
    committed: u64,
    epoch: u64,
    online: bool,
    materialized: HashSet<ItemId>,
    ops: HashMap<String, IpcResponse>,
    /// Content hash each create op was first executed with.
    create_hashes: HashMap<String, u64>,
    /// Per item: (base content version, resulting content version) of the last content modify
    /// this client applied.
    last_mod: HashMap<ItemId, (u64, u64)>,
    local_meta: HashMap<ItemId, LocalMeta>,
    seen_seq: u64,
    /// Per item: (base + recursive of a delete call, the `seen_seq` of its first attempt);
    /// released when the system enumerates the item again (rule 6).
    delete_seen: HashMap<ItemId, (String, u64)>,
    faults: HashSet<ReplyFault>,
    events: Sender<EngineEvent>,
    fetch_seq: u64,
    conflict_seq: u64,
    pub_log: Vec<String>,
    /// Rule 8: VM removals held back until the user confirms (`ConfirmPaused`).
    paused: bool,
    /// One-shot: the user confirmed; the next settle applies the held removals.
    guard_confirmed: bool,
    /// Removals the user declined: kept in the replica (and on the Mac) for good.
    declined: HashSet<ItemId>,
    /// Items this client deleted itself (own mutation results are never held).
    own_removed: HashSet<ItemId>,
    /// `ScriptedEngine::settle` confirms a pause by itself (the fuzzer's agent really did
    /// delete that much); scenarios about the guard turn it off.
    auto_confirm: bool,
}

/// Shared handle to the scripted engine + VM.
#[derive(Clone)]
pub struct ScriptedEngine {
    st: Arc<Mutex<State>>,
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

fn perr(code: ErrorCode, msg: impl Into<String>) -> IpcResponse {
    IpcResponse::Error {
        code,
        msg: msg.into(),
        current: None,
    }
}

impl ScriptedEngine {
    /// A fresh engine whose VM holds only the root. Events go to the returned receiver (give it
    /// to [`super::FpSim::new`]).
    pub fn new(flaws: Flaws) -> (ScriptedEngine, Receiver<EngineEvent>) {
        let (tx, rx) = channel();
        let mut vm = BTreeMap::new();
        let root = VItem {
            parent: ItemId::ROOT,
            name: String::new(),
            kind: Kind::Dir,
            content: Vec::new(),
            target: None,
            mode: 0o755,
            mtime_ns: 0,
        };
        vm.insert(ItemId::ROOT, root.clone());
        let mut replica = BTreeMap::new();
        replica.insert(
            ItemId::ROOT,
            RItem {
                v: root,
                version: Version {
                    content: 1,
                    meta: 1,
                },
                changed_seq: 1,
                prev_parent: None,
            },
        );
        let st = State {
            flaws,
            vm,
            replica,
            tombstones: Vec::new(),
            next_id: 2,
            seq: 1,
            committed: 1,
            epoch: 0,
            online: true,
            materialized: HashSet::new(),
            ops: HashMap::new(),
            create_hashes: HashMap::new(),
            last_mod: HashMap::new(),
            local_meta: HashMap::new(),
            seen_seq: 0,
            delete_seen: HashMap::new(),
            faults: HashSet::new(),
            events: tx,
            fetch_seq: 0,
            conflict_seq: 0,
            pub_log: Vec::new(),
            paused: false,
            guard_confirmed: false,
            declined: HashSet::new(),
            own_removed: HashSet::new(),
            auto_confirm: true,
        };
        (
            ScriptedEngine {
                st: Arc::new(Mutex::new(st)),
            },
            rx,
        )
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.st.lock() {
            Ok(g) => g,
            // A panicking test thread must not wedge every later assertion.
            Err(p) => p.into_inner(),
        }
    }

    /// The IPC side (give it to fpsim).
    pub fn backend(&self) -> ScriptedBackend {
        ScriptedBackend { eng: self.clone() }
    }

    /// The agent side (VM filesystem operations).
    pub fn vm(&self) -> ScriptedVm {
        ScriptedVm { eng: self.clone() }
    }

    /// Engine observes and commits every VM change so far, then signals (unless offline).
    /// A mass-deletion pause is confirmed unless [`ScriptedEngine::set_auto_confirm`] is off.
    pub fn settle(&self) {
        let mut st = self.lock();
        st.settle();
        if st.paused && st.auto_confirm {
            st.confirm_paused(true);
        }
    }

    pub fn set_auto_confirm(&self, on: bool) {
        self.lock().auto_confirm = on;
    }

    pub fn set_online(&self, online: bool) {
        let mut st = self.lock();
        let was = st.online;
        st.online = online;
        if online && !was {
            st.settle();
            if !st.flaws.no_error_resolved {
                let _ = st.events.send(EngineEvent::ErrorResolved);
            }
        }
    }

    pub fn is_online(&self) -> bool {
        self.lock().online
    }

    /// Restart the engine process (replica + journal are persisted unless `expire_on_restart`).
    pub fn restart(&self) {
        let mut st = self.lock();
        if st.flaws.expire_on_restart {
            st.epoch += 1;
            st.tombstones.clear();
        }
    }

    pub fn arm(&self, f: ReplyFault) {
        self.lock().faults.insert(f);
    }

    pub fn set_flaws(&self, flaws: Flaws) {
        self.lock().flaws = flaws;
    }

    /// Mutation log (what the "daemon" executed), for debugging failures.
    pub fn log(&self) -> Vec<String> {
        self.lock().pub_log.clone()
    }

    /// The materialized set the engine was told about.
    pub fn materialized(&self) -> HashSet<ItemId> {
        self.lock().materialized.clone()
    }
}

impl State {
    fn bump(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn signal(&self) {
        let _ = self.events.send(EngineEvent::WorkingSetChanged {
            anchor: self.anchor_bytes(self.committed),
        });
    }

    fn anchor_bytes(&self, seq: u64) -> Vec<u8> {
        let mut v = self.epoch.to_le_bytes().to_vec();
        v.extend_from_slice(&seq.to_le_bytes());
        v
    }

    /// What the Mac is shown for every item, apart from the item's own fields: the display
    /// name (rule 11 depends on siblings) and the symlink verdict (rule 9 depends on depth).
    fn all_display_names(&self) -> HashMap<ItemId, (String, bool)> {
        self.replica
            .keys()
            .filter(|id| **id != ItemId::ROOT)
            .map(|id| (*id, (self.display_name(*id), self.is_blocked(*id))))
            .collect()
    }

    fn is_blocked(&self, id: ItemId) -> bool {
        self.replica.get(&id).is_some_and(|r| {
            r.v.kind == Kind::Symlink
                && matches!(
                    symlink_view(
                        &self.depth_path(id),
                        r.v.target.as_deref().unwrap_or_default(),
                        None
                    ),
                    SymlinkView::Blocked
                )
        })
    }

    /// VM → replica: the daemon's event batch plus the engine's commit.
    fn settle(&mut self) {
        if !self.online {
            return;
        }
        let shown_before = self.all_display_names();
        let base = self.seq;
        let mut changed = false;
        let ids: Vec<ItemId> = self.vm.keys().copied().collect();
        for id in ids {
            let v = self.vm[&id].clone();
            match self.replica.get(&id).cloned() {
                None => {
                    let s = self.bump();
                    self.replica.insert(
                        id,
                        RItem {
                            v,
                            version: Version {
                                content: s,
                                meta: s,
                            },
                            changed_seq: s,
                            prev_parent: None,
                        },
                    );
                    changed = true;
                }
                Some(r) if r.v != v => {
                    let s = self.bump();
                    let mut n = r.clone();
                    let content_changed = r.v.content != v.content || r.v.target != v.target;
                    let size_same = r.v.content.len() == v.content.len();
                    if content_changed && !(self.flaws.stale_content_version && size_same) {
                        n.version.content = s;
                    }
                    if r.v.parent != v.parent
                        || r.v.name != v.name
                        || r.v.mode != v.mode
                        || r.v.target != v.target
                    {
                        n.version.meta = s;
                    }
                    if r.v.parent != v.parent {
                        n.prev_parent = Some(r.v.parent);
                    }
                    n.v = v;
                    n.changed_seq = s;
                    self.replica.insert(id, n);
                    changed = true;
                }
                Some(_) => {}
            }
        }
        // Removed items: tombstones children first (review (a)5 "Directory removal").
        let mut gone: HashSet<ItemId> = self
            .replica
            .keys()
            .filter(|id| !self.vm.contains_key(id) && !self.declined.contains(id))
            .copied()
            .collect();
        if !self.flaws.no_mass_delete_guard && !self.guard_confirmed {
            // Rule 8: removals the VM made (not this client) that would take more than 20% of
            // M (and more than 32 items), or more than 1000, wait for the user.
            let foreign = gone
                .iter()
                .filter(|id| !self.own_removed.contains(id) && self.materialized.contains(id))
                .count();
            let m = self.materialized.len();
            if !self.paused && (foreign > 1000 || (foreign > 32 && foreign as f64 > 0.2 * m as f64))
            {
                self.paused = true;
                self.pub_log.push(format!(
                    "paused: {foreign} downloaded items would be removed"
                ));
            }
            if self.paused {
                gone.retain(|id| self.own_removed.contains(id));
            }
        }
        self.own_removed
            .retain(|id| self.replica.contains_key(id) && !gone.contains(id));
        if !gone.is_empty() {
            let mut order: Vec<ItemId> = Vec::new();
            let tops: Vec<ItemId> = gone
                .iter()
                .filter(|id| !gone.contains(&self.replica[id].v.parent))
                .copied()
                .collect();
            for t in tops {
                self.post_order_replica(t, &gone, &mut order);
            }
            for id in order {
                let parent = self.replica[&id].v.parent;
                let top = !gone.contains(&parent);
                self.replica.remove(&id);
                if self.flaws.dir_tombstone_only && !top {
                    continue;
                }
                let s = self.bump();
                self.pub_log
                    .push(format!("tombstone {id} (parent {parent}) seq {s}"));
                self.tombstones.push((id, parent, s));
            }
            changed = true;
        }
        if changed && !self.flaws.no_display_rename_report {
            // Rule 11: a newcomer that sorts first takes the plain name from an existing sibling,
            // whose display name changes although the VM did not touch it: report it too, and
            // *before* the newcomer so the Mac never holds two names that fold equal.
            let displaced: Vec<ItemId> = self
                .replica
                .iter()
                .filter(|(id, r)| {
                    r.changed_seq <= base
                        && shown_before
                            .get(id)
                            .is_some_and(|b| b.0 != self.display_name(**id))
                })
                .map(|(id, _)| *id)
                .collect();
            if !displaced.is_empty() {
                self.resequence(base, &displaced);
            }
        }
        if changed && !self.flaws.symlink_depth_not_reevaluated {
            // Rule 9 is relative to the link's depth: an ancestor move can turn `../../x`
            // from in-root into an escape (or back) without touching the link.
            let flipped: Vec<ItemId> = self
                .replica
                .iter()
                .filter(|(id, r)| {
                    r.changed_seq <= base
                        && shown_before
                            .get(id)
                            .is_some_and(|b| b.1 != self.is_blocked(**id))
                })
                .map(|(id, _)| *id)
                .collect();
            for id in flipped {
                let s = self.bump();
                if let Some(r) = self.replica.get_mut(&id) {
                    r.changed_seq = s;
                }
            }
        }
        if changed {
            self.committed = self.seq;
            if !self.flaws.signal_before_commit {
                self.signal();
            }
        }
    }

    /// The user answered the mass-deletion pause (rule 8): apply the held removals, or keep
    /// those items for good.
    fn confirm_paused(&mut self, apply: bool) {
        if !self.paused {
            return;
        }
        self.paused = false;
        if apply {
            self.guard_confirmed = true;
            self.settle();
            self.guard_confirmed = false;
        } else {
            let held: Vec<ItemId> = self
                .replica
                .keys()
                .filter(|id| !self.vm.contains_key(id))
                .copied()
                .collect();
            self.declined.extend(held);
            self.settle();
        }
    }

    /// Renumber the batch after `base` so `first` get the lowest seqs (versions follow).
    fn resequence(&mut self, base: u64, first: &[ItemId]) {
        enum Slot {
            Item(ItemId),
            Tomb(usize),
        }
        let mut rest: Vec<(u64, Slot)> = Vec::new();
        for (id, r) in &self.replica {
            if r.changed_seq > base && !first.contains(id) {
                rest.push((r.changed_seq, Slot::Item(*id)));
            }
        }
        for (i, t) in self.tombstones.iter().enumerate() {
            if t.2 > base {
                rest.push((t.2, Slot::Tomb(i)));
            }
        }
        rest.sort_by_key(|(s, _)| *s);
        let mut next = base;
        for id in first {
            next += 1;
            if let Some(r) = self.replica.get_mut(id) {
                r.changed_seq = next;
            }
        }
        for (old, slot) in rest {
            next += 1;
            match slot {
                Slot::Item(id) => {
                    if let Some(r) = self.replica.get_mut(&id) {
                        r.changed_seq = next;
                        if r.version.content == old {
                            r.version.content = next;
                        }
                        if r.version.meta == old {
                            r.version.meta = next;
                        }
                    }
                }
                Slot::Tomb(i) => self.tombstones[i].2 = next,
            }
        }
        self.seq = next;
    }

    fn post_order_replica(&self, id: ItemId, gone: &HashSet<ItemId>, out: &mut Vec<ItemId>) {
        let kids: Vec<ItemId> = self
            .replica
            .iter()
            .filter(|(k, r)| r.v.parent == id && **k != id && gone.contains(k))
            .map(|(k, _)| *k)
            .collect();
        for k in kids {
            self.post_order_replica(k, gone, out);
        }
        out.push(id);
    }

    /// A VM mutation happened (agent side).
    fn vm_changed(&mut self) {
        if self.flaws.signal_before_commit && self.online {
            self.signal();
        }
    }

    // ---- VM tree helpers --------------------------------------------------------------------

    fn vm_children(&self, dir: ItemId) -> Vec<ItemId> {
        self.vm
            .iter()
            .filter(|(id, v)| v.parent == dir && **id != dir)
            .map(|(id, _)| *id)
            .collect()
    }

    fn vm_child(&self, dir: ItemId, name: &str) -> Option<ItemId> {
        self.vm
            .iter()
            .find(|(id, v)| v.parent == dir && **id != dir && v.name == name)
            .map(|(id, _)| *id)
    }

    fn vm_resolve(&self, path: &str) -> io::Result<ItemId> {
        let mut cur = ItemId::ROOT;
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        for (i, c) in comps.iter().enumerate() {
            cur = self
                .vm_child(cur, c)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.to_string()))?;
            let k = self.vm[&cur].kind;
            if i + 1 < comps.len() && k != Kind::Dir {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{path}: not a dir / symlinked ancestor"),
                ));
            }
        }
        Ok(cur)
    }

    fn vm_parent_of(&self, path: &str) -> io::Result<(ItemId, String)> {
        let (dir, name) = match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        };
        let d = self.vm_resolve(dir)?;
        if self.vm[&d].kind != Kind::Dir {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "parent not a dir",
            ));
        }
        Ok((d, name.to_string()))
    }

    fn vm_path(&self, mut id: ItemId) -> String {
        let mut parts = Vec::new();
        while id != ItemId::ROOT {
            let Some(v) = self.vm.get(&id) else { break };
            parts.push(v.name.clone());
            id = v.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    fn vm_is_descendant(&self, mut id: ItemId, anc: ItemId) -> bool {
        loop {
            if id == anc {
                return true;
            }
            if id == ItemId::ROOT {
                return false;
            }
            match self.vm.get(&id) {
                Some(v) => id = v.parent,
                None => return false,
            }
        }
    }

    fn vm_remove_subtree(&mut self, id: ItemId) {
        for c in self.vm_children(id) {
            self.vm_remove_subtree(c);
        }
        self.vm.remove(&id);
    }

    fn new_id(&mut self) -> ItemId {
        let id = ItemId(self.next_id);
        self.next_id += 1;
        id
    }

    fn free_name(&self, dir: ItemId, name: &str, taken_extra: &str) -> String {
        let (stem, ext) = split_ext(name);
        for n in 2.. {
            let cand = format!("{stem} {n}{ext}");
            if cand != taken_extra && self.vm_child(dir, &cand).is_none() {
                return cand;
            }
        }
        unreachable!("unbounded name search")
    }

    // ---- IPC view ---------------------------------------------------------------------------

    fn display_name(&self, id: ItemId) -> String {
        let Some(r) = self.replica.get(&id) else {
            return String::new();
        };
        if self.flaws.no_display_mapping || id == ItemId::ROOT {
            return r.v.name.clone();
        }
        let sibs: Vec<&str> = self
            .replica
            .iter()
            .filter(|(k, x)| x.v.parent == r.v.parent && **k != ItemId::ROOT)
            .map(|(_, x)| x.v.name.as_str())
            .collect();
        display_names(sibs)
            .get(&r.v.name)
            .cloned()
            .unwrap_or_else(|| r.v.name.clone())
    }

    fn depth_path(&self, mut id: ItemId) -> String {
        let mut parts = Vec::new();
        while id != ItemId::ROOT {
            let Some(r) = self.replica.get(&id) else {
                break;
            };
            parts.push(r.v.name.clone());
            id = r.v.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    fn ipc_item(&self, id: ItemId) -> Option<IpcItem> {
        let r = self.replica.get(&id)?;
        let blocked = self.is_blocked(id);
        let size = match r.v.kind {
            Kind::File => r.v.content.len() as u64,
            Kind::Symlink => r.v.target.as_ref().map_or(0, |t| t.len() as u64),
            Kind::Dir => 0,
        };
        let entry = Entry {
            id,
            parent: r.v.parent,
            name: r.v.name.clone(),
            kind: r.v.kind,
            size,
            mtime_ns: r.v.mtime_ns,
            mode: r.v.mode,
            version: r.version,
            symlink_target: r.v.target.clone(),
            lazy: false,
            seq: r.changed_seq,
            access: unlatch_proto::ACCESS_R | unlatch_proto::ACCESS_W | unlatch_proto::ACCESS_X,
        };
        let c = if blocked {
            caps::READING | caps::DELETING | caps::RENAMING | caps::REPARENTING
        } else {
            caps::READING | caps::WRITING | caps::DELETING | caps::RENAMING | caps::REPARENTING
        };
        Some(IpcItem {
            entry,
            display_name: self.display_name(id),
            caps: c,
            local: if self.flaws.local_meta_not_merged {
                LocalMeta::default()
            } else {
                self.local_meta.get(&id).cloned().unwrap_or_default()
            },
            user_exec: self.flaws.exec_bit_exposed
                && r.v.kind == Kind::File
                && r.v.mode & 0o100 != 0,
            symlink_blocked: blocked,
        })
    }

    fn parse_anchor(&self, a: &[u8]) -> Option<u64> {
        if a.len() != 16 {
            return None;
        }
        let mut e = [0u8; 8];
        e.copy_from_slice(&a[..8]);
        let mut s = [0u8; 8];
        s.copy_from_slice(&a[8..]);
        (u64::from_le_bytes(e) == self.epoch).then_some(u64::from_le_bytes(s))
    }

    fn in_m(&self, id: ItemId) -> bool {
        self.materialized.contains(&id)
    }

    fn changes_since(&mut self, anchor: &[u8], limit: u32) -> IpcResponse {
        if !self.online && self.flaws.changes_fail_offline {
            return perr(ErrorCode::Offline, "offline");
        }
        let Some(from) = self.parse_anchor(anchor).filter(|s| *s <= self.committed) else {
            return perr(ErrorCode::AnchorExpired, "anchor from another journal");
        };
        enum Ch {
            Up(ItemId),
            Rm(ItemId),
        }
        let mut all: Vec<(u64, Ch)> = Vec::new();
        for (id, r) in &self.replica {
            if *id == ItemId::ROOT || r.changed_seq <= from || r.changed_seq > self.committed {
                continue;
            }
            let relevant = if self.flaws.ws_only_materialized_ids {
                self.in_m(*id)
            } else {
                self.in_m(*id)
                    || self.in_m(r.v.parent)
                    || r.prev_parent.is_some_and(|p| self.in_m(p))
            };
            if relevant {
                all.push((r.changed_seq, Ch::Up(*id)));
            }
        }
        // A reported directory tombstone carries its descendants' tombstones with it: the Mac
        // can hold a child it never enumerated (moved in through the working set), and it keeps
        // a directory until every child is reported deleted.
        let window: Vec<&(ItemId, ItemId, u64)> = self
            .tombstones
            .iter()
            .filter(|(_, _, s)| *s > from && *s <= self.committed)
            .collect();
        let mut reported: HashSet<ItemId> = HashSet::new();
        for (id, parent, _) in window.iter().rev() {
            let direct = self.in_m(*id) || self.in_m(*parent);
            let via_parent = !self.flaws.tombstone_filter_strict && reported.contains(parent);
            if direct || via_parent {
                reported.insert(*id);
            }
        }
        for (id, _, s) in window {
            if reported.contains(id) {
                all.push((*s, Ch::Rm(*id)));
            }
        }
        all.sort_by_key(|(s, _)| *s);
        let more = all.len() > limit as usize;
        all.truncate(limit.max(1) as usize);
        let to = if more {
            all.last().map_or(self.committed, |(s, _)| *s)
        } else {
            self.committed
        };
        let mut updated = Vec::new();
        let mut removed = Vec::new();
        for (_, c) in all {
            match c {
                Ch::Up(id) => updated.extend(self.ipc_item(id)),
                Ch::Rm(id) => removed.push(id),
            }
        }
        self.seen_seq = self.seen_seq.max(to);
        IpcResponse::Changes {
            updated,
            removed,
            anchor: self.anchor_bytes(to),
            more,
        }
    }

    fn enumerate(&self, container: ItemId, cursor: Option<Vec<u8>>, limit: u32) -> IpcResponse {
        match self.replica.get(&container) {
            None => return perr(ErrorCode::NotFound, "no such container"),
            Some(r) if r.v.kind != Kind::Dir => return perr(ErrorCode::NotDir, "not a dir"),
            _ => {}
        }
        let mut items: Vec<IpcItem> = self
            .replica
            .iter()
            .filter(|(k, r)| r.v.parent == container && **k != ItemId::ROOT)
            .filter_map(|(k, _)| self.ipc_item(*k))
            .collect();
        items.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        let start = cursor
            .and_then(|c| <[u8; 8]>::try_from(c.as_slice()).ok())
            .map_or(0, |b| u64::from_le_bytes(b) as usize);
        let end = (start + limit.max(1) as usize).min(items.len());
        let next = (end < items.len()).then(|| (end as u64).to_le_bytes().to_vec());
        IpcResponse::Page {
            items: items
                .get(start..end)
                .map(<[IpcItem]>::to_vec)
                .unwrap_or_default(),
            next,
        }
    }

    fn fetch(&mut self, id: ItemId, dest_dir: &str) -> IpcResponse {
        if !self.online {
            return perr(ErrorCode::Offline, "offline");
        }
        let Some(r) = self.replica.get(&id).cloned() else {
            return perr(ErrorCode::NotFound, "gone");
        };
        let body = match r.v.kind {
            Kind::Dir => return perr(ErrorCode::IsDir, "dir"),
            Kind::File => r.v.content.clone(),
            Kind::Symlink => r.v.target.clone().unwrap_or_default().into_bytes(),
        };
        self.fetch_seq += 1;
        let path = PathBuf::from(dest_dir).join(format!("scripted-{}-{}", id.0, self.fetch_seq));
        if let Err(e) = std::fs::write(&path, &body) {
            return perr(ErrorCode::Io, e.to_string());
        }
        match self.ipc_item(id) {
            Some(item) => IpcResponse::Fetched {
                path: path.to_string_lossy().into_owned(),
                item,
            },
            None => perr(ErrorCode::NotFound, "gone"),
        }
    }

    /// A replayed op (ops-table hit). The stored reply describes the item as it was; the system
    /// believes whatever it gets (MQ-013), so refresh it: current version, `should_fetch_content`
    /// when the content moved on, and — if the item is gone — re-emit its tombstone so the Mac's
    /// copy is reported deleted (MQ-080 then re-offers any local work as a create).
    fn replay(&mut self, stored: IpcResponse) -> IpcResponse {
        if self.flaws.stale_replay_reply {
            return stored;
        }
        let IpcResponse::Done {
            item,
            still_pending,
            should_fetch_content,
            conflict_copy,
        } = stored
        else {
            return stored;
        };
        let id = item.entry.id;
        match self.ipc_item(id) {
            Some(now) => {
                let moved_on = now.entry.version.content != item.entry.version.content;
                IpcResponse::Done {
                    item: now,
                    still_pending,
                    should_fetch_content: should_fetch_content || moved_on,
                    conflict_copy,
                }
            }
            None => {
                let s = self.bump();
                self.tombstones.push((id, item.entry.parent, s));
                self.committed = self.seq;
                self.pub_log.push(format!(
                    "replay of deleted {id}: tombstone re-emitted seq {s}"
                ));
                self.signal();
                IpcResponse::Done {
                    item,
                    still_pending,
                    should_fetch_content,
                    conflict_copy,
                }
            }
        }
    }

    fn done(&self, id: ItemId, should_fetch: bool, conflict: Option<ItemId>) -> IpcResponse {
        match self.ipc_item(id) {
            Some(item) => IpcResponse::Done {
                item,
                still_pending: 0,
                should_fetch_content: should_fetch,
                conflict_copy: conflict.and_then(|c| self.ipc_item(c)),
            },
            None => perr(ErrorCode::NotFound, "gone after op"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &mut self,
        template_id: &str,
        parent: ItemId,
        name: &str,
        kind: CreateKind,
        symlink_target: Option<String>,
        content: Vec<u8>,
        mtime_ns: Option<i64>,
        local: LocalMeta,
        may_already_exist: bool,
    ) -> IpcResponse {
        let op = format!("create:{template_id}");
        if !self.flaws.non_idempotent {
            if let Some(r) = self.ops.get(&op).cloned() {
                let changed = self
                    .create_hashes
                    .get(&op)
                    .is_some_and(|h| *h != hash_bytes(&content));
                if changed && !self.flaws.create_replay_ignores_new_content {
                    // The user changed the new file before the replay: the op id (template id)
                    // matches but the bytes do not. Apply them as a modify of the created item
                    // on the version the first attempt produced.
                    if let IpcResponse::Done { item, .. } = &r {
                        let (id, v) = (item.entry.id, item.entry.version);
                        if self.vm.contains_key(&id) {
                            self.create_hashes.insert(op, hash_bytes(&content));
                            let base = BaseVersion {
                                content: Some(v.content),
                                meta: Some(v.meta),
                            };
                            return self.modify(
                                id,
                                base,
                                fields::CONTENTS,
                                None,
                                None,
                                Some(content),
                                None,
                                local,
                            );
                        }
                    }
                }
                return self.replay(r);
            }
        }
        self.create_hashes.insert(op.clone(), hash_bytes(&content));
        if !self.online {
            return perr(ErrorCode::Offline, "offline");
        }
        let kind = match kind {
            CreateKind::File => Kind::File,
            CreateKind::Dir => Kind::Dir,
            CreateKind::Symlink => Kind::Symlink,
            CreateKind::Package | CreateKind::Alias => {
                return perr(ErrorCode::ExcludedFromSync, "v1")
            }
        };
        self.settle();
        if !self.vm.get(&parent).is_some_and(|p| p.kind == Kind::Dir) {
            return perr(ErrorCode::NotFound, "parent gone");
        }
        let existing = self.vm_child(parent, name);
        if is_mac_local_name(name) && existing.is_none() {
            return perr(ErrorCode::ExcludedFromSync, "mac-local name");
        }
        let mut final_name = name.to_string();
        if let Some(e) = existing {
            let ev = self.vm[&e].clone();
            let same = ev.kind == kind
                && (may_already_exist
                    || (!self.flaws.non_idempotent
                        && ((kind == Kind::File && ev.content == content)
                            || (kind == Kind::Symlink && ev.target == symlink_target))));
            if same {
                let should_fetch = kind == Kind::File && ev.content != content;
                // Rule 7: other bytes re-offered for an existing file are kept as its conflict
                // copy, never dropped.
                let conflict = if should_fetch {
                    let (stem, ext) = split_ext(&ev.name);
                    self.conflict_seq += 1;
                    let cname = format!(
                        "{stem} (conflict from fpsim 2026-09-30 12.{:02}){ext}",
                        self.conflict_seq
                    );
                    let cid = self.new_id();
                    self.vm.insert(
                        cid,
                        VItem {
                            parent: ev.parent,
                            name: cname.clone(),
                            kind: Kind::File,
                            content: content.clone(),
                            target: None,
                            mode: ev.mode,
                            mtime_ns: 0,
                        },
                    );
                    self.pub_log.push(format!("conflict copy {cid} {cname:?}"));
                    self.settle();
                    Some(cid)
                } else {
                    None
                };
                let r = self.done(e, should_fetch, conflict);
                self.ops.insert(op, r.clone());
                return r;
            }
            if self.flaws.create_returns_exists {
                return perr(ErrorCode::Exists, "name taken");
            }
            final_name = self.free_name(parent, name, "");
        }
        let id = self.new_id();
        self.vm.insert(
            id,
            VItem {
                parent,
                name: final_name.clone(),
                kind,
                content,
                target: symlink_target,
                mode: if kind == Kind::Dir { 0o755 } else { 0o644 },
                mtime_ns: mtime_ns.unwrap_or(0),
            },
        );
        self.local_meta.insert(id, local);
        self.pub_log
            .push(format!("create {id} {final_name:?} in {parent}"));
        self.settle();
        let r = self.done(id, false, None);
        self.ops.insert(op, r.clone());
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn modify(
        &mut self,
        id: ItemId,
        base: BaseVersion,
        changed: u32,
        new_parent: Option<ItemId>,
        new_name: Option<String>,
        content: Option<Vec<u8>>,
        user_exec: Option<bool>,
        local: LocalMeta,
    ) -> IpcResponse {
        let op = format!(
            "modify:{id}:{base:?}:{changed}:{:?}:{new_parent:?}:{new_name:?}",
            content.as_deref().map(hash_bytes)
        );
        if !self.flaws.non_idempotent {
            if let Some(r) = self.ops.get(&op).cloned() {
                return self.replay(r);
            }
        }
        const LOCAL: u32 = fields::TAG_DATA
            | fields::LAST_USED_DATE
            | fields::FAVORITE_RANK
            | fields::CREATION_DATE
            | fields::EXTENDED_ATTRIBUTES
            | fields::TYPE_AND_CREATOR;
        if changed & LOCAL != 0 {
            self.local_meta.insert(id, local);
        }
        let is_dir = self.replica.get(&id).is_some_and(|r| r.v.kind == Kind::Dir);
        let vm_bits = changed
            & !LOCAL
            & !(if is_dir {
                fields::CONTENT_MODIFICATION_DATE
            } else {
                0
            });
        if is_dir
            && changed & fields::CONTENT_MODIFICATION_DATE != 0
            && self.flaws.dir_modify_unsupported
        {
            return perr(ErrorCode::Unsupported, "dir mtime");
        }
        if vm_bits & !fields::CONTENT_MODIFICATION_DATE == 0 && content.is_none() {
            // Mac-only metadata (rule 5): no network, never an error.
            return match self.ipc_item(id) {
                Some(_) => self.done(id, false, None),
                None => perr(ErrorCode::NotFound, "gone"),
            };
        }
        if !self.online {
            return perr(ErrorCode::Offline, "offline");
        }
        self.settle();
        if !self.vm.contains_key(&id) {
            if self.flaws.modify_missing_ok {
                // Invents success for bytes that went nowhere.
                return match self.replica_or_ghost(id) {
                    Some(item) => IpcResponse::Done {
                        item,
                        still_pending: 0,
                        should_fetch_content: false,
                        conflict_copy: None,
                    },
                    None => perr(ErrorCode::NotFound, "gone"),
                };
            }
            return perr(ErrorCode::NotFound, "gone");
        }
        let cur = self.replica.get(&id).cloned();
        let mut should_fetch = false;
        let mut conflict = None;
        if let (Some(bytes), Some(cur)) = (content, cur.clone()) {
            let base_ok = base.content.is_none_or_eq(cur.version.content);
            let same_bytes = !self.flaws.non_idempotent && cur.v.content == bytes;
            // The mismatch is this client's own earlier write whose reply was lost (the system
            // then re-sends newer bytes on the old base): fast-forward, never a conflict copy of
            // the user's own save.
            let own_write = !self.flaws.no_self_fastforward
                && !self.flaws.non_idempotent
                && base
                    .content
                    .is_some_and(|b| self.last_mod.get(&id) == Some(&(b, cur.version.content)));
            if base_ok || same_bytes || own_write {
                if let Some(v) = self.vm.get_mut(&id) {
                    v.content = bytes;
                }
                self.pub_log.push(format!("write {id}"));
                self.settle();
                if let (Some(b), Some(now)) = (
                    base.content,
                    self.replica.get(&id).map(|r| r.version.content),
                ) {
                    // Keep the chain's original base: every retry of it still fast-forwards.
                    let root = if own_write {
                        self.last_mod.get(&id).map_or(b, |(rb, _)| *rb)
                    } else {
                        b
                    };
                    self.last_mod.insert(id, (root, now));
                }
            } else {
                // Rule 3: never overwrite; the upload lands next to it and the Mac refetches.
                let (stem, ext) = split_ext(&cur.v.name);
                self.conflict_seq += 1;
                let cname = format!(
                    "{stem} (conflict from fpsim 2026-09-30 12.{:02}){ext}",
                    self.conflict_seq
                );
                let cid = self.new_id();
                self.vm.insert(
                    cid,
                    VItem {
                        parent: cur.v.parent,
                        name: cname.clone(),
                        kind: Kind::File,
                        content: bytes,
                        target: None,
                        mode: cur.v.mode,
                        mtime_ns: 0,
                    },
                );
                self.pub_log.push(format!("conflict copy {cid} {cname:?}"));
                should_fetch = !self.flaws.conflict_without_fetch;
                conflict = Some(cid);
            }
        }
        if changed & (fields::FILENAME | fields::PARENT) != 0 {
            let meta_ok = cur
                .as_ref()
                .is_some_and(|c| base.meta.is_none_or_eq(c.version.meta));
            if !meta_ok && self.flaws.rename_conflict_errors {
                return perr(ErrorCode::VersionMismatch, "renamed on the VM");
            }
            if meta_ok {
                let dest = new_parent.unwrap_or_else(|| self.vm[&id].parent);
                let cur_name = self.vm[&id].name.clone();
                let mut name = match new_name {
                    Some(n) => n,
                    // The flaw: the item's display name stands in for "unchanged".
                    None if self.flaws.display_name_leaks_to_vm => self.display_name(id),
                    None => cur_name.clone(),
                };
                // Display → real mapping (rule 11): the item's own display name means "unchanged".
                if Some(name.as_str()) == Some(self.display_name(id).as_str())
                    && dest == self.vm[&id].parent
                {
                    name = cur_name.clone();
                } else if let Some(real) = strip_unlatch_marker(&name) {
                    if real == cur_name && !self.flaws.display_name_leaks_to_vm {
                        name = real;
                    }
                }
                let dest_ok = self.vm.get(&dest).is_some_and(|d| d.kind == Kind::Dir)
                    && !self.vm_is_descendant(dest, id);
                let free = self.vm_child(dest, &name).is_none_or_eq(id);
                if dest_ok && free {
                    if let Some(v) = self.vm.get_mut(&id) {
                        v.parent = dest;
                        v.name = name;
                    }
                    self.pub_log.push(format!("rename {id}"));
                }
                // Rule 4: metadata never errors — a refused move returns the server's state.
            }
        }
        if let (Some(x), Some(v)) = (user_exec, self.vm.get_mut(&id)) {
            v.mode = if x { v.mode | 0o100 } else { v.mode & !0o100 };
        }
        self.settle();
        // MQ-013: the reply's version is believed with whatever bytes the Mac holds, so any
        // content newer than the base the Mac sent must be re-fetched — renames included.
        if conflict.is_none() && !self.flaws.metadata_reply_hides_content_change {
            let now = self.replica.get(&id).map(|r| r.version.content);
            if let (Some(b), Some(now)) = (base.content, now) {
                if b != now && !(changed & fields::CONTENTS != 0 && !should_fetch) {
                    should_fetch = true;
                }
            }
        }
        let r = self.done(id, should_fetch, conflict);
        self.ops.insert(op, r.clone());
        r
    }

    fn replica_or_ghost(&self, id: ItemId) -> Option<IpcItem> {
        self.ipc_item(id).or_else(|| {
            Some(IpcItem {
                entry: Entry {
                    id,
                    parent: ItemId::ROOT,
                    name: format!("ghost-{id}"),
                    kind: Kind::File,
                    size: 0,
                    mtime_ns: 0,
                    mode: 0o644,
                    version: Version {
                        content: self.seq,
                        meta: self.seq,
                    },
                    symlink_target: None,
                    lazy: false,
                    seq: self.seq,
                    access: 7,
                },
                display_name: format!("ghost-{id}"),
                caps: 0,
                local: LocalMeta::default(),
                user_exec: false,
                symlink_blocked: false,
            })
        })
    }

    fn delete(&mut self, id: ItemId, base: BaseVersion, recursive: bool) -> IpcResponse {
        // A retry of the same call keeps the seen_seq of its first attempt.
        let call = format!("{base:?}:{recursive}");
        let seen = match self.delete_seen.get(&id) {
            Some((c, s)) if *c == call && !self.flaws.delete_retry_widens_seen => *s,
            _ => self.seen_seq,
        };
        self.delete_seen.insert(id, (call, seen));
        let op = format!("delete:{id}:{base:?}:{recursive}:{seen}");
        if !self.flaws.non_idempotent {
            if let Some(r) = self.ops.get(&op) {
                return r.clone();
            }
        }
        if !self.online {
            return perr(ErrorCode::Offline, "offline");
        }
        self.settle();
        if !self.vm.contains_key(&id) {
            return IpcResponse::Deleted;
        }
        if id == ItemId::ROOT {
            return perr(ErrorCode::Permission, "root");
        }
        let cur = self.replica.get(&id).cloned();
        let reject = |st: &State, why: &str| IpcResponse::Error {
            code: ErrorCode::DeletionRejected,
            msg: why.into(),
            current: st.ipc_item(id),
        };
        if let Some(c) = &cur {
            let meta_ok = base.meta.is_none_or_eq(c.version.meta);
            // An unknown content base proves nothing: only the anchor the system consumed.
            let content_ok = c.v.kind == Kind::Dir
                || match base.content {
                    Some(b) => b == c.version.content,
                    None => c.changed_seq <= seen,
                };
            if !meta_ok || !content_ok {
                return reject(self, "changed since seen");
            }
        }
        let kids = self.vm_children(id);
        if !kids.is_empty() && !recursive {
            return reject(self, "not empty");
        }
        // Keep everything the Mac has not seen (seq > seen_seq) and its ancestors (rule 6).
        let mut kept: HashSet<ItemId> = HashSet::new();
        let mut stack = kids;
        let mut all = vec![id];
        while let Some(k) = stack.pop() {
            all.push(k);
            let unseen = !self.flaws.delete_ignores_seen_seq
                && self.replica.get(&k).is_none_or(|r| r.changed_seq > seen);
            if unseen {
                let mut a = k;
                while a != id && kept.insert(a) {
                    a = self.vm[&a].parent;
                }
                kept.insert(id);
            }
            stack.extend(self.vm_children(k));
        }
        for k in all.iter().rev() {
            if !kept.contains(k) && self.vm_children(*k).is_empty() {
                self.vm.remove(k);
                self.own_removed.insert(*k);
            }
        }
        self.pub_log
            .push(format!("remove {id} kept {}", kept.len()));
        self.settle();
        let r = if kept.is_empty() {
            IpcResponse::Deleted
        } else {
            reject(self, "partial delete")
        };
        self.ops.insert(op, r.clone());
        r
    }

    fn status(&self) -> EngineStatus {
        EngineStatus {
            state: if self.paused {
                ConnState::Paused {
                    reason: "scripted mass deletion".into(),
                }
            } else if self.online {
                ConnState::Live
            } else {
                ConnState::Offline {
                    error: "scripted".into(),
                    retry_in_ms: 0,
                }
            },
            entries: self.replica.len() as u64,
            anchor: self.anchor_bytes(self.committed),
            rtt_us: None,
            cache_bytes: 0,
            pending_uploads: 0,
            server: None,
        }
    }
}

/// `Option<u64>::None` means "unknown base" (beforeFirstSync) and matches anything.
trait NoneOrEq<T> {
    fn is_none_or_eq(&self, v: T) -> bool;
}

impl<T: PartialEq + Copy> NoneOrEq<T> for Option<T> {
    fn is_none_or_eq(&self, v: T) -> bool {
        match self {
            None => true,
            Some(x) => *x == v,
        }
    }
}

fn read_fd(fd: Option<OwnedFd>) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    if let Some(fd) = fd {
        std::fs::File::from(fd).read_to_end(&mut out)?;
    }
    Ok(out)
}

/// IPC side of [`ScriptedEngine`].
pub struct ScriptedBackend {
    eng: ScriptedEngine,
}

impl ScriptedBackend {
    /// Handle one request exactly like the engine's IPC dispatcher would.
    pub fn handle(&self, req: IpcRequest, fd: Option<OwnedFd>) -> Result<IpcResponse, ProtoError> {
        let mut st = self.eng.lock();
        let fault = match &req {
            IpcRequest::Create { .. } => Some(ReplyFault::Create),
            IpcRequest::Modify { .. } => Some(ReplyFault::Modify),
            IpcRequest::Delete { .. } => Some(ReplyFault::Delete),
            IpcRequest::Fetch { .. } => Some(ReplyFault::Fetch),
            _ => None,
        };
        let resp = match req {
            IpcRequest::Hello { .. } => IpcResponse::Hello {
                proto: PROTO_VERSION,
                domain: "scripted".into(),
            },
            IpcRequest::Item { id } => {
                if !st.online && st.flaws.item_not_found_offline && id != ItemId::ROOT {
                    perr(ErrorCode::NotFound, "not loaded")
                } else {
                    st.ipc_item(id)
                        .map_or_else(|| perr(ErrorCode::NotFound, "gone"), IpcResponse::Item)
                }
            }
            IpcRequest::Enumerate {
                container,
                cursor,
                limit,
                ..
            } => {
                if cursor.is_none() {
                    // The system re-learns the folder: a later delete is a new call.
                    st.delete_seen.remove(&container);
                }
                st.enumerate(container, cursor, limit)
            }
            IpcRequest::CurrentAnchor => IpcResponse::Anchor(st.anchor_bytes(st.committed)),
            IpcRequest::ChangesSince { anchor, limit } => st.changes_since(&anchor, limit),
            IpcRequest::MaterializedChanged {
                added,
                removed,
                full,
            } => {
                st.pub_log
                    .push(format!("materialized +{added:?} -{removed:?} full={full}"));
                if full {
                    st.materialized.clear();
                }
                st.materialized.extend(added);
                for r in removed {
                    st.materialized.remove(&r);
                }
                IpcResponse::Ok
            }
            IpcRequest::Fetch { id, dest_dir, .. } => st.fetch(id, &dest_dir),
            IpcRequest::Create {
                template_id,
                parent,
                name,
                kind,
                has_content,
                symlink_target,
                mtime_ns,
                local,
                may_already_exist,
                ..
            } => {
                let content = if has_content {
                    read_fd(fd).map_err(|e| ProtoError::new(ErrorCode::Io, e.to_string()))?
                } else {
                    Vec::new()
                };
                st.create(
                    &template_id,
                    parent,
                    &name,
                    kind,
                    symlink_target,
                    content,
                    mtime_ns,
                    local,
                    may_already_exist,
                )
            }
            IpcRequest::Modify {
                id,
                base,
                changed_fields,
                new_parent,
                new_name,
                has_content,
                user_exec,
                local,
                ..
            } => {
                let content = if has_content {
                    Some(read_fd(fd).map_err(|e| ProtoError::new(ErrorCode::Io, e.to_string()))?)
                } else {
                    None
                };
                st.modify(
                    id,
                    base,
                    changed_fields,
                    new_parent,
                    new_name,
                    content,
                    user_exec,
                    local,
                )
            }
            IpcRequest::Delete {
                id,
                base,
                recursive,
            } => st.delete(id, base, recursive),
            IpcRequest::Cancel { .. } => IpcResponse::Ok,
            IpcRequest::ConfirmPaused { apply } => {
                st.confirm_paused(apply);
                IpcResponse::Ok
            }
            IpcRequest::Status => IpcResponse::Status(st.status()),
        };
        if let Some(f) = fault {
            if st.faults.remove(&f) {
                // die_before_ipc_reply: the op ran, the reply is lost with the connection.
                return Err(ProtoError::new(
                    ErrorCode::Offline,
                    format!("injected: reply to {f:?} dropped"),
                ));
            }
        }
        Ok(resp)
    }
}

impl Backend for ScriptedBackend {
    fn call(
        &mut self,
        req: IpcRequest,
        content: Option<OwnedFd>,
    ) -> Result<IpcResponse, ProtoError> {
        self.handle(req, content)
    }
}

/// Agent side of [`ScriptedEngine`]: filesystem operations on the in-memory VM.
pub struct ScriptedVm {
    eng: ScriptedEngine,
}

fn nf(p: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, p.to_string())
}

impl VmFs for ScriptedVm {
    fn write(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        let mut st = self.eng.lock();
        let (dir, name) = st.vm_parent_of(path)?;
        match st.vm_child(dir, &name) {
            Some(id) => {
                let v = st.vm.get_mut(&id).ok_or_else(|| nf(path))?;
                if v.kind != Kind::File {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
                }
                v.content = data.to_vec();
                v.mtime_ns += 1;
            }
            None => {
                let id = st.new_id();
                st.vm.insert(
                    id,
                    VItem {
                        parent: dir,
                        name,
                        kind: Kind::File,
                        content: data.to_vec(),
                        target: None,
                        mode: 0o644,
                        mtime_ns: 1,
                    },
                );
            }
        }
        st.vm_changed();
        Ok(())
    }

    fn append(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        let mut cur = self.read(path).unwrap_or_default();
        cur.extend_from_slice(data);
        self.write(path, &cur)
    }

    fn read(&mut self, path: &str) -> io::Result<Vec<u8>> {
        let st = self.eng.lock();
        let id = st.vm_resolve(path)?;
        match &st.vm[&id] {
            v if v.kind == Kind::File => Ok(v.content.clone()),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file")),
        }
    }

    fn mkdir(&mut self, path: &str) -> io::Result<()> {
        let mut st = self.eng.lock();
        let (dir, name) = st.vm_parent_of(path)?;
        if st.vm_child(dir, &name).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                path.to_string(),
            ));
        }
        let id = st.new_id();
        st.vm.insert(
            id,
            VItem {
                parent: dir,
                name,
                kind: Kind::Dir,
                content: Vec::new(),
                target: None,
                mode: 0o755,
                mtime_ns: 1,
            },
        );
        st.vm_changed();
        Ok(())
    }

    fn remove_file(&mut self, path: &str) -> io::Result<()> {
        let mut st = self.eng.lock();
        let id = st.vm_resolve(path)?;
        if st.vm[&id].kind == Kind::Dir {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "is a dir"));
        }
        st.vm.remove(&id);
        st.vm_changed();
        Ok(())
    }

    fn remove_dir_all(&mut self, path: &str) -> io::Result<()> {
        let mut st = self.eng.lock();
        let id = st.vm_resolve(path)?;
        if id == ItemId::ROOT {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "root"));
        }
        st.vm_remove_subtree(id);
        st.vm_changed();
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> io::Result<()> {
        let mut st = self.eng.lock();
        let src = st.vm_resolve(from)?;
        let (dir, name) = st.vm_parent_of(to)?;
        if st.vm_is_descendant(dir, src) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "into own subtree",
            ));
        }
        if let Some(dst) = st.vm_child(dir, &name) {
            if dst == src {
                return Ok(());
            }
            let (sk, dk) = (st.vm[&src].kind, st.vm[&dst].kind);
            match (sk, dk) {
                (Kind::Dir, Kind::Dir) if st.vm_children(dst).is_empty() => {
                    st.vm.remove(&dst);
                }
                (Kind::Dir, _) | (_, Kind::Dir) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "rename over dir/nondir",
                    ));
                }
                (sk, dk) if sk != dk => {
                    // Kinds differ: never reuse the id (review (a)3); the source moves in.
                    st.vm.remove(&dst);
                }
                _ => {
                    // Rename-over: the destination's id survives (review (a)3 reuse rule).
                    let s = st.vm.remove(&src).ok_or_else(|| nf(from))?;
                    if let Some(d) = st.vm.get_mut(&dst) {
                        d.kind = s.kind;
                        d.content = s.content;
                        d.target = s.target;
                        d.mode = s.mode;
                        d.mtime_ns = s.mtime_ns;
                    }
                    st.vm_changed();
                    return Ok(());
                }
            }
        }
        if let Some(v) = st.vm.get_mut(&src) {
            v.parent = dir;
            v.name = name;
        }
        st.vm_changed();
        Ok(())
    }

    fn symlink(&mut self, target: &str, path: &str) -> io::Result<()> {
        let mut st = self.eng.lock();
        let (dir, name) = st.vm_parent_of(path)?;
        if st.vm_child(dir, &name).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                path.to_string(),
            ));
        }
        let id = st.new_id();
        st.vm.insert(
            id,
            VItem {
                parent: dir,
                name,
                kind: Kind::Symlink,
                content: Vec::new(),
                target: Some(target.to_string()),
                mode: 0o777,
                mtime_ns: 1,
            },
        );
        st.vm_changed();
        Ok(())
    }

    fn hard_link(&mut self, existing: &str, new: &str) -> io::Result<()> {
        // The scripted VM has no shared inodes; a hardlink is modelled as an independent copy
        // (every link gets its own id in Unlatch too, review (a)3).
        let data = self.read(existing)?;
        if self.kind(new).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                new.to_string(),
            ));
        }
        self.write(new, &data)
    }

    fn set_mode(&mut self, path: &str, mode: u32) -> io::Result<()> {
        let mut st = self.eng.lock();
        let id = st.vm_resolve(path)?;
        if let Some(v) = st.vm.get_mut(&id) {
            v.mode = mode & 0o7777;
        }
        st.vm_changed();
        Ok(())
    }

    fn kind(&mut self, path: &str) -> Option<VmKind> {
        let st = self.eng.lock();
        let id = st.vm_resolve(path).ok()?;
        Some(match st.vm[&id].kind {
            Kind::File => VmKind::File,
            Kind::Dir => VmKind::Dir,
            Kind::Symlink => VmKind::Symlink,
        })
    }

    fn list(&mut self, dir: &str) -> io::Result<Vec<String>> {
        let st = self.eng.lock();
        let id = st.vm_resolve(dir)?;
        if st.vm[&id].kind != Kind::Dir {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a dir"));
        }
        let mut v: Vec<String> = st
            .vm_children(id)
            .into_iter()
            .map(|c| st.vm[&c].name.clone())
            .collect();
        v.sort();
        Ok(v)
    }

    fn snapshot(&mut self) -> io::Result<VmTree> {
        let st = self.eng.lock();
        let mut t = VmTree::default();
        for (id, v) in &st.vm {
            if *id == ItemId::ROOT {
                continue;
            }
            let kind = match v.kind {
                Kind::File => VmKind::File,
                Kind::Dir => VmKind::Dir,
                Kind::Symlink => VmKind::Symlink,
            };
            let size = match v.kind {
                Kind::File => v.content.len() as u64,
                Kind::Symlink => v.target.as_ref().map_or(0, |x| x.len() as u64),
                Kind::Dir => 0,
            };
            t.nodes.insert(
                st.vm_path(*id),
                VmNode {
                    kind,
                    size,
                    content: (v.kind == Kind::File).then(|| v.content.clone()),
                    target: v.target.clone(),
                    mode: v.mode,
                },
            );
        }
        Ok(t)
    }

    fn root_abs(&self) -> Option<PathBuf> {
        None
    }
}
