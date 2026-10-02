//! The apply thread: the replica's single writer. Applies snapshot chunks, events, listings and
//! mutation results (LWW, rule 13), batches whatever is queued into one SQLite transaction,
//! emits `ReplicaChanged` (in-memory state, for FUSE) just before that commit, and only after
//! the commit publishes the new anchor and requests the coalesced `WorkingSetChanged`
//! (review §2(a)5). Also runs the mass-deletion guard (rule 8).

use super::replica::{now_secs, Db, DeleteSeen, Delta, Source, State};
use super::Shared;
use crate::{EngineEvent, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::mpsc as smpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};
use unlatch_proto::ipc::LocalMeta;
use unlatch_proto::wire::{Change, ServerInfo, WelcomeMode};
use unlatch_proto::{Entry, IndexId, ItemId};

const GC_EVERY: Duration = Duration::from_secs(3600);
/// Retry backoff for a replica transaction that failed (disk full, I/O error).
const RETRY_MIN: Duration = Duration::from_millis(50);
const RETRY_MAX: Duration = Duration::from_secs(5);
/// Commit early when one drain accumulates this many changes.
const MAX_BATCH_CHANGES: usize = 20_000;

pub(crate) struct WelcomeInfo {
    pub index: IndexId,
    pub mode: WelcomeMode,
    pub root: Entry,
    pub info: ServerInfo,
}

pub(crate) enum ApplyMsg {
    Welcome(Box<WelcomeInfo>, tokio::sync::oneshot::Sender<()>),
    Chunk {
        entries: Vec<Entry>,
        complete_dirs: Vec<ItemId>,
    },
    SnapDone {
        seq: u64,
    },
    Events {
        seq: u64,
        changes: Vec<Change>,
    },
    Listing {
        dir: Entry,
        entries: Vec<Entry>,
        last: bool,
        since: u64,
        done: Option<smpsc::Sender<Result<()>>>,
    },
    /// Results of our own mutations (applied even while paused).
    Local {
        upserts: Vec<(Entry, Source)>,
        removes: Vec<ItemId>,
        done: smpsc::Sender<()>,
    },
    Materialized {
        added: Vec<ItemId>,
        removed: Vec<ItemId>,
        full: bool,
        done: smpsc::Sender<()>,
    },
    LocalMeta {
        id: ItemId,
        meta: LocalMeta,
        done: smpsc::Sender<()>,
    },
    Override {
        id: ItemId,
        display: Option<String>,
        done: smpsc::Sender<()>,
    },
    /// Freeze (`Some`) or release (`None`) the `seen_seq` of a delete of `id` (rule 6).
    DeleteSeen {
        id: ItemId,
        entry: Option<DeleteSeen>,
        done: smpsc::Sender<()>,
    },
    Barrier(smpsc::Sender<Result<()>>),
    Confirm {
        apply: bool,
        done: smpsc::Sender<Result<()>>,
    },
    SessionEnded,
    Flush(smpsc::Sender<()>),
    Shutdown,
}

/// What the guard is holding back until `confirm_paused`.
enum Held {
    Events {
        seq: u64,
        changes: Vec<Change>,
    },
    SnapDrop {
        candidates: HashSet<ItemId>,
        seq: u64,
    },
    Wipe(Box<WelcomeInfo>, tokio::sync::oneshot::Sender<()>),
}

enum Done {
    Unit(smpsc::Sender<()>),
    Res(smpsc::Sender<Result<()>>),
}

pub(crate) struct Applier {
    shared: Arc<Shared>,
    db: Db,
    snap: Option<HashSet<ItemId>>,
    snap_max_event: u64,
    listings: HashMap<ItemId, HashSet<ItemId>>,
    held: Option<Held>,
    queue: VecDeque<ApplyMsg>,
    delta: Delta,
    dones: Vec<Done>,
    reimport: bool,
    last_gc: Instant,
    changes_in_batch: usize,
    /// A commit failed: its batch is still in `delta`; retry at `.0` (backoff `.1`).
    retry: Option<(Instant, Duration)>,
}

/// Mass-deletion guard (`EngineConfig::mass_delete_*`): more than `abs` removals, or more than
/// `frac` of M once the batch removes more than `min` items (below `min` the fraction rule never
/// trips: a user deleting 2 of 5 downloaded files is not a mass deletion).
pub(crate) fn guard_trips(count: usize, m_len: usize, frac: f64, abs: u64, min: u64) -> bool {
    count as u64 > abs || (count as u64 > min && count as f64 > frac * m_len as f64)
}

fn cfg_trips(cfg: &crate::EngineConfig, count: usize, m_len: usize) -> bool {
    guard_trips(
        count,
        m_len,
        cfg.mass_delete_frac,
        cfg.mass_delete_abs,
        cfg.mass_delete_min,
    )
}

impl Applier {
    pub fn new(shared: Arc<Shared>, db: Db) -> Applier {
        Applier {
            shared,
            db,
            snap: None,
            snap_max_event: 0,
            listings: HashMap::new(),
            held: None,
            queue: VecDeque::new(),
            delta: Delta::default(),
            dones: Vec::new(),
            reimport: false,
            last_gc: Instant::now() - GC_EVERY,
            changes_in_batch: 0,
            retry: None,
        }
    }

    pub fn run(mut self, rx: smpsc::Receiver<ApplyMsg>) {
        loop {
            if self.last_gc.elapsed() >= GC_EVERY {
                self.last_gc = Instant::now();
                let mut st = self.shared.write_state();
                st.gc(now_secs(), &mut self.delta);
                drop(st);
                self.commit();
            }
            let wait = self.retry.map_or(GC_EVERY, |(at, _)| {
                at.saturating_duration_since(Instant::now()).min(GC_EVERY)
            });
            let first = match rx.recv_timeout(wait) {
                Ok(m) => m,
                Err(smpsc::RecvTimeoutError::Timeout) => {
                    if self.retry.is_some_and(|(at, _)| Instant::now() >= at) {
                        self.commit();
                    }
                    continue;
                }
                Err(smpsc::RecvTimeoutError::Disconnected) => break,
            };
            let mut stop = self.handle(first);
            while !stop {
                match rx.try_recv() {
                    Ok(m) => stop = self.handle(m),
                    Err(_) => break,
                }
                if self.changes_in_batch >= MAX_BATCH_CHANGES {
                    self.commit();
                }
            }
            self.commit();
            if stop {
                break;
            }
        }
        self.commit();
    }

    fn keep_set(&self, st: &State) -> HashSet<ItemId> {
        let ids: Vec<ItemId> = self.shared.inflight_ids();
        st.with_ancestors(ids)
    }

    /// Returns true on Shutdown.
    fn handle(&mut self, msg: ApplyMsg) -> bool {
        self.shared.apply_queued.fetch_sub(1, Ordering::AcqRel);
        let paused = self.held.is_some();
        match msg {
            ApplyMsg::Shutdown => return true,
            // Signals/events are flushed by the waiting caller, never here: a host handler may
            // be waiting on this very thread.
            // While a failed batch is pending, the waiter is answered once it is committed.
            ApplyMsg::Flush(tx) => {
                if self.commit() {
                    let _ = tx.send(());
                } else {
                    self.dones.push(Done::Unit(tx));
                }
            }
            ApplyMsg::Barrier(tx) => {
                if self.commit() {
                    let _ = tx.send(Ok(()));
                } else {
                    self.dones.push(Done::Res(tx));
                }
            }
            ApplyMsg::SessionEnded => {
                // An unfinished snapshot is abandoned (snapshot_complete stays false → the next
                // Hello asks for a new one). Listing waiters see their sender dropped (Offline).
                self.snap = None;
                self.listings.clear();
            }
            ApplyMsg::Confirm { apply, done } => self.confirm(apply, done),
            ApplyMsg::Welcome(w, tx)
                if matches!(self.held, Some(Held::Events { .. } | Held::SnapDrop { .. })) =>
            {
                // A new session while the guard waits for the user: never park its handshake
                // behind the question (no reader, no pings, every op Offline, unlatchd queueing
                // for a session nobody reads). Nothing held or queued has been applied, so
                // server_seq / snapshot_complete still point before it and the new session
                // replays it (Resume) or re-sends it (Snapshot); the guard then asks again
                // about the same batch. Only a held Wipe must block the Welcome (rule 7).
                self.supersede_held();
                self.welcome(w, tx);
            }
            m @ (ApplyMsg::Welcome(..)
            | ApplyMsg::Chunk { .. }
            | ApplyMsg::SnapDone { .. }
            | ApplyMsg::Events { .. })
                if paused =>
            {
                self.shared.apply_queued.fetch_add(1, Ordering::AcqRel);
                self.queue.push_back(m);
            }
            ApplyMsg::Welcome(w, tx) => self.welcome(w, tx),
            ApplyMsg::Chunk {
                entries,
                complete_dirs,
            } => {
                self.changes_in_batch += entries.len();
                let mut st = self.shared.write_state();
                for e in entries {
                    if let Some(seen) = self.snap.as_mut() {
                        seen.insert(e.id);
                    }
                    st.upsert(e, Source::Snapshot, &mut self.delta);
                }
                for d in complete_dirs {
                    st.set_complete(d, true, &mut self.delta);
                }
            }
            ApplyMsg::SnapDone { seq } => self.snap_done(seq),
            ApplyMsg::Events { seq, changes } => self.events(seq, changes),
            ApplyMsg::Listing {
                dir,
                entries,
                last,
                since,
                done,
            } => {
                self.changes_in_batch += entries.len();
                let mut st = self.shared.write_state();
                let dir_id = dir.id;
                st.upsert(dir, Source::Listing, &mut self.delta);
                let acc = self.listings.entry(dir_id).or_default();
                for e in entries {
                    acc.insert(e.id);
                    if let Some(seen) = self.snap.as_mut() {
                        seen.insert(e.id);
                    }
                    st.upsert(e, Source::Listing, &mut self.delta);
                }
                if last {
                    let listed = self.listings.remove(&dir_id).unwrap_or_default();
                    // Children missing from the listing and not changed since the request was
                    // sent are gone (anything newer arrived through events after `since`).
                    let gone: HashSet<ItemId> = st
                        .children_of(dir_id)
                        .into_iter()
                        .filter(|c| {
                            !listed.contains(c)
                                && st.nodes.get(c).is_some_and(|n| n.entry.seq <= since)
                        })
                        .collect();
                    if !gone.is_empty() {
                        let keep = st.with_ancestors(self.shared.inflight_ids());
                        st.drop_where(&gone, &keep, &mut self.delta);
                    }
                    st.set_complete(dir_id, true, &mut self.delta);
                    if let Some(d) = done {
                        self.dones.push(Done::Res(d));
                    }
                }
            }
            ApplyMsg::Local {
                upserts,
                removes,
                done,
            } => {
                let mut st = self.shared.write_state();
                for (e, src) in upserts {
                    if let Some(seen) = self.snap.as_mut() {
                        seen.insert(e.id);
                    }
                    st.upsert(e, src, &mut self.delta);
                }
                let none = HashSet::new();
                for id in removes {
                    if let Some(s) = st.nodes.get(&id).map(|n| n.entry.seq) {
                        st.remove(id, s, &none, &mut self.delta);
                    }
                }
                self.dones.push(Done::Unit(done));
            }
            ApplyMsg::Materialized {
                added,
                removed,
                full,
                done,
            } => {
                let mut st = self.shared.write_state();
                st.set_materialized(&added, &removed, full, &mut self.delta);
                self.dones.push(Done::Unit(done));
            }
            ApplyMsg::LocalMeta { id, meta, done } => {
                let mut st = self.shared.write_state();
                if st.nodes.contains_key(&id) {
                    if meta == LocalMeta::default() {
                        st.local_meta.remove(&id);
                    } else {
                        st.local_meta.insert(id, meta);
                    }
                    self.delta.local_meta.push(id);
                }
                self.dones.push(Done::Unit(done));
            }
            ApplyMsg::Override { id, display, done } => {
                let mut st = self.shared.write_state();
                st.set_override(id, display, &mut self.delta);
                self.dones.push(Done::Unit(done));
            }
            ApplyMsg::DeleteSeen { id, entry, done } => {
                let mut st = self.shared.write_state();
                st.set_delete_seen(id, entry, &mut self.delta);
                self.dones.push(Done::Unit(done));
            }
        }
        false
    }

    /// Drop the held batch and everything queued behind it (all of it server-replayable, see
    /// the Welcome arm of `handle`) and clear the pause.
    fn supersede_held(&mut self) {
        self.held = None;
        let q: Vec<ApplyMsg> = self.queue.drain(..).collect();
        self.shared
            .apply_queued
            .fetch_sub(q.len() as u64, Ordering::AcqRel);
        // A queued Welcome of an earlier session sees its sender dropped (that session ends).
        drop(q);
        tracing::info!("mass-deletion guard: new session; the held batch will be replayed");
        self.shared.set_paused(None);
    }

    fn pause(&mut self, held: Held, reason: String) {
        tracing::warn!("mass-deletion guard: {reason}");
        self.held = Some(held);
        self.shared.set_paused(Some(reason));
    }

    fn welcome(&mut self, w: Box<WelcomeInfo>, tx: tokio::sync::oneshot::Sender<()>) {
        self.commit();
        let changed = {
            let st = self.shared.read_state();
            st.index.is_some_and(|ix| ix != w.index)
        };
        if changed {
            let (count, m_len) = {
                let st = self.shared.read_state();
                let c = st
                    .materialized
                    .iter()
                    .filter(|x| **x != ItemId::ROOT && st.nodes.contains_key(x))
                    .count();
                (c, st.materialized.len())
            };
            let cfg = &self.shared.cfg;
            if cfg_trips(cfg, count, m_len) {
                let reason = format!(
                    "The VM's file index changed (re-imaged, or the root folder was replaced): {count} downloaded items would be removed"
                );
                self.pause(Held::Wipe(w, tx), reason);
                return;
            }
        }
        self.apply_welcome(w, changed);
        self.commit();
        let _ = tx.send(());
    }

    fn apply_welcome(&mut self, w: Box<WelcomeInfo>, wipe: bool) {
        let mut st = self.shared.write_state();
        if wipe {
            // Rule 7: ids from the old index mean nothing now. New replica uuid → every anchor
            // the system holds expires; the host reimports.
            st.wipe(&mut self.delta);
            self.reimport = true;
            self.snap = None;
            self.listings.clear();
        }
        st.index = Some(w.index);
        st.info = Some(w.info);
        self.delta.meta = true;
        st.upsert(w.root, Source::Snapshot, &mut self.delta);
        match w.mode {
            WelcomeMode::Snapshot => {
                st.snapshot_complete = false;
                let mut seen = HashSet::new();
                seen.insert(ItemId::ROOT);
                self.snap = Some(seen);
                self.snap_max_event = 0;
            }
            WelcomeMode::Resume => {
                self.snap = None;
            }
        }
    }

    fn snap_done(&mut self, seq: u64) {
        let Some(seen) = self.snap.take() else {
            // Snapshot tracking was lost (session restarted mid-way): nothing to drop.
            return;
        };
        let candidates: HashSet<ItemId> = {
            let st = self.shared.read_state();
            st.nodes
                .keys()
                .filter(|id| **id != ItemId::ROOT && !seen.contains(id))
                .copied()
                .collect()
        };
        let (count, m_len) = {
            let st = self.shared.read_state();
            let c = candidates
                .iter()
                .filter(|x| st.materialized.contains(x))
                .count();
            (c, st.materialized.len())
        };
        let cfg = &self.shared.cfg;
        if cfg_trips(cfg, count, m_len) {
            let reason = format!("A full resync would remove {count} downloaded items");
            self.pause(Held::SnapDrop { candidates, seq }, reason);
            return;
        }
        self.finish_snapshot(candidates, seq, true);
    }

    fn finish_snapshot(&mut self, candidates: HashSet<ItemId>, seq: u64, drop: bool) {
        let mut st = self.shared.write_state();
        if drop && !candidates.is_empty() {
            // Rule 13: the post-snapshot drop is one journal batch and never drops ids with
            // in-flight local mutations (or their ancestors).
            let keep = self.keep_set(&st);
            st.drop_where(&candidates, &keep, &mut self.delta);
        }
        st.snapshot_complete = true;
        st.server_seq = st.server_seq.max(seq).max(self.snap_max_event);
        self.delta.meta = true;
    }

    fn events(&mut self, seq: u64, changes: Vec<Change>) {
        self.changes_in_batch += changes.len();
        let (count, m_len) = {
            let st = self.shared.read_state();
            let roots: Vec<ItemId> = changes
                .iter()
                .filter_map(|c| match c {
                    Change::Remove { id, seq }
                        if *seq > st.server_seq
                            && st.nodes.get(id).is_some_and(|n| n.entry.seq <= *seq) =>
                    {
                        Some(*id)
                    }
                    _ => None,
                })
                .collect();
            if roots.is_empty() {
                (0, 0)
            } else {
                (st.materialized_in_subtrees(&roots), st.materialized.len())
            }
        };
        let cfg = &self.shared.cfg;
        if count > 0 && cfg_trips(cfg, count, m_len) {
            let reason = format!("A change on the VM would remove {count} downloaded items");
            self.pause(Held::Events { seq, changes }, reason);
            return;
        }
        self.apply_events(seq, changes, &HashSet::new(), true);
    }

    fn apply_events(
        &mut self,
        seq: u64,
        changes: Vec<Change>,
        keep: &HashSet<ItemId>,
        removes: bool,
    ) {
        let mut st = self.shared.write_state();
        // Every event ≤ server_seq was already applied — or its removal was declined by the
        // user ("Keep My Files") or kept for an in-flight edit. A replay of it (a Resume from
        // a Hello built before that) must not remove those items now, nor ask again.
        let applied_upto = st.server_seq;
        for c in changes {
            match c {
                Change::Upsert(e) => {
                    if let Some(seen) = self.snap.as_mut() {
                        seen.insert(e.id);
                    }
                    st.upsert(e, Source::Event, &mut self.delta);
                }
                Change::Remove { id, seq: s } => {
                    if removes && s > applied_upto {
                        st.remove(id, s, keep, &mut self.delta);
                    }
                }
            }
        }
        if self.snap.is_some() {
            self.snap_max_event = self.snap_max_event.max(seq);
        } else {
            st.server_seq = st.server_seq.max(seq);
        }
        self.delta.meta = true;
    }

    fn confirm(&mut self, apply: bool, done: smpsc::Sender<Result<()>>) {
        self.commit();
        let Some(held) = self.held.take() else {
            // Paused by the supervisor (RootReplaced) rather than by a held batch: the next
            // Welcome carries the new index and goes through the guard again.
            self.shared.set_paused(None);
            let _ = done.send(Ok(()));
            return;
        };
        match held {
            Held::Events { seq, changes } => {
                let keep = {
                    let st = self.shared.read_state();
                    self.keep_set(&st)
                };
                // apply=false: keep the local items (skip the removals), take everything else.
                self.apply_events(seq, changes, &keep, apply);
            }
            Held::SnapDrop { candidates, seq } => self.finish_snapshot(candidates, seq, apply),
            Held::Wipe(w, tx) => {
                if apply {
                    self.apply_welcome(w, true);
                    self.commit();
                    let _ = tx.send(());
                } else {
                    // Cannot use ids of a new index with the old replica: keep it read-only and
                    // stay disconnected until the user asks again.
                    drop(tx);
                    let q: Vec<ApplyMsg> = self.queue.drain(..).collect();
                    self.shared
                        .apply_queued
                        .fetch_sub(q.len() as u64, Ordering::AcqRel);
                    self.shared.set_paused(None);
                    self.shared.hold(
                        "Sync stopped: the VM's file index changed and removal was declined".into(),
                    );
                    let _ = done.send(Ok(()));
                    return;
                }
            }
        }
        self.shared.set_paused(None);
        self.commit();
        let q: Vec<ApplyMsg> = self.queue.drain(..).collect();
        for m in q {
            // `handle` accounts one dequeue per message.
            self.handle(m);
        }
        self.commit();
        let _ = done.send(Ok(()));
    }

    /// Persist the batch. Returns false when the transaction failed: the batch then stays
    /// pending (in `delta`, with its waiters) and is retried with backoff, together with
    /// whatever is applied meanwhile.
    fn commit(&mut self) -> bool {
        self.changes_in_batch = 0;
        let delta = std::mem::take(&mut self.delta);
        let dones = std::mem::take(&mut self.dones);
        if delta.is_empty() && !self.reimport {
            // Nothing to persist: typically our own mutation's result repeating what the pushed
            // `Events` already applied (LWW no-op). Every earlier batch was committed on this
            // thread before this point, so what the waiters wait for (their change in the
            // durable replica) already holds: release them without a transaction and its fsync
            // (it would only rewrite identical meta rows). No anchor is minted (seq unchanged).
            if !dones.is_empty() {
                Self::release(dones);
                if self.held.is_none() {
                    let st = self.shared.read_state();
                    let (complete, server_seq) = (st.snapshot_complete, st.server_seq);
                    drop(st);
                    self.shared.maybe_live(complete, server_seq);
                }
            }
            return true;
        }
        let (mut ws, seq_now, snapshot_complete, server_seq, prev_anchor) = {
            let mut st = self.shared.write_state();
            let seq_now = st.seq;
            let ss = st.server_seq;
            // anchor → server seq, for `seen_seq` (rule 6).
            let prev = st.anchors.insert(seq_now, ss);
            (
                delta.write_set(&st),
                seq_now,
                st.snapshot_complete,
                ss,
                prev,
            )
        };
        ws.anchor = Some((seq_now, server_seq));
        // `ReplicaChanged` goes out *before* the SQLite commit: it only tells a frontend that
        // reads the in-memory replica (FUSE) to drop kernel caches, and that replica already
        // holds the change (`handle` applied it under the state lock). Waiting for the commit
        // (an fsync, ~2 ms here and far more on a busy disk) only delayed what `ls` shows. A
        // crash before the commit loses nothing the server cannot replay (server_seq on disk
        // is not advanced). Anchors, `WorkingSetChanged` and waiters stay after the commit
        // (D5: the system is only ever signalled for committed anchors).
        let ids = delta.ids();
        let parents: Vec<ItemId> = delta.parents.iter().copied().collect();
        if !ids.is_empty() || !parents.is_empty() {
            self.shared
                .emit(EngineEvent::ReplicaChanged { ids, parents });
        }
        self.shared.replica_writes.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self.write_db(&ws) {
            // Keep serving from memory, but never drop the batch: every write carries
            // meta.server_seq, so a later commit without these rows would persist a resume
            // point past changes that never reached disk (lost for good after a restart). The
            // batch stays pending and later changes append to it; the retry writes them all in
            // one transaction. Until then no anchor is published (D5), no waiter is told it is
            // durable, and the status says why. A crash meanwhile loses nothing the server
            // cannot replay: server_seq on disk is still the last committed one.
            {
                let mut st = self.shared.write_state();
                match prev_anchor {
                    Some(prev) => st.anchors.insert(seq_now, prev),
                    None => st.anchors.remove(&seq_now),
                };
            }
            debug_assert!(self.delta.is_empty() && self.dones.is_empty());
            self.delta = delta;
            self.dones = dones;
            let backoff = self
                .retry
                .map_or(RETRY_MIN, |(_, b)| (b * 2).min(RETRY_MAX));
            self.retry = Some((Instant::now() + backoff, backoff));
            tracing::error!(
                "replica commit failed (retrying in {} ms, nothing is dropped): {e}",
                backoff.as_millis()
            );
            self.shared.set_replica_error(Some(format!(
                "Unlatch cannot save its file index on this Mac (is the disk full?): {}",
                e.msg
            )));
            return false;
        }
        if self.retry.take().is_some() {
            tracing::info!("replica commit succeeded after retrying");
            self.shared.set_replica_error(None);
        }
        self.shared.committed.store(seq_now, Ordering::Release);
        for (id, ver) in &delta.content_changed {
            self.shared.cache.invalidate_older(*id, *ver);
        }
        if delta.wipe {
            self.shared.cache.clear();
        }
        if self.reimport {
            self.reimport = false;
            self.shared.emit(EngineEvent::Reimport {
                below: ItemId::ROOT,
            });
        }
        if delta.ws {
            self.shared.signaller.request();
        }
        Self::release(dones);
        if self.held.is_none() {
            self.shared.maybe_live(snapshot_complete, server_seq);
        }
        true
    }

    fn write_db(&mut self, ws: &super::replica::WriteSet) -> Result<()> {
        #[cfg(test)]
        if take_one(&self.shared.fail_replica_writes) {
            return Err(crate::err(
                unlatch_proto::ErrorCode::NoSpace,
                "simulated: database or disk is full",
            ));
        }
        self.db.write(ws)
    }

    fn release(dones: Vec<Done>) {
        for d in dones {
            match d {
                Done::Unit(tx) => {
                    let _ = tx.send(());
                }
                Done::Res(tx) => {
                    let _ = tx.send(Ok(()));
                }
            }
        }
    }
}

/// Coalesced `WorkingSetChanged` (≤ 1 per 5 ms), emitted on its own thread after commits.
pub(crate) struct Signaller {
    st: Mutex<SigState>,
    cv: Condvar,
}

struct SigState {
    pending: bool,
    last: Option<Instant>,
    stop: bool,
    emitting: bool,
}

pub(crate) const SIGNAL_MIN_GAP: Duration = Duration::from_millis(5);

impl Signaller {
    pub fn new() -> Signaller {
        Signaller {
            st: Mutex::new(SigState {
                pending: false,
                last: None,
                stop: false,
                emitting: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SigState> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn request(&self) {
        self.lock().pending = true;
        self.cv.notify_all();
    }

    /// Wait until any requested signal has been emitted (bounded).
    pub fn flush(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut g = self.lock();
        while (g.pending || g.emitting) && !g.stop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            g = self
                .cv
                .wait_timeout(g, deadline - now)
                .map(|(g, _)| g)
                .unwrap_or_else(|p| p.into_inner().0);
        }
    }

    pub fn stop(&self) {
        self.lock().stop = true;
        self.cv.notify_all();
    }

    pub fn run(&self, shared: Weak<Shared>) {
        let mut g = self.lock();
        loop {
            if g.stop {
                return;
            }
            if !g.pending {
                g = self.cv.wait(g).unwrap_or_else(|p| p.into_inner());
                continue;
            }
            if let Some(last) = g.last {
                let el = last.elapsed();
                if el < SIGNAL_MIN_GAP {
                    g = self
                        .cv
                        .wait_timeout(g, SIGNAL_MIN_GAP - el)
                        .map(|(g, _)| g)
                        .unwrap_or_else(|p| p.into_inner().0);
                    continue;
                }
            }
            g.pending = false;
            g.emitting = true;
            g.last = Some(Instant::now());
            drop(g);
            if let Some(sh) = shared.upgrade() {
                let anchor = sh.anchor_bytes();
                sh.emit(EngineEvent::WorkingSetChanged { anchor });
            }
            g = self.lock();
            g.emitting = false;
            self.cv.notify_all();
        }
    }
}

/// Decrement a test fault counter if it is positive; true when one fault was taken.
/// (A compare-exchange loop: `fetch_update` is deprecated on newer toolchains.)
#[cfg(test)]
fn take_one(n: &std::sync::atomic::AtomicU64) -> bool {
    let mut cur = n.load(Ordering::Acquire);
    while cur > 0 {
        match n.compare_exchange_weak(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(seen) => cur = seen,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_thresholds() {
        // fraction rule needs more than the floor
        assert!(!guard_trips(5, 10, 0.2, 1000, 32));
        assert!(!guard_trips(32, 40, 0.2, 1000, 32));
        assert!(guard_trips(33, 40, 0.2, 1000, 32));
        assert!(!guard_trips(33, 1000, 0.2, 1000, 32));
        assert!(guard_trips(1001, 100_000, 0.2, 1000, 32));
        assert!(!guard_trips(0, 0, 0.2, 1000, 32));
        // `mass_delete_min` moves the floor of the fraction rule (not the absolute rule).
        assert!(guard_trips(2, 5, 0.2, 1000, 1));
        assert!(!guard_trips(1, 5, 0.2, 1000, 1));
        assert!(!guard_trips(33, 40, 0.2, 1000, 100));
        assert!(guard_trips(1001, 1001, 0.2, 1000, 5000));
    }
}
