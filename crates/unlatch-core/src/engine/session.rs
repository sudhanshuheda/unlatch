//! One wire session with `unlatchd`: Hello/Welcome, a reader task that routes every server frame
//! (events → applier, replies → the waiting caller by `req_id`), a writer task with an
//! interactive and a credit-limited bulk lane (rule 12, D10), credit grants for the server's bulk
//! data, and the ping-based liveness check (review §2(a)4).

use super::applier::{ApplyMsg, WelcomeInfo};
use super::Shared;
use crate::{err, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc as smpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, Notify};
use unlatch_proto::frame::{self, aio, BULK_CHUNK};
use unlatch_proto::wire::{ClientMsg, Request, Resume, ServerMsg, WelcomeMode};
use unlatch_proto::{ErrorCode, IndexId, ProtoError, PROTO_VERSION};

/// Implicit bulk grant each side starts with (wire.rs).
pub(crate) const INITIAL_CREDIT: i64 = 256 * 1024;
const MIN_WINDOW: f64 = 128.0 * 1024.0;
pub(crate) const MAX_WINDOW: f64 = 1.5 * 1024.0 * 1024.0;
/// Standing queue our own bulk data may build at the bottleneck, per direction:
/// `min(min_rtt / 16, QUEUE_TARGET_MAX)`, plus one credit unit of granularity; the delay
/// feedback term (see [`QD_LOW`]) adds or trims what the credit loop's own latency needs. The review's
/// `1.25 × rate × min_rtt` (rule 12) would let one direction queue 25 ms at 100 ms RTT, and an
/// interactive round trip crosses both directions (T13: ≤ RTT + 30 ms).
const QUEUE_TARGET_MAX: Duration = Duration::from_micros(2500);
/// Granularity of the credit loop (bytes): grants are sent as soon as one unit is owed, and
/// uploads below a 1 MiB window go in chunks of one unit (unlatchd returns upload credit per
/// chunk). About [`UNIT_TIME`] at the measured rate, within [`MIN_UNIT`, `MAX_UNIT`]: every
/// frame is atomic on the wire, so an interactive frame can wait one unit's serialization per
/// direction, and coarser steps also need more standing queue to keep the link busy.
pub(crate) const MAX_UNIT: usize = 16 * 1024;
const MIN_UNIT: usize = 4 * 1024;
const UNIT_TIME: Duration = Duration::from_millis(2);
/// Window above the implicit [`INITIAL_CREDIT`] a session starts with (granted at once). Credit
/// is charged per frame body as sent, so a 256 KiB file (the small-file and prefetch size) costs
/// a little more than 256 KiB: with a window of exactly the initial credit its last bytes waited
/// a whole credit round trip for the first grant (T7: 86 → ~100 ms at RTT 40).
const START_SLACK: f64 = MAX_UNIT as f64;
/// Delivery-rate samples kept by the windowed max filter (× [`RATE_SLICE`] = 1 s).
const BW_SAMPLES: usize = 10;
/// Queueing-delay band for the window's feedback term: an RTT probe less than `QD_LOW` above
/// min_rtt adds one credit unit per slice, one more than `QD_HIGH` above removes one. The
/// probe crosses both directions, so this bounds what an interactive round trip queues.
const QD_LOW: Duration = Duration::from_millis(4);
const QD_HIGH: Duration = Duration::from_millis(10);
/// Window gain during startup.
const STARTUP_GAIN: f64 = 1.5;
/// Rate sample length.
const RATE_SLICE: Duration = Duration::from_millis(100);
/// A slice longer than this was mostly idle: restart it instead of sampling a bogus low rate.
const IDLE_SLICE: Duration = Duration::from_millis(500);

/// RTT probes while our bulk data moves (fresh "RTT under load" samples for rule 12).
const PROBE_EVERY: Duration = Duration::from_millis(200);
/// RTT probes during a drain.
const PROBE_EVERY_DRAIN: Duration = Duration::from_millis(50);
/// A min-RTT sample older than this is re-measured (drain) — paths change (BBR: 10 s).
const MIN_RTT_LIFETIME: Duration = Duration::from_secs(10);
/// Bulk counts as active this long after our last bulk byte was sent or received.
const BULK_ACTIVE_FOR: Duration = Duration::from_millis(250);
/// A min RTT measured under load is replaced (drain) after this much continuous bulk.
const DIRTY_MIN_GRACE: Duration = Duration::from_millis(500);
/// Both bulk windows during a drain.
const DRAIN_WINDOW: f64 = 64.0 * 1024.0;
/// A drain never lasts longer than this.
const DRAIN_MAX: Duration = Duration::from_secs(1);
/// Minimum spacing between drains.
const DRAIN_SPACING: Duration = Duration::from_secs(2);

/// Session-wide RTT state: min-RTT filter, bulk activity and ProbeRTT drains.
///
/// The window formula needs the path's *unloaded* RTT. A sample taken while our own bulk data
/// is queued (e.g. the first ping of a session that starts with a snapshot) overstates it and
/// the window would then keep that queue forever. So each sample is marked clean (taken with
/// no bulk of ours in flight, or at the end of a drain) or dirty; a dirty or expired minimum
/// triggers a short drain — both windows capped at 64 KiB until a probe sent after the queues
/// had time to empty comes back (BBR's ProbeRTT).
#[derive(Debug)]
pub(crate) struct RttTracker {
    min: Option<Duration>,
    min_at: Instant,
    min_clean: bool,
    last_bulk: Option<Instant>,
    bulk_since: Option<Instant>,
    /// (start, settled_at): probes sent at or after `settled_at` measure the empty path.
    drain: Option<(Instant, Instant)>,
    last_drain_end: Option<Instant>,
    last_probe: Option<Instant>,
}

impl RttTracker {
    pub fn new(now: Instant) -> Self {
        RttTracker {
            min: None,
            min_at: now,
            min_clean: false,
            last_bulk: None,
            bulk_since: None,
            drain: None,
            last_drain_end: None,
            last_probe: None,
        }
    }

    pub fn min_rtt(&self) -> Option<Duration> {
        self.min
    }

    pub fn note_bulk(&mut self, now: Instant) {
        if !self.bulk_active(now) {
            self.bulk_since = Some(now);
        }
        self.last_bulk = Some(now);
    }

    pub fn bulk_active(&self, now: Instant) -> bool {
        self.last_bulk
            .is_some_and(|t| now.saturating_duration_since(t) < BULK_ACTIVE_FOR)
    }

    pub fn draining(&self) -> bool {
        self.drain.is_some()
    }

    /// Would a probe sent now measure the unloaded path?
    pub fn clean_now(&self, now: Instant) -> bool {
        match self.drain {
            Some((_, settled)) => now >= settled,
            None => !self.bulk_active(now),
        }
    }

    /// An RTT sample of a probe/ping sent at `sent` (`clean` as judged then). Returns true when
    /// a drain ended (windows may reopen).
    pub fn on_sample(&mut self, rtt: Duration, sent: Instant, clean: bool, now: Instant) -> bool {
        let expired = now.saturating_duration_since(self.min_at) > MIN_RTT_LIFETIME;
        let better = self.min.is_none_or(|m| rtt <= m);
        if better || expired || (clean && !self.min_clean) {
            self.min = Some(rtt);
            self.min_at = now;
            self.min_clean = clean;
        } else if self.min.is_some_and(|m| rtt <= m + m / 8) {
            // A sample close to the minimum confirms it (the path has not changed, and our own
            // queue is small), so no drain is needed to re-measure it.
            self.min_at = now;
        }
        match self.drain {
            Some((_, settled)) if clean && sent >= settled => {
                self.drain = None;
                self.last_drain_end = Some(now);
                true
            }
            _ => false,
        }
    }

    /// Periodic tick: start/stop drains and decide whether to send an RTT probe now. Returns
    /// (send a probe, the drain ended).
    pub fn tick(&mut self, now: Instant) -> (bool, bool) {
        let mut ended = false;
        if let Some((start, _)) = self.drain {
            if now.saturating_duration_since(start) >= DRAIN_MAX {
                self.drain = None;
                self.last_drain_end = Some(now);
                ended = true;
            }
        }
        let active = self.bulk_active(now);
        if active && self.drain.is_none() {
            let spaced = self
                .last_drain_end
                .is_none_or(|t| now.saturating_duration_since(t) >= DRAIN_SPACING);
            let dirty = !self.min_clean
                && self
                    .bulk_since
                    .is_some_and(|t| now.saturating_duration_since(t) >= DIRTY_MIN_GRACE);
            let old = now.saturating_duration_since(self.min_at) > MIN_RTT_LIFETIME;
            if spaced && (self.min.is_none() || dirty || old) {
                // Our queues empty within about one (possibly overstated) RTT.
                let settle = self
                    .min
                    .unwrap_or(Duration::from_millis(100))
                    .min(Duration::from_millis(250));
                self.drain = Some((now, now + settle));
            }
        }
        let every = if self.drain.is_some() {
            PROBE_EVERY_DRAIN
        } else if active {
            PROBE_EVERY
        } else {
            return (false, ended);
        };
        let due = self
            .last_probe
            .is_none_or(|t| now.saturating_duration_since(t) >= every);
        if due {
            self.last_probe = Some(now);
        }
        (due, ended)
    }
}

/// Bulk window of one direction (rule 12): `bw × (min_rtt + Q) + unit`, clamped to
/// [128 KiB, 1.5 MiB], where `bw` is the **max** delivery rate over the last second (100 ms
/// slices; BBR's bandwidth filter) and `Q = min(min_rtt / 16, 2.5 ms)`. Together with one
/// unit (≈ 2 ms) that stays under CoDel's 5 ms target on fq_codel bottlenecks: a CoDel drop in
/// our single TCP connection would head-of-line block the interactive frames for an RTT. A
/// startup phase (window = 1.5 × bw × min_rtt until bw stops growing 25% per slice for 3
/// slices, like slow start)
/// finds the rate quickly (and ends early once probes see the queue building, like HyStart);
/// two RTT samples in a row under load above `min_rtt + 25 ms` multiply a
/// back-off factor by 0.75 (recovering 10% per slice without one).
///
/// Why this bounds interactive latency: with `W ≈ bw × (min_rtt + Q)` the link stays full
/// (W ≥ one BDP) while at most `bw × Q` plus one unit of our own bulk bytes wait in any queue —
/// pipes, ssh channel, socket buffers or the bottleneck — ahead of an interactive frame. An
/// averaged rate would not do: the credit loop's latency includes the other direction's queue
/// and the grant granularity, so `W = avg_rate × RTT` is neutrally stable and sticks below the
/// link rate; the max filter keeps probing up to it.
#[derive(Debug)]
pub(crate) struct WindowCtl {
    window: f64,
    min_rtt: Option<Duration>,
    /// Newest RTT sample not yet acted on.
    fresh_rtt: Option<Duration>,
    samples: std::collections::VecDeque<f64>,
    /// Feedback term (bytes, signed): covers the credit loop's non-network latency (thread
    /// hops, pipes, ssh) that `bw × min_rtt` does not, and trims rate overestimates.
    extra: f64,
    /// Previous RTT sample (the "loaded" test uses the smaller of the last two, so one
    /// probe stalled on the daemon is not mistaken for a queue).
    prev_rtt: Option<Duration>,
    win_start: Instant,
    win_bytes: u64,
    startup: bool,
    best_rate: f64,
    plateau: u8,
    backoff: f64,
    last_sample: f64,
}

impl WindowCtl {
    pub fn new(now: Instant) -> Self {
        WindowCtl {
            window: INITIAL_CREDIT as f64 + START_SLACK,
            min_rtt: None,
            fresh_rtt: None,
            samples: std::collections::VecDeque::with_capacity(BW_SAMPLES),
            extra: 0.0,
            prev_rtt: None,
            win_start: now,
            win_bytes: 0,
            startup: true,
            best_rate: 0.0,
            plateau: 0,
            backoff: 1.0,
            last_sample: 0.0,
        }
    }

    pub fn window(&self) -> f64 {
        self.window
    }

    /// Credit unit for the current rate (see [`MAX_UNIT`]).
    pub fn unit(&self) -> usize {
        let bw = self.bw();
        if bw <= 0.0 {
            return MAX_UNIT;
        }
        ((bw * UNIT_TIME.as_secs_f64()) as usize).clamp(MIN_UNIT, MAX_UNIT) / 1024 * 1024
    }

    /// Max-filtered delivery rate (bytes/s).
    pub fn bw(&self) -> f64 {
        self.samples.iter().copied().fold(0.0, f64::max)
    }

    /// An RTT sample and the session's current min RTT ([`RttTracker`]).
    pub fn on_rtt(&mut self, rtt: Duration, min_rtt: Duration) {
        self.min_rtt = Some(min_rtt);
        self.fresh_rtt = Some(rtt);
    }

    /// `n` bulk bytes delivered (received, or acknowledged by the peer's grant) at `now`.
    pub fn on_delivered(&mut self, n: u64, now: Instant) {
        self.win_bytes += n;
        let el = now.saturating_duration_since(self.win_start);
        if el < RATE_SLICE {
            return;
        }
        if el > IDLE_SLICE {
            // Mostly idle: not a rate signal. Start a fresh slice.
            self.win_start = now;
            self.win_bytes = 0;
            return;
        }
        let r = self.win_bytes as f64 / el.as_secs_f64();
        if self.samples.len() == BW_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(r);
        self.last_sample = r;
        self.win_start = now;
        self.win_bytes = 0;
        if self.startup {
            if r >= 1.25 * self.best_rate {
                self.best_rate = r;
                self.plateau = 0;
            } else {
                self.plateau += 1;
                if self.plateau >= 3 {
                    self.startup = false;
                }
            }
        }
        self.recompute();
    }

    fn recompute(&mut self) {
        let Some(min_rtt) = self.min_rtt else { return };
        let m = min_rtt.as_secs_f64();
        let bw = self.bw();
        let unit = self.unit() as f64;
        let fresh = self.fresh_rtt.take();
        let settled = fresh.map(|r| self.prev_rtt.map_or(r, |p| p.min(r)));
        if fresh.is_some() {
            self.prev_rtt = fresh;
        }
        let loaded = settled.is_some_and(|r| r > min_rtt + Duration::from_millis(25));
        if loaded {
            // Someone queues on the path: back off (once per sample).
            self.startup = false;
            self.backoff = (self.backoff * 0.75).max(0.25);
        } else {
            self.backoff = (self.backoff * 1.1).min(1.0);
        }
        if let Some(r) = settled {
            let qd = r.saturating_sub(min_rtt);
            if self.startup && qd > QD_HIGH {
                // Delay-based startup exit (like TCP HyStart): the queue is building, so the
                // link is full; do not wait for the rate to plateau for three more slices.
                self.startup = false;
            }
            if qd < QD_LOW {
                self.extra += unit;
            } else if qd > QD_HIGH {
                self.extra -= unit;
            }
            // Signed: the rate term overstates the window after bursts (the max filter keeps
            // them for a second), and the delay signal must be able to pull it back.
            let bdp = (bw * m).max(unit);
            self.extra = self.extra.clamp(-bdp / 2.0, bdp);
        }
        let w = if self.startup {
            (STARTUP_GAIN * bw * m).max(self.window)
        } else {
            let q = (m / 16.0).min(QUEUE_TARGET_MAX.as_secs_f64());
            let w = self.backoff * (bw * (m + q) + unit + self.extra);
            // Grow at most 2× per slice, shrink at once.
            if w > self.window {
                w.min(self.window * 2.0)
            } else {
                w
            }
        };
        self.window = w.clamp(MIN_WINDOW, MAX_WINDOW);
        tracing::trace!(
            rate = self.last_sample as u64,
            bw = bw as u64,
            min_rtt_us = min_rtt.as_micros() as u64,
            loaded,
            startup = self.startup,
            extra = self.extra as i64,
            window = self.window as u64,
            "bulk window"
        );
    }
}

/// Credit we grant the server (download direction): its window is [`WindowCtl`].
#[derive(Debug)]
pub(crate) struct CreditIn {
    /// What the server may still send without a new grant (as we account it, wire bytes).
    outstanding: i64,
    ctl: WindowCtl,
}

impl CreditIn {
    pub fn new(now: Instant) -> Self {
        CreditIn {
            outstanding: INITIAL_CREDIT,
            ctl: WindowCtl::new(now),
        }
    }

    pub fn on_rtt(&mut self, rtt: Duration, min_rtt: Duration) {
        self.ctl.on_rtt(rtt, min_rtt);
    }

    /// Account `n` received bulk bytes (frame body as sent); returns a grant to send, if due.
    /// While `draining`, the server's window is held at [`DRAIN_WINDOW`].
    pub fn consume(&mut self, n: u64, now: Instant, draining: bool) -> Option<u32> {
        self.outstanding -= n as i64;
        if n > 0 {
            self.ctl.on_delivered(n, now);
        }
        let w = if draining {
            DRAIN_WINDOW as i64
        } else {
            self.ctl.window() as i64
        };
        // Top the server's window up as soon as one unit is owed: coarse grants would let the
        // bytes in flight dip below one BDP (idle link) unless the window carried more slack.
        if w - self.outstanding >= self.ctl.unit() as i64 {
            let grant = (w - self.outstanding).clamp(1, u32::MAX as i64);
            self.outstanding += grant;
            return Some(grant as u32);
        }
        None
    }

    /// A read stream ended: grant whatever is owed, even less than one unit, so the next read
    /// starts with the whole window. (Held back for granularity, up to a unit would be missing
    /// and the tail of a window-sized file would wait a credit round trip.)
    pub fn settle(&mut self, draining: bool) -> Option<u32> {
        let w = if draining {
            DRAIN_WINDOW as i64
        } else {
            self.ctl.window() as i64
        };
        let owed = (w - self.outstanding).min(u32::MAX as i64);
        if owed <= 0 {
            return None;
        }
        self.outstanding += owed;
        Some(owed as u32)
    }
}

/// A handshaken byte stream to `unlatchd` (from `transport::open`, or an in-memory pipe in tests).
pub(crate) struct LinkParts {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    pub child: Option<tokio::process::Child>,
}

/// What a caller waiting on a request receives.
#[derive(Debug)]
pub(crate) enum Reply {
    Resp(unlatch_proto::wire::Response),
    Chunk {
        offset: u64,
        data: Vec<u8>,
        last: bool,
        version: u64,
    },
    Err(ProtoError),
}

/// Where the reply of one `req_id` goes.
pub(crate) enum Sink {
    Chan(smpsc::Sender<Reply>),
    /// Liveness ping (answer only updates RTT / last-heard). `clean`: sent with none of our
    /// bulk data queued ([`RttTracker::clean_now`]).
    Ping {
        sent: Instant,
        clean: bool,
    },
    /// RTT probe (`Stat` of the root: no server-side effects) for the bulk windows.
    Probe {
        sent: Instant,
        clean: bool,
    },
    /// `server_barrier`: after the Pong, the applier drains everything received before it.
    Barrier(smpsc::Sender<Result<()>>),
    /// `ListDir`: parts go to the applier (LWW with events); `since` = server seq applied when
    /// the request was sent (children not listed and not newer than it are dropped).
    Listing {
        since: u64,
        done: Option<smpsc::Sender<Result<()>>>,
    },
}

struct BulkFrame {
    frame: Vec<u8>,
    cost: i64,
}

/// Upload direction: the credit the server granted us (for `WriteChunk`s), plus our own sender
/// window. The server returns credit as it consumes chunks, so `inflight` (sent, not yet
/// returned) is what sits in pipes, ssh and socket buffers and the network; the writer keeps it
/// within [`WindowCtl`]'s window even when the server's grant is larger.
struct CreditOut {
    avail: AtomicI64,
    inflight: AtomicI64,
    ctl: Mutex<WindowCtl>,
    /// Session-wide RTT state (shared with the download side through the handle).
    rtt: Mutex<RttTracker>,
    notify: Notify,
}

impl CreditOut {
    fn new(now: Instant) -> Self {
        CreditOut {
            avail: AtomicI64::new(INITIAL_CREDIT),
            inflight: AtomicI64::new(0),
            ctl: Mutex::new(WindowCtl::new(now)),
            rtt: Mutex::new(RttTracker::new(now)),
            notify: Notify::new(),
        }
    }

    fn ctl(&self) -> std::sync::MutexGuard<'_, WindowCtl> {
        self.ctl.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn rtt(&self) -> std::sync::MutexGuard<'_, RttTracker> {
        self.rtt.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A `Credit` from the server: returned bytes shrink `inflight` (and are a delivery-rate
    /// sample); anything beyond is a window extension.
    fn on_grant(&self, n: u32, now: Instant) {
        let n = n as i64;
        let inflight = self.inflight.load(Ordering::Acquire);
        let returned = n.min(inflight.max(0));
        if returned > 0 {
            self.inflight.fetch_sub(returned, Ordering::AcqRel);
            self.ctl().on_delivered(returned as u64, now);
        }
        self.avail.fetch_add(n, Ordering::AcqRel);
        self.notify.notify_one();
    }

    /// Upload chunk size: one credit unit while the window is small (fine-grained in-flight
    /// accounting), full 64 KiB frames on fast paths.
    fn chunk_size(&self) -> usize {
        let c = self.ctl();
        if c.window() >= 1024.0 * 1024.0 {
            BULK_CHUNK
        } else {
            c.unit()
        }
    }

    /// May a bulk frame of `cost` go out now?
    fn may_send(&self, cost: i64) -> bool {
        if cost == 0 {
            return true;
        }
        if self.avail.load(Ordering::Acquire) < cost {
            return false;
        }
        let inflight = self.inflight.load(Ordering::Acquire);
        let window = if self.rtt().draining() {
            DRAIN_WINDOW
        } else {
            self.ctl().window()
        };
        inflight <= 0 || (inflight + cost) as f64 <= window
    }

    fn sent(&self, cost: i64) {
        self.avail.fetch_sub(cost, Ordering::AcqRel);
        self.inflight.fetch_add(cost, Ordering::AcqRel);
        if cost > 0 {
            self.rtt().note_bulk(Instant::now());
        }
    }
}

/// The live session's handle (callers send requests through it).
pub(crate) struct SessionHandle {
    pub index: IndexId,
    inter: mpsc::UnboundedSender<Vec<u8>>,
    bulk: mpsc::Sender<BulkFrame>,
    pending: Mutex<HashMap<u32, Sink>>,
    next_id: AtomicU32,
    closed: AtomicBool,
    kill: Notify,
    last_rx: AtomicU64,
    epoch: Instant,
    credit_in: Mutex<CreditIn>,
    credit_out: Arc<CreditOut>,
}

fn offline(msg: &str) -> ProtoError {
    err(ErrorCode::Offline, msg)
}

impl SessionHandle {
    fn new(
        index: IndexId,
        inter: mpsc::UnboundedSender<Vec<u8>>,
        bulk: mpsc::Sender<BulkFrame>,
    ) -> Self {
        let now = Instant::now();
        SessionHandle {
            index,
            inter,
            bulk,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            closed: AtomicBool::new(false),
            kill: Notify::new(),
            last_rx: AtomicU64::new(0),
            epoch: now,
            credit_in: Mutex::new(CreditIn::new(now)),
            credit_out: Arc::new(CreditOut::new(now)),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Sink>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn send_msg(&self, msg: &ClientMsg) -> Result<()> {
        let f = frame::encode(msg, true).map_err(|e| err(ErrorCode::Protocol, e.to_string()))?;
        self.inter.send(f).map_err(|_| offline("connection closed"))
    }

    /// Send a request; its reply (or replies) go to `sink`.
    pub fn request(&self, req: Request, sink: Sink) -> Result<u32> {
        if self.is_closed() {
            return Err(offline("connection closed"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).max(1);
        self.pending().insert(id, sink);
        if let Err(e) = self.send_msg(&ClientMsg::Request { req_id: id, req }) {
            self.pending().remove(&id);
            return Err(e);
        }
        Ok(id)
    }

    /// Size for the next upload chunk (≤ [`BULK_CHUNK`]).
    pub fn upload_chunk_size(&self) -> usize {
        self.credit_out.chunk_size()
    }

    /// Queue one upload chunk on the bulk lane (blocks while the lane is full).
    pub fn send_chunk(&self, req_id: u32, data: Vec<u8>, last: bool) -> Result<()> {
        debug_assert!(data.len() <= BULK_CHUNK);
        let cost = data.len() as i64;
        let f = frame::encode(&ClientMsg::WriteChunk { req_id, data, last }, true)
            .map_err(|e| err(ErrorCode::Protocol, e.to_string()))?;
        if self.is_closed() {
            return Err(offline("connection closed"));
        }
        self.bulk
            .blocking_send(BulkFrame { frame: f, cost })
            .map_err(|_| offline("connection closed"))
    }

    /// Abort a request (a `Read` stops streaming; a `Write` discards staged data).
    pub fn cancel(&self, req_id: u32) {
        let _ = self.send_msg(&ClientMsg::Cancel { req_id });
    }

    pub fn forget(&self, req_id: u32) {
        self.pending().remove(&req_id);
    }

    /// Kill the session (pending callers see `Offline`).
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.kill.notify_one();
        self.pending().clear();
    }

    fn touch(&self) {
        self.last_rx
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn since_rx(&self) -> Duration {
        let last = Duration::from_millis(self.last_rx.load(Ordering::Relaxed));
        self.epoch.elapsed().saturating_sub(last)
    }

    fn grant(&self, bytes: u64) {
        self.grant_ex(bytes, false);
    }

    /// [`Self::grant`]; `settle`: a read stream ended, return everything owed
    /// ([`CreditIn::settle`]).
    fn grant_ex(&self, bytes: u64, settle: bool) {
        let now = Instant::now();
        let draining = {
            let mut r = self.credit_out.rtt();
            if bytes > 0 {
                r.note_bulk(now);
            }
            r.draining()
        };
        let g = {
            let mut c = self.credit_in.lock().unwrap_or_else(|p| p.into_inner());
            let g = c.consume(bytes, now, draining);
            let s = if settle { c.settle(draining) } else { None };
            match (g, s) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (a, b) => a.or(b),
            }
        };
        if let Some(bulk_bytes) = g {
            let _ = self.send_msg(&ClientMsg::Credit { bulk_bytes });
        }
    }

    /// An RTT sample from a ping or probe sent at `sent`.
    fn on_rtt_sample(&self, sent: Instant, clean: bool) {
        let now = Instant::now();
        let rtt = now.saturating_duration_since(sent);
        tracing::trace!(rtt_us = rtt.as_micros() as u64, clean, "rtt sample");
        let (min, drain_ended) = {
            let mut r = self.credit_out.rtt();
            let ended = r.on_sample(rtt, sent, clean, now);
            (r.min_rtt().unwrap_or(rtt), ended)
        };
        self.credit_in
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .on_rtt(rtt, min);
        self.credit_out.ctl().on_rtt(rtt, min);
        if drain_ended {
            self.reopen();
        }
    }

    /// A drain ended: let the writer and the server's window move again.
    fn reopen(&self) {
        self.credit_out.notify.notify_one();
        self.grant(0);
    }

    /// Probe tick (see [`RttTracker::tick`]).
    fn probe_tick(&self) {
        let now = Instant::now();
        let (probe, ended, clean) = {
            let mut r = self.credit_out.rtt();
            let (probe, ended) = r.tick(now);
            (probe, ended, r.clean_now(now))
        };
        if ended {
            self.reopen();
        }
        if probe {
            let _ = self.request(
                Request::Stat {
                    id: unlatch_proto::ItemId::ROOT,
                },
                Sink::Probe { sent: now, clean },
            );
        }
    }
}

/// Credit measure of a received bulk frame (`unlatch_proto::wire`, "Credit measure"): its body
/// length as sent — flags byte + payload, i.e. the *compressed* size of an LZ4 frame, without
/// the 4-byte length prefix. That is exactly what unlatchd charges, so the window bounds the wire
/// bytes queued below its scheduler (pipes, ssh window) even for highly compressible data;
/// granting the decompressed size would let it run ahead by the compression ratio.
pub(crate) fn credit_len(body: &[u8]) -> u64 {
    body.len() as u64
}

pub(crate) enum SessionEnd {
    /// Never got a Welcome.
    Handshake(ProtoError),
    /// Session ran (possibly long) and ended.
    Ended {
        error: String,
        fatal: Option<ProtoError>,
    },
}

async fn writer_task(
    mut w: Box<dyn AsyncWrite + Send + Unpin>,
    mut inter_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut bulk_rx: mpsc::Receiver<BulkFrame>,
    credit: Arc<CreditOut>,
) -> std::io::Result<()> {
    let mut pending_bulk: Option<BulkFrame> = None;
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    loop {
        // Interactive lane always first.
        while let Ok(f) = inter_rx.try_recv() {
            buf.extend_from_slice(&f);
            if buf.len() >= 256 * 1024 {
                break;
            }
        }
        if !buf.is_empty() {
            w.write_all(&buf).await?;
            buf.clear();
            continue;
        }
        if pending_bulk.is_none() {
            if let Ok(b) = bulk_rx.try_recv() {
                pending_bulk = Some(b);
            }
        }
        if let Some(b) = &pending_bulk {
            if credit.may_send(b.cost) {
                credit.sent(b.cost);
                if let Some(b) = pending_bulk.take() {
                    w.write_all(&b.frame).await?;
                }
                continue;
            }
        }
        w.flush().await?;
        tokio::select! {
            biased;
            f = inter_rx.recv() => match f {
                Some(f) => buf.extend_from_slice(&f),
                None => return Ok(()),
            },
            _ = credit.notify.notified(), if pending_bulk.is_some() => {}
            b = bulk_rx.recv(), if pending_bulk.is_none() => {
                if let Some(b) = b { pending_bulk = Some(b) }
            }
        }
    }
}

fn map_frame_err(e: frame::FrameError) -> ProtoError {
    match e {
        frame::FrameError::Io(e) => offline(&format!("connection lost: {e}")),
        other => err(ErrorCode::Protocol, other.to_string()),
    }
}

/// Run one session to completion. Installs the handle in `shared.session` once Welcome is
/// processed; clears it on exit.
pub(crate) async fn run(shared: Arc<Shared>, link: LinkParts) -> SessionEnd {
    let LinkParts {
        mut reader,
        mut writer,
        child,
    } = link;
    let _child = ChildGuard(child);
    // ---- Hello
    let (resume, expect_index) = {
        let st = shared.read_state();
        let resume = match (st.index, st.snapshot_complete) {
            (Some(index), true) => Some(Resume {
                index,
                seq: st.server_seq,
            }),
            _ => None,
        };
        (resume, st.index)
    };
    let resume_seq = resume.as_ref().map(|r| r.seq);
    let hello = ClientMsg::Hello {
        proto: PROTO_VERSION,
        root: shared.cfg.remote_root.clone(),
        resume,
        expect_index,
        default_lazy_names: shared.cfg.default_lazy_names.clone(),
        client_name: shared.client_name.clone(),
    };
    if let Err(e) = aio::write(&mut writer, &hello, false).await {
        return SessionEnd::Handshake(map_frame_err(e));
    }
    if let Err(e) = writer.flush().await {
        return SessionEnd::Handshake(offline(&format!("connection lost: {e}")));
    }
    // ---- Welcome
    let first = match tokio::time::timeout(
        shared.timing.welcome_timeout,
        aio::read::<_, ServerMsg>(&mut reader),
    )
    .await
    {
        Err(_) => {
            return SessionEnd::Handshake(err(ErrorCode::Timeout, "no Welcome from unlatchd"))
        }
        Ok(Err(e)) => return SessionEnd::Handshake(map_frame_err(e)),
        Ok(Ok(None)) => return SessionEnd::Handshake(offline("unlatchd closed the connection")),
        Ok(Ok(Some(m))) => m,
    };
    let (index, seq, mode) = match first {
        ServerMsg::Welcome {
            proto,
            index,
            seq,
            mode,
            root,
            info,
            lazy_names: _,
        } => {
            if proto != PROTO_VERSION {
                return SessionEnd::Handshake(err(
                    ErrorCode::Protocol,
                    format!("unlatchd speaks proto {proto}"),
                ));
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            shared.apply(ApplyMsg::Welcome(
                Box::new(WelcomeInfo {
                    index,
                    mode: mode.clone(),
                    root,
                    info,
                }),
                tx,
            ));
            if rx.await.is_err() {
                return SessionEnd::Handshake(err(ErrorCode::Io, "applier stopped"));
            }
            (index, seq, mode)
        }
        ServerMsg::Error { err: e, .. } => return SessionEnd::Handshake(e),
        _ => return SessionEnd::Handshake(err(ErrorCode::Protocol, "expected Welcome")),
    };
    let (inter_tx, inter_rx) = mpsc::unbounded_channel();
    let (bulk_tx, bulk_rx) = mpsc::channel(8);
    let handle = Arc::new(SessionHandle::new(index, inter_tx, bulk_tx));
    handle.touch();
    // The window starts [`START_SLACK`] above the implicit initial credit: grant it now.
    handle.grant(0);
    shared.install_session(Some(handle.clone()));
    match mode {
        // Live once the snapshot is complete (the Welcome apply cleared snapshot_complete).
        WelcomeMode::Snapshot => shared.begin_sync(0),
        WelcomeMode::Resume if resume_seq.is_some_and(|r| r >= seq) => shared.begin_sync(0),
        WelcomeMode::Resume => {
            // Live when the replay reaches Welcome.seq, or — since the replay's last Events.seq
            // need not equal Welcome.seq — once a Ping sent now comes back and everything the
            // server queued before it has been applied.
            shared.begin_sync(seq);
            let (tx, rx) = smpsc::channel();
            if handle
                .request(
                    Request::Ping {
                        nonce: rand::random(),
                    },
                    Sink::Barrier(tx),
                )
                .is_ok()
            {
                let sh = shared.clone();
                let wait = shared.timing.welcome_timeout * 4;
                tokio::task::spawn_blocking(move || {
                    if let Ok(Ok(())) = rx.recv_timeout(wait) {
                        sh.caught_up();
                    }
                });
            }
        }
    }

    let mut writer_h = tokio::spawn(writer_task(
        writer,
        inter_rx,
        bulk_rx,
        handle.credit_out.clone(),
    ));
    let fatal: Arc<Mutex<Option<ProtoError>>> = Arc::new(Mutex::new(None));
    let mut reader_h = {
        let shared = shared.clone();
        let handle = handle.clone();
        let fatal = fatal.clone();
        tokio::spawn(async move { reader_loop(shared, handle, reader, fatal).await })
    };

    let mut tick = tokio::time::interval(shared.timing.ping_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // RTT probes / drains for the bulk windows (cheap no-op while no bulk data moves).
    let mut probe_tick = tokio::time::interval(PROBE_EVERY_DRAIN);
    probe_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let error: String = loop {
        tokio::select! {
            _ = probe_tick.tick() => handle.probe_tick(),
            _ = tick.tick() => {
                if handle.is_closed() { break "connection dropped".into(); }
                let now = Instant::now();
                let clean = handle.credit_out.rtt().clean_now(now);
                let _ = handle.request(Request::Ping { nonce: rand::random() }, Sink::Ping { sent: now, clean });
                // Dead = nothing received for `dead_after` AND a ping written ≥ `dead_after` ago
                // is still unanswered (review §2(a)4) — a busy link is never declared dead.
                let dead_after = shared.timing.dead_after;
                let stale_ping = handle.pending().values().any(|s| matches!(s, Sink::Ping { sent, .. } if sent.elapsed() >= dead_after));
                if handle.since_rx() >= dead_after && stale_ping {
                    break "unlatchd stopped responding".into();
                }
            }
            r = &mut reader_h => {
                break match r { Ok(Ok(())) => "unlatchd closed the connection".into(), Ok(Err(e)) => e.msg, Err(e) => e.to_string() };
            }
            r = &mut writer_h => {
                break match r { Ok(Ok(())) => "writer closed".into(), Ok(Err(e)) => format!("write failed: {e}"), Err(e) => e.to_string() };
            }
            _ = handle.kill.notified() => break "connection dropped".into(),
        }
    };
    handle.close();
    shared.install_session(None);
    reader_h.abort();
    writer_h.abort();
    shared.apply(ApplyMsg::SessionEnded);
    let fatal = fatal.lock().unwrap_or_else(|p| p.into_inner()).take();
    SessionEnd::Ended { error, fatal }
}

struct ChildGuard(Option<tokio::process::Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            let _ = c.start_kill();
        }
    }
}

async fn reader_loop(
    shared: Arc<Shared>,
    handle: Arc<SessionHandle>,
    mut reader: Box<dyn AsyncRead + Send + Unpin>,
    fatal: Arc<Mutex<Option<ProtoError>>>,
) -> Result<()> {
    let mut batch: Vec<unlatch_proto::wire::Change> = Vec::new();
    loop {
        let body = match aio::read_body(&mut reader).await {
            Ok(Some(b)) => b,
            Ok(None) => return Ok(()),
            Err(e) => return Err(map_frame_err(e)),
        };
        handle.touch();
        // Credit is measured in frame bodies as sent (wire.rs), not decompressed bytes.
        let wire = credit_len(&body);
        let msg: ServerMsg =
            frame::decode_body(&body).map_err(|e| err(ErrorCode::Protocol, e.to_string()))?;
        match msg {
            ServerMsg::Welcome { .. } => {
                return Err(err(ErrorCode::Protocol, "unexpected second Welcome"))
            }
            ServerMsg::SnapshotChunk {
                entries,
                complete_dirs,
            } => {
                handle.grant(wire);
                shared.note_received(entries.len() as u64);
                shared.apply(ApplyMsg::Chunk {
                    entries,
                    complete_dirs,
                });
            }
            ServerMsg::SnapshotDone { seq } => shared.apply(ApplyMsg::SnapDone { seq }),
            ServerMsg::Events {
                seq,
                changes,
                batch_end,
            } => {
                batch.extend(changes);
                if batch_end {
                    shared.apply(ApplyMsg::Events {
                        seq,
                        changes: std::mem::take(&mut batch),
                    });
                }
            }
            ServerMsg::Credit { bulk_bytes } => {
                handle.credit_out.on_grant(bulk_bytes, Instant::now());
            }
            ServerMsg::ReadChunk {
                req_id,
                offset,
                data,
                last,
                version,
            } => {
                handle.grant_ex(wire, last);
                let mut p = handle.pending();
                if let Some(Sink::Chan(tx)) = p.get(&req_id) {
                    let gone = tx
                        .send(Reply::Chunk {
                            offset,
                            data,
                            last,
                            version,
                        })
                        .is_err();
                    if last || gone {
                        p.remove(&req_id);
                    }
                }
            }
            ServerMsg::Response { req_id, resp } => {
                let listing = matches!(resp, unlatch_proto::wire::Response::ListingPart { .. });
                if listing {
                    handle.grant(wire);
                }
                let mut p = handle.pending();
                match p.remove(&req_id) {
                    Some(Sink::Chan(tx)) => {
                        let _ = tx.send(Reply::Resp(resp));
                    }
                    Some(Sink::Ping { sent, clean }) => {
                        shared.note_rtt(sent.elapsed());
                        drop(p);
                        handle.on_rtt_sample(sent, clean);
                    }
                    Some(Sink::Probe { sent, clean }) => {
                        drop(p);
                        handle.on_rtt_sample(sent, clean);
                    }
                    Some(Sink::Barrier(tx)) => {
                        shared.apply(ApplyMsg::Barrier(tx));
                    }
                    Some(Sink::Listing { since, mut done }) => match resp {
                        unlatch_proto::wire::Response::ListingPart { dir, entries, last } => {
                            let d = if last { done.take() } else { None };
                            shared.apply(ApplyMsg::Listing {
                                dir,
                                entries,
                                last,
                                since,
                                done: d,
                            });
                            if !last {
                                p.insert(req_id, Sink::Listing { since, done });
                            }
                        }
                        _ => {
                            if let Some(d) = done {
                                let _ = d.send(Err(err(
                                    ErrorCode::Protocol,
                                    "unexpected ListDir reply",
                                )));
                            }
                        }
                    },
                    None => {}
                }
            }
            ServerMsg::Error {
                req_id: Some(req_id),
                err: e,
            } => {
                let mut p = handle.pending();
                match p.remove(&req_id) {
                    Some(Sink::Chan(tx)) => {
                        let _ = tx.send(Reply::Err(e));
                    }
                    Some(Sink::Barrier(tx)) => {
                        let _ = tx.send(Err(e));
                    }
                    Some(Sink::Listing { done: Some(d), .. }) => {
                        let _ = d.send(Err(e));
                    }
                    _ => {}
                }
            }
            ServerMsg::Error {
                req_id: None,
                err: e,
            } => {
                let msg = e.msg.clone();
                *fatal.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.clone());
                return Err(err(e.code, msg));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// Feed `ctl` a steady delivery of `rate` B/s for `secs`, in 10 ms steps.
    fn feed(ctl: &mut WindowCtl, t: &mut Instant, rate: f64, secs: f64) {
        for _ in 0..(secs * 100.0) as u32 {
            *t += 10 * MS;
            ctl.on_delivered((rate / 100.0) as u64, *t);
        }
    }

    #[test]
    fn window_tracks_rate_times_rtt_plus_queue_target() {
        let t0 = Instant::now();
        let mut t = t0;
        // 40 ms RTT, 5 MB/s: startup, then bw × (40 + 2.5) ms + a unit (+ feedback) ≈ 230 KB.
        let mut c = WindowCtl::new(t0);
        c.on_rtt(40 * MS, 40 * MS);
        feed(&mut c, &mut t, 5e6, 3.0);
        assert!(!c.startup, "startup ends when the rate plateaus");
        let w = c.window();
        assert!((230e3..250e3).contains(&w), "{w}");
        // 100 ms RTT, 1.25 MB/s: queue target capped at 2.5 ms → 1.25 MB/s × 102.5 ms + 4 KiB units.
        let mut c = WindowCtl::new(t0);
        c.on_rtt(100 * MS, 100 * MS);
        feed(&mut c, &mut t, 1.25e6, 3.0);
        let w = c.window();
        assert!((133e3..145e3).contains(&w), "{w}");
        assert_eq!(c.unit(), 4096, "≈ 2 ms at 1.25 MB/s, floored at 4 KiB");
        // Fast link: clamped to 1.5 MiB.
        let mut c = WindowCtl::new(t0);
        c.on_rtt(40 * MS, 40 * MS);
        feed(&mut c, &mut t, 100e6, 3.0);
        assert_eq!(c.window(), MAX_WINDOW);
        // ~0 RTT: floor of 128 KiB.
        let mut c = WindowCtl::new(t0);
        c.on_rtt(Duration::from_micros(50), Duration::from_micros(50));
        feed(&mut c, &mut t, 1e6, 2.0);
        assert_eq!(c.window(), MIN_WINDOW);
    }

    #[test]
    fn loaded_rtt_shrinks_once_per_sample() {
        let t0 = Instant::now();
        let mut t = t0;
        let mut c = WindowCtl::new(t0);
        c.on_rtt(40 * MS, 40 * MS);
        feed(&mut c, &mut t, 10e6, 3.0);
        let before = c.window();
        // One stalled probe (e.g. the daemon was busy) is not a loaded path…
        c.on_rtt(200 * MS, 40 * MS);
        t += 100 * MS;
        c.on_delivered(1_000_000, t);
        assert!(c.window() > 0.9 * before, "{before} → {}", c.window());
        // …two in a row are: back off ×0.75 (plus the delay term's one unit).
        let mid = c.window();
        c.on_rtt(200 * MS, 40 * MS);
        t += 100 * MS;
        c.on_delivered(1_000_000, t);
        let shrunk = c.window();
        assert!(shrunk < 0.8 * mid, "{mid} → {shrunk}");
        // Samples are consumed once: without new ones the window recovers.
        feed(&mut c, &mut t, 10e6, 0.5);
        assert!(c.window() > shrunk, "recovers after the loaded samples");
        assert_eq!(c.min_rtt, Some(40 * MS));
    }

    #[test]
    fn max_filter_forgets_old_peaks() {
        let t0 = Instant::now();
        let mut t = t0;
        let mut c = WindowCtl::new(t0);
        c.on_rtt(40 * MS, 40 * MS);
        feed(&mut c, &mut t, 10e6, 3.0);
        let high = c.window();
        // The path slows down (e.g. shared with another flow): within ~1 s the window follows.
        feed(&mut c, &mut t, 2e6, 1.5);
        assert!(c.window() < high / 3.0, "{high} → {}", c.window());
    }

    #[test]
    fn startup_ends_when_the_queue_builds() {
        let t0 = Instant::now();
        let mut t = t0;
        let mut c = WindowCtl::new(t0);
        c.on_rtt(40 * MS, 40 * MS);
        t += 100 * MS;
        c.on_delivered(500_000, t);
        assert!(c.startup);
        // Rate still growing, but two probes see 30 ms of queue: leave startup now.
        c.on_rtt(70 * MS, 40 * MS);
        t += 100 * MS;
        c.on_delivered(1_000_000, t);
        c.on_rtt(70 * MS, 40 * MS);
        t += 100 * MS;
        c.on_delivered(2_000_000, t);
        assert!(!c.startup);
    }

    #[test]
    fn idle_gap_is_not_a_rate_sample() {
        let t0 = Instant::now();
        let mut t = t0;
        let mut c = WindowCtl::new(t0);
        c.on_rtt(40 * MS, 40 * MS);
        feed(&mut c, &mut t, 5e6, 3.0);
        let w = c.window();
        t += Duration::from_secs(10);
        c.on_delivered(1000, t);
        assert_eq!(c.window(), w);
    }

    #[test]
    fn grants_keep_server_window_open() {
        let t0 = Instant::now();
        let mut c = CreditIn::new(t0);
        let mut server_credit: i64 = INITIAL_CREDIT;
        // The server sends whenever it has credit; we must never starve it.
        for _ in 0..1000 {
            assert!(server_credit >= 64 * 1024, "server starved");
            server_credit -= 64 * 1024;
            if let Some(g) = c.consume(64 * 1024, t0, false) {
                server_credit += g as i64;
            }
        }
    }

    /// Wire cost (credit) of a file of `size` incompressible bytes streamed in 64 KiB chunks.
    fn read_costs(size: usize) -> Vec<u64> {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut costs = Vec::new();
        let mut off = 0;
        while off < size {
            let n = (size - off).min(BULK_CHUNK);
            let data: Vec<u8> = (0..n)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect();
            let f = frame::encode(
                &ServerMsg::ReadChunk {
                    req_id: 7,
                    offset: off as u64,
                    data,
                    last: off + n == size,
                    version: 1,
                },
                true,
            )
            .expect("encode");
            costs.push(credit_len(&f[4..]));
            off += n;
        }
        costs
    }

    #[test]
    fn a_small_file_read_never_waits_for_a_grant() {
        // T7: framing makes a 256 KiB file cost a little more than 256 KiB of credit. With the
        // start grant and the settle at the end of each read, every such read finds the whole
        // file's credit at the server, even when the grants sent during a read arrive only
        // after it ended (one RTT later), and after sub-unit remainders from small reads.
        let t0 = Instant::now();
        let mut c = CreditIn::new(t0);
        let mut server: i64 = INITIAL_CREDIT;
        server += c.consume(0, t0, false).expect("start grant") as i64;
        let big = read_costs(256 * 1024);
        assert!(
            big.iter().sum::<u64>() > INITIAL_CREDIT as u64,
            "framing costs credit"
        );
        for size in [
            4096,
            256 * 1024,
            3000,
            5000,
            256 * 1024,
            256 * 1024,
            12_000,
            256 * 1024,
        ] {
            let costs = if size == 256 * 1024 {
                big.clone()
            } else {
                read_costs(size)
            };
            let total: u64 = costs.iter().sum();
            assert!(
                server >= total as i64,
                "a {size} B read would wait for a grant: server has {server}, needs {total}"
            );
            let mut granted = 0i64;
            for (i, &k) in costs.iter().enumerate() {
                server -= k as i64;
                granted += c.consume(k, t0, false).map_or(0, |g| g as i64);
                if i + 1 == costs.len() {
                    granted += c.settle(false).map_or(0, |g| g as i64);
                }
            }
            server += granted;
        }
    }

    #[test]
    fn credit_is_the_body_length_as_sent() {
        // A highly compressible chunk: the grant is its compressed body, not the 64 KiB it
        // decodes to (wire.rs "Credit measure"; unlatchd charges `frame.len() - 4`).
        let f = frame::encode(
            &ServerMsg::ReadChunk {
                req_id: 1,
                offset: 0,
                data: vec![0u8; BULK_CHUNK],
                last: true,
                version: 1,
            },
            true,
        )
        .expect("encode");
        let body = &f[4..];
        assert_ne!(body[0] & frame::FLAG_LZ4, 0, "chunk was compressed");
        assert_eq!(credit_len(body), (f.len() - 4) as u64);
        assert!(
            credit_len(body) < (BULK_CHUNK / 16) as u64,
            "{}",
            credit_len(body)
        );
        assert_eq!(credit_len(&[0, 1, 2, 3]), 4);
    }

    #[test]
    fn upload_window_limits_inflight() {
        let t0 = Instant::now();
        let c = CreditOut::new(t0);
        // The server's post-Welcome extension: no bytes in flight, so no rate sample.
        c.on_grant(768 * 1024, t0);
        assert_eq!(c.avail.load(Ordering::Acquire), 1024 * 1024);
        assert_eq!(c.inflight.load(Ordering::Acquire), 0);
        // Initial window 256 KiB: four 64 KiB chunks, then wait.
        let mut sent = 0;
        while c.may_send(64 * 1024) {
            c.sent(64 * 1024);
            sent += 1;
        }
        assert_eq!(sent, 4);
        assert!(c.may_send(0), "an empty last chunk always goes");
        c.on_grant(64 * 1024, t0 + 10 * MS);
        assert!(c.may_send(64 * 1024));
        assert_eq!(c.inflight.load(Ordering::Acquire), 192 * 1024);
    }

    #[test]
    fn dirty_min_rtt_triggers_a_drain_that_ends_on_a_clean_probe() {
        let t0 = Instant::now();
        let mut r = RttTracker::new(t0);
        // The session starts with bulk data moving (a snapshot): the first sample is dirty.
        r.note_bulk(t0);
        assert!(!r.clean_now(t0));
        let (probe, _) = r.tick(t0);
        assert!(probe, "probes while bulk moves");
        // No min yet → a drain starts at once.
        assert!(r.draining());
        let mut t = t0;
        // Probes during the settle time are not clean.
        t += 20 * MS;
        r.note_bulk(t);
        assert!(!r.clean_now(t));
        assert!(!r.on_sample(190 * MS, t, false, t + 190 * MS));
        assert_eq!(r.min_rtt(), Some(190 * MS));
        // After the settle time a probe is clean; its answer ends the drain and fixes min_rtt.
        t += 100 * MS;
        assert!(r.clean_now(t));
        assert!(r.on_sample(41 * MS, t, true, t + 41 * MS));
        assert!(!r.draining());
        assert_eq!(r.min_rtt(), Some(41 * MS));
        // A later, larger (loaded) sample never raises a clean, fresh minimum.
        r.on_sample(170 * MS, t + 200 * MS, false, t + 370 * MS);
        assert_eq!(r.min_rtt(), Some(41 * MS));
        // Continuous bulk for > 10 s without a clean confirmation: expires → re-measured.
        let mut t2 = t + 400 * MS;
        let mut drained = false;
        for _ in 0..300 {
            t2 += 50 * MS;
            r.note_bulk(t2);
            r.tick(t2);
            if r.draining() {
                drained = true;
                break;
            }
        }
        assert!(drained, "min RTT older than 10 s is re-measured");
    }

    #[test]
    fn idle_link_needs_no_probes() {
        let t0 = Instant::now();
        let mut r = RttTracker::new(t0);
        assert_eq!(r.tick(t0), (false, false));
        assert!(r.clean_now(t0));
        r.on_sample(40 * MS, t0, true, t0 + 40 * MS);
        r.note_bulk(t0 + 50 * MS);
        // A clean minimum: no drain when bulk starts.
        let (probe, _) = r.tick(t0 + 100 * MS);
        assert!(probe);
        assert!(!r.draining());
    }

    #[test]
    fn drain_holds_both_windows() {
        let t0 = Instant::now();
        let c = CreditOut::new(t0);
        c.on_grant(768 * 1024, t0);
        c.rtt().note_bulk(t0);
        c.rtt().tick(t0);
        assert!(c.rtt().draining());
        c.sent(64 * 1024);
        assert!(!c.may_send(64 * 1024), "64 KiB in flight while draining");
        let mut d = CreditIn::new(t0);
        // Draining: the server's window is refilled only up to 64 KiB.
        let g = d.consume(256 * 1024, t0, true).expect("grant");
        assert_eq!(g, 64 * 1024);
    }
}
