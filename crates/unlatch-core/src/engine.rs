//! Engine internals (connection supervisor, session, replica, journal, working set, cache,
//! prefetch, ops). Private to the crate; the public surface is `crate::Engine`.
//!
//! Threads: a small tokio runtime runs the connection (supervisor, session reader/writer, ping);
//! one apply thread is the replica's only writer (`applier`); one signaller thread emits the
//! coalesced `WorkingSetChanged`; two prefetch workers. Public calls run on the caller's thread:
//! reads take the replica's read lock (µs), mutations/fetches block on replies from the session.

mod applier;
mod cache;
mod names;
mod ops;
mod prefetch;
mod replica;
mod rules;
mod session;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

use crate::*;
use applier::{Applier, ApplyMsg, Signaller};
use cache::Cache;
use replica::{Db, Source, State};
use session::{LinkParts, Reply, SessionEnd, SessionHandle, Sink};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as smpsc;
use std::sync::{Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;
use unlatch_proto::ipc::{ConnState, EngineStatus, IpcItem};
use unlatch_proto::wire::{Request, Response};
use unlatch_proto::{Entry, IndexId, Kind};

/// Fault injection (shared contract): `UNLATCH_FAULT` is a comma-separated token list read once
/// per process, e.g. `die_before_ipc_reply:create`.
pub(crate) fn fault(token: &str) -> bool {
    static TOKENS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    TOKENS
        .get_or_init(|| {
            std::env::var("UNLATCH_FAULT")
                .map(|v| {
                    v.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        })
        .iter()
        .any(|t| t == token)
}

/// Opens a link to unlatchd (`transport::open` in production; an in-memory pipe in tests).
pub(crate) type Opener =
    Arc<dyn Fn(bool) -> Pin<Box<dyn Future<Output = Result<LinkParts>> + Send>> + Send + Sync>;

/// Connection timing (tests shorten these).
#[derive(Clone, Debug)]
pub(crate) struct Timing {
    pub ping_interval: Duration,
    pub dead_after: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub welcome_timeout: Duration,
    pub connect_timeout: Duration,
    /// Replies to mutations / stats (per message, not per transfer).
    pub reply_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            ping_interval: Duration::from_secs(5),
            dead_after: Duration::from_secs(10),
            backoff_min: Duration::from_millis(250),
            backoff_max: Duration::from_secs(8),
            welcome_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(90),
            reply_timeout: Duration::from_secs(60),
        }
    }
}

/// Same rules as `transport::sanitize_client_name` (review §2(d)12); kept local so the engine
/// does not depend on transport internals.
pub(crate) fn sanitize_client_name(name: &str) -> String {
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
    let out = out.trim().to_string();
    if out.is_empty() {
        "mac".into()
    } else {
        out
    }
}

const EVENT_THREAD: &str = "unlatch-events";

pub(crate) enum EventMsg {
    Ev(Box<EngineEvent>),
    Flush(smpsc::Sender<()>),
}

fn event_loop(handler: Option<EventHandler>, rx: smpsc::Receiver<EventMsg>) {
    for m in rx {
        match m {
            EventMsg::Ev(ev) => {
                if let Some(h) = &handler {
                    // A panicking host callback must not take the engine down.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(*ev)));
                }
            }
            EventMsg::Flush(tx) => {
                let _ = tx.send(());
            }
        }
    }
}

struct Conn {
    state: ConnState,
    paused: Option<String>,
    /// A human must act; no automatic reconnect until `connect_interactive`.
    hold: Option<String>,
    live_target: u64,
    attempts: u64,
    last_attempt: Option<Result<()>>,
    received: u64,
    last_status_emit: Option<Instant>,
}

/// Everything the engine's threads share.
pub(crate) struct Shared {
    pub cfg: EngineConfig,
    pub client_name: String,
    pub timing: Timing,
    state: RwLock<State>,
    pub committed: AtomicU64,
    /// Replica transactions written (diagnostics and tests).
    pub replica_writes: AtomicU64,
    /// Why the last replica transaction failed, while its batch is pending a retry (shown as
    /// an Offline-like status; `None` once a commit succeeds).
    replica_error: Mutex<Option<String>>,
    /// Tests: fail this many upcoming replica transactions (simulated ENOSPC).
    #[cfg(test)]
    pub fail_replica_writes: AtomicU64,
    apply_tx: Mutex<Option<smpsc::Sender<ApplyMsg>>>,
    pub apply_queued: AtomicU64,
    session: Mutex<Option<Arc<SessionHandle>>>,
    conn: Mutex<Conn>,
    conn_cv: Condvar,
    events: Mutex<Option<smpsc::Sender<EventMsg>>>,
    pub signaller: Signaller,
    inflight: Mutex<HashMap<ItemId, u32>>,
    mutations: Mutex<u32>,
    mutations_cv: Condvar,
    pub uploads: AtomicU64,
    rtt_us: AtomicU64,
    pub cache: Cache,
    wake: tokio::sync::Notify,
    interactive_req: AtomicBool,
    shutdown: AtomicBool,
    pub prefetch: prefetch::Prefetcher,
}

impl Shared {
    pub fn read_state(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|p| p.into_inner())
    }

    pub fn write_state(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|p| p.into_inner())
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Conn> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    pub fn apply(&self, msg: ApplyMsg) {
        let tx = self.apply_tx.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(tx) = tx.as_ref() {
            self.apply_queued.fetch_add(1, Ordering::AcqRel);
            if tx.send(msg).is_err() {
                self.apply_queued.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// Send to the applier and wait for the commit.
    fn apply_wait(&self, make: impl FnOnce(smpsc::Sender<()>) -> ApplyMsg) -> Result<()> {
        let (tx, rx) = smpsc::channel();
        self.apply(make(tx));
        rx.recv_timeout(Duration::from_secs(120))
            .map_err(|_| err(ErrorCode::Io, "replica applier stopped"))
    }

    /// Queue an event for the host. Delivered in order on the `unlatch-events` thread, so a
    /// handler may call back into the engine (even into calls that wait on the applier).
    pub fn emit(&self, ev: EngineEvent) {
        if let Some(tx) = self
            .events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            let _ = tx.send(EventMsg::Ev(Box::new(ev)));
        }
    }

    /// Signal the working set and wait until the host has the `WorkingSetChanged` (bounded),
    /// for a mutation reply that cannot carry all of its effects: a conflict copy is a second
    /// item that only reaches the Mac through the working set. The system believes the reply
    /// at once (MQ-013), so the signal should already be with the host when the reply leaves
    /// rather than still coalescing on the signal thread (≤ `SIGNAL_MIN_GAP`) or queued on the
    /// event thread. Without this the requested signal still arrives, only a few ms after the
    /// reply, so the copy appears late, never not at all. With a synchronous host (fpsim) this
    /// makes the order deterministic; on macOS the host forwards events asynchronously and
    /// `signalEnumerator` travels on a different path from the reply, so there it only narrows
    /// the window (a late signal still triggers the enumeration).
    pub(crate) fn signal_working_set_now(&self) {
        self.signaller.request();
        self.signaller.flush();
        self.flush_events();
    }

    /// Wait until every event queued so far has been delivered (bounded).
    pub fn flush_events(&self) {
        let (tx, rx) = smpsc::channel();
        let sent = match self
            .events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            Some(ev) => ev.send(EventMsg::Flush(tx)).is_ok(),
            None => false,
        };
        if sent && std::thread::current().name() != Some(EVENT_THREAD) {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }

    pub fn inflight_ids(&self) -> Vec<ItemId> {
        self.inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .copied()
            .collect()
    }

    pub fn anchor_bytes(&self) -> Vec<u8> {
        let uuid = self.read_state().replica_uuid;
        encode_anchor(uuid, self.committed.load(Ordering::Acquire))
    }

    pub fn note_rtt(&self, rtt: Duration) {
        self.rtt_us
            .store(rtt.as_micros().max(1) as u64, Ordering::Relaxed);
    }

    pub fn note_received(&self, n: u64) {
        let mut c = self.conn();
        c.received += n;
        if let ConnState::Syncing { .. } = c.state {
            c.state = ConnState::Syncing {
                received: c.received,
            };
            let due = c
                .last_status_emit
                .is_none_or(|t| t.elapsed() >= Duration::from_millis(250));
            if due {
                c.last_status_emit = Some(Instant::now());
                drop(c);
                self.emit(EngineEvent::StatusChanged(self.status()));
            }
        }
    }

    fn set_conn_state(&self, s: ConnState) {
        let mut c = self.conn();
        if c.state == s {
            return;
        }
        let was_live = c.state == ConnState::Live;
        let now_live = s == ConnState::Live;
        c.state = s.clone();
        c.last_status_emit = Some(Instant::now());
        drop(c);
        self.conn_cv.notify_all();
        if let ConnState::NeedsUser { reason, url } = &s {
            self.emit(EngineEvent::NeedsUser {
                reason: reason.clone(),
                url: url.clone(),
            });
        }
        self.emit(EngineEvent::StatusChanged(self.status()));
        if now_live && !was_live {
            // MQ-005: the host must clear the system's backoff and re-enumerate the working set.
            self.emit(EngineEvent::ErrorResolved);
            self.signaller.request();
        }
    }

    pub fn set_paused(&self, reason: Option<String>) {
        self.conn().paused = reason;
        self.conn_cv.notify_all();
        self.emit(EngineEvent::StatusChanged(self.status()));
    }

    /// The applier could (`None`) / could not (`Some(why)`) persist its last batch. Shown as
    /// an Offline status until a retry commits; the connection itself is left alone.
    pub fn set_replica_error(&self, why: Option<String>) {
        let cleared = {
            let mut g = self.replica_error.lock().unwrap_or_else(|p| p.into_inner());
            if *g == why {
                return;
            }
            let cleared = g.is_some() && why.is_none();
            *g = why;
            cleared
        };
        self.conn_cv.notify_all();
        self.emit(EngineEvent::StatusChanged(self.status()));
        if cleared && self.conn().state == ConnState::Live {
            // As when the connection comes back (MQ-005): clear the system's backoff.
            self.emit(EngineEvent::ErrorResolved);
            self.signaller.request();
        }
    }

    pub fn hold(&self, reason: String) {
        self.conn().hold = Some(reason.clone());
        self.set_conn_state(ConnState::Offline {
            error: reason,
            retry_in_ms: 0,
        });
        if let Some(s) = self.current_session() {
            s.close();
        }
    }

    /// After Welcome: Syncing until the snapshot completes / the replay reaches `target`.
    pub fn begin_sync(&self, target: u64) {
        {
            let mut c = self.conn();
            c.live_target = target;
            c.received = 0;
        }
        let (complete, server_seq) = {
            let st = self.read_state();
            (st.snapshot_complete, st.server_seq)
        };
        if complete && server_seq >= target {
            self.set_conn_state(ConnState::Live);
        } else {
            self.set_conn_state(ConnState::Syncing { received: 0 });
        }
    }

    /// The Resume replay has been applied (barrier after Welcome).
    pub fn caught_up(&self) {
        self.conn().live_target = 0;
        let (complete, server_seq) = {
            let st = self.read_state();
            (st.snapshot_complete, st.server_seq)
        };
        self.maybe_live(complete, server_seq);
    }

    pub fn maybe_live(&self, snapshot_complete: bool, server_seq: u64) {
        let go = {
            let c = self.conn();
            matches!(c.state, ConnState::Syncing { .. })
                && snapshot_complete
                && server_seq >= c.live_target
        };
        if go && self.current_session().is_some() {
            self.set_conn_state(ConnState::Live);
        }
    }

    pub fn install_session(&self, s: Option<Arc<SessionHandle>>) {
        let ok = s.is_some();
        *self.session.lock().unwrap_or_else(|p| p.into_inner()) = s;
        if ok {
            self.finish_attempt(Ok(()));
        }
        self.conn_cv.notify_all();
    }

    fn finish_attempt(&self, r: Result<()>) {
        let mut c = self.conn();
        c.attempts += 1;
        c.last_attempt = Some(r);
        drop(c);
        self.conn_cv.notify_all();
    }

    pub fn current_session(&self) -> Option<Arc<SessionHandle>> {
        self.session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .filter(|s| !s.is_closed())
    }

    pub fn status(&self) -> EngineStatus {
        let (state, paused) = {
            let c = self.conn();
            (c.state.clone(), c.paused.clone())
        };
        let replica_error = self
            .replica_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let state = match (paused, replica_error) {
            (Some(reason), _) => ConnState::Paused { reason },
            (None, Some(error)) => ConnState::Offline {
                error,
                retry_in_ms: 0,
            },
            (None, None) => state,
        };
        let (entries, server) = {
            let st = self.read_state();
            (st.nodes.len() as u64, st.info.clone())
        };
        let rtt = self.rtt_us.load(Ordering::Relaxed);
        EngineStatus {
            state,
            entries,
            anchor: self.anchor_bytes(),
            rtt_us: (rtt > 0).then_some(rtt),
            cache_bytes: self.cache.total(),
            pending_uploads: self.uploads.load(Ordering::Relaxed) as u32,
            server,
        }
    }

    /// The live session for an op addressed to `index`; waits up to `wait` while a connection
    /// is being (re)established. Never runs an id-addressed op across index ids (rule 7).
    pub fn session_for(
        &self,
        index: Option<IndexId>,
        wait: Duration,
    ) -> Result<Arc<SessionHandle>> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(s) = self.current_session() {
                if index.is_some_and(|ix| ix != s.index) {
                    return Err(err(
                        ErrorCode::IndexChanged,
                        "the VM's file index changed; reimporting",
                    ));
                }
                return Ok(s);
            }
            let c = self.conn();
            match &c.state {
                ConnState::NeedsUser { reason, .. } => {
                    return Err(err(ErrorCode::Offline, reason.clone()))
                }
                ConnState::Offline { error, .. } if c.hold.is_some() => {
                    return Err(err(ErrorCode::Offline, error.clone()))
                }
                _ => {}
            }
            let now = Instant::now();
            if now >= deadline || self.is_shutdown() {
                return Err(err(ErrorCode::Offline, "not connected to the VM"));
            }
            let _ = self
                .conn_cv
                .wait_timeout(c, (deadline - now).min(Duration::from_millis(50)));
        }
    }

    /// One request → one final reply.
    pub fn call(&self, s: &SessionHandle, req: Request, timeout: Duration) -> Result<Response> {
        let (tx, rx) = smpsc::channel();
        let id = s.request(req, Sink::Chan(tx))?;
        match rx.recv_timeout(timeout) {
            Ok(Reply::Resp(r)) => Ok(r),
            Ok(Reply::Err(e)) => Err(e),
            Ok(Reply::Chunk { .. }) => Err(err(ErrorCode::Protocol, "unexpected ReadChunk")),
            Err(smpsc::RecvTimeoutError::Timeout) => {
                s.forget(id);
                Err(err(ErrorCode::Timeout, "unlatchd did not reply"))
            }
            Err(smpsc::RecvTimeoutError::Disconnected) => {
                Err(err(ErrorCode::Offline, "connection lost"))
            }
        }
    }

    /// Apply entries from our own requests (LWW) and wait for the commit.
    pub fn apply_local(&self, upserts: Vec<(Entry, Source)>, removes: Vec<ItemId>) -> Result<()> {
        self.apply_wait(|done| ApplyMsg::Local {
            upserts,
            removes,
            done,
        })
    }

    pub fn apply_msg_wait(&self, make: impl FnOnce(smpsc::Sender<()>) -> ApplyMsg) -> Result<()> {
        self.apply_wait(make)
    }

    pub fn item(&self, id: ItemId) -> Result<IpcItem> {
        self.read_state()
            .ipc_item(id, self.cfg.expose_exec)
            .ok_or_else(|| err(ErrorCode::NotFound, format!("no item {id}")))
    }

    /// Refresh one item from the server (`Stat`), applying the result.
    pub fn refresh(&self, id: ItemId) -> Result<()> {
        let index = self.read_state().index;
        let s = self.session_for(index, self.cfg.list_timeout)?;
        match self.call(&s, Request::Stat { id }, self.timing.reply_timeout) {
            Ok(Response::Entry(e)) => self.apply_local(vec![(e, Source::Other)], vec![]),
            Ok(_) => Err(err(ErrorCode::Protocol, "unexpected Stat reply")),
            Err(e) if e.code == ErrorCode::NotFound => {
                self.apply_local(vec![], vec![id])?;
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Ensure `dir`'s one-level listing is known: priority `ListDir`, wait ≤ `wait`.
    pub fn ensure_listed(&self, dir: ItemId, wait: Duration) -> Result<()> {
        let (index, since) = {
            let st = self.read_state();
            (st.index, st.server_seq)
        };
        let s = self.session_for(index, wait)?;
        let (tx, rx) = smpsc::channel();
        s.request(
            Request::ListDir { dir },
            Sink::Listing {
                since,
                done: Some(tx),
            },
        )?;
        match rx.recv_timeout(wait) {
            Ok(r) => r,
            Err(smpsc::RecvTimeoutError::Timeout) => {
                Err(err(ErrorCode::Timeout, "listing still in progress"))
            }
            Err(smpsc::RecvTimeoutError::Disconnected) => {
                Err(err(ErrorCode::Offline, "connection lost"))
            }
        }
    }

    // ---- content ------------------------------------------------------------------------------

    /// Stream `id` at content version `ver` into a cache temp file. Returns (tmp, version read).
    fn download(
        &self,
        id: ItemId,
        ver: u64,
        total: u64,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<(std::path::PathBuf, u64)> {
        use std::os::unix::fs::FileExt;
        let index = self.read_state().index;
        let s = self.session_for(index, self.cfg.list_timeout)?;
        let (tx, rx) = smpsc::channel();
        let req_id = s.request(
            Request::Read {
                id,
                offset: 0,
                len: None,
                expect: Some(ver),
            },
            Sink::Chan(tx),
        )?;
        let tmp = self.cache.tmp_path();
        let abort = |p: &std::path::Path, e: ProtoError| {
            s.cancel(req_id);
            s.forget(req_id);
            let _ = std::fs::remove_file(p);
            Err(e)
        };
        let file = match std::fs::File::create(&tmp) {
            Ok(f) => f,
            Err(e) => return abort(&tmp, err(ErrorCode::Io, format!("cache temp: {e}"))),
        };
        let mut done = 0u64;
        let mut version: u64;
        let mut idle = Instant::now();
        loop {
            if cancel.is_cancelled() {
                return abort(&tmp, err(ErrorCode::Cancelled, "cancelled"));
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Reply::Chunk {
                    offset,
                    data,
                    last,
                    version: v,
                }) => {
                    idle = Instant::now();
                    if let Err(e) = file.write_all_at(&data, offset) {
                        let code = if e.raw_os_error() == Some(libc::ENOSPC) {
                            ErrorCode::NoSpace
                        } else {
                            ErrorCode::Io
                        };
                        return abort(&tmp, err(code, format!("cache write: {e}")));
                    }
                    done = done.max(offset + data.len() as u64);
                    version = v;
                    progress(done, total.max(done));
                    if last {
                        break;
                    }
                }
                Ok(Reply::Err(e)) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                Ok(Reply::Resp(_)) => {
                    return abort(&tmp, err(ErrorCode::Protocol, "unexpected reply to Read"))
                }
                Err(smpsc::RecvTimeoutError::Timeout) => {
                    if idle.elapsed() >= self.timing.reply_timeout {
                        return abort(&tmp, err(ErrorCode::Timeout, "download stalled"));
                    }
                }
                Err(smpsc::RecvTimeoutError::Disconnected) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(err(ErrorCode::Offline, "connection lost during download"));
                }
            }
        }
        if done == 0 {
            progress(0, 0);
        }
        drop(file);
        Ok((tmp, version))
    }

    /// The cached file for `id`'s current content (downloading it if needed), pinned; the caller
    /// must `cache.unpin(id, item.entry.version.content)`.
    pub fn ensure_cached(
        &self,
        id: ItemId,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<(std::path::PathBuf, IpcItem)> {
        let mut last_err = None;
        for _ in 0..4 {
            let mut item = self.item(id)?;
            if item.entry.kind == Kind::Dir {
                return Err(err(ErrorCode::IsDir, "is a directory"));
            }
            let ver = item.entry.version.content;
            if let Some(p) = self.cache.get_pinned(id, ver) {
                return Ok((p, item));
            }
            let lock = self.cache.key_lock(id, ver);
            let _g = lock.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(p) = self.cache.get_pinned(id, ver) {
                return Ok((p, item));
            }
            match self.download(id, ver, item.entry.size, progress, cancel) {
                Ok((tmp, got)) => {
                    // Pinned as it enters the cache: its own eviction pass must not remove it,
                    // even when this one file exceeds the whole budget.
                    let p = self.cache.insert_pinned(id, got, &tmp)?;
                    if got != ver {
                        // Content moved on between our replica and the read: refresh metadata
                        // so the returned item carries the version of these bytes (MQ-013).
                        let _ = self.refresh(id);
                        item = match self.item(id) {
                            Ok(it) => it,
                            Err(e) => {
                                self.cache.unpin(id, got);
                                return Err(e);
                            }
                        };
                        item.entry.version.content = got;
                    }
                    return Ok((p, item));
                }
                Err(e) if e.code == ErrorCode::VersionMismatch => {
                    self.refresh(id)?;
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| err(ErrorCode::VersionMismatch, "file keeps changing")))
    }

    /// Mutation bookkeeping (in-flight ids are never dropped by snapshot/guarded removals).
    pub fn begin_mutation(&self, ids: &[ItemId]) -> MutationGuard<'_> {
        {
            let mut m = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
            for id in ids {
                *m.entry(*id).or_insert(0) += 1;
            }
        }
        *self.mutations.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        MutationGuard {
            shared: self,
            ids: ids.to_vec(),
        }
    }
}

pub(crate) struct MutationGuard<'a> {
    shared: &'a Shared,
    ids: Vec<ItemId>,
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        {
            let mut m = self
                .shared
                .inflight
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for id in &self.ids {
                if let Some(n) = m.get_mut(id) {
                    *n -= 1;
                    if *n == 0 {
                        m.remove(id);
                    }
                }
            }
        }
        let mut n = self
            .shared
            .mutations
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *n = n.saturating_sub(1);
        self.shared.mutations_cv.notify_all();
    }
}

pub(crate) fn encode_anchor(uuid: u128, seq: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(24);
    v.extend_from_slice(&uuid.to_le_bytes());
    v.extend_from_slice(&seq.to_le_bytes());
    v
}

pub(crate) fn decode_anchor(a: &[u8]) -> Option<(u128, u64)> {
    if a.len() != 24 {
        return None;
    }
    let uuid = u128::from_le_bytes(a[..16].try_into().ok()?);
    let seq = u64::from_le_bytes(a[16..].try_into().ok()?);
    Some((uuid, seq))
}

fn jitter(d: Duration) -> Duration {
    let f: f64 = 0.8 + 0.4 * rand::random::<f64>();
    d.mul_f64(f)
}

/// Pull a URL out of an ssh/transport message ("visit https://…").
fn find_url(msg: &str) -> Option<String> {
    let i = msg.find("https://").or_else(|| msg.find("http://"))?;
    Some(
        msg[i..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches(['.', ',', ')'])
            .to_string(),
    )
}

async fn supervise(shared: Arc<Shared>, opener: Opener) {
    let mut backoff = shared.timing.backoff_min;
    loop {
        if shared.is_shutdown() {
            break;
        }
        let held = shared.conn().hold.is_some();
        if held {
            shared.wake.notified().await;
            continue;
        }
        let interactive = shared.interactive_req.swap(false, Ordering::SeqCst);
        shared.set_conn_state(ConnState::Connecting);
        let open = tokio::spawn(opener(interactive));
        let res = match tokio::time::timeout(shared.timing.connect_timeout, open).await {
            Err(_) => Err(err(ErrorCode::Timeout, "connecting to the VM timed out")),
            Ok(Err(join)) => Err(err(ErrorCode::Offline, format!("connect failed: {join}"))),
            Ok(Ok(r)) => r,
        };
        let failure = match res {
            Ok(link) => {
                let started = Instant::now();
                match session::run(shared.clone(), link).await {
                    SessionEnd::Handshake(e) => {
                        shared.finish_attempt(Err(e.clone()));
                        e
                    }
                    SessionEnd::Ended { error, fatal } => {
                        if started.elapsed() > Duration::from_secs(10) {
                            backoff = shared.timing.backoff_min;
                        }
                        match fatal {
                            Some(e) if e.code == ErrorCode::RootReplaced => {
                                shared.set_paused(Some(format!(
                                    "The VM root folder was replaced: {}",
                                    e.msg
                                )));
                                e
                            }
                            Some(e) => e,
                            None => err(ErrorCode::Offline, error),
                        }
                    }
                }
            }
            Err(e) => {
                shared.finish_attempt(Err(e.clone()));
                e
            }
        };
        if shared.is_shutdown() {
            break;
        }
        if failure.code == ErrorCode::NeedsUser {
            // Host key / auth / 2FA / login URL: never retried automatically (D21).
            shared.conn().hold = Some(failure.msg.clone());
            shared.set_conn_state(ConnState::NeedsUser {
                reason: failure.msg.clone(),
                url: find_url(&failure.msg),
            });
            continue;
        }
        let delay = jitter(backoff);
        shared.set_conn_state(ConnState::Offline {
            error: failure.msg.clone(),
            retry_in_ms: delay.as_millis() as u64,
        });
        tokio::select! {
            _ = tokio::time::sleep(delay) => {
                backoff = (backoff * 2).min(shared.timing.backoff_max);
            }
            _ = shared.wake.notified() => {
                backoff = shared.timing.backoff_min;
            }
        }
    }
}

pub(crate) struct Inner {
    shared: Arc<Shared>,
    rt: Mutex<Option<tokio::runtime::Runtime>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    stopped: AtomicBool,
}

fn io(what: &str, e: std::io::Error) -> ProtoError {
    err(ErrorCode::Io, format!("{what}: {e}"))
}

impl Inner {
    pub(crate) fn start(cfg: EngineConfig, events: Option<EventHandler>) -> Result<Arc<Inner>> {
        let c = cfg.clone();
        let opener: Opener = Arc::new(move |interactive| {
            let c = c.clone();
            Box::pin(async move {
                let l = crate::transport::open(&c, interactive).await?;
                Ok(LinkParts {
                    reader: l.reader,
                    writer: l.writer,
                    child: l.child,
                })
            })
        });
        Self::start_with(cfg, events, opener, Timing::default())
    }

    /// Crate-private constructor with an injectable link opener (tests: an in-memory fake server).
    pub(crate) fn start_with(
        cfg: EngineConfig,
        events: Option<EventHandler>,
        opener: Opener,
        timing: Timing,
    ) -> Result<Arc<Inner>> {
        std::fs::create_dir_all(&cfg.state_dir).map_err(|e| io("create state dir", e))?;
        std::fs::create_dir_all(&cfg.temp_dir).map_err(|e| io("create temp dir", e))?;
        let db = Db::open(&cfg.state_dir.join("replica.sqlite3"))?;
        let state = db.load()?;
        let committed = state.seq;
        let cache = Cache::open(&cfg.cache_dir, cfg.cache_budget)?;
        let (apply_tx, apply_rx) = smpsc::channel();
        let (ev_tx, ev_rx) = smpsc::channel();
        let shared = Arc::new(Shared {
            client_name: sanitize_client_name(&cfg.client_name),
            prefetch: prefetch::Prefetcher::new(&cfg.prefetch),
            cfg,
            timing,
            state: RwLock::new(state),
            committed: AtomicU64::new(committed),
            replica_writes: AtomicU64::new(0),
            replica_error: Mutex::new(None),
            #[cfg(test)]
            fail_replica_writes: AtomicU64::new(0),
            apply_tx: Mutex::new(Some(apply_tx)),
            apply_queued: AtomicU64::new(0),
            session: Mutex::new(None),
            conn: Mutex::new(Conn {
                state: ConnState::Connecting,
                paused: None,
                hold: None,
                live_target: 0,
                attempts: 0,
                last_attempt: None,
                received: 0,
                last_status_emit: None,
            }),
            conn_cv: Condvar::new(),
            events: Mutex::new(Some(ev_tx)),
            signaller: Signaller::new(),
            inflight: Mutex::new(HashMap::new()),
            mutations: Mutex::new(0),
            mutations_cv: Condvar::new(),
            uploads: AtomicU64::new(0),
            rtt_us: AtomicU64::new(0),
            cache,
            wake: tokio::sync::Notify::new(),
            interactive_req: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        });
        let mut threads = Vec::new();
        threads.push(
            std::thread::Builder::new()
                .name(EVENT_THREAD.into())
                .spawn(move || event_loop(events, ev_rx))
                .map_err(|e| io("spawn event thread", e))?,
        );
        let applier = Applier::new(shared.clone(), db);
        threads.push(
            std::thread::Builder::new()
                .name("unlatch-apply".into())
                .spawn(move || applier.run(apply_rx))
                .map_err(|e| io("spawn applier", e))?,
        );
        let weak = Arc::downgrade(&shared);
        let sh = shared.clone();
        threads.push(
            std::thread::Builder::new()
                .name("unlatch-signal".into())
                .spawn(move || sh.signaller.run(weak))
                .map_err(|e| io("spawn signaller", e))?,
        );
        threads.extend(prefetch::spawn_workers(&shared)?);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("unlatch-net")
            .enable_all()
            .build()
            .map_err(|e| io("tokio runtime", e))?;
        rt.spawn(supervise(shared.clone(), opener));
        Ok(Arc::new(Inner {
            shared,
            rt: Mutex::new(Some(rt)),
            threads: Mutex::new(threads),
            stopped: AtomicBool::new(false),
        }))
    }

    pub(crate) fn status(&self) -> EngineStatus {
        self.shared.status()
    }

    /// Domain name (`Engine::name`, the IPC `Hello` check).
    pub(crate) fn name(&self) -> &str {
        &self.shared.cfg.name
    }

    pub(crate) fn connect_interactive(&self) -> Result<()> {
        let sh = &self.shared;
        if sh.current_session().is_some() {
            return Ok(());
        }
        let start = {
            let mut c = sh.conn();
            c.hold = None;
            c.attempts
        };
        sh.interactive_req.store(true, Ordering::SeqCst);
        sh.wake.notify_one();
        let deadline = Instant::now() + sh.timing.connect_timeout + sh.timing.welcome_timeout;
        let mut c = sh.conn();
        loop {
            if c.attempts > start {
                return c.last_attempt.clone().unwrap_or(Ok(()));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(err(ErrorCode::Timeout, "connect timed out"));
            }
            c = sh
                .conn_cv
                .wait_timeout(c, deadline - now)
                .map(|(g, _)| g)
                .unwrap_or_else(|p| p.into_inner().0);
        }
    }

    pub(crate) fn wait_live(&self, timeout: Duration) -> Result<()> {
        let sh = &self.shared;
        let deadline = Instant::now() + timeout;
        let mut c = sh.conn();
        loop {
            match &c.state {
                ConnState::Live if c.paused.is_none() => return Ok(()),
                ConnState::NeedsUser { reason, .. } => {
                    return Err(err(ErrorCode::NeedsUser, reason.clone()))
                }
                _ => {}
            }
            let now = Instant::now();
            if now >= deadline {
                let why = match &c.state {
                    ConnState::Offline { error, .. } => format!("not live: {error}"),
                    s => format!("not live: {s:?}"),
                };
                return Err(err(ErrorCode::Timeout, why));
            }
            c = sh
                .conn_cv
                .wait_timeout(c, deadline - now)
                .map(|(g, _)| g)
                .unwrap_or_else(|p| p.into_inner().0);
        }
    }

    pub(crate) fn server_barrier(&self, timeout: Duration) -> Result<()> {
        let sh = &self.shared;
        let s = sh.session_for(None, timeout)?;
        let (tx, rx) = smpsc::channel();
        s.request(
            Request::Ping {
                nonce: rand::random(),
            },
            Sink::Barrier(tx),
        )?;
        match rx.recv_timeout(timeout) {
            Ok(r) => r?,
            Err(smpsc::RecvTimeoutError::Timeout) => {
                return Err(err(ErrorCode::Timeout, "barrier timed out"))
            }
            Err(smpsc::RecvTimeoutError::Disconnected) => {
                return Err(err(ErrorCode::Offline, "connection lost"))
            }
        }
        // Applied and committed; now make sure the resulting signals reached the host.
        sh.signaller.flush();
        sh.flush_events();
        Ok(())
    }

    pub(crate) fn wait_idle(&self, timeout: Duration) -> Result<()> {
        let sh = &self.shared;
        let deadline = Instant::now() + timeout;
        {
            let mut n = sh.mutations.lock().unwrap_or_else(|p| p.into_inner());
            while *n > 0 {
                let now = Instant::now();
                if now >= deadline {
                    return Err(err(ErrorCode::Timeout, "mutations still pending"));
                }
                n = sh
                    .mutations_cv
                    .wait_timeout(n, deadline - now)
                    .map(|(g, _)| g)
                    .unwrap_or_else(|p| p.into_inner().0);
            }
        }
        loop {
            let (tx, rx) = smpsc::channel();
            sh.apply(ApplyMsg::Flush(tx));
            let left = deadline.saturating_duration_since(Instant::now());
            rx.recv_timeout(left)
                .map_err(|_| err(ErrorCode::Timeout, "apply queue not drained"))?;
            if sh.apply_queued.load(Ordering::Acquire) == 0 {
                sh.signaller.flush();
                sh.flush_events();
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(err(ErrorCode::Timeout, "apply queue not drained"));
            }
        }
    }

    pub(crate) fn item(&self, id: ItemId) -> Result<IpcItem> {
        self.shared.item(id)
    }

    pub(crate) fn list(
        &self,
        container: ItemId,
        cursor: Option<&[u8]>,
        limit: u32,
        viewer: bool,
    ) -> Result<Page> {
        let sh = &self.shared;
        self.ensure_container(container)?;
        // The system (re-)enumerates this folder: it now knows what is in it, so a later delete
        // of it is a new call, not a retry of an earlier one (rule 6, `DeleteSeen`).
        if cursor.is_none() && sh.read_state().delete_seen.contains_key(&container) {
            sh.apply_msg_wait(|done| ApplyMsg::DeleteSeen {
                id: container,
                entry: None,
                done,
            })?;
        }
        let (items, next) = {
            let st = sh.read_state();
            let (ids, next) = st.list_ids(container, cursor, limit as usize);
            let items: Vec<IpcItem> = ids
                .iter()
                .filter_map(|id| st.ipc_item(*id, sh.cfg.expose_exec))
                .collect();
            (items, next)
        };
        if viewer {
            sh.prefetch.offer(sh, container, &items);
        }
        Ok(Page { items, next })
    }

    /// `container` must be a known dir; its listing is fetched first when not known yet.
    fn ensure_container(&self, container: ItemId) -> Result<()> {
        let sh = &self.shared;
        let deadline = Instant::now() + sh.cfg.list_timeout;
        let (kind, complete, has_children) = loop {
            {
                let st = sh.read_state();
                if let Some(n) = st.nodes.get(&container) {
                    break (n.entry.kind, n.complete, st.child_count(container) > 0);
                }
            }
            // Maybe still coming in with the initial snapshot.
            let syncing = matches!(
                sh.conn().state,
                ConnState::Syncing { .. } | ConnState::Connecting
            );
            if !syncing || Instant::now() >= deadline {
                return Err(err(ErrorCode::NotFound, format!("no item {container}")));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        if kind != Kind::Dir {
            return Err(err(ErrorCode::NotDir, "not a directory"));
        }
        if complete {
            return Ok(());
        }
        match sh.ensure_listed(
            container,
            deadline.saturating_duration_since(Instant::now()),
        ) {
            Ok(()) => Ok(()),
            // Partial listings are fine: the rest arrives through the working set (the listing
            // keeps applying in the background) — an error would be throttled (MQ-005).
            Err(e) if e.code == ErrorCode::Timeout => Ok(()),
            Err(e) if e.code == ErrorCode::Offline && has_children => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn lookup(&self, parent: ItemId, name: &str) -> Result<IpcItem> {
        let sh = &self.shared;
        {
            let st = sh.read_state();
            if let Some(id) = st.lookup_id(parent, name) {
                if let Some(it) = st.ipc_item(id, sh.cfg.expose_exec) {
                    return Ok(it);
                }
            }
            if st.nodes.get(&parent).is_some_and(|n| n.complete) {
                return Err(err(ErrorCode::NotFound, format!("{name}: not found")));
            }
        }
        self.ensure_container(parent)?;
        let st = sh.read_state();
        st.lookup_id(parent, name)
            .and_then(|id| st.ipc_item(id, sh.cfg.expose_exec))
            .ok_or_else(|| err(ErrorCode::NotFound, format!("{name}: not found")))
    }

    pub(crate) fn materialized_changed(
        &self,
        added: &[ItemId],
        removed: &[ItemId],
        full: bool,
    ) -> Result<()> {
        let (added, removed) = (added.to_vec(), removed.to_vec());
        self.shared.apply_wait(|done| ApplyMsg::Materialized {
            added,
            removed,
            full,
            done,
        })
    }

    pub(crate) fn anchor(&self) -> Vec<u8> {
        let sh = &self.shared;
        let seq = sh.committed.load(Ordering::Acquire);
        let mut st = sh.write_state();
        // currentSyncAnchor: the system has now seen everything up to here (seen_seq, rule 6).
        st.consumed = st.consumed.max(seq);
        encode_anchor(st.replica_uuid, seq)
    }

    pub(crate) fn changes_since(&self, anchor: &[u8], limit: u32) -> Result<Changes> {
        let sh = &self.shared;
        let before = sh.committed.load(Ordering::Acquire);
        let (uuid, from) = decode_anchor(anchor)
            .ok_or_else(|| err(ErrorCode::AnchorExpired, "malformed anchor"))?;
        let res = {
            let st = sh.read_state();
            if uuid != st.replica_uuid {
                None
            } else if from < st.gc_horizon || from > before {
                // Below the tombstone GC horizon, or from the future (replica lost its tail):
                // the system re-asks from a fresh anchor (MQ-006).
                return Err(err(ErrorCode::AnchorExpired, "anchor expired"));
            } else {
                let limit = if limit == 0 { 1000 } else { limit as usize };
                Some((
                    st.changes(from, before, limit, sh.cfg.expose_exec),
                    st.replica_uuid,
                ))
            }
        };
        let Some(((updated, removed, to, more), uuid)) = res else {
            sh.emit(EngineEvent::Reimport {
                below: ItemId::ROOT,
            });
            return Err(err(ErrorCode::AnchorExpired, "anchor from another replica"));
        };
        {
            let mut st = sh.write_state();
            st.consumed = st.consumed.max(to);
        }
        // A commit landed while we were enumerating: signal again so the system comes back
        // after finishEnumeratingChanges (D5).
        if sh.committed.load(Ordering::Acquire) != before {
            sh.signaller.request();
        }
        Ok(Changes {
            updated,
            removed,
            anchor: encode_anchor(uuid, to),
            more,
        })
    }

    pub(crate) fn fetch(
        &self,
        id: ItemId,
        _version: Option<u64>,
        dest_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Fetched> {
        let sh = &self.shared;
        let item = sh.item(id)?;
        let dest = dest_dir.join(format!("unlatch-{}-{:016x}", id.0, rand::random::<u64>()));
        if item.symlink_blocked || item.entry.kind == Kind::Symlink {
            let target = sh
                .read_state()
                .nodes
                .get(&id)
                .and_then(|n| n.entry.symlink_target.clone())
                .unwrap_or_default();
            std::fs::write(&dest, target.as_bytes()).map_err(|e| io("write fetched file", e))?;
            progress(target.len() as u64, target.len() as u64);
            return Ok(Fetched { path: dest, item });
        }
        let (path, item) = sh.ensure_cached(id, progress, cancel)?;
        let ver = item.entry.version.content;
        let r = cache::clone_file(&path, &dest);
        sh.cache.unpin(id, ver);
        r.map_err(|e| {
            let code = if e.raw_os_error() == Some(libc::ENOSPC) {
                ErrorCode::NoSpace
            } else {
                ErrorCode::Io
            };
            err(code, format!("clone into {}: {e}", dest_dir.display()))
        })?;
        Ok(Fetched { path: dest, item })
    }

    pub(crate) fn read(&self, id: ItemId, offset: u64, len: u32) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let sh = &self.shared;
        let (kind, symlink) = {
            let st = sh.read_state();
            let n = st
                .nodes
                .get(&id)
                .ok_or_else(|| err(ErrorCode::NotFound, format!("no item {id}")))?;
            let t = match n.entry.kind {
                Kind::Symlink => Some(n.entry.symlink_target.clone().unwrap_or_default()),
                _ => None,
            };
            (n.entry.kind, t)
        };
        if kind == Kind::Dir {
            return Err(err(ErrorCode::IsDir, "is a directory"));
        }
        if let Some(t) = symlink {
            let b = t.as_bytes();
            let s = usize::try_from(offset).unwrap_or(usize::MAX).min(b.len());
            let e = s.saturating_add(len as usize).min(b.len());
            return Ok(b[s..e].to_vec());
        }
        let (path, item) = sh.ensure_cached(id, &|_, _| {}, &CancelToken::new())?;
        let ver = item.entry.version.content;
        let r = (|| {
            let f = std::fs::File::open(&path)?;
            let size = f.metadata()?.len();
            if offset >= size {
                return Ok(Vec::new());
            }
            let n = (len as u64).min(size - offset) as usize;
            let mut buf = vec![0u8; n];
            f.read_exact_at(&mut buf, offset)?;
            Ok(buf)
        })();
        sh.cache.unpin(id, ver);
        r.map_err(|e: std::io::Error| io("read cache", e))
    }

    pub(crate) fn create(
        &self,
        req: CreateRequest,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Modified> {
        ops::create(&self.shared, req, &ops::Xfer { progress, cancel })
    }

    pub(crate) fn modify(
        &self,
        id: ItemId,
        base: BaseVersion,
        req: ModifyRequest,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Modified> {
        ops::modify(&self.shared, id, base, req, &ops::Xfer { progress, cancel })
    }

    pub(crate) fn delete(&self, id: ItemId, base: BaseVersion, recursive: bool) -> Result<()> {
        ops::delete(&self.shared, id, base, recursive)
    }

    pub(crate) fn confirm_paused(&self, apply: bool) -> Result<()> {
        let (tx, rx) = smpsc::channel();
        self.shared.apply(ApplyMsg::Confirm { apply, done: tx });
        rx.recv_timeout(Duration::from_secs(300))
            .map_err(|_| err(ErrorCode::Io, "replica applier stopped"))?
    }

    pub(crate) fn network_changed(&self) {
        let sh = &self.shared;
        if sh.current_session().is_none() && sh.conn().hold.is_none() {
            sh.wake.notify_one();
        }
    }

    pub(crate) fn drop_connection(&self) {
        if let Some(s) = self.shared.current_session() {
            s.close();
        }
    }

    pub(crate) fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let sh = &self.shared;
        sh.shutdown.store(true, Ordering::SeqCst);
        sh.prefetch.stop();
        if let Some(s) = sh.current_session() {
            s.close();
        }
        sh.wake.notify_one();
        sh.conn_cv.notify_all();
        let (tx, rx) = smpsc::channel();
        sh.apply(ApplyMsg::Flush(tx));
        let _ = rx.recv_timeout(Duration::from_secs(30));
        sh.apply(ApplyMsg::Shutdown);
        *sh.apply_tx.lock().unwrap_or_else(|p| p.into_inner()) = None;
        sh.signaller.stop();
        *sh.events.lock().unwrap_or_else(|p| p.into_inner()) = None;
        if let Some(rt) = self.rt.lock().unwrap_or_else(|p| p.into_inner()).take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                // Called from async context (blocking there would panic).
                rt.shutdown_background();
            } else {
                rt.shutdown_timeout(Duration::from_secs(2));
            }
        }
        let me = std::thread::current().id();
        let threads: Vec<_> = self
            .threads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect();
        for t in threads {
            // The last Engine handle may be dropped inside an event handler (on an engine thread).
            if t.thread().id() != me {
                let _ = t.join();
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown();
    }
}
