//! The interactive path must not wait for a Write's post-publish checks.
//!
//! A Write publishes our bytes and then proves that nobody else wrote to the inode before
//! unlatchd observed it (observation + fstat + a re-hash of files up to 64 MiB). Those checks
//! must not hold the core lock for time proportional to the file size: every Stat / ListDir /
//! Ping of every session waits on that lock. Two sessions share one `unlatchd serve`: one
//! uploads 64 MiB files (create, then replace), the other samples `Stat(root)` back to back;
//! the samples taken while a Write is in flight are the interactive latency it causes.
//!
//! The tree lives on tmpfs (`/dev/shm`) when there is one, or in `UNLATCHD_STALL_DIR`: the
//! journal commit and the directory fsync still run under the lock, and on a shared disk their
//! latency (tens to hundreds of ms on this VM, in the old and the new code alike) would drown
//! the size-proportional work this test is about. Without tmpfs it only reports.
//! `cargo test -p unlatchd --release --test write_stall -- --nocapture` prints the numbers;
//! `UNLATCHD_STALL_REPORT=1` only reports.

mod common;
use common::*;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unlatch_proto::wire::{Request, Response, ServerMsg};
use unlatch_proto::ItemId;

/// A scratch dir and whether it is off any disk (tmpfs, or chosen by `UNLATCHD_STALL_DIR`).
fn dir() -> (tempfile::TempDir, bool) {
    if let Some(d) = std::env::var_os("UNLATCHD_STALL_DIR") {
        std::fs::create_dir_all(&d).unwrap();
        return (tempfile::tempdir_in(d).unwrap(), true);
    }
    match tempfile::tempdir_in("/dev/shm") {
        Ok(d) => (d, true),
        Err(_) => (tmp(), false),
    }
}

struct StopOnDrop<'a>(&'a Path);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .arg("stop")
            .arg("--state")
            .arg(self.0)
            .output();
    }
}

fn connect(root: &Path, state: &Path) -> Client {
    let mut c = Client::spawn(
        root,
        state,
        Opts {
            connect: true,
            ..Default::default()
        },
    );
    c.wait_snapshot();
    c
}

/// (start, latency) of every Stat(root) the prober completed.
type Samples = Vec<(Instant, Duration)>;

fn prober(mut c: Client, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Samples> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let s0 = Instant::now();
            let sid = c.request(Request::Stat { id: ItemId::ROOT });
            let r = c.next_for(sid, T).expect("stat reply");
            assert!(matches!(r, ServerMsg::Response { .. }), "{r:?}");
            out.push((s0, s0.elapsed()));
            std::thread::sleep(Duration::from_micros(200));
        }
        c.close();
        out
    })
}

// A wall-clock measurement on a shared host: run it with `--ignored` (the bench and race probes
// do the same). The deterministic guard for the same regression is
// `ops::leased_write_rehashes_nothing_under_the_core_lock` (0 bytes re-hashed under the lock).
#[test]
#[ignore = "timing measurement; deterministic guard: ops::leased_write_rehashes_nothing_under_the_core_lock"]
fn large_write_does_not_block_interactive_requests() {
    let (root, off_disk) = dir();
    let (state, _) = dir();
    make_tree(root.path(), 100, 50);
    let _stop = StopOnDrop(state.path());
    let mut w = connect(root.path(), state.path());
    let p = connect(root.path(), state.path());
    let size = 64usize << 20;
    let mut data: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let probe = prober(p, stop.clone());
    std::thread::sleep(Duration::from_millis(200));
    // (label, write start, reply time)
    let mut writes: Vec<(String, Instant, Instant)> = Vec::new();
    for round in 0..3u64 {
        let name = format!("big{round}.bin");
        data[0] = round as u8;
        let t0 = Instant::now();
        let r = w.write(
            op(10 + 2 * round),
            ItemId::ROOT,
            &name,
            None,
            None,
            &data,
            false,
        );
        assert!(matches!(r, Ok(Response::Written { .. })), "{r:?}");
        writes.push((format!("create {round}"), t0, Instant::now()));
        w.ping();
        let e = w.find(&name).expect("created");
        data[1] = round as u8 + 1;
        let t0 = Instant::now();
        let r = w.write(
            op(11 + 2 * round),
            ItemId::ROOT,
            &name,
            Some(e.id),
            Some(e.version.content),
            &data,
            false,
        );
        assert!(matches!(r, Ok(Response::Written { .. })), "{r:?}");
        writes.push((format!("replace {round}"), t0, Instant::now()));
        w.ping();
    }
    std::thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::Relaxed);
    let samples = probe.join().expect("prober");
    let mut idle: Vec<Duration> = samples
        .iter()
        .filter(|(s, d)| !writes.iter().any(|(_, a, b)| *s + *d >= *a && *s <= *b))
        .map(|(_, d)| *d)
        .collect();
    idle.sort();
    let mut worst = Duration::ZERO;
    let mut per_write = Vec::new();
    for (label, a, b) in &writes {
        let mut during: Vec<Duration> = samples
            .iter()
            .filter(|(s, d)| *s + *d >= *a && *s <= *b)
            .map(|(_, d)| *d)
            .collect();
        during.sort();
        let max = during.last().copied().unwrap_or_default();
        worst = worst.max(max);
        per_write.push(max);
        eprintln!(
            "{label} 64 MiB: write {:?}, {} Stat samples during it, p50 {:?}, max {max:?}",
            b.duration_since(*a),
            during.len(),
            during.get(during.len() / 2).copied().unwrap_or_default(),
        );
    }
    eprintln!(
        "Stat outside the writes: {} samples, p50 {:?}, p99 {:?}",
        idle.len(),
        idle.get(idle.len() / 2).copied().unwrap_or_default(),
        idle.get(idle.len() * 99 / 100).copied().unwrap_or_default()
    );
    per_write.sort();
    let typical = per_write[per_write.len() / 2];
    eprintln!("worst Stat during a write: {worst:?}; median over the writes {typical:?}");
    // Re-hashing 64 MiB under the lock held it 30–60 ms on every write (this VM, load ≈ 100);
    // without it the longest Stat during a write was 4–15 ms (the old inode's unlink, the
    // journal commit), as before the re-hash existed. The median over the writes ignores a
    // single scheduling hiccup of a loaded host.
    if off_disk && std::env::var_os("UNLATCHD_STALL_REPORT").is_none() {
        assert!(
            typical < Duration::from_millis(25),
            "Stats waited {per_write:?} behind 64 MiB Writes"
        );
    }
}
