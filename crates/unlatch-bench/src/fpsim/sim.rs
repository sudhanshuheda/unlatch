//! The fileproviderd model: user actions → provider calls, provider signals → change
//! enumeration, retries, and the local disk they produce.
//!
//! Time is virtual: backoffs measured in minutes (MQ-005, MQ-035) are simulated by advancing
//! [`FpSim::now`], never by sleeping. Everything that reaches the provider goes through
//! [`Backend::call`] with exactly the `IpcRequest`s the Swift shim forwards.

use super::backend::{request_kind, Backend};
use super::disk::{LocalDisk, Node, NodeKey, NsError};
use super::names::{bounce_name, finder_copy_name, fold};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;
use unlatch_core::EngineEvent;
use unlatch_proto::ipc::{caps, fields, IpcItem, IpcRequest, IpcResponse, LocalMeta};
use unlatch_proto::{
    is_mac_local_name, BaseVersion, ErrorCode, ItemId, Kind, ProtoError, Version, PROTO_VERSION,
};

/// How the shim answers `enumerator(for: .trashContainer)`. The IPC has no trash identifier: the
/// shim answers it without asking the engine (review (e)2), so this is part of the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrashAnswer {
    /// `NSFeatureUnsupportedError`: the system gives up after two attempts (MQ-010).
    FeatureUnsupported,
    /// `.noSuchItem`: delete/rematerialize/ask loop at ~1 Hz, for ever (MQ-009).
    NoSuchItem,
}

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub domain: String,
    /// Scratch directory for content fds and fetch destinations (the extension's
    /// `temporaryDirectoryURL`).
    pub scratch: PathBuf,
    pub page_limit: u32,
    pub changes_limit: u32,
    pub trash_answer: TrashAnswer,
    /// Upper bound of loop iterations per [`FpSim::pump`] (livelock detector).
    pub max_iterations: u64,
}

impl SimConfig {
    pub fn new(domain: &str, scratch: &Path) -> SimConfig {
        SimConfig {
            domain: domain.to_string(),
            scratch: scratch.to_path_buf(),
            page_limit: 128,
            changes_limit: 256,
            trash_answer: TrashAnswer::FeatureUnsupported,
            max_iterations: 200_000,
        }
    }
}

/// A provider call as fpsim issued it (for assertions and failure reports).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRecord {
    pub kind: &'static str,
    pub id: Option<ItemId>,
    pub fields: u32,
    pub template_id: Option<String>,
    pub at: Duration,
    /// `None` = transport failure; `Some(None)` = success; `Some(Some(code))` = provider error.
    pub outcome: Option<Option<ErrorCode>>,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub calls: BTreeMap<&'static str, u64>,
    /// Container enumerations per container id (MQ-001: must stay ≤ 1 outside reimports).
    pub enumerations: HashMap<ItemId, u32>,
    pub trash_asks: u64,
    pub transport_errors: u64,
    pub ws_failures: u64,
    pub error_resolved: u64,
    pub conflict_replies: u64,
    pub still_pending: u64,
    pub evictions_on_update: u64,
    pub bounces: u64,
    pub item_not_found_deletes: u64,
    pub deletion_rejected: u64,
    /// Replayed creates whose item had already arrived through the working set.
    pub replay_merges: u64,
    pub reimports: u64,
    pub anchor_expired: u64,
    /// Contract violations noticed while talking to the provider (always a test failure).
    pub violations: Vec<String>,
}

/// Why a user action failed (what the app/Finder would show).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionError {
    NotFound(String),
    Collision(String),
    NotADirectory(String),
    IsADirectory(String),
    Refused(String),
    /// `fetchContents` failed; the item stays, nothing retries it (MQ-012, MQ-036).
    Fetch(ErrorCode),
    Io(String),
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionError::NotFound(p) => write!(f, "{p}: no such item"),
            ActionError::Collision(p) => write!(f, "{p}: name already taken"),
            ActionError::NotADirectory(p) => write!(f, "{p}: not a directory"),
            ActionError::IsADirectory(p) => write!(f, "{p}: is a directory"),
            ActionError::Refused(p) => write!(f, "refused: {p}"),
            ActionError::Fetch(c) => write!(f, "fetch failed: {c:?}"),
            ActionError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for ActionError {}

#[derive(Clone, Copy, Debug, Default)]
struct Retry {
    attempts: u32,
    next_at: Duration,
}

#[derive(Clone, Debug)]
struct PendingDelete {
    id: ItemId,
    base: Version,
    recursive: bool,
    seq: u64,
    retry: Retry,
    /// Where the item lived, for restoring after `DeletionRejected`.
    parent: ItemId,
}

#[derive(Clone, Debug, Default)]
struct WorkingSet {
    signalled: bool,
    failures: u32,
    next_allowed: Duration,
}

#[derive(Clone, Debug, Default)]
struct Trash {
    looping: bool,
    next_at: Duration,
}

/// What one [`FpSim::pump`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PumpReport {
    pub iterations: u64,
    /// The iteration cap was hit (something re-schedules itself without ever finishing).
    pub livelock: bool,
    /// Local changes still waiting for the provider.
    pub pending: usize,
    /// A working-set enumeration is still owed (throttled or failing).
    pub ws_pending: bool,
}

/// Initial retry delay of a queued write and its growth (MQ-035: 5.50, 10.56, 20.30, 43.03 …).
const WRITE_RETRY_FIRST: f64 = 5.5;
const WRITE_RETRY_GROWTH: f64 = 1.93;
/// MQ-014: a colliding create is retried after 0.04 s, 5 s, 15 s, then doubling.
const COLLISION_RETRY: [f64; 3] = [0.04, 5.0, 15.0];
/// MQ-005: throttle of a failing change enumeration, fitted as 30 s × 1.18^n, capped at 47 min.
const WS_THROTTLE_BASE: f64 = 30.0;
const WS_THROTTLE_GROWTH: f64 = 1.18;
const WS_THROTTLE_CEILING: f64 = 2820.0;
const DAY: f64 = 86_400.0;

/// Delay before re-offering a write after `attempts` failures (MQ-035; no ceiling observed, we
/// stop growing at a day so the virtual clock never overflows).
pub fn write_retry_delay(attempts: u32) -> Duration {
    let n = attempts.max(1) - 1;
    Duration::from_secs_f64((WRITE_RETRY_FIRST * WRITE_RETRY_GROWTH.powi(n as i32)).min(DAY))
}

/// Delay before re-offering a create that failed with a name collision (MQ-014).
pub fn collision_retry_delay(attempts: u32) -> Duration {
    let i = attempts.max(1) as usize - 1;
    let s = match COLLISION_RETRY.get(i) {
        Some(s) => *s,
        None => (COLLISION_RETRY[2] * 2f64.powi((i - 2) as i32)).min(DAY),
    };
    Duration::from_secs_f64(s)
}

/// Throttle after `failures` consecutive failed change enumerations (MQ-005).
pub fn ws_throttle(failures: u32) -> Duration {
    Duration::from_secs_f64(
        (WS_THROTTLE_BASE * WS_THROTTLE_GROWTH.powi(failures as i32)).min(WS_THROTTLE_CEILING),
    )
}

/// The simulated fileproviderd for one domain.
pub struct FpSim<B: Backend> {
    cfg: SimConfig,
    backend: B,
    events: Receiver<EngineEvent>,
    disk: LocalDisk,
    now: Duration,
    anchor: Option<Vec<u8>>,
    ws: WorkingSet,
    trash: Trash,
    retries: HashMap<NodeKey, Retry>,
    deletes: Vec<PendingDelete>,
    reimports: Vec<ItemId>,
    reported_m: Option<BTreeSet<ItemId>>,
    change_sets: u64,
    change_seq: u64,
    template_seq: u64,
    fetch_seq: u64,
    pub stats: Stats,
    pub history: Vec<CallRecord>,
    /// Names fpsim itself asked the provider to create or rename to (fuzz bookkeeping).
    pub names_sent: HashSet<String>,
}

const HISTORY_CAP: usize = 200_000;

impl<B: Backend> FpSim<B> {
    pub fn new(cfg: SimConfig, backend: B, events: Receiver<EngineEvent>) -> FpSim<B> {
        FpSim {
            cfg,
            backend,
            events,
            disk: LocalDisk::new(),
            now: Duration::ZERO,
            anchor: None,
            ws: WorkingSet::default(),
            trash: Trash::default(),
            retries: HashMap::new(),
            deletes: Vec::new(),
            reimports: Vec::new(),
            reported_m: None,
            change_sets: 0,
            change_seq: 0,
            template_seq: 0,
            fetch_seq: 0,
            stats: Stats::default(),
            history: Vec::new(),
            names_sent: HashSet::new(),
        }
    }

    pub fn disk(&self) -> &LocalDisk {
        &self.disk
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn config(&self) -> &SimConfig {
        &self.cfg
    }

    /// Virtual time since the domain was added.
    pub fn now(&self) -> Duration {
        self.now
    }

    /// Let virtual time pass without doing anything (then call [`FpSim::pump`]).
    pub fn advance(&mut self, d: Duration) {
        self.now += d;
    }

    pub fn anchor(&self) -> Option<&[u8]> {
        self.anchor.as_deref()
    }

    /// Number of non-empty change sets applied so far (the anchor the system consumed moved).
    pub fn change_sets_applied(&self) -> u64 {
        self.change_sets
    }

    /// Number of local changes still owed to the provider.
    pub fn pending(&self) -> usize {
        let nodes = self
            .disk
            .all()
            .into_iter()
            .filter(|k| self.disk.get(*k).is_some_and(Node::has_pending))
            .count();
        nodes + self.deletes.len()
    }

    /// Paths of nodes whose upload is owed, plus pending deletes (for failure reports).
    pub fn pending_report(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .disk
            .all()
            .into_iter()
            .filter_map(|k| {
                let n = self.disk.get(k)?;
                n.has_pending().then(|| {
                    format!(
                        "{} (create={} dirty={:#x} attempts={})",
                        self.disk.path_of(k),
                        n.pending_create,
                        n.dirty,
                        self.retries.get(&k).map_or(0, |r| r.attempts)
                    )
                })
            })
            .collect();
        v.extend(
            self.deletes
                .iter()
                .map(|d| format!("delete id {} (attempts={})", d.id, d.retry.attempts)),
        );
        v
    }

    /// Nodes whose upload failed permanently (`cannotSynchronize` / refused), by path.
    pub fn sync_errors(&self) -> Vec<(String, ErrorCode)> {
        self.disk
            .all()
            .into_iter()
            .filter_map(|k| {
                let n = self.disk.get(k)?;
                Some((self.disk.path_of(k), n.sync_error?))
            })
            .collect()
    }

    // ---- provider plumbing ------------------------------------------------------------------

    fn call(&mut self, req: IpcRequest, fd: Option<OwnedFd>) -> Result<IpcResponse, ProtoError> {
        let kind = request_kind(&req);
        let (id, flds, template_id) = match &req {
            IpcRequest::Item { id }
            | IpcRequest::Fetch { id, .. }
            | IpcRequest::Delete { id, .. } => (Some(*id), 0, None),
            IpcRequest::Enumerate { container, .. } => (Some(*container), 0, None),
            IpcRequest::Modify {
                id, changed_fields, ..
            } => (Some(*id), *changed_fields, None),
            IpcRequest::Create {
                parent,
                changed_fields,
                template_id,
                ..
            } => (Some(*parent), *changed_fields, Some(template_id.clone())),
            _ => (None, 0, None),
        };
        *self.stats.calls.entry(kind).or_default() += 1;
        let r = self.backend.call(req, fd);
        let outcome = match &r {
            Err(_) => None,
            Ok(IpcResponse::Error { code, .. }) => Some(Some(*code)),
            Ok(_) => Some(None),
        };
        if r.is_err() {
            self.stats.transport_errors += 1;
        }
        if self.history.len() < HISTORY_CAP {
            self.history.push(CallRecord {
                kind,
                id,
                fields: flds,
                template_id,
                at: self.now,
                outcome,
            });
        }
        r
    }

    fn violation(&mut self, msg: String) {
        self.stats.violations.push(msg);
    }

    /// Content handed to the provider as a file descriptor (an unnamed temp file, rewound).
    fn content_fd(&self, bytes: &[u8]) -> std::io::Result<OwnedFd> {
        std::fs::create_dir_all(&self.cfg.scratch)?;
        let mut f = tempfile::tempfile_in(&self.cfg.scratch)?;
        f.write_all(bytes)?;
        f.seek(SeekFrom::Start(0))?;
        Ok(OwnedFd::from(f))
    }

    fn next_template(&mut self) -> String {
        self.template_seq += 1;
        format!(
            "fpsim-template-{}-{}",
            std::process::id(),
            self.template_seq
        )
    }

    fn touch_seq(&mut self) -> u64 {
        self.change_seq += 1;
        self.change_seq
    }

    // ---- domain lifecycle -------------------------------------------------------------------

    /// `NSFileProviderManager.add(domain)` + the user opening the domain in Finder: fetch the
    /// working-set anchor, probe the trash (MQ-075), enumerate the root.
    pub fn add_domain(&mut self) -> Result<(), ActionError> {
        match self.call(
            IpcRequest::Hello {
                proto: PROTO_VERSION,
                domain: self.cfg.domain.clone(),
            },
            None,
        ) {
            Ok(IpcResponse::Hello { .. }) | Ok(IpcResponse::Ok) => {}
            Ok(other) => self.violation(format!("Hello answered with {other:?}")),
            Err(e) => return Err(ActionError::Io(e.to_string())),
        }
        self.refresh_anchor()?;
        if let Ok(IpcResponse::Item(root)) = self.call(IpcRequest::Item { id: ItemId::ROOT }, None)
        {
            self.apply_item(&root);
        }
        // MQ-075: the system creates the trash node itself and asks for its children twice.
        self.ask_trash();
        self.ask_trash();
        self.browse("")
    }

    fn refresh_anchor(&mut self) -> Result<(), ActionError> {
        match self.call(IpcRequest::CurrentAnchor, None) {
            Ok(IpcResponse::Anchor(a)) => {
                self.anchor = Some(a);
                Ok(())
            }
            Ok(IpcResponse::Error { code, .. }) => Err(ActionError::Fetch(code)),
            Ok(other) => {
                self.violation(format!("CurrentAnchor answered with {other:?}"));
                Err(ActionError::Io("bad anchor reply".into()))
            }
            Err(e) => Err(ActionError::Io(e.to_string())),
        }
    }

    fn ask_trash(&mut self) {
        self.stats.trash_asks += 1;
        match self.cfg.trash_answer {
            // MQ-010: throttled, abandoned after two attempts.
            TrashAnswer::FeatureUnsupported => self.trash.looping = false,
            // MQ-009: delete + rematerialize + ask again, about once a second, for ever.
            TrashAnswer::NoSuchItem => {
                self.trash.looping = true;
                self.trash.next_at = self.now + Duration::from_secs(1);
            }
        }
    }

    // ---- signals ----------------------------------------------------------------------------

    fn drain_events(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                EngineEvent::WorkingSetChanged { .. } => self.ws.signalled = true,
                EngineEvent::ErrorResolved => {
                    // signalErrorResolved(.serverUnreachable): lifts the enumeration throttle
                    // (MQ-005) and is the only thing that flushes queued writes (MQ-037). The
                    // host then signals the working set.
                    self.stats.error_resolved += 1;
                    self.ws.failures = 0;
                    self.ws.next_allowed = self.now;
                    self.ws.signalled = true;
                    for r in self.retries.values_mut() {
                        r.next_at = self.now;
                    }
                    for d in &mut self.deletes {
                        d.retry.next_at = self.now;
                    }
                }
                EngineEvent::Reimport { below } => self.reimports.push(below),
                EngineEvent::ReplicaChanged { .. }
                | EngineEvent::StatusChanged(_)
                | EngineEvent::NeedsUser { .. } => {}
            }
        }
    }

    /// The host signals the working set (e.g. after a network change) without a new commit.
    pub fn signal_working_set(&mut self) {
        self.ws.signalled = true;
    }

    /// fileproviderd enumerates the working set right now, ahead of anything queued (e.g. a
    /// signal handled while a failed write backs off; the order of the two is not measured).
    pub fn enumerate_working_set_now(&mut self) {
        self.drain_events();
        self.enumerate_changes();
    }

    /// Hand exactly one due queued write or delete to the provider, nothing else (no signal
    /// handling): for scenarios that need one precise interleaving. False = nothing was due.
    pub fn run_one_queued_op(&mut self) -> bool {
        self.run_one_due_op()
    }

    /// Run everything that is due, advancing virtual time by at most `budget` to reach scheduled
    /// retries. Returns when nothing is due within the budget.
    pub fn pump(&mut self, budget: Duration) -> PumpReport {
        let deadline = self.now + budget;
        let mut report = PumpReport::default();
        loop {
            report.iterations += 1;
            if report.iterations > self.cfg.max_iterations {
                report.livelock = true;
                break;
            }
            self.drain_events();
            if let Some(below) = self.reimports.pop() {
                self.reimport(below);
                continue;
            }
            // Queued writes first: after signalErrorResolved the write arrives before the
            // working-set enumeration (MQ-037 measured it 20 ms after the signal).
            if self.run_one_due_op() {
                continue;
            }
            if self.ws.signalled && self.now >= self.ws.next_allowed {
                self.enumerate_changes();
                continue;
            }
            if self.trash.looping && self.now >= self.trash.next_at {
                self.stats.trash_asks += 1;
                self.trash.next_at = self.now + Duration::from_secs(1);
                continue;
            }
            self.report_materialized();
            match self.next_wakeup() {
                Some(t) if t <= deadline => self.now = self.now.max(t),
                _ => break,
            }
        }
        report.pending = self.pending();
        report.ws_pending = self.ws.signalled;
        report
    }

    fn next_wakeup(&self) -> Option<Duration> {
        let mut t: Option<Duration> = None;
        let mut take = |x: Duration| t = Some(t.map_or(x, |y: Duration| y.min(x)));
        for (k, r) in &self.retries {
            if self.disk.get(*k).is_some_and(|n| n.has_pending()) && self.op_ready(*k) {
                take(r.next_at);
            }
        }
        for d in &self.deletes {
            take(d.retry.next_at);
        }
        if self.ws.signalled {
            take(self.ws.next_allowed);
        }
        if self.trash.looping {
            take(self.trash.next_at);
        }
        t
    }

    // ---- working set ------------------------------------------------------------------------

    fn enumerate_changes(&mut self) {
        if self.anchor.is_none() && self.refresh_anchor().is_err() {
            self.ws_failed();
            return;
        }
        loop {
            let held = self.anchor.clone().unwrap_or_default();
            let req = IpcRequest::ChangesSince {
                anchor: held.clone(),
                limit: self.cfg.changes_limit,
            };
            match self.call(req, None) {
                Ok(IpcResponse::Changes {
                    updated,
                    removed,
                    anchor,
                    more,
                }) => {
                    self.ws.failures = 0;
                    if updated.is_empty() && removed.is_empty() && anchor == held {
                        // MQ-004: an empty set at the anchor we hold means "up to date"; anything
                        // committed later is only seen after the next signal.
                        self.ws.signalled = false;
                        return;
                    }
                    // Removals first: a name freed by a removal can be taken by an update in
                    // the same set without a transient collision (unmeasured; lenient order).
                    for id in removed {
                        self.apply_removal(id);
                    }
                    for it in &updated {
                        self.apply_item(it);
                    }
                    self.settle_bounces();
                    self.anchor = Some(anchor);
                    self.change_sets += 1;
                    if !more {
                        self.ws.signalled = false;
                        self.report_materialized();
                        return;
                    }
                }
                Ok(IpcResponse::Error {
                    code: ErrorCode::AnchorExpired,
                    ..
                }) => {
                    // MQ-006: re-ask from a fresh anchor; the working set is NOT re-enumerated.
                    self.stats.anchor_expired += 1;
                    match self.refresh_anchor() {
                        Ok(()) => self.ws.signalled = false,
                        Err(_) => self.ws_failed(),
                    }
                    return;
                }
                Ok(IpcResponse::Error { .. }) | Err(_) => {
                    self.ws_failed();
                    return;
                }
                Ok(other) => {
                    self.violation(format!("ChangesSince answered with {other:?}"));
                    self.ws_failed();
                    return;
                }
            }
        }
    }

    fn ws_failed(&mut self) {
        // MQ-005: consecutive failures throttle the enumeration; signals do not bypass it.
        self.stats.ws_failures += 1;
        self.ws.failures += 1;
        self.ws.signalled = true;
        self.ws.next_allowed = self.now + ws_throttle(self.ws.failures);
    }

    /// Apply one item the provider reported (enumeration page, change set, or reply).
    fn apply_item(&mut self, it: &IpcItem) {
        let e = &it.entry;
        if e.id == ItemId::ROOT {
            let root = self.disk.root();
            if let Some(n) = self.disk.get_mut(root) {
                n.version = Some(e.version);
                n.caps = it.caps;
            }
            return;
        }
        let parent_key = self
            .disk
            .by_id(e.parent)
            .filter(|k| self.disk.get(*k).is_some_and(Node::is_dir));
        let Some(k) = self.disk.by_id(e.id) else {
            // New ancestors reported through the working set are not ingested (MQ-029).
            let Some(pk) = parent_key else { return };
            let mut n = self.disk.blank(Some(pk), it.display_name.clone(), e.kind);
            n.id = Some(e.id);
            fill_from_item(&mut n, it, true);
            self.insert_bouncing(n);
            return;
        };
        let Some(node) = self.disk.get(k).cloned() else {
            return;
        };
        if node.kind != e.kind {
            self.violation(format!(
                "item {} changed kind {:?} -> {:?}",
                e.id, node.kind, e.kind
            ));
            let gone = self.disk.remove_subtree(k);
            self.forget_retries(&gone);
            self.apply_item(it);
            return;
        }
        if node.dirty & (fields::FILENAME | fields::PARENT) == 0 {
            match parent_key {
                None => {
                    // Moved under a container the system has never seen: gone from here.
                    if !self.subtree_has_pending(k) {
                        let gone = self.disk.remove_subtree(k);
                        self.forget_retries(&gone);
                        if let Some(p) = node.parent {
                            self.cascade_empty(p);
                        }
                    }
                    return;
                }
                Some(pk) => {
                    // Still colliding with the item it was bounced away from: keep the bounce
                    // name rather than bouncing again (no flapping).
                    let keep_bounce = node.bounced_from.as_deref()
                        == Some(it.display_name.as_str())
                        && node.parent == Some(pk)
                        && self
                            .disk
                            .lookup(pk, &it.display_name)
                            .is_some_and(|o| o != k);
                    if !keep_bounce && (node.parent != Some(pk) || node.name != it.display_name) {
                        let old_parent = node.parent;
                        self.place(k, pk, &it.display_name);
                        if let Some(p) = old_parent.filter(|p| *p != pk) {
                            self.cascade_empty(p);
                        }
                    } else if !keep_bounce && node.bounced_from.is_some() {
                        // Already where and as the provider says (a local bounce that matches
                        // the provider's name, MQ-016): settled.
                        self.settle_bounce_of(k);
                    }
                }
            }
        } else if node.dirty & fields::PARENT == 0 {
            // Only the filename is pending locally: merge field-wise — the server's move to
            // another folder applies, the local name stays pending (unmeasured; see TESTING.md).
            if let Some(pk) = parent_key.filter(|pk| node.parent != Some(*pk)) {
                let old_parent = node.parent;
                self.place(k, pk, &node.name);
                if let Some(p) = old_parent {
                    self.cascade_empty(p);
                }
            }
        }
        let Some(n) = self.disk.get_mut(k) else {
            return;
        };
        if n.dirty & fields::CONTENTS == 0 && !n.pending_create {
            let content_changed = n.version.map(|v| v.content) != Some(e.version.content);
            if content_changed && n.content.is_some() {
                // Root policy .downloadLazilyAndEvictOnRemoteUpdate (D9): drop local bytes.
                n.content = None;
                self.stats.evictions_on_update += 1;
            }
        }
        fill_from_item(n, it, false);
    }

    /// Move `k` to `(parent, name)`; on a case/normalization collision the *older* item is
    /// renamed locally with no provider call (MQ-016).
    fn place(&mut self, k: NodeKey, parent: NodeKey, name: &str) {
        match self.disk.rename(k, parent, name) {
            Ok(()) => {
                if let Some(n) = self.disk.get_mut(k) {
                    n.bounced_from = None;
                }
            }
            Err(NsError::Collision(other)) => {
                let k_born = self.disk.get(k).map_or(0, |n| n.born);
                let o_born = self.disk.get(other).map_or(0, |n| n.born);
                if o_born < k_born {
                    self.bounce(other);
                    if self.disk.rename(k, parent, name).is_err() {
                        self.bounce_into(k, parent, name);
                    }
                } else {
                    self.bounce_into(k, parent, name);
                }
            }
            Err(e) => self.violation(format!("cannot place node {k} as {name:?}: {e}")),
        }
    }

    fn insert_bouncing(&mut self, n: Node) {
        let name = n.name.clone();
        let parent = n.parent;
        match self.disk.insert(n.clone()) {
            Ok(_) => {}
            Err(NsError::Collision(other)) => {
                // The existing item is the older one (MQ-016).
                self.bounce(other);
                if let Err(e) = self.disk.insert(n) {
                    self.violation(format!(
                        "insert {name:?} under {parent:?} failed after bounce: {e}"
                    ));
                }
            }
            Err(e) => self.violation(format!("insert {name:?} under {parent:?} failed: {e}")),
        }
    }

    /// Rename `k` in place to the first free `stem N.ext`.
    fn bounce(&mut self, k: NodeKey) {
        let Some(n) = self.disk.get(k) else { return };
        // Bounce relative to the provider's name, not to an earlier bounce name.
        let wanted = n.bounced_from.clone().unwrap_or_else(|| n.name.clone());
        let parent = n.parent.unwrap_or(self.disk.root());
        self.bounce_into(k, parent, &wanted);
    }

    fn bounce_into(&mut self, k: NodeKey, parent: NodeKey, wanted: &str) {
        self.stats.bounces += 1;
        // A bounce must vacate the node's current name (someone else needs it).
        let current = self.disk.get(k).map(|n| fold(&n.name));
        for i in 2..10_000 {
            let cand = bounce_name(wanted, i);
            if current.as_deref() == Some(fold(&cand).as_str()) {
                continue;
            }
            let free = self.disk.lookup(parent, &cand).is_none_or(|o| o == k);
            if free && self.disk.rename(k, parent, &cand).is_ok() {
                if let Some(n) = self.disk.get_mut(k) {
                    n.bounced_from = Some(wanted.to_string());
                }
                return;
            }
        }
        self.violation(format!("no free bounce name for {wanted:?}"));
    }

    /// The provider's name for `k` is the one it has locally: no bounce to undo any more.
    fn settle_bounce_of(&mut self, k: NodeKey) {
        if let Some(n) = self.disk.get_mut(k) {
            n.bounced_from = None;
        }
    }

    /// After a change set or page is applied: items bounced by a collision that no longer
    /// exists take the provider's name again (the system keeps a bounce only while it must).
    fn settle_bounces(&mut self) {
        let bounced: Vec<(NodeKey, NodeKey, String)> = self
            .disk
            .all()
            .into_iter()
            .filter_map(|k| {
                let n = self.disk.get(k)?;
                Some((k, n.parent?, n.bounced_from.clone()?))
            })
            .collect();
        for (k, parent, want) in bounced {
            if self.disk.lookup(parent, &want).is_none_or(|o| o == k)
                && self.disk.rename(k, parent, &want).is_ok()
            {
                if let Some(n) = self.disk.get_mut(k) {
                    n.bounced_from = None;
                }
            }
        }
    }

    fn apply_removal(&mut self, id: ItemId) {
        if id == ItemId::ROOT {
            self.violation("provider reported the root removed".into());
            return;
        }
        if let Some(k) = self.disk.by_id(id) {
            self.remove_or_keep(k);
        }
    }

    fn remove_or_keep(&mut self, k: NodeKey) {
        let Some(node) = self.disk.get(k).cloned() else {
            return;
        };
        if node.kind != Kind::Dir && node.dirty & fields::CONTENTS != 0 {
            // MQ-080: the local edit survives and is re-offered as a createItem.
            self.convert_to_create(k, true);
            return;
        }
        if node.is_dir() && self.disk.has_children(k) {
            // A directory stays until its children are deleted. If what keeps it is local work,
            // it has to be re-created for that work to land.
            if self.subtree_has_pending(k) {
                self.convert_to_create(k, true);
            } else if let Some(n) = self.disk.get_mut(k) {
                n.remove_when_empty = true;
            }
            return;
        }
        let gone = self.disk.remove_subtree(k);
        self.forget_retries(&gone);
        if let Some(p) = node.parent {
            self.cascade_empty(p);
        }
    }

    fn cascade_empty(&mut self, mut key: NodeKey) {
        while let Some(n) = self.disk.get(key) {
            if !n.remove_when_empty || self.disk.has_children(key) {
                return;
            }
            let parent = n.parent;
            let gone = self.disk.remove_subtree(key);
            self.forget_retries(&gone);
            match parent {
                Some(p) => key = p,
                None => return,
            }
        }
    }

    fn convert_to_create(&mut self, k: NodeKey, deletion_conflicted: bool) {
        let template = self.next_template();
        let seq = self.touch_seq();
        self.disk.set_id(k, None);
        if let Some(n) = self.disk.get_mut(k) {
            n.pending_create = true;
            n.deletion_conflicted = deletion_conflicted;
            n.template_id = template;
            n.version = None;
            n.dirty = 0;
            n.remove_when_empty = false;
            n.dirty_seq = seq;
        }
        self.retries.insert(
            k,
            Retry {
                attempts: 0,
                next_at: self.now,
            },
        );
    }

    fn subtree_has_pending(&self, k: NodeKey) -> bool {
        std::iter::once(k).chain(self.disk.descendants(k)).any(|d| {
            self.disk
                .get(d)
                .is_some_and(|n| n.pending_create || n.dirty & fields::CONTENTS != 0)
        })
    }

    fn forget_retries(&mut self, gone: &[Node]) {
        for n in gone {
            self.retries.remove(&n.key);
        }
    }

    // ---- container enumeration --------------------------------------------------------------

    /// `enumerateItems(for: container)` — once per container, ever (MQ-001).
    fn ensure_enumerated(&mut self, key: NodeKey) -> Result<(), ActionError> {
        let Some(node) = self.disk.get(key) else {
            return Err(ActionError::NotFound(format!("node {key}")));
        };
        if node.enumerated {
            return Ok(());
        }
        let Some(id) = node.id else {
            // Created locally and not uploaded yet: its (empty) listing is known.
            return Ok(());
        };
        let mut cursor: Option<Vec<u8>> = None;
        let mut listed: HashSet<ItemId> = HashSet::new();
        loop {
            let req = IpcRequest::Enumerate {
                container: id,
                cursor: cursor.clone(),
                limit: self.cfg.page_limit,
                viewer: true,
            };
            match self.call(req, None) {
                Ok(IpcResponse::Page { items, next }) => {
                    for it in &items {
                        if it.entry.parent != id {
                            self.violation(format!(
                                "enumerate {id}: item {} has parent {}",
                                it.entry.id, it.entry.parent
                            ));
                        }
                        listed.insert(it.entry.id);
                        self.apply_item(it);
                    }
                    self.settle_bounces();
                    match next {
                        Some(c) if Some(&c) == cursor.as_ref() => {
                            self.violation(format!("enumerate {id}: cursor did not advance"));
                            break;
                        }
                        Some(c) => cursor = Some(c),
                        None => break,
                    }
                }
                Ok(IpcResponse::Error { code, .. }) => return Err(ActionError::Fetch(code)),
                Ok(other) => {
                    self.violation(format!("Enumerate answered with {other:?}"));
                    return Err(ActionError::Io("bad enumerate reply".into()));
                }
                Err(e) => return Err(ActionError::Io(e.to_string())),
            }
        }
        // The listing is authoritative for this container: known children it does not contain
        // (and that carry no local work) are gone.
        for c in self.disk.children(key) {
            let Some(n) = self.disk.get(c) else { continue };
            if let Some(cid) = n.id {
                if !listed.contains(&cid) && !n.local_only && !self.subtree_has_pending(c) {
                    let gone = self.disk.remove_subtree(c);
                    self.forget_retries(&gone);
                }
            }
        }
        if let Some(n) = self.disk.get_mut(key) {
            n.enumerated = true;
        }
        *self.stats.enumerations.entry(id).or_default() += 1;
        self.report_materialized();
        Ok(())
    }

    /// `materializedItemsDidChange` → the host walks `enumeratorForMaterializedItems()` and
    /// forwards the difference as `MaterializedChanged`.
    fn report_materialized(&mut self) {
        let mut now: BTreeSet<ItemId> = BTreeSet::new();
        for k in self.disk.all() {
            let Some(n) = self.disk.get(k) else { continue };
            let Some(id) = n.id else { continue };
            let materialized = match n.kind {
                Kind::Dir => n.enumerated,
                Kind::File | Kind::Symlink => {
                    n.content.is_some() || (n.kind == Kind::Symlink && !n.symlink_blocked)
                }
            };
            if materialized {
                now.insert(id);
            }
        }
        let (added, removed, full) = match &self.reported_m {
            None => (now.iter().copied().collect::<Vec<_>>(), Vec::new(), true),
            Some(prev) if *prev == now => return,
            Some(prev) => (
                now.difference(prev).copied().collect(),
                prev.difference(&now).copied().collect(),
                false,
            ),
        };
        match self.call(
            IpcRequest::MaterializedChanged {
                added,
                removed,
                full,
            },
            None,
        ) {
            Ok(IpcResponse::Ok) => self.reported_m = Some(now),
            Ok(IpcResponse::Error { .. }) | Err(_) => {}
            Ok(other) => self.violation(format!("MaterializedChanged answered with {other:?}")),
        }
    }

    /// `reimportItems(below:)`: re-enumerate every container the system had enumerated.
    fn reimport(&mut self, below: ItemId) {
        self.stats.reimports += 1;
        let Some(start) = self.disk.by_id(below) else {
            return;
        };
        let mut dirs: Vec<NodeKey> = std::iter::once(start)
            .chain(self.disk.descendants(start))
            .filter(|k| {
                self.disk
                    .get(*k)
                    .is_some_and(|n| n.is_dir() && n.enumerated)
            })
            .collect();
        // What the provider still knows below `start`: the fresh listings of the containers
        // the system had enumerated (it re-enumerates them anyway).
        let mut known: HashSet<ItemId> = HashSet::from([below]);
        let mut listed: HashSet<NodeKey> = HashSet::new();
        for &d in &dirs {
            let Some(id) = self.disk.get(d).and_then(|n| n.id) else {
                continue;
            };
            if let Some(ids) = self.list_ids(id) {
                known.extend(ids);
                listed.insert(d);
            }
        }
        // Local items under identifiers the provider no longer has (a previous index, rule 7):
        // the system re-offers those carrying local work — pending edits, pending creates
        // below them — as creates that may already exist (the engine matches them by path),
        // parents first; clean ones go (the fresh listings bring them back). Below a listed
        // container, or below a stale one (a stale folder's children are stale too).
        let mut stale: HashSet<NodeKey> = HashSet::new();
        for k in self.disk.descendants(start) {
            let Some(n) = self.disk.get(k) else { continue };
            let Some(id) = n.id else { continue };
            let parent = n.parent;
            let parent_judged = parent.is_some_and(|p| listed.contains(&p) || stale.contains(&p));
            if known.contains(&id) || !parent_judged {
                continue;
            }
            stale.insert(k);
            let has_work = self.subtree_has_pending(k)
                || self
                    .disk
                    .get(k)
                    .is_some_and(|n| n.dirty & fields::CONTENTS != 0);
            if has_work {
                self.convert_to_create(k, false);
                if let Some(n) = self.disk.get_mut(k) {
                    n.may_already_exist = true;
                }
            } else if !self.disk.get(k).is_some_and(|n| n.local_only) {
                let gone = self.disk.remove_subtree(k);
                self.forget_retries(&gone);
            }
        }
        // The system sends those before it ingests the fresh listings (unmeasured), so the
        // provider's answers name the re-offered items and nothing is bounced twice.
        for _ in 0..10_000 {
            if !self.run_one_due_op() {
                break;
            }
        }
        for k in &dirs {
            if let Some(n) = self.disk.get_mut(*k) {
                n.enumerated = false;
            }
        }
        dirs.retain(|k| self.disk.get(*k).is_some());
        for k in dirs {
            if self.disk.get(k).is_some() {
                let _ = self.ensure_enumerated(k);
            }
        }
        if self.refresh_anchor().is_err() {
            self.ws.signalled = true;
        }
    }

    /// The ids of a container's current listing (every page), without applying them.
    fn list_ids(&mut self, container: ItemId) -> Option<HashSet<ItemId>> {
        let mut ids = HashSet::new();
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            let req = IpcRequest::Enumerate {
                container,
                cursor: cursor.clone(),
                limit: self.cfg.page_limit,
                viewer: false,
            };
            match self.call(req, None) {
                Ok(IpcResponse::Page { items, next }) => {
                    ids.extend(items.iter().map(|it| it.entry.id));
                    match next {
                        Some(c) if Some(&c) != cursor.as_ref() => cursor = Some(c),
                        _ => return Some(ids),
                    }
                }
                _ => return None,
            }
        }
    }

    // ---- uploads ----------------------------------------------------------------------------

    /// Dependencies of a node's pending op are satisfied (parent exists on the provider side).
    fn op_ready(&self, k: NodeKey) -> bool {
        let Some(n) = self.disk.get(k) else {
            return false;
        };
        let parent_ok = n
            .parent
            .and_then(|p| self.disk.get(p))
            .is_some_and(|p| p.id.is_some());
        if n.pending_create {
            return parent_ok;
        }
        if n.dirty & fields::PARENT != 0 {
            return parent_ok && n.id.is_some();
        }
        n.id.is_some()
    }

    fn run_one_due_op(&mut self) -> bool {
        enum Work {
            Node(NodeKey),
            Delete(usize),
        }
        let mut best: Option<(u64, Work)> = None;
        for k in self.disk.all() {
            let Some(n) = self.disk.get(k) else { continue };
            if !n.has_pending() || !self.op_ready(k) {
                continue;
            }
            let due = self.retries.get(&k).is_none_or(|r| r.next_at <= self.now);
            if due && best.as_ref().is_none_or(|(s, _)| n.dirty_seq < *s) {
                best = Some((n.dirty_seq, Work::Node(k)));
            }
        }
        for (i, d) in self.deletes.iter().enumerate() {
            if d.retry.next_at <= self.now && best.as_ref().is_none_or(|(s, _)| d.seq < *s) {
                best = Some((d.seq, Work::Delete(i)));
            }
        }
        match best {
            Some((_, Work::Node(k))) => {
                self.sync_node(k);
                true
            }
            Some((_, Work::Delete(i))) => {
                self.sync_delete(i);
                true
            }
            None => false,
        }
    }

    fn schedule_retry(&mut self, k: NodeKey, collision: bool) {
        let now = self.now;
        let r = self.retries.entry(k).or_default();
        r.attempts += 1;
        r.next_at = now
            + if collision {
                collision_retry_delay(r.attempts)
            } else {
                write_retry_delay(r.attempts)
            };
    }

    fn sync_node(&mut self, k: NodeKey) {
        let Some(node) = self.disk.get(k).cloned() else {
            return;
        };
        let parent_id = node
            .parent
            .and_then(|p| self.disk.get(p))
            .and_then(|p| p.id);
        if node.pending_create {
            let Some(parent) = parent_id else { return };
            let kind = match node.kind {
                Kind::File => unlatch_proto::ipc::CreateKind::File,
                Kind::Dir => unlatch_proto::ipc::CreateKind::Dir,
                Kind::Symlink => unlatch_proto::ipc::CreateKind::Symlink,
            };
            let content = if node.kind == Kind::File {
                Some(node.content.clone().unwrap_or_default())
            } else {
                None
            };
            let fd = match content.as_deref().map(|c| self.content_fd(c)).transpose() {
                Ok(fd) => fd,
                Err(e) => {
                    self.violation(format!("staging content for create failed: {e}"));
                    self.schedule_retry(k, false);
                    return;
                }
            };
            let sent_fields = fields::CONTENTS
                | fields::CONTENT_MODIFICATION_DATE
                | fields::FILENAME
                | fields::PARENT;
            let req = IpcRequest::Create {
                template_id: node.template_id.clone(),
                parent,
                name: node.name.clone(),
                kind,
                has_content: fd.is_some(),
                symlink_target: node.symlink_target.clone(),
                mtime_ns: Some(node.mtime_ns),
                user_exec: Some(node.user_exec),
                changed_fields: sent_fields,
                local: node.local.clone(),
                may_already_exist: node.may_already_exist,
                deletion_conflicted: node.deletion_conflicted,
            };
            self.names_sent.insert(node.name.clone());
            match self.call(req, fd) {
                Ok(IpcResponse::Done {
                    item,
                    still_pending,
                    should_fetch_content,
                    conflict_copy,
                }) => {
                    self.done(
                        k,
                        &item,
                        still_pending,
                        should_fetch_content,
                        conflict_copy.is_some(),
                        u32::MAX,
                    );
                    // A new folder is materialized from birth: tell the host right away.
                    self.report_materialized();
                }
                Ok(IpcResponse::Error {
                    code: ErrorCode::Exists,
                    ..
                }) => {
                    // MQ-014: filenameCollision is retried for ever; the user was told it worked.
                    self.schedule_retry(k, true)
                }
                Ok(IpcResponse::Error { code, .. }) => self.upload_error(k, code),
                Ok(other) => {
                    self.violation(format!("Create answered with {other:?}"));
                    self.schedule_retry(k, false)
                }
                Err(_) => self.schedule_retry(k, false),
            }
            return;
        }
        if node.dirty == 0 {
            return;
        }
        let Some(id) = node.id else { return };
        let dirty = node.dirty;
        let has_content = dirty & fields::CONTENTS != 0 && node.kind == Kind::File;
        let fd = if has_content {
            match self.content_fd(node.content.as_deref().unwrap_or_default()) {
                Ok(fd) => Some(fd),
                Err(e) => {
                    self.violation(format!("staging content for modify failed: {e}"));
                    self.schedule_retry(k, false);
                    return;
                }
            }
        } else {
            None
        };
        let req = IpcRequest::Modify {
            id,
            base: node.version.map(BaseVersion::from).unwrap_or_default(),
            changed_fields: dirty,
            new_parent: (dirty & fields::PARENT != 0).then_some(parent_id).flatten(),
            new_name: (dirty & fields::FILENAME != 0).then(|| node.name.clone()),
            has_content: fd.is_some(),
            mtime_ns: (dirty & fields::CONTENT_MODIFICATION_DATE != 0).then_some(node.mtime_ns),
            user_exec: (dirty & fields::FILE_SYSTEM_FLAGS != 0).then_some(node.user_exec),
            local: node.local.clone(),
        };
        if dirty & fields::FILENAME != 0 {
            self.names_sent.insert(node.name.clone());
        }
        match self.call(req, fd) {
            Ok(IpcResponse::Done {
                item,
                still_pending,
                should_fetch_content,
                conflict_copy,
            }) => {
                if item.entry.id != id {
                    self.violation(format!(
                        "modify of {id} answered for item {}",
                        item.entry.id
                    ));
                }
                self.done(
                    k,
                    &item,
                    still_pending,
                    should_fetch_content,
                    conflict_copy.is_some(),
                    dirty,
                )
            }
            Ok(IpcResponse::Error {
                code: ErrorCode::NotFound,
                ..
            }) => {
                // The item is gone on the provider side. A content edit is kept and re-offered as
                // a create (MQ-080); a metadata-only change dies with the item.
                if has_content {
                    self.convert_to_create(k, true);
                } else {
                    let parent = node.parent;
                    let gone = self.disk.remove_subtree(k);
                    self.forget_retries(&gone);
                    if let Some(p) = parent {
                        self.cascade_empty(p);
                    }
                }
            }
            Ok(IpcResponse::Error { code, .. }) => self.upload_error(k, code),
            Ok(other) => {
                self.violation(format!("Modify answered with {other:?}"));
                self.schedule_retry(k, false)
            }
            Err(_) => self.schedule_retry(k, false),
        }
    }

    fn upload_error(&mut self, k: NodeKey, code: ErrorCode) {
        match code {
            ErrorCode::ExcludedFromSync => {
                if let Some(n) = self.disk.get_mut(k) {
                    n.local_only = true;
                    n.pending_create = false;
                    n.dirty = 0;
                }
                self.retries.remove(&k);
            }
            // `.cannotSynchronize`: kept locally with an uploading error, not retried (MQ-078).
            ErrorCode::CannotSync | ErrorCode::Permission | ErrorCode::InvalidName => {
                if let Some(n) = self.disk.get_mut(k) {
                    n.sync_error = Some(code);
                }
                self.retries.remove(&k);
            }
            // Everything else — including serverUnreachable — is re-offered for ever (MQ-035).
            _ => self.schedule_retry(k, false),
        }
    }

    /// A create/modify reply. `sent` = the fields that were in flight (`u32::MAX` for a create).
    fn done(
        &mut self,
        k: NodeKey,
        item: &IpcItem,
        still_pending: u32,
        should_fetch: bool,
        conflict: bool,
        sent: u32,
    ) {
        self.retries.remove(&k);
        if conflict {
            self.stats.conflict_replies += 1;
        }
        if still_pending != 0 {
            self.stats.still_pending += 1;
        }
        let e = &item.entry;
        if let Some(other) = self.disk.by_id(e.id).filter(|o| *o != k) {
            if sent != u32::MAX {
                // A modify answered for an item another node holds: nothing sane to merge.
                self.violation(format!(
                    "modify reply for node {k} names item {} held by node {other}",
                    e.id
                ));
                if let Some(n) = self.disk.get_mut(k) {
                    n.dirty &= !sent;
                }
                return;
            }
            // A replayed create (lost reply) whose item the working set delivered meanwhile: the
            // item database is keyed by identifier, so the two nodes are one item. The mirror
            // wins when the user changed it since (newer intent); the create's bytes already
            // landed as that item.
            self.stats.replay_merges += 1;
            let (survivor, loser) = if self.subtree_has_pending(other) {
                (other, k)
            } else {
                (k, other)
            };
            let enumerated = self.disk.get(loser).is_some_and(|n| n.enumerated);
            for c in self.disk.children(loser) {
                let name = self.disk.get(c).map(|n| n.name.clone()).unwrap_or_default();
                if self.disk.rename(c, survivor, &name).is_err() {
                    self.bounce_into(c, survivor, &name);
                }
            }
            let parent = self.disk.get(loser).and_then(|n| n.parent);
            let gone = self.disk.remove_subtree(loser);
            self.forget_retries(&gone);
            if let Some(n) = self.disk.get_mut(survivor) {
                // The merged item's listing is known only if the server-side copy's was: a
                // folder created locally "knows" it is empty, but the agent may have filled the
                // server copy since.
                n.enumerated = if survivor == k {
                    enumerated
                } else {
                    n.enumerated || enumerated
                };
            }
            if let Some(p) = parent {
                self.cascade_empty(p);
            }
            if survivor == other {
                return;
            }
        }
        self.disk.set_id(k, Some(e.id));
        let parent_key = self.disk.by_id(e.parent);
        if let Some(n) = self.disk.get_mut(k) {
            n.pending_create = false;
            n.deletion_conflicted = false;
            n.may_already_exist = false;
            n.dirty &= !sent;
            // MQ-013: whatever version the reply carries is believed, with the local bytes.
            n.version = Some(e.version);
            if should_fetch {
                n.content = None;
                n.size = e.size;
            } else if let Some(c) = &n.content {
                n.size = c.len() as u64;
            }
            n.caps = item.caps;
            n.symlink_blocked = item.symlink_blocked;
            n.local = item.local.clone();
            n.server_name = Some(item.display_name.clone());
        }
        // The system adopts the filename/parent the reply carries (rule 2 `name 2.ext`).
        if let (Some(pk), Some(n)) = (parent_key, self.disk.get(k)) {
            if n.parent.is_some() && (n.parent != Some(pk) || n.name != item.display_name) {
                self.place(k, pk, &item.display_name);
            } else if n.parent == Some(pk) {
                // The provider named the item exactly as it stands locally — also when that is
                // a name a local bounce gave it (MQ-016): the two agree, there is no bounce
                // left to undo.
                self.settle_bounce_of(k);
            }
        }
    }

    fn sync_delete(&mut self, i: usize) {
        let d = self.deletes[i].clone();
        let req = IpcRequest::Delete {
            id: d.id,
            base: d.base.into(),
            recursive: d.recursive,
        };
        match self.call(req, None) {
            Ok(IpcResponse::Deleted)
            | Ok(IpcResponse::Error {
                code: ErrorCode::NotFound,
                ..
            }) => {
                self.deletes.remove(i);
            }
            Ok(IpcResponse::Error {
                code: ErrorCode::DeletionRejected,
                current,
                ..
            }) => {
                // fileProviderErrorForRejectedDeletion: the item comes back as it is now.
                self.stats.deletion_rejected += 1;
                self.deletes.remove(i);
                match current {
                    Some(item) => self.restore(&item),
                    None => {
                        self.violation(format!(
                            "DeletionRejected for {} without current item",
                            d.id
                        ));
                        let _ = d.parent;
                    }
                }
            }
            Ok(IpcResponse::Error {
                code: ErrorCode::CannotSync,
                ..
            }) => {
                self.deletes.remove(i);
            }
            Ok(IpcResponse::Error { .. }) | Err(_) => {
                let now = self.now;
                let r = &mut self.deletes[i].retry;
                r.attempts += 1;
                r.next_at = now + write_retry_delay(r.attempts);
            }
            Ok(other) => {
                self.violation(format!("Delete answered with {other:?}"));
                self.deletes.remove(i);
            }
        }
    }

    fn restore(&mut self, item: &IpcItem) {
        self.apply_item(item);
        let Some(k) = self.disk.by_id(item.entry.id) else {
            return;
        };
        if self.disk.get(k).is_some_and(Node::is_dir) {
            if let Some(n) = self.disk.get_mut(k) {
                n.enumerated = false;
            }
            let _ = self.ensure_enumerated(k);
        }
    }

    // ---- user actions -----------------------------------------------------------------------

    fn resolve_path(&self, path: &str) -> Result<NodeKey, ActionError> {
        self.disk
            .resolve(path)
            .ok_or_else(|| ActionError::NotFound(path.to_string()))
    }

    /// Open a Finder window on `path`: enumerate every not-yet-enumerated folder on the way.
    pub fn browse(&mut self, path: &str) -> Result<(), ActionError> {
        let root = self.disk.root();
        self.ensure_enumerated(root)?;
        let mut cur = root;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = self
                .disk
                .lookup(cur, comp)
                .ok_or_else(|| ActionError::NotFound(path.to_string()))?;
            match self.disk.get(cur).map(|n| n.kind) {
                Some(Kind::Dir) => self.ensure_enumerated(cur)?,
                _ => return Err(ActionError::NotADirectory(path.to_string())),
            }
        }
        Ok(())
    }

    /// Browse every folder the system knows about (repeat until no new folders appear).
    pub fn browse_all(&mut self) -> Vec<(String, ActionError)> {
        let mut errors = Vec::new();
        let mut tried: HashSet<NodeKey> = HashSet::new();
        loop {
            let todo: Vec<NodeKey> = self
                .disk
                .all()
                .into_iter()
                .filter(|k| {
                    !tried.contains(k)
                        && self
                            .disk
                            .get(*k)
                            .is_some_and(|n| n.is_dir() && !n.enumerated && n.id.is_some())
                })
                .collect();
            if todo.is_empty() {
                return errors;
            }
            for k in todo {
                tried.insert(k);
                if self.disk.get(k).is_none() {
                    continue;
                }
                if let Err(e) = self.ensure_enumerated(k) {
                    errors.push((self.disk.path_of(k), e));
                }
            }
        }
    }

    /// Read a file (double-click / `cat`). Dataless files are looked up (`item(for:)`) and then
    /// fetched; a failed fetch is reported and never retried (MQ-036).
    pub fn open(&mut self, path: &str) -> Result<Vec<u8>, ActionError> {
        let k = self.resolve_path(path)?;
        self.open_key(k, path)
    }

    fn open_key(&mut self, k: NodeKey, path: &str) -> Result<Vec<u8>, ActionError> {
        let node = self
            .disk
            .get(k)
            .cloned()
            .ok_or_else(|| ActionError::NotFound(path.to_string()))?;
        match node.kind {
            Kind::Dir => return Err(ActionError::IsADirectory(path.to_string())),
            Kind::Symlink if !node.symlink_blocked => {
                return Ok(node.symlink_target.unwrap_or_default().into_bytes());
            }
            _ => {}
        }
        if let Some(c) = node.content {
            return Ok(c);
        }
        let Some(id) = node.id else {
            return Ok(Vec::new());
        };
        match self.call(IpcRequest::Item { id }, None) {
            Ok(IpcResponse::Error {
                code: ErrorCode::NotFound,
                ..
            }) => {
                // MQ-011: noSuchItem from item(for:) = deleted; the system removes the file.
                self.stats.item_not_found_deletes += 1;
                let parent = node.parent;
                let gone = self.disk.remove_subtree(k);
                self.forget_retries(&gone);
                if let Some(p) = parent {
                    self.cascade_empty(p);
                }
                return Err(ActionError::NotFound(path.to_string()));
            }
            Ok(IpcResponse::Item(it)) => {
                if it.entry.id != id {
                    self.violation(format!("item({id}) answered with item {}", it.entry.id));
                } else {
                    self.apply_item(&it);
                }
            }
            _ => {}
        }
        let Some(k) = self.disk.by_id(id) else {
            return Err(ActionError::NotFound(path.to_string()));
        };
        let version = self.disk.get(k).and_then(|n| n.version).map(|v| v.content);
        let dest = self.cfg.scratch.join("fetch");
        std::fs::create_dir_all(&dest).map_err(|e| ActionError::Io(e.to_string()))?;
        self.fetch_seq += 1;
        let req = IpcRequest::Fetch {
            id,
            version,
            dest_dir: dest.to_string_lossy().into_owned(),
        };
        match self.call(req, None) {
            Ok(IpcResponse::Fetched { path: p, item }) => {
                let p = PathBuf::from(p);
                if p.parent() != Some(dest.as_path()) {
                    self.violation(format!(
                        "fetch of {id} returned a path outside dest_dir: {}",
                        p.display()
                    ));
                }
                let mut bytes = Vec::new();
                std::fs::File::open(&p)
                    .and_then(|mut f| f.read_to_end(&mut bytes))
                    .map_err(|e| {
                        ActionError::Io(format!("reading fetched {}: {e}", p.display()))
                    })?;
                let _ = std::fs::remove_file(&p);
                if item.entry.size != bytes.len() as u64 {
                    self.violation(format!(
                        "fetch of {id}: item says {} bytes, file has {}",
                        item.entry.size,
                        bytes.len()
                    ));
                }
                if let Some(n) = self.disk.get_mut(k) {
                    if n.dirty & fields::CONTENTS == 0 {
                        n.content = Some(bytes.clone());
                        n.version = Some(item.entry.version);
                        n.size = bytes.len() as u64;
                    }
                }
                self.report_materialized();
                Ok(bytes)
            }
            Ok(IpcResponse::Error { code, .. }) => Err(ActionError::Fetch(code)),
            Ok(other) => {
                self.violation(format!("Fetch answered with {other:?}"));
                Err(ActionError::Io("bad fetch reply".into()))
            }
            Err(_) => Err(ActionError::Fetch(ErrorCode::Offline)),
        }
    }

    /// Finder honours the item's capabilities (0 = not reported yet: allow).
    fn require_cap(&self, k: NodeKey, bit: u32, what: &str) -> Result<(), ActionError> {
        match self.disk.get(k) {
            Some(n) if n.id.is_some() && n.caps != 0 && n.caps & bit == 0 => {
                Err(ActionError::Refused(format!(
                    "{what}: {} lacks capability {bit:#x}",
                    self.disk.path_of(k)
                )))
            }
            _ => Ok(()),
        }
    }

    fn mark_dirty(&mut self, k: NodeKey, bits: u32) {
        let seq = self.touch_seq();
        if let Some(n) = self.disk.get_mut(k) {
            if !n.pending_create {
                n.dirty |= bits;
            }
            n.dirty_seq = seq;
            n.sync_error = None;
        }
        // A new local change is offered right away, whatever the old backoff was.
        self.retries.insert(
            k,
            Retry {
                attempts: 0,
                next_at: self.now,
            },
        );
    }

    /// Write `bytes` into an existing file in place (the app materializes it first).
    pub fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), ActionError> {
        let k = self.resolve_path(path)?;
        match self.disk.get(k).map(|n| (n.kind, n.symlink_blocked)) {
            Some((Kind::Dir, _)) => return Err(ActionError::IsADirectory(path.to_string())),
            Some((Kind::Symlink, false)) => {
                return Err(ActionError::Refused(format!("{path} is a symlink")))
            }
            Some((_, true)) => return Err(ActionError::Refused(format!("{path} is read-only"))),
            _ => {}
        }
        self.require_cap(k, caps::WRITING, "write")?;
        if self
            .disk
            .get(k)
            .is_some_and(|n| n.content.is_none() && n.id.is_some())
        {
            self.open_key(k, path)?;
        }
        let k = self.resolve_path(path)?;
        let now_ns = self.mtime_now();
        if let Some(n) = self.disk.get_mut(k) {
            n.content = Some(bytes.to_vec());
            n.size = bytes.len() as u64;
            n.mtime_ns = now_ns;
        }
        self.mark_dirty(k, fields::CONTENTS | fields::CONTENT_MODIFICATION_DATE);
        Ok(())
    }

    /// Save like TextEdit (temp + rename over): the parent gets an mtime-only modify, then the
    /// file gets ONE modifyItem on its original identifier (MQ-049). A missing file is created.
    pub fn save_atomic(&mut self, path: &str, bytes: &[u8]) -> Result<(), ActionError> {
        let (dir, name) = split_path(path);
        let pk = self.resolve_path(dir)?;
        let Some(k) = self.disk.lookup(pk, name) else {
            self.create_file(dir, name, bytes)?;
            return Ok(());
        };
        if self
            .disk
            .get(k)
            .is_some_and(|n| n.kind != Kind::File || n.symlink_blocked)
        {
            return Err(ActionError::Refused(format!(
                "{path} is not a regular file"
            )));
        }
        self.require_cap(k, caps::WRITING, "save")?;
        self.require_cap(pk, caps::ADDING_SUB_ITEMS, "save")?;
        let now_ns = self.mtime_now();
        if let Some(p) = self.disk.get_mut(pk) {
            p.mtime_ns = now_ns;
        }
        if self.disk.get(pk).is_some_and(|p| p.id.is_some()) {
            self.mark_dirty(pk, fields::CONTENT_MODIFICATION_DATE);
        }
        if let Some(n) = self.disk.get_mut(k) {
            n.content = Some(bytes.to_vec());
            n.size = bytes.len() as u64;
            n.mtime_ns = now_ns;
        }
        self.mark_dirty(
            k,
            fields::CONTENTS
                | fields::LAST_USED_DATE
                | fields::CONTENT_MODIFICATION_DATE
                | fields::EXTENDED_ATTRIBUTES,
        );
        Ok(())
    }

    fn new_local(&mut self, dir: &str, name: &str, kind: Kind) -> Result<NodeKey, ActionError> {
        let pk = self.resolve_path(dir)?;
        if !self.disk.get(pk).is_some_and(Node::is_dir) {
            return Err(ActionError::NotADirectory(dir.to_string()));
        }
        self.require_cap(pk, caps::ADDING_SUB_ITEMS, "create")?;
        // MQ-015: collisions inside Finder never reach the provider — Finder picks "x copy".
        let mut final_name = name.to_string();
        let mut i = 1;
        while self.disk.lookup(pk, &final_name).is_some() {
            final_name = finder_copy_name(name, i);
            i += 1;
        }
        let template = self.next_template();
        let seq = self.touch_seq();
        let mtime = self.mtime_now();
        let mut n = self.disk.blank(Some(pk), final_name.clone(), kind);
        n.template_id = template;
        n.mtime_ns = mtime;
        n.dirty_seq = seq;
        // MQ-046: .DS_Store & co never reach the extension.
        if is_mac_local_name(&final_name) {
            n.local_only = true;
        } else {
            n.pending_create = true;
        }
        if kind == Kind::Dir {
            n.enumerated = true;
        }
        let k = self
            .disk
            .insert(n)
            .map_err(|e| ActionError::Collision(format!("{dir}/{final_name}: {e}")))?;
        self.retries.insert(
            k,
            Retry {
                attempts: 0,
                next_at: self.now,
            },
        );
        Ok(k)
    }

    /// Create a new file (save dialog / `echo > x`). Returns the local path it got.
    pub fn create_file(
        &mut self,
        dir: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<String, ActionError> {
        let k = self.new_local(dir, name, Kind::File)?;
        if let Some(n) = self.disk.get_mut(k) {
            n.content = Some(bytes.to_vec());
            n.size = bytes.len() as u64;
        }
        Ok(self.disk.path_of(k))
    }

    /// Drag files in from elsewhere (one create each).
    pub fn drag_in(
        &mut self,
        dir: &str,
        files: &[(String, Vec<u8>)],
    ) -> Result<Vec<String>, ActionError> {
        files
            .iter()
            .map(|(name, bytes)| self.create_file(dir, name, bytes))
            .collect()
    }

    pub fn mkdir(&mut self, dir: &str, name: &str) -> Result<String, ActionError> {
        let k = self.new_local(dir, name, Kind::Dir)?;
        Ok(self.disk.path_of(k))
    }

    /// `ln -s target name` inside the mount.
    pub fn symlink(&mut self, dir: &str, name: &str, target: &str) -> Result<String, ActionError> {
        let k = self.new_local(dir, name, Kind::Symlink)?;
        if let Some(n) = self.disk.get_mut(k) {
            n.symlink_target = Some(target.to_string());
            n.size = target.len() as u64;
        }
        Ok(self.disk.path_of(k))
    }

    /// Finder rename: one modifyItem with `.filename` (MQ-048).
    pub fn rename(&mut self, path: &str, new_name: &str) -> Result<(), ActionError> {
        let k = self.resolve_path(path)?;
        let pk = self
            .disk
            .get(k)
            .and_then(|n| n.parent)
            .ok_or_else(|| ActionError::Refused("root".into()))?;
        self.require_cap(k, caps::RENAMING, "rename")?;
        self.disk.rename(k, pk, new_name).map_err(|e| match e {
            NsError::Collision(_) => ActionError::Collision(new_name.to_string()),
            other => ActionError::Refused(other.to_string()),
        })?;
        if let Some(n) = self.disk.get_mut(k) {
            n.bounced_from = None;
        }
        self.mark_dirty(k, fields::FILENAME);
        Ok(())
    }

    /// Drag to another folder of the same domain: modifyItem with `.parentItemIdentifier`.
    pub fn move_to(&mut self, path: &str, dest_dir: &str) -> Result<(), ActionError> {
        let k = self.resolve_path(path)?;
        let dk = self.resolve_path(dest_dir)?;
        let name = self
            .disk
            .get(k)
            .map(|n| n.name.clone())
            .ok_or_else(|| ActionError::NotFound(path.into()))?;
        let old_parent = self.disk.get(k).and_then(|n| n.parent);
        if old_parent.is_none() {
            return Err(ActionError::Refused("cannot move the root".into()));
        }
        if old_parent == Some(dk) {
            return Ok(());
        }
        self.require_cap(k, caps::REPARENTING, "move")?;
        self.require_cap(dk, caps::ADDING_SUB_ITEMS, "move")?;
        self.disk.rename(k, dk, &name).map_err(|e| match e {
            NsError::Collision(_) => ActionError::Collision(format!("{dest_dir}/{name}")),
            NsError::NotADirectory => ActionError::NotADirectory(dest_dir.to_string()),
            other => ActionError::Refused(other.to_string()),
        })?;
        self.mark_dirty(k, fields::PARENT);
        if let Some(p) = old_parent {
            self.cascade_empty(p);
        }
        Ok(())
    }

    /// Delete (Finder "Delete Immediately…" or "Move to Trash" — both end in deleteItem because
    /// trash sync is off, MQ-058). A folder goes as one recursive delete.
    pub fn delete(&mut self, path: &str) -> Result<(), ActionError> {
        let k = self.resolve_path(path)?;
        let node = self
            .disk
            .get(k)
            .cloned()
            .ok_or_else(|| ActionError::NotFound(path.into()))?;
        if node.parent.is_none() {
            return Err(ActionError::Refused("cannot delete the domain root".into()));
        }
        self.require_cap(k, caps::DELETING, "delete")?;
        let gone = self.disk.remove_subtree(k);
        self.forget_retries(&gone);
        if let (Some(id), false) = (node.id, node.local_only) {
            let seq = self.touch_seq();
            let parent = node
                .parent
                .and_then(|p| self.disk.get(p))
                .and_then(|p| p.id)
                .unwrap_or(ItemId::ROOT);
            self.deletes.push(PendingDelete {
                id,
                base: node.version.unwrap_or_default(),
                recursive: node.is_dir(),
                seq,
                retry: Retry {
                    attempts: 0,
                    next_at: self.now,
                },
                parent,
            });
        }
        if let Some(p) = node.parent {
            self.cascade_empty(p);
        }
        Ok(())
    }

    /// `chmod u±x` in the mount: only owner-execute is carried; no change → no call (MQ-047).
    pub fn chmod_exec(&mut self, path: &str, exec: bool) -> Result<(), ActionError> {
        let k = self.resolve_path(path)?;
        if self.disk.get(k).is_some_and(|n| n.user_exec == exec) {
            return Ok(());
        }
        if let Some(n) = self.disk.get_mut(k) {
            n.user_exec = exec;
        }
        self.mark_dirty(k, fields::FILE_SYSTEM_FLAGS);
        Ok(())
    }

    /// Finder tags: one modifyItem with `.tagData` only (MQ-042).
    pub fn set_tags(&mut self, path: &str, tag_data: &[u8]) -> Result<(), ActionError> {
        let k = self.resolve_path(path)?;
        if let Some(n) = self.disk.get_mut(k) {
            n.local.tag_data = Some(tag_data.to_vec());
        }
        self.mark_dirty(k, fields::TAG_DATA);
        Ok(())
    }

    /// "Remove Download": make a materialized file dataless again (nothing pending on it).
    pub fn evict(&mut self, path: &str) -> Result<(), ActionError> {
        let k = self.resolve_path(path)?;
        match self.disk.get_mut(k) {
            Some(n) if n.has_pending() => {
                Err(ActionError::Refused(format!("{path} has unsynced edits")))
            }
            Some(n) if n.kind == Kind::File => {
                n.content = None;
                self.report_materialized();
                Ok(())
            }
            _ => Err(ActionError::Refused(format!("{path} is not a file"))),
        }
    }

    /// Menu-bar "apply" on a paused mass deletion (the host UI is another IPC client).
    pub fn confirm_paused(&mut self, apply: bool) -> Result<(), ActionError> {
        match self.call(IpcRequest::ConfirmPaused { apply }, None) {
            Ok(IpcResponse::Ok) => Ok(()),
            Ok(IpcResponse::Error { code, .. }) => Err(ActionError::Fetch(code)),
            Ok(other) => Err(ActionError::Io(format!(
                "ConfirmPaused answered with {other:?}"
            ))),
            Err(e) => Err(ActionError::Io(e.to_string())),
        }
    }

    /// Engine status (host UI).
    pub fn engine_status(&mut self) -> Option<unlatch_proto::ipc::EngineStatus> {
        match self.call(IpcRequest::Status, None) {
            Ok(IpcResponse::Status(s)) => Some(s),
            _ => None,
        }
    }

    fn mtime_now(&self) -> i64 {
        // Virtual clock on top of a fixed epoch keeps runs reproducible.
        1_790_000_000_000_000_000i64 + self.now.as_nanos() as i64
    }

    /// Every file the system shows, by local path (materialized content where present).
    pub fn visible(&self) -> Vec<VisibleItem> {
        let mut out = Vec::new();
        for k in self.disk.all() {
            let Some(n) = self.disk.get(k) else { continue };
            if n.parent.is_none() {
                continue;
            }
            out.push(VisibleItem {
                path: self.disk.path_of(k),
                kind: n.kind,
                size: n.size,
                content: n.content.clone(),
                symlink_target: n.symlink_target.clone(),
                symlink_blocked: n.symlink_blocked,
                enumerated: n.enumerated,
                local_only: n.local_only,
                pending: n.has_pending(),
                sync_error: n.sync_error.is_some(),
                parent_enumerated: n
                    .parent
                    .and_then(|p| self.disk.get(p))
                    .is_some_and(|p| p.enumerated),
                id: n.id,
            });
        }
        out
    }

    /// Paths of all non-dir items the system could open (for "open everything" sweeps).
    pub fn file_paths(&self) -> Vec<String> {
        self.visible()
            .into_iter()
            .filter(|v| v.kind != Kind::Dir && !v.local_only)
            .map(|v| v.path)
            .collect()
    }
}

/// One entry of the simulated Mac's visible tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleItem {
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub content: Option<Vec<u8>>,
    pub symlink_target: Option<String>,
    pub symlink_blocked: bool,
    pub enumerated: bool,
    pub local_only: bool,
    pub pending: bool,
    /// Upload failed permanently; the Mac legitimately diverges here (error badge).
    pub sync_error: bool,
    pub parent_enumerated: bool,
    pub id: Option<ItemId>,
}

fn fill_from_item(n: &mut Node, it: &IpcItem, fresh: bool) {
    let e = &it.entry;
    n.server_name = Some(it.display_name.clone());
    let content_pending = n.dirty & fields::CONTENTS != 0 || n.pending_create;
    if fresh || !content_pending {
        n.version = Some(e.version);
        n.size = e.size;
    }
    n.symlink_target = e.symlink_target.clone();
    n.symlink_blocked = it.symlink_blocked;
    n.caps = it.caps;
    if fresh || n.dirty & fields::FILE_SYSTEM_FLAGS == 0 {
        n.user_exec = it.user_exec;
    }
    if fresh || n.dirty & fields::CONTENT_MODIFICATION_DATE == 0 {
        n.mtime_ns = e.mtime_ns;
    }
    const LOCAL_FIELDS: u32 = fields::TAG_DATA
        | fields::LAST_USED_DATE
        | fields::FAVORITE_RANK
        | fields::CREATION_DATE
        | fields::EXTENDED_ATTRIBUTES
        | fields::TYPE_AND_CREATOR;
    if fresh || n.dirty & LOCAL_FIELDS == 0 {
        n.local = it.local.clone();
    }
    if n.local == LocalMeta::default() && fresh {
        n.local = it.local.clone();
    }
}

/// `"a/b/c"` → `("a/b", "c")`; `"c"` → `("", "c")`.
pub fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_curves_match_measurements() {
        // MQ-005: ~94 s after 7 failures, 2820 s ceiling reached by 27.
        let at7 = ws_throttle(7).as_secs_f64();
        assert!((90.0..100.0).contains(&at7), "{at7}");
        let at27 = ws_throttle(27).as_secs_f64();
        assert!((2500.0..=2820.0).contains(&at27), "{at27}");
        assert_eq!(ws_throttle(40).as_secs(), 2820);
        // MQ-035: 5.5 s, ~10.6 s, ~20.5 s … no ceiling within the measured range.
        assert_eq!(write_retry_delay(1).as_millis(), 5500);
        assert!((10.0..11.0).contains(&write_retry_delay(2).as_secs_f64()));
        assert!(write_retry_delay(7).as_secs_f64() > 250.0);
        // MQ-014: 0.04 s, 5 s, 15 s, then doubling.
        assert_eq!(collision_retry_delay(1).as_millis(), 40);
        assert_eq!(collision_retry_delay(2).as_secs(), 5);
        assert_eq!(collision_retry_delay(3).as_secs(), 15);
        assert_eq!(collision_retry_delay(4).as_secs(), 30);
    }

    #[test]
    fn split_paths() {
        assert_eq!(split_path("a/b/c"), ("a/b", "c"));
        assert_eq!(split_path("c"), ("", "c"));
    }
}
