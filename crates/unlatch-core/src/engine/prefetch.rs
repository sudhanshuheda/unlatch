//! Prefetch of small files for *viewer* enumerations only (D25): files ≤ `max_file`, a byte
//! budget per container, and a global token bucket (64 MiB/min, 16 MiB burst by default).
//! Runs on two low-priority worker threads; cancelled on shutdown.

use super::Shared;
use crate::{err, CancelToken, PrefetchConfig, Result};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc as smpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unlatch_proto::ipc::IpcItem;
use unlatch_proto::{ErrorCode, ItemId, Kind};

const WORKERS: usize = 2;
/// A container's prefetch budget refills after this long.
const CONTAINER_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) struct Bucket {
    tokens: f64,
    rate_per_sec: f64,
    burst: f64,
    last: Instant,
}

impl Bucket {
    pub fn new(bytes_per_min: u64, burst: u64, now: Instant) -> Bucket {
        Bucket {
            tokens: burst as f64,
            rate_per_sec: bytes_per_min as f64 / 60.0,
            burst: burst as f64,
            last: now,
        }
    }

    pub fn take(&mut self, n: u64, now: Instant) -> bool {
        let el = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + el * self.rate_per_sec).min(self.burst);
        if self.tokens >= n as f64 {
            self.tokens -= n as f64;
            true
        } else {
            false
        }
    }
}

struct Job {
    id: ItemId,
    ver: u64,
}

pub(crate) struct Prefetcher {
    cfg: PrefetchConfig,
    tx: Mutex<Option<smpsc::Sender<Job>>>,
    rx: Mutex<Option<smpsc::Receiver<Job>>>,
    bucket: Mutex<Bucket>,
    containers: Mutex<HashMap<ItemId, (Instant, u64)>>,
    queued: Mutex<HashSet<(ItemId, u64)>>,
    cancel: CancelToken,
}

impl Prefetcher {
    pub fn new(cfg: &PrefetchConfig) -> Prefetcher {
        let (tx, rx) = smpsc::channel();
        Prefetcher {
            cfg: cfg.clone(),
            tx: Mutex::new(Some(tx)),
            rx: Mutex::new(Some(rx)),
            bucket: Mutex::new(Bucket::new(cfg.bytes_per_min, cfg.burst, Instant::now())),
            containers: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashSet::new()),
            cancel: CancelToken::new(),
        }
    }

    /// Queue small files of a viewer enumeration page, within budgets.
    pub fn offer(&self, shared: &Shared, container: ItemId, items: &[IpcItem]) {
        if self.cfg.max_file == 0 || self.cancel.is_cancelled() {
            return;
        }
        let now = Instant::now();
        let tx = self.tx.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let Some(tx) = tx else { return };
        let mut containers = self.containers.lock().unwrap_or_else(|p| p.into_inner());
        containers.retain(|_, (t, _)| now.duration_since(*t) < CONTAINER_WINDOW);
        let used = containers.entry(container).or_insert((now, 0));
        let mut bucket = self.bucket.lock().unwrap_or_else(|p| p.into_inner());
        let mut queued = self.queued.lock().unwrap_or_else(|p| p.into_inner());
        for it in items {
            let e = &it.entry;
            let key = (e.id, e.version.content);
            if e.kind != Kind::File
                || it.symlink_blocked
                || e.size > self.cfg.max_file
                || queued.contains(&key)
                || shared.cache.contains(e.id, e.version.content)
            {
                continue;
            }
            if used.1 + e.size > self.cfg.per_container {
                break;
            }
            if !bucket.take(e.size, now) {
                break;
            }
            used.1 += e.size;
            queued.insert(key);
            if tx
                .send(Job {
                    id: e.id,
                    ver: e.version.content,
                })
                .is_err()
            {
                break;
            }
        }
    }

    pub fn stop(&self) {
        self.cancel.cancel();
        *self.tx.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

pub(crate) fn spawn_workers(shared: &Arc<Shared>) -> Result<Vec<std::thread::JoinHandle<()>>> {
    let rx = shared
        .prefetch
        .rx
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .ok_or_else(|| err(ErrorCode::Io, "prefetch workers already started"))?;
    let rx = Arc::new(Mutex::new(rx));
    let mut v = Vec::new();
    for i in 0..WORKERS {
        let rx = rx.clone();
        let weak = Arc::downgrade(shared);
        let h = std::thread::Builder::new()
            .name(format!("unlatch-prefetch-{i}"))
            .spawn(move || loop {
                let job = {
                    let rx = rx.lock().unwrap_or_else(|p| p.into_inner());
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(j) => Some(j),
                        Err(smpsc::RecvTimeoutError::Timeout) => None,
                        Err(smpsc::RecvTimeoutError::Disconnected) => return,
                    }
                };
                let Some(sh) = weak.upgrade() else { return };
                if sh.prefetch.cancel.is_cancelled() || sh.is_shutdown() {
                    return;
                }
                let Some(job) = job else { continue };
                // Only if the item is still at that version (a newer one is fetched on demand).
                let current = sh.item(job.id).map(|it| it.entry.version.content).ok();
                if current == Some(job.ver) {
                    let cancel = sh.prefetch.cancel.clone();
                    if let Ok((_, it)) = sh.ensure_cached(job.id, &|_, _| {}, &cancel) {
                        sh.cache.unpin(job.id, it.entry.version.content);
                    }
                }
                sh.prefetch
                    .queued
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&(job.id, job.ver));
            })
            .map_err(|e| err(ErrorCode::Io, format!("spawn prefetch worker: {e}")))?;
        v.push(h);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket() {
        let t0 = Instant::now();
        let mut b = Bucket::new(60_000, 10_000, t0); // 1000 B/s, burst 10 kB
        assert!(b.take(8_000, t0));
        assert!(!b.take(3_000, t0));
        assert!(b.take(3_000, t0 + Duration::from_secs(1)));
        // Never above burst.
        assert!(!b.take(10_001, t0 + Duration::from_secs(3600)));
        assert!(b.take(10_000, t0 + Duration::from_secs(3600)));
    }
}
