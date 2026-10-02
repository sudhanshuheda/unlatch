//! Measurements (run explicitly: `cargo test -p unlatchd --release --test measure -- --ignored
//! --nocapture`). They print numbers for the report; assertions are loose sanity bounds.

mod common;
use common::*;
use std::time::{Duration, Instant};
use unlatch_proto::wire::{Resume, WelcomeMode};

fn rss_kib(pid: u32) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    s.lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn settle_rss(c: &mut Client) -> u64 {
    // Let the verify walk / checkpoint finish, then sample.
    for _ in 0..5 {
        c.ping();
        std::thread::sleep(Duration::from_millis(200));
    }
    rss_kib(c.child.id())
}

#[test]
#[ignore]
fn t12_rss_per_entry_100k() {
    let empty = tmp();
    let state0 = tmp();
    let mut c0 = Client::spawn(empty.path(), state0.path(), Opts::default());
    c0.wait_snapshot();
    let base = settle_rss(&mut c0);
    drop(c0);

    let root = tmp();
    let state = tmp();
    let t0 = Instant::now();
    make_tree(root.path(), 1000, 99); // 1000 dirs + 99 000 files = 100 000 entries
    eprintln!("tree built in {:?}", t0.elapsed());
    let t0 = Instant::now();
    let log = state.path().join("m.log");
    let env = vec![(
        "UNLATCHD_LOG".to_string(),
        log.to_string_lossy().into_owned(),
    )];
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            env,
            ..Default::default()
        },
    );
    let welcome = t0.elapsed();
    c.wait_snapshot();
    let snap = t0.elapsed();
    let entries = c.replica.len();
    let rss = settle_rss(&mut c);
    let per = (rss.saturating_sub(base) * 1024) as f64 / entries as f64;
    eprintln!(
        "T12: {entries} entries, RSS {rss} KiB (empty root {base} KiB) → {per:.0} B/entry; fresh Welcome {welcome:?}, snapshot done {snap:?}, snapshot bytes {}",
        c.stats.snapshot_bytes.load(std::sync::atomic::Ordering::Relaxed)
    );
    let index = c.welcome().index;
    let seq = c.ping();
    c.close();
    eprintln!("{}", std::fs::read_to_string(&log).unwrap_or_default());
    // T16 at 100k entries.
    let t0 = Instant::now();
    let log2 = state.path().join("m2.log");
    let env = vec![(
        "UNLATCHD_LOG".to_string(),
        log2.to_string_lossy().into_owned(),
    )];
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            resume: Some(Resume { index, seq }),
            env,
            ..Default::default()
        },
    );
    let w = t0.elapsed();
    assert_eq!(c.welcome().mode, WelcomeMode::Resume);
    let rss2 = settle_rss(&mut c);
    eprintln!(
        "T16: restart Welcome (Resume) after {w:?} for {entries} entries; RSS after verify {rss2} KiB → {:.0} B/entry; snapshot bytes {}",
        (rss2.saturating_sub(base) * 1024) as f64 / entries as f64,
        c.stats.snapshot_bytes.load(std::sync::atomic::Ordering::Relaxed)
    );
    assert_eq!(
        c.stats
            .snapshot_bytes
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    eprintln!("{}", std::fs::read_to_string(&log2).unwrap_or_default());
}

#[test]
#[ignore]
fn throughput_and_burst() {
    use unlatch_proto::wire::{ClientMsg, Request, ServerMsg};
    let root = tmp();
    let state = tmp();
    let size = 256usize << 20;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    std::fs::write(root.path().join("big"), &data).unwrap();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let id = c.find("big").unwrap().id;
    // T8-style: one streamed Read under credit (1.5 MiB window, granted as consumed).
    c.send(&ClientMsg::Credit {
        bulk_bytes: (1536 - 256) * 1024,
    });
    let t0 = Instant::now();
    let rid = c.request(Request::Read {
        id,
        offset: 0,
        len: None,
        expect: None,
    });
    let mut got = 0usize;
    loop {
        match c.next_for(rid, T).unwrap() {
            ServerMsg::ReadChunk { data, last, .. } => {
                got += data.len();
                let raw = c.take_raw(rid);
                c.grant(raw);
                if last {
                    break;
                }
            }
            other => panic!("{other:?}"),
        }
    }
    let dt = t0.elapsed();
    assert_eq!(got, size);
    eprintln!(
        "read 256 MiB in {dt:?} = {:.0} MiB/s (stdio pipe, no network)",
        256.0 / dt.as_secs_f64()
    );
    // Upload 64 MiB.
    let up: Vec<u8> = data[..64 << 20].to_vec();
    let t0 = Instant::now();
    let r = c.write(
        op(1),
        unlatch_proto::ItemId::ROOT,
        "up.bin",
        None,
        None,
        &up,
        false,
    );
    let dt = t0.elapsed();
    assert!(r.is_ok());
    eprintln!(
        "upload 64 MiB (stage+verify+fsync+publish) in {dt:?} = {:.0} MiB/s",
        64.0 / dt.as_secs_f64()
    );
    // T5-style burst: 1000 files in < 1 s; all visible shortly after the burst ends.
    let n0 = c.replica.len();
    let t0 = Instant::now();
    for i in 0..1000 {
        std::fs::write(root.path().join(format!("burst{i:04}")), b"x").unwrap();
    }
    let burst = t0.elapsed();
    let t1 = Instant::now();
    assert!(c.pump_until(Duration::from_secs(5), |c| c.replica.len() >= n0 + 1000));
    eprintln!(
        "burst: 1000 files written in {burst:?}; all visible {:?} after the burst ended",
        t1.elapsed()
    );
}

/// unlatchd's CPU time (user + system clock ticks) from /proc.
fn cpu_ticks(pid: u32) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let rest = s.rsplit_once(')').map(|x| x.1).unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    let p = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    p(11) + p(12)
}

/// Cost of git-like double atomic saves on a 100k-entry tree: each round writes
/// `work/index.lock`, renames it to `work/index`, and does it again at once (one batch: the
/// first saved inode is renamed over before the batch runs). Prints wall time to the Pong and
/// unlatchd CPU. Count statx by wrapping the daemon:
/// `UNLATCHD_TEST_WRAP="strace -f --seccomp-bpf -c -U name,calls -e trace=statx -o /tmp/s"`
/// (compare against `UNLATCHD_MEASURE_ROUNDS=0` for the scan's own statx).
#[test]
#[ignore]
fn double_atomic_save_burst_100k() {
    let env_n = |k: &str, d: u64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let rounds = env_n("UNLATCHD_MEASURE_ROUNDS", 100);
    let gap = Duration::from_millis(env_n("UNLATCHD_MEASURE_GAP_MS", 10));
    let root = tmp();
    let state = tmp();
    let r = root.path();
    make_tree(r, 1000, 99); // 1000 dirs + 99 000 files
    std::fs::create_dir(r.join("work")).unwrap();
    std::fs::write(r.join("work/index"), b"i").unwrap();
    let log = state.path().join("m.log");
    let env = vec![(
        "UNLATCHD_LOG".to_string(),
        log.to_string_lossy().into_owned(),
    )];
    let mut c = Client::spawn(
        r,
        state.path(),
        Opts {
            env,
            ..Default::default()
        },
    );
    c.wait_snapshot();
    let entries = c.replica.len();
    for _ in 0..3 {
        c.ping();
        std::thread::sleep(Duration::from_millis(300));
    }
    let pid = c.child.id();
    let cpu0 = cpu_ticks(pid);
    let t0 = Instant::now();
    for i in 0..rounds {
        for k in 0..2u64 {
            let body = vec![b'i'; (64 + i * 2 + k) as usize];
            std::fs::write(r.join("work/index.lock"), &body).unwrap();
            std::fs::rename(r.join("work/index.lock"), r.join("work/index")).unwrap();
        }
        std::thread::sleep(gap);
    }
    let burst = t0.elapsed();
    c.ping();
    let to_pong = t0.elapsed();
    let cpu = cpu_ticks(pid) - cpu0;
    let want = 64 + rounds.saturating_sub(1) * 2 + 1;
    if rounds > 0 {
        assert_eq!(c.find("work/index").map(|e| e.size), Some(want));
    }
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    eprintln!(
        "double-save burst: {entries} entries, {rounds} rounds (gap {gap:?}): burst {burst:?}, \
         burst→Pong {to_pong:?}, unlatchd CPU {cpu} ticks; loadavg {}",
        load.trim()
    );
    c.close();
    let l = std::fs::read_to_string(&log).unwrap_or_default();
    for line in l
        .lines()
        .filter(|x| x.contains("audit") || x.contains("statx calls"))
    {
        eprintln!("  {line}");
    }
}
