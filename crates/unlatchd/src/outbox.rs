//! Per-session output: two lanes (interactive first, bulk second) drained by one writer thread,
//! and send credit for bulk data (D10). Bulk producers acquire credit *before* queueing, so at
//! most one credit window of bulk data is ever queued below the scheduler.
//!
//! Credit accounting (wire.rs "Credit measure", docs/PROTOCOL-NOTES.md): server → client, every
//! `ReadChunk`, `SnapshotChunk` and `ListingPart` costs [`credit_cost`] of its frame (the frame body
//! length on the wire); client → server, a `WriteChunk` costs `data.len()`.
//! Bulk frames wait for positive credit; a single-part listing on the interactive lane is charged
//! without waiting (the balance may dip below zero by at most one frame).

use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use unlatch_proto::wire::Change;

/// Implicit initial grant in each direction (wire.rs).
pub const INITIAL_CREDIT: i64 = 256 * 1024;

/// Credit cost of an encoded frame (`frame` includes its 4-byte length prefix): the frame body
/// as it travels (flags byte + possibly-compressed payload). Credit bounds bytes queued below
/// the scheduler (pipes, ssh window), which are wire bytes. A receiver that grants more (e.g.
/// the decompressed length) only loosens the window; one that grants this much never stalls.
pub fn credit_cost(frame: &[u8]) -> i64 {
    frame.len().saturating_sub(4) as i64
}

#[derive(Default)]
struct OutQ {
    inter: VecDeque<Vec<u8>>,
    bulk: VecDeque<Vec<u8>>,
    closed: bool,
}

#[derive(Default)]
pub struct Outbox {
    q: Mutex<OutQ>,
    cv: Condvar,
}

impl Outbox {
    pub fn push_inter(&self, frame: Vec<u8>) {
        if let Ok(mut q) = self.q.lock() {
            if !q.closed {
                q.inter.push_back(frame);
            }
        }
        self.cv.notify_all();
    }

    pub fn push_inter_many(&self, frames: &[Vec<u8>]) {
        if let Ok(mut q) = self.q.lock() {
            if !q.closed {
                q.inter.extend(frames.iter().cloned());
            }
        }
        self.cv.notify_all();
    }

    pub fn push_bulk(&self, frame: Vec<u8>) {
        if let Ok(mut q) = self.q.lock() {
            if !q.closed {
                q.bulk.push_back(frame);
            }
        }
        self.cv.notify_all();
    }

    pub fn close(&self) {
        if let Ok(mut q) = self.q.lock() {
            q.closed = true;
        }
        self.cv.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.q.lock().map(|q| q.closed).unwrap_or(true)
    }

    /// Writer loop: interactive frames always first. Returns when closed and drained, or on a
    /// write error. Flushes whenever the queues run empty.
    pub fn run_writer<W: Write>(&self, w: &mut W) -> std::io::Result<()> {
        loop {
            let frame = {
                let Ok(mut q) = self.q.lock() else {
                    return Ok(());
                };
                loop {
                    if let Some(f) = q.inter.pop_front() {
                        break Some(f);
                    }
                    if let Some(f) = q.bulk.pop_front() {
                        break Some(f);
                    }
                    if q.closed {
                        break None;
                    }
                    q = match self.cv.wait(q) {
                        Ok(g) => g,
                        Err(_) => return Ok(()),
                    };
                }
            };
            self.cv.notify_all();
            let Some(frame) = frame else {
                w.flush()?;
                return Ok(());
            };
            w.write_all(&frame)?;
            let empty = self
                .q
                .lock()
                .map(|q| q.inter.is_empty() && q.bulk.is_empty())
                .unwrap_or(true);
            if empty {
                w.flush()?;
            }
        }
    }
}

/// Bulk send credit granted by the peer.
pub struct Credit {
    avail: Mutex<i64>,
    cv: Condvar,
}

impl Default for Credit {
    fn default() -> Self {
        Credit {
            avail: Mutex::new(INITIAL_CREDIT),
            cv: Condvar::new(),
        }
    }
}

impl Credit {
    pub fn grant(&self, n: u32) {
        if let Ok(mut a) = self.avail.lock() {
            *a += n as i64;
        }
        self.cv.notify_all();
    }

    /// Take up to `want` bytes of credit, blocking while none is available. Returns 0 when
    /// cancelled/closed.
    pub fn take_up_to(&self, want: usize, cancel: &dyn Fn() -> bool) -> usize {
        let Ok(mut a) = self.avail.lock() else {
            return 0;
        };
        loop {
            if cancel() {
                return 0;
            }
            if *a > 0 {
                let n = (*a as usize).min(want);
                *a -= n as i64;
                return n;
            }
            match self.cv.wait_timeout(a, Duration::from_millis(100)) {
                Ok((g, _)) => a = g,
                Err(_) => return 0,
            }
        }
    }

    /// Deduct `n` without waiting (the balance may go negative).
    pub fn charge(&self, n: i64) {
        if let Ok(mut a) = self.avail.lock() {
            *a -= n;
        }
    }

    /// Block until the balance is positive. False when cancelled/closed.
    pub fn wait_positive(&self, cancel: &dyn Fn() -> bool) -> bool {
        let Ok(mut a) = self.avail.lock() else {
            return false;
        };
        loop {
            if cancel() {
                return false;
            }
            if *a > 0 {
                return true;
            }
            match self.cv.wait_timeout(a, Duration::from_millis(100)) {
                Ok((g, _)) => a = g,
                Err(_) => return false,
            }
        }
    }

    /// Return unused credit (e.g. a short read).
    pub fn refund(&self, n: usize) {
        if let Ok(mut a) = self.avail.lock() {
            *a += n as i64;
        }
        self.cv.notify_all();
    }

    pub fn available(&self) -> i64 {
        self.avail.lock().map(|a| *a).unwrap_or(0)
    }

    pub fn wake(&self) {
        self.cv.notify_all();
    }
}

/// Snapshot walker state (D15): BFS queue fed by the walker itself *and* by every directory
/// upserted in Events during the snapshot that the walker has not listed yet.
#[derive(Default)]
pub struct SnapFeed {
    pub queue: VecDeque<u64>,
    pub queued: HashSet<u64>,
    pub sent: HashSet<u64>,
    /// Directory being sent across several chunks, with its remaining entries.
    pub partial: Option<(u64, VecDeque<unlatch_proto::Entry>)>,
}

impl SnapFeed {
    pub fn push(&mut self, id: u64) {
        if !self.sent.contains(&id) && self.queued.insert(id) {
            self.queue.push_back(id);
        }
    }
}

/// State shared between a session's threads and the core (which publishes into it).
pub struct SessionShared {
    pub id: u64,
    pub out: Outbox,
    pub credit: Credit,
    pub snap: Mutex<Option<SnapFeed>>,
    pub closed: AtomicBool,
    pub created: Instant,
}

impl SessionShared {
    pub fn new(id: u64) -> SessionShared {
        SessionShared {
            id,
            out: Outbox::default(),
            credit: Credit::default(),
            snap: Mutex::new(None),
            closed: AtomicBool::new(false),
            created: Instant::now(),
        }
    }

    /// Called by the core (under its lock) for every published batch.
    pub fn publish(&self, frames: &[Vec<u8>], changes: &[Change]) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut g) = self.snap.lock() {
            if let Some(feed) = g.as_mut() {
                for c in changes {
                    if let Change::Upsert(e) = c {
                        if e.kind == unlatch_proto::Kind::Dir && !e.lazy {
                            feed.push(e.id.0);
                        }
                    }
                }
            }
        }
        self.out.push_inter_many(frames);
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.out.close();
        self.credit.wake();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_overtakes_bulk() {
        let o = Outbox::default();
        o.push_bulk(vec![2]);
        o.push_bulk(vec![3]);
        o.push_inter(vec![1]);
        o.close();
        let mut out = Vec::new();
        o.run_writer(&mut out).unwrap();
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn credit_cost_is_the_wire_body() {
        use unlatch_proto::wire::ServerMsg;
        let small =
            unlatch_proto::frame::encode(&ServerMsg::Credit { bulk_bytes: 5 }, true).unwrap();
        assert_eq!(credit_cost(&small), (small.len() - 4) as i64);
        let big = ServerMsg::ReadChunk {
            req_id: 1,
            offset: 0,
            data: vec![0; 60_000],
            last: false,
            version: 1,
        };
        let f = unlatch_proto::frame::encode(&big, true).unwrap();
        assert!(f.len() < 10_000, "compressed");
        assert_eq!(
            credit_cost(&f),
            (f.len() - 4) as i64,
            "compressed frames cost their wire size"
        );
    }

    #[test]
    fn credit_blocks_and_grants() {
        let c = std::sync::Arc::new(Credit::default());
        let never = || false;
        assert_eq!(
            c.take_up_to(INITIAL_CREDIT as usize + 10, &never),
            INITIAL_CREDIT as usize
        );
        let c2 = c.clone();
        let h = std::thread::spawn(move || c2.take_up_to(100, &|| false));
        std::thread::sleep(Duration::from_millis(50));
        c.grant(40);
        assert_eq!(h.join().unwrap(), 40);
        assert_eq!(c.take_up_to(10, &|| true), 0);
    }
}
