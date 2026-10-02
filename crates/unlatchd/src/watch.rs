//! inotify plumbing (D14): one instance, a dedicated reader thread that drains the fd into an
//! unbounded userspace queue (so index work never backs up the kernel queue), a wd → dir map,
//! and the per-uid watch budget.

use crate::sys::{self, InotifyEvent};
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

struct Queue {
    events: Vec<InotifyEvent>,
    buf: Vec<u8>,
    /// Test hook / real overflow: pretend the kernel queue overflowed.
    injected_overflow: bool,
    /// `UNLATCH_FAULT=overflow_after_events:<n>`: events still to read before the injected
    /// overflow (None = not armed / already fired).
    overflow_after: Option<u64>,
}

struct Maps {
    wd_to_dir: HashMap<i32, u64>,
    dir_to_wd: HashMap<u64, i32>,
    /// Dirs this process could not read at their indexed path (see [`Watcher::mark_stale`]),
    /// with the dirty names (and hints) blocked under them.
    stale: HashMap<u64, Vec<(String, u8)>>,
}

pub struct Watcher {
    ifd: Option<OwnedFd>,
    q: Mutex<Queue>,
    cv: Condvar,
    maps: Mutex<Maps>,
    budget: AtomicU64,
    stop: AtomicBool,
    wake: (OwnedFd, OwnedFd),
    last_len: std::sync::atomic::AtomicUsize,
    warnings: Mutex<Option<String>>,
}

impl Watcher {
    /// `enabled = false` → no inotify at all (polled mode; D14/D22).
    pub fn new(enabled: bool, budget: u64) -> io::Result<Arc<Watcher>> {
        let ifd = if enabled {
            Some(sys::inotify_init()?)
        } else {
            None
        };
        let w = Arc::new(Watcher {
            ifd,
            q: Mutex::new(Queue {
                events: Vec::new(),
                buf: vec![0u8; 256 * 1024],
                injected_overflow: false,
                overflow_after: crate::fault::overflow_after_events(),
            }),
            cv: Condvar::new(),
            maps: Mutex::new(Maps {
                wd_to_dir: HashMap::new(),
                dir_to_wd: HashMap::new(),
                stale: HashMap::new(),
            }),
            budget: AtomicU64::new(budget),
            stop: AtomicBool::new(false),
            wake: sys::pipe()?,
            last_len: std::sync::atomic::AtomicUsize::new(0),
            warnings: Mutex::new(None),
        });
        if w.ifd.is_some() {
            let w2 = w.clone();
            std::thread::Builder::new()
                .name("inotify-reader".into())
                .spawn(move || w2.reader_loop())?;
        }
        Ok(w)
    }

    pub fn enabled(&self) -> bool {
        self.ifd.is_some()
    }

    fn reader_loop(&self) {
        let Some(ifd) = self.ifd.as_ref().map(|f| f.as_raw_fd()) else {
            return;
        };
        let wake = self.wake.0.as_raw_fd();
        while !self.stop.load(Ordering::Relaxed) {
            match sys::poll_fds(&[(ifd, libc::POLLIN), (wake, libc::POLLIN)], 1000) {
                Ok(r) if r[0] & libc::POLLIN != 0 => {
                    let got = self.drain();
                    if got > 0 {
                        self.cv.notify_all();
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    crate::log!("inotify poll: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// Read everything the kernel has queued into the userspace queue. Holding the queue lock
    /// around read+push keeps event order even when a flush drains concurrently with the reader.
    fn drain(&self) -> usize {
        let Some(ifd) = self.ifd.as_ref().map(|f| f.as_raw_fd()) else {
            return 0;
        };
        let Ok(mut q) = self.q.lock() else { return 0 };
        let q = &mut *q;
        let before = q.events.len();
        let n = match sys::inotify_read(ifd, &mut q.buf, &mut q.events) {
            Ok(n) => n,
            Err(e) => {
                crate::log!("inotify read: {e}");
                0
            }
        };
        if let Some(left) = q.overflow_after {
            if (n as u64) > left {
                // Fault injection: the kernel queue "filled up" after `left` more events —
                // everything after them is lost and IN_Q_OVERFLOW is all that remains.
                let keep = before + left as usize;
                crate::log!(
                    "UNLATCH_FAULT overflow_after_events: dropping {} events, IN_Q_OVERFLOW",
                    q.events.len() - keep
                );
                q.events.truncate(keep);
                q.events.push(InotifyEvent {
                    wd: -1,
                    mask: sys::IN_Q_OVERFLOW,
                    cookie: 0,
                    name: Vec::new(),
                });
                q.overflow_after = None;
            } else {
                q.overflow_after = Some(left - n as u64);
            }
        }
        n
    }

    /// Drain the kernel queue and take everything pending (flush before a mutation, §2(d)1).
    pub fn take(&self) -> (Vec<InotifyEvent>, bool) {
        self.drain();
        match self.q.lock() {
            Ok(mut q) => {
                self.last_len.store(0, Ordering::Relaxed);
                let ov = std::mem::take(&mut q.injected_overflow);
                (std::mem::take(&mut q.events), ov)
            }
            Err(_) => (Vec::new(), false),
        }
    }

    /// Take the queued events up to and including the last IN_MOVED_TO whose cookie is in
    /// `cookies` (the kernel queue is drained first); nothing when none of them is queued.
    /// Later events stay queued for the next batch.
    pub fn take_moved_to(&self, cookies: &HashSet<u32>) -> Vec<InotifyEvent> {
        self.drain();
        let Ok(mut q) = self.q.lock() else {
            return Vec::new();
        };
        let last = q
            .events
            .iter()
            .rposition(|e| e.mask & sys::IN_MOVED_TO != 0 && cookies.contains(&e.cookie));
        let Some(last) = last else {
            return Vec::new();
        };
        let taken: Vec<InotifyEvent> = q.events.drain(..=last).collect();
        self.last_len.store(q.events.len(), Ordering::Relaxed);
        taken
    }

    /// Test hook: put events back at the head of the queue, as if not read yet (a batch
    /// boundary at a chosen point of a syscall sequence).
    #[cfg(test)]
    pub fn requeue(&self, mut events: Vec<InotifyEvent>) {
        if let Ok(mut q) = self.q.lock() {
            events.append(&mut q.events);
            q.events = events;
        }
    }

    /// `(dir id, name)` of every name event that happened before this call and has not been
    /// taken yet (the kernel queue is drained first).
    pub fn queued_names(&self) -> HashSet<(u64, Vec<u8>)> {
        self.drain();
        let named: Vec<(i32, Vec<u8>)> = match self.q.lock() {
            Ok(q) => q
                .events
                .iter()
                .filter(|e| !e.name.is_empty())
                .map(|e| (e.wd, e.name.clone()))
                .collect(),
            Err(_) => return HashSet::new(),
        };
        let Ok(m) = self.maps.lock() else {
            return HashSet::new();
        };
        named
            .into_iter()
            .filter_map(|(wd, n)| m.wd_to_dir.get(&wd).map(|&d| (d, n)))
            .collect()
    }

    /// True if the queue grew since the previous call (burst detection for the debounce).
    pub fn take_is_growing(&self) -> bool {
        self.drain();
        let n = self.q.lock().map(|q| q.events.len()).unwrap_or(0);
        let prev = self.last_len.swap(n, Ordering::Relaxed);
        n > prev
    }

    /// No unprocessed event anywhere: the kernel queue is read into ours first, and ours is
    /// empty. Leaves the queue as it is (unlike [`Watcher::take`]).
    pub fn quiet(&self) -> bool {
        self.drain();
        !self.pending()
    }

    pub fn pending(&self) -> bool {
        self.q
            .lock()
            .map(|q| !q.events.is_empty() || q.injected_overflow)
            .unwrap_or(false)
    }

    /// Wait until events are queued or `timeout` elapses.
    pub fn wait(&self, timeout: Duration) -> bool {
        let Ok(q) = self.q.lock() else { return false };
        if !q.events.is_empty() || q.injected_overflow {
            return true;
        }
        match self.cv.wait_timeout(q, timeout) {
            Ok((q, _)) => !q.events.is_empty() || q.injected_overflow,
            Err(_) => false,
        }
    }

    /// Wake anyone in [`Watcher::wait`] (e.g. the actor for timers or shutdown).
    pub fn nudge(&self) {
        self.cv.notify_all();
    }

    /// Simulate IN_Q_OVERFLOW (tests; the kernel sysctl needs root).
    pub fn inject_overflow(&self) {
        if let Ok(mut q) = self.q.lock() {
            q.injected_overflow = true;
        }
        self.cv.notify_all();
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = sys::write_all_fd(self.wake.1.as_raw_fd(), b"x");
        self.cv.notify_all();
    }

    /// Refine the budget on a background thread (scanning /proc takes 50–250 ms on a busy
    /// box; Welcome and the first scan never wait for it). Until then the provisional budget
    /// from [`provisional_budget`] applies. Its warning, if any, is kept for
    /// [`Watcher::warnings`]. Never takes a core lock.
    pub fn compute_budget_async(self: &Arc<Self>, config_max: Option<u64>) {
        let w = self.clone();
        let _ = std::thread::Builder::new()
            .name("watch-budget".into())
            .spawn(move || {
                let (b, warn) = compute_budget(config_max);
                w.set_budget(b);
                if let Ok(mut ws) = w.warnings.lock() {
                    *ws = warn;
                }
            });
    }

    pub fn warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .ok()
            .and_then(|w| w.clone())
            .into_iter()
            .collect()
    }

    pub fn budget(&self) -> u64 {
        self.budget.load(Ordering::Relaxed)
    }
    pub fn set_budget(&self, b: u64) {
        self.budget.store(b, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.maps
            .lock()
            .map(|m| m.wd_to_dir.len() as u64)
            .unwrap_or(0)
    }

    /// Watch the directory behind `dirfd` for `dir_id` (before its readdir). `Ok(None)`: budget
    /// exhausted or inotify disabled → the caller polls this dir.
    pub fn add(&self, dirfd: RawFd, dir_id: u64) -> io::Result<Option<i32>> {
        let Some(ifd) = self.ifd.as_ref().map(|f| f.as_raw_fd()) else {
            return Ok(None);
        };
        let mut m = self
            .maps
            .lock()
            .map_err(|_| io::Error::other("watch map poisoned"))?;
        if let Some(&wd) = m.dir_to_wd.get(&dir_id) {
            // Re-scan of a watched dir: re-add (same inode → same wd) to refresh the mapping.
            let nwd = sys::inotify_add_watch_fd(ifd, dirfd, sys::WATCH_MASK)?;
            if nwd != wd {
                // The id now stands for another inode (mount / atomic dir replace): drop the
                // watch on the old one.
                m.wd_to_dir.remove(&wd);
                sys::inotify_rm_watch(ifd, wd);
                m.wd_to_dir.insert(nwd, dir_id);
                m.dir_to_wd.insert(dir_id, nwd);
            }
            return Ok(Some(nwd));
        }
        if m.wd_to_dir.len() as u64 >= self.budget() {
            return Ok(None);
        }
        match sys::inotify_add_watch_fd(ifd, dirfd, sys::WATCH_MASK) {
            Ok(wd) => {
                if let Some(old) = m.wd_to_dir.insert(wd, dir_id) {
                    // Same inode already watched under another id (should not happen: dup
                    // dirs are never expanded) — keep the newest mapping.
                    m.dir_to_wd.remove(&old);
                }
                m.dir_to_wd.insert(dir_id, wd);
                Ok(Some(wd))
            }
            Err(e) if sys::is_errno(&e, libc::ENOSPC) => {
                // Kernel per-uid limit hit despite our budget: shrink the budget and poll.
                self.set_budget(m.wd_to_dir.len() as u64);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    pub fn remove_dir(&self, dir_id: u64) {
        let Ok(mut m) = self.maps.lock() else { return };
        m.stale.remove(&dir_id);
        if let Some(wd) = m.dir_to_wd.remove(&dir_id) {
            m.wd_to_dir.remove(&wd);
            if let Some(ifd) = self.ifd.as_ref() {
                sys::inotify_rm_watch(ifd.as_raw_fd(), wd);
            }
        }
    }

    /// The kernel dropped a watch (IN_IGNORED).
    pub fn forget_wd(&self, wd: i32) {
        let Ok(mut m) = self.maps.lock() else { return };
        if let Some(d) = m.wd_to_dir.remove(&wd) {
            if m.dir_to_wd.get(&d) == Some(&wd) {
                m.dir_to_wd.remove(&d);
            }
        }
    }

    /// A dir id now stands for another id (reconcile merge): move its watch mapping.
    pub fn remap(&self, from: u64, to: u64) {
        let Ok(mut m) = self.maps.lock() else { return };
        if let Some(names) = m.stale.remove(&from) {
            m.stale.entry(to).or_default().extend(names);
        }
        if let Some(wd) = m.dir_to_wd.remove(&from) {
            if let Some(old) = m.dir_to_wd.insert(to, wd) {
                m.wd_to_dir.remove(&old);
                if let Some(ifd) = self.ifd.as_ref() {
                    sys::inotify_rm_watch(ifd.as_raw_fd(), old);
                }
            }
            m.wd_to_dir.insert(wd, to);
        }
    }

    /// Dir `dir_id` could not be opened at its indexed path: it moved (the move not applied
    /// yet — its MOVED pair may only be read by a later batch) or it is gone. Its listing
    /// (and with it this process's watch, and `name` with its hint) is retried by every
    /// later reconcile batch until it is reachable again; removing the dir drops the entry.
    pub fn mark_stale(&self, dir_id: u64, name: Option<(String, u8)>) {
        let Ok(mut m) = self.maps.lock() else { return };
        let e = m.stale.entry(dir_id).or_default();
        if let Some((n, h)) = name {
            match e.iter_mut().find(|(x, _)| *x == n) {
                Some(x) => x.1 |= h,
                None => e.push((n, h)),
            }
        }
    }

    pub fn take_stale(&self) -> Vec<(u64, Vec<(String, u8)>)> {
        self.maps
            .lock()
            .map(|mut m| m.stale.drain().collect())
            .unwrap_or_default()
    }

    pub fn has_stale(&self) -> bool {
        self.maps
            .lock()
            .map(|m| !m.stale.is_empty())
            .unwrap_or(false)
    }

    pub fn dir_of(&self, wd: i32) -> Option<u64> {
        self.maps
            .lock()
            .ok()
            .and_then(|m| m.wd_to_dir.get(&wd).copied())
    }

    pub fn is_watched(&self, dir_id: u64) -> bool {
        self.maps
            .lock()
            .map(|m| m.dir_to_wd.contains_key(&dir_id))
            .unwrap_or(false)
    }

    pub fn watched_dirs(&self) -> Vec<u64> {
        self.maps
            .lock()
            .map(|m| m.dir_to_wd.keys().copied().collect())
            .unwrap_or_default()
    }
}

/// Budget usable before the /proc scan finishes: `min(config, max_user_watches / 8)` — well
/// under the 50%-of-free rule unless other processes already use most watches.
pub fn provisional_budget(config_max: Option<u64>) -> u64 {
    let max_user: u64 = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(8192);
    config_max
        .map(|c| c.min(max_user / 8))
        .unwrap_or(max_user / 8)
}

/// Watch budget (D14): `min(config, 50% of the watches this uid can still add)`. Usage by the
/// uid's other processes is counted from `/proc/*/fdinfo` ("inotify wd:" lines).
pub fn compute_budget(config_max: Option<u64>) -> (u64, Option<String>) {
    let max_user: u64 = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(8192);
    let others = count_uid_watches(true);
    let free = max_user.saturating_sub(others);
    let half = free / 2;
    let budget = config_max.map(|c| c.min(half)).unwrap_or(half);
    let warn = if budget < 1024 {
        Some(format!("inotify: only {budget} watches available (max_user_watches={max_user}, in use by other processes={others}); remaining directories are polled"))
    } else {
        None
    };
    (budget, warn)
}

/// Watches in use by this uid's processes (`exclude_self`: skip our own pid).
pub fn count_uid_watches(exclude_self: bool) -> u64 {
    let uid = sys::geteuid();
    let me = sys::getpid().to_string();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut total = 0u64;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str() else { continue };
        if !pid.bytes().all(|b| b.is_ascii_digit()) || (exclude_self && pid == me) {
            continue;
        }
        use std::os::unix::fs::MetadataExt;
        match e.metadata() {
            Ok(md) if md.uid() == uid => {}
            _ => continue,
        }
        let fd_dir = e.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if link.as_os_str().as_encoded_bytes() != b"anon_inode:inotify" {
                continue;
            }
            let info = e.path().join("fdinfo").join(fd.file_name());
            if let Ok(s) = std::fs::read_to_string(info) {
                total += s.lines().filter(|l| l.starts_with("inotify wd:")).count() as u64;
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn watch_events_and_budget() {
        let t = tempfile::tempdir().unwrap();
        let w = Watcher::new(true, 1).unwrap();
        let d = sys::open_path(t.path().as_os_str().as_bytes(), sys::DIR_FLAGS).unwrap();
        assert!(w.add(d.as_raw_fd(), 7).unwrap().is_some());
        std::fs::create_dir(t.path().join("sub")).unwrap();
        let s =
            sys::open_path(t.path().join("sub").as_os_str().as_bytes(), sys::DIR_FLAGS).unwrap();
        assert!(
            w.add(s.as_raw_fd(), 8).unwrap().is_none(),
            "budget of 1 exhausted"
        );
        std::fs::write(t.path().join("f"), b"x").unwrap();
        assert!(w.wait(Duration::from_secs(2)));
        let (evs, ov) = w.take();
        assert!(!ov);
        assert!(evs
            .iter()
            .any(|e| e.name == b"f" && e.mask & sys::IN_CREATE != 0));
        assert!(evs.iter().all(|e| w.dir_of(e.wd) == Some(7)));
        w.inject_overflow();
        assert!(w.take().1);
        w.remove_dir(7);
        assert_eq!(w.count(), 0);
        w.shutdown();
    }

    #[test]
    fn budget_is_bounded() {
        let (b, _) = compute_budget(Some(10));
        assert!(b <= 10);
    }
}
