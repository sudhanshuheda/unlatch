//! sshfs baseline (and raw `cat` over the same shaped bridge, system `raw`).
//!
//! sshfs reaches the VM through `-o ssh_command="unlatch-bench netlab connect <sock> <svc>"`; the
//! service spawns `/usr/lib/openssh/sftp-server` per connection. So sshfs speaks plain SFTP over
//! the identical shaped TCP path Unlatch uses (neither side pays ssh crypto in the bench). sshfs
//! runs with its defaults (dir cache 20 s, attr/entry timeout 1 s), as users run it.

use super::{local::walk, ls_la, mbit, poll_until, read_file, t8_file, Ctx, IDLE};
use crate::measure::{Better, Measurement, Samples, Status, System};
use crate::netlab::ServiceHost;
use crate::targets::metric as m;
use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// sshfs's default `dcache_timeout`.
const SSHFS_DIR_CACHE: Duration = Duration::from_secs(20);

pub struct SshfsMount {
    pub mnt: PathBuf,
    _svc: ServiceHost,
}

impl SshfsMount {
    pub fn mount(ctx: &Ctx, tag: &str, extra_opts: &[&str]) -> Result<SshfsMount> {
        let sshfs = ctx
            .bins
            .sshfs
            .as_ref()
            .ok_or_else(|| anyhow!("sshfs not found"))?;
        let sftp = ctx
            .bins
            .sftp_server
            .as_ref()
            .ok_or_else(|| anyhow!("sftp-server not found"))?;
        let rem = super::remote(
            ctx,
            &format!("sftp-{tag}"),
            vec![sftp.display().to_string()],
            Vec::new(),
        )?;
        let mnt = ctx
            .work
            .join(format!("mnt-sshfs-{}-{tag}", std::process::id()));
        let _ = std::fs::create_dir_all(&mnt);
        // Raw bridge: the bridge ignores the ssh arguments sshfs appends. `-ssh` profiles: a
        // real `ssh -F <cfg>` (host `bench`), whose sshd runs the sftp subsystem.
        let ssh_command = rem.ssh_command.clone().unwrap_or_else(|| rem.argv.clone());
        let mut cmd = Command::new(sshfs);
        cmd.arg("-o")
            .arg(format!("ssh_command={}", ssh_command.join(" ")));
        for o in extra_opts {
            cmd.arg("-o").arg(o);
        }
        cmd.arg(format!("bench:{}", ctx.tree.root.display()))
            .arg(&mnt);
        let svc = rem.host;
        super::note(format_args!("sshfs mount {tag}"));
        let out = cmd.stdin(Stdio::null()).output().context("spawn sshfs")?;
        super::note(format_args!("sshfs returned {}", out.status));
        if !out.status.success() {
            bail!(
                "sshfs mount failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        // sshfs daemonizes after the mount is up; double-check.
        let ok = poll_until(Duration::from_secs(10), Duration::from_millis(10), || {
            is_fuse_mount(&mnt)
        });
        if ok.is_none() {
            bail!("sshfs mount did not appear at {}", mnt.display());
        }
        super::note(format_args!("sshfs mounted {}", mnt.display()));
        Ok(SshfsMount { mnt, _svc: svc })
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.mnt.join(rel)
    }
}

impl Drop for SshfsMount {
    fn drop(&mut self) {
        unmount(&self.mnt);
        let _ = std::fs::remove_dir(&self.mnt);
        super::note("sshfs unmounted");
    }
}

/// `fusermount3 -u`, bounded to 2 s, then a lazy `-uz`. On this shared VM a plain unmount
/// occasionally blocked for a minute under load from other FUSE users; unmount time is never
/// measured, so it must not stall the run.
pub fn unmount(mnt: &Path) {
    let spawned = Command::new("fusermount3")
        .arg("-u")
        .arg(mnt)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let clean = match spawned {
        Ok(mut c) => {
            let done = poll_until(Duration::from_secs(2), Duration::from_millis(5), || {
                matches!(c.try_wait(), Ok(Some(_)))
            });
            match done {
                Some(_) => matches!(c.wait(), Ok(st) if st.success()),
                None => {
                    let _ = c.kill();
                    let _ = c.wait();
                    false
                }
            }
        }
        Err(_) => false,
    };
    if !clean {
        super::note(format_args!(
            "fusermount3 -u {} did not finish cleanly; lazy unmount",
            mnt.display()
        ));
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(mnt)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Is `p` a FUSE mount point (per `/proc/self/mountinfo`)?
pub fn is_fuse_mount(p: &Path) -> bool {
    let Ok(canon) = p.canonicalize() else {
        return false;
    };
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    info.lines().any(|l| {
        let f: Vec<&str> = l.split(' ').collect();
        let sep = f.iter().position(|x| *x == "-");
        match (f.get(4), sep.and_then(|i| f.get(i + 1))) {
            (Some(mp), Some(fstype)) => Path::new(mp) == canon && fstype.starts_with("fuse"),
            _ => false,
        }
    })
}

pub fn run(ctx: &Ctx, id: &str, system: System) -> Result<Vec<Measurement>> {
    if system == System::Raw {
        return run_raw(ctx, id);
    }
    let row = |metric: &str, unit: &str, better| ctx.row(id, metric, System::Sshfs, unit, better);
    if ctx.bins.sshfs.is_none() || ctx.bins.sftp_server.is_none() {
        return Ok(vec![row("*", "", Better::Lower)
            .status(Status::Unavailable, "sshfs or sftp-server not found")]);
    }
    Ok(match id {
        "T3" => t3(ctx, &row)?,
        "T4" => t4(ctx, &row)?,
        "T5" => t5(ctx, &row)?,
        "T6" => {
            let mnt = SshfsMount::mount(ctx, "t6", &[])?;
            let files: Vec<_> = ctx
                .tree
                .flat_small
                .iter()
                .take(if ctx.quick { 10 } else { 30 })
                .collect();
            for f in &files {
                read_file(&mnt.path(f))?;
            }
            let mut s = Samples::new();
            for f in &files {
                s.push(read_file(&mnt.path(f))?.1);
            }
            vec![row(m::OPEN_SMALL_WARM_P50_MS, "ms", Better::Lower)
                .value(s.p50().unwrap_or(0.0), s.len())
                .detail(
                    "second open+read+close of the same file (sshfs defaults: no kernel_cache)",
                )]
        }
        "T7" => t7(ctx, &row)?,
        "T8" => {
            let (p, size) = t8_file(ctx)?;
            let rel = p.strip_prefix(&ctx.tree.root)?.display().to_string();
            let mnt = SshfsMount::mount(ctx, "t8", &[])?;
            let (got, d) = read_file(&mnt.path(&rel))?;
            if got != size {
                bail!("short read {got} != {size}");
            }
            vec![row(m::THROUGHPUT_MBIT, "Mbit/s", Better::Higher)
                .value(mbit(size, d), 1)
                .detail(format!("{} MiB, open→EOF", size >> 20))]
        }
        "T9" => {
            let dir = ctx.vm_scratch("t9-sshfs")?;
            let rel = dir.rel(&ctx.tree.root);
            let mnt = SshfsMount::mount(ctx, "t9", &[])?;
            let data = vec![0x42u8; 4096];
            let mut s = Samples::new();
            for i in 0..(if ctx.quick { 10 } else { 20 }) {
                let p = mnt.path(&format!("{rel}/u{i}.txt"));
                let t = Instant::now();
                let mut f = std::fs::File::create(&p)?;
                f.write_all(&data)?;
                f.sync_all()?;
                drop(f); // close → SSH_FXP_CLOSE; data is on the VM when close returns
                s.push(t.elapsed());
                let vm = dir.path.join(format!("u{i}.txt"));
                if std::fs::metadata(&vm).map(|m| m.len()).unwrap_or(0) != 4096 {
                    bail!("upload not on VM after close: {}", vm.display());
                }
            }
            vec![row(m::UPLOAD_4K_P50_MS, "ms", Better::Lower)
                .value(s.p50().unwrap_or(0.0), s.len())
                .detail("create + write + fsync + close via the mount")]
        }
        "T11" => {
            let mnt = SshfsMount::mount(ctx, "t11", &[])?;
            let budget = Duration::from_secs(if ctx.quick { 8 } else { 120 });
            let t = Instant::now();
            let (seen, done) = walk(&mnt.mnt, Some(t + budget))?;
            let el = t.elapsed().as_secs_f64();
            let total = ctx.tree.eager_entries + ctx.tree.lazy_dirs + ctx.tree.lazy_files;
            if done {
                vec![row(m::FULL_TREE_KNOWN_S, "s", Better::Lower)
                    .value(el, 1)
                    .detail(format!(
                        "recursive readdir+lstat of {seen} entries via the mount"
                    ))]
            } else {
                let est = el * total as f64 / seen.max(1) as f64;
                vec![row(m::FULL_TREE_KNOWN_S, "s", Better::Lower)
                    .value(est, 1)
                    .detail(format!(
                        "extrapolated: walked {seen} of {total} entries in {el:.0} s (budget)"
                    ))]
            }
        }
        "T13" => t13(ctx, &row)?,
        other => vec![row("*", "", Better::Lower)
            .status(Status::Skipped, format!("{other}: no sshfs baseline"))],
    })
}

pub type RowFn<'a> = dyn Fn(&str, &str, Better) -> Measurement + 'a;

fn t3(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let mut cold = Samples::new();
    for i in 0..(if ctx.quick { 3 } else { 5 }) {
        let mnt = SshfsMount::mount(ctx, &format!("t3c{i}"), &[])?;
        cold.push(ls_la(&mnt.path("flat"))?);
    }
    let mnt = SshfsMount::mount(ctx, "t3w", &[])?;
    ls_la(&mnt.path("flat"))?;
    let mut warm = Samples::new();
    for _ in 0..(if ctx.quick { 10 } else { 30 }) {
        warm.push(ls_la(&mnt.path("flat"))?);
    }
    Ok(vec![
        row(m::LS_LA_COLD_MS, "ms", Better::Lower)
            .value(cold.p50().unwrap_or(0.0), cold.len())
            .detail("first `ls -la` after mount (dir cache empty)"),
        row(m::LS_LA_MS, "ms", Better::Lower)
            .value(warm.p50().unwrap_or(0.0), warm.len())
            .detail("repeat within sshfs's 20 s dir cache (can be stale)"),
    ])
}

fn t4(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let dir = ctx.vm_scratch("t4-sshfs")?;
    let rel = dir.rel(&ctx.tree.root);
    let mnt = SshfsMount::mount(ctx, "t4", &[])?;
    let mdir = mnt.path(&rel);
    // (a) stat polling of the exact new path: sshfs has no negative cache by default, so each
    // poll is one LSTAT round trip.
    let mut stat = Samples::new();
    for i in 0..(if ctx.quick { 10 } else { 30 }) {
        let name = format!("s{i}.txt");
        let t0 = Instant::now();
        std::fs::write(dir.path.join(&name), b"x")?;
        let p = mdir.join(&name);
        let d = poll_until(Duration::from_secs(10), Duration::from_micros(500), || {
            p.symlink_metadata().is_ok()
        })
        .ok_or_else(|| anyhow!("stat never saw {name}"))?;
        stat.push(t0.elapsed().max(d));
    }
    let mut rows = vec![row(m::VISIBLE_P50_MS, "ms", Better::Lower)
        .value(stat.p50().unwrap_or(0.0), stat.len())
        .detail("poll stat() of the exact new path")];
    // (b) what a file browser sees: the directory listing, cached for dcache_timeout (20 s).
    if ctx.quick {
        rows.push(row(m::VISIBLE_READDIR_P50_MS, "ms", Better::Lower).status(
            Status::Skipped,
            "quick mode (sshfs listing refresh is bounded by its 20 s dir cache; run --full)",
        ));
    } else {
        let mut rd = Samples::new();
        let trials = 3;
        for i in 0..trials {
            // Spread the listing's cache age uniformly over the TTL.
            std::fs::read_dir(&mdir)?.count();
            std::thread::sleep(SSHFS_DIR_CACHE.mul_f64(i as f64 / trials as f64));
            let name = format!("r{i}.txt");
            let t0 = Instant::now();
            std::fs::write(dir.path.join(&name), b"x")?;
            let d = poll_until(SSHFS_DIR_CACHE * 2, Duration::from_millis(5), || {
                std::fs::read_dir(&mdir)
                    .map(|it| it.flatten().any(|e| e.file_name() == name.as_str()))
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow!("listing never showed {name}"))?;
            rd.push(t0.elapsed().max(d));
        }
        rows.push(
            row(m::VISIBLE_READDIR_P50_MS, "ms", Better::Lower)
                .value(rd.p50().unwrap_or(0.0), rd.len())
                .detail("poll readdir() until the new name appears; listing cache age spread over the 20 s TTL"),
        );
    }
    Ok(rows)
}

fn t5(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    if ctx.quick {
        return Ok(vec![row(m::BURST_ALL_VISIBLE_MS, "ms", Better::Lower)
            .status(
                Status::Skipped,
                "quick mode (bounded by sshfs's 20 s dir cache; run --full)",
            )]);
    }
    let dir = ctx.vm_scratch("t5-sshfs")?;
    let rel = dir.rel(&ctx.tree.root);
    let mnt = SshfsMount::mount(ctx, "t5", &[])?;
    let mdir = mnt.path(&rel);
    std::fs::read_dir(&mdir)?.count(); // listing cached, as in an open Finder window
    let end = burst(&dir.path, 1000, Duration::from_secs(1))?;
    let d = poll_until(Duration::from_secs(60), Duration::from_millis(5), || {
        std::fs::read_dir(&mdir)
            .map(|it| it.count() == 1000)
            .unwrap_or(false)
    });
    Ok(vec![match d {
        Some(_) => row(m::BURST_ALL_VISIBLE_MS, "ms", Better::Lower)
            .value(end.elapsed().as_secs_f64() * 1e3, 1)
            .detail("readdir() count reaches 1000"),
        None => row(m::BURST_ALL_VISIBLE_MS, "ms", Better::Lower)
            .status(Status::Timeout, "not all visible within 60 s"),
    }])
}

/// Write `n` small files into `dir`, paced evenly over `over`. Returns the burst end instant.
pub fn burst(dir: &Path, n: usize, over: Duration) -> Result<Instant> {
    let start = Instant::now();
    for i in 0..n {
        let due = start + over.mul_f64(i as f64 / n as f64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        std::fs::write(dir.join(format!("b{i:04}.txt")), format!("burst {i}\n"))?;
    }
    Ok(Instant::now())
}

fn t7(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let mnt = SshfsMount::mount(ctx, "t7", &[])?;
    let plan = T7Plan::new(ctx);
    let mut rows = Vec::new();
    for (metric, files, idle) in plan.cases() {
        let mut s = Samples::new();
        for f in files {
            if idle {
                std::thread::sleep(IDLE);
            } else {
                // Keep the connection warm: one cheap round trip first.
                let _ = std::fs::symlink_metadata(mnt.path("flat"));
            }
            s.push(read_file(&mnt.path(f))?.1);
        }
        rows.push(t7_row(row, metric, idle, &s));
    }
    Ok(rows)
}

pub fn t7_row(row: &RowFn, metric: &str, idle: bool, s: &Samples) -> Measurement {
    if s.is_empty() {
        return row(metric, "ms", Better::Lower)
            .status(Status::Skipped, "no files of this size in the tree");
    }
    row(metric, "ms", Better::Lower)
        .value(s.p50().unwrap_or(0.0), s.len())
        .detail(if idle {
            "never-opened file after 2 s idle"
        } else {
            "never-opened file, warm connection"
        })
}

/// Disjoint sets of never-opened files for the four T7 cases (≈4 KiB and 256 KiB, warm and
/// after idle), shared by the sshfs and Unlatch scenarios.
pub struct T7Plan {
    pub small_warm: Vec<String>,
    pub small_idle: Vec<String>,
    pub big_warm: Vec<String>,
    pub big_idle: Vec<String>,
}

impl T7Plan {
    pub fn new(ctx: &Ctx) -> T7Plan {
        let warm_n = if ctx.quick { 5 } else { 10 };
        let idle_n = if ctx.quick { 1 } else { 3 };
        let mut small: Vec<String> = ctx
            .tree
            .flat_small
            .iter()
            .filter(|p| file_size(ctx, p) <= 4096 + 512)
            .cloned()
            .collect();
        if small.len() < warm_n + idle_n {
            small = ctx.tree.flat_small.clone();
        }
        let big = &ctx.tree.flat_256k;
        let half = big.len() / 2;
        T7Plan {
            small_warm: small.iter().take(warm_n).cloned().collect(),
            small_idle: small.iter().skip(warm_n).take(idle_n).cloned().collect(),
            big_warm: big.iter().take(warm_n.min(half)).cloned().collect(),
            big_idle: big.iter().skip(half).take(idle_n).cloned().collect(),
        }
    }

    pub fn cases(&self) -> [(&'static str, &[String], bool); 4] {
        [
            (m::OPEN_4K_COLD_MS, &self.small_warm, false),
            (m::OPEN_256K_COLD_MS, &self.big_warm, false),
            (m::OPEN_4K_COLD_IDLE_MS, &self.small_idle, true),
            (m::OPEN_256K_COLD_IDLE_MS, &self.big_idle, true),
        ]
    }
}

fn file_size(ctx: &Ctx, rel: &str) -> u64 {
    std::fs::metadata(ctx.tree.path(rel))
        .map(|m| m.len())
        .unwrap_or(u64::MAX)
}

fn t13(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    if !matches!(ctx.profile.rate_mbit, Some(r) if r <= 50) {
        return Ok(vec![row(
            m::P99_INTERACTIVE_UNDER_LOAD_MS,
            "ms",
            Better::Lower,
        )
        .status(Status::Skipped, "T13 runs at 20 and 50 Mbit/s only")]);
    }
    let up_dir = ctx.vm_scratch("t13-sshfs")?;
    let rel_up = up_dir.rel(&ctx.tree.root);
    let mnt = SshfsMount::mount(ctx, "t13", &[])?;
    let stop = Arc::new(AtomicBool::new(false));
    let down_bytes = Arc::new(AtomicU64::new(0));
    let big = mnt.path(&ctx.tree.big);
    let dl = {
        let stop = stop.clone();
        let down = down_bytes.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 20];
            while !stop.load(Ordering::Relaxed) {
                let Ok(mut f) = std::fs::File::open(&big) else {
                    return;
                };
                while !stop.load(Ordering::Relaxed) {
                    match f.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(k) => {
                            down.fetch_add(k as u64, Ordering::Relaxed);
                        }
                    }
                }
            }
        })
    };
    let up = {
        let stop = stop.clone();
        let p = mnt.path(&format!("{rel_up}/upload.bin"));
        std::thread::spawn(move || {
            let chunk = vec![0x33u8; 1 << 20];
            let Ok(mut f) = std::fs::File::create(&p) else {
                return;
            };
            while !stop.load(Ordering::Relaxed) {
                if f.write_all(&chunk).is_err() {
                    return;
                }
            }
        })
    };
    std::thread::sleep(Duration::from_secs(1)); // let both transfers ramp up
    let window = Duration::from_secs(if ctx.quick { 5 } else { 20 });
    let t = Instant::now();
    let mut s = Samples::new();
    let mut i = 0u64;
    while t.elapsed() < window {
        let p = mnt.path(&format!("flat/nonexistent-{}-{i}", std::process::id()));
        let t0 = Instant::now();
        let _ = std::fs::symlink_metadata(&p);
        s.push(t0.elapsed());
        i += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, Ordering::Relaxed);
    let moved = down_bytes.load(Ordering::Relaxed);
    drop(mnt); // unmount unblocks any read/write stuck in the kernel
    let _ = dl.join();
    let _ = up.join();
    Ok(vec![row(m::P99_INTERACTIVE_UNDER_LOAD_MS, "ms", Better::Lower)
        .value(s.p99().unwrap_or(0.0), s.len())
        .detail(format!(
            "p99 uncached lstat (1 SFTP round trip) during a download + upload on the same mount; p50 {:.1} ms; {:.1} MiB downloaded",
            s.p50().unwrap_or(0.0),
            moved as f64 / (1 << 20) as f64
        ))])
}

/// T13 floor: p99 of a 1-byte echo on its own TCP connection while plain-TCP bulk transfers
/// saturate the link in both directions — what the link's bottleneck queue alone costs an
/// interactive round trip (no application scheduling involved).
fn raw_t13(ctx: &Ctx) -> Result<Vec<Measurement>> {
    let row = |metric: &str| ctx.row("T13", metric, System::Raw, "ms", Better::Lower);
    if !matches!(ctx.profile.rate_mbit, Some(r) if r <= 50) {
        return Ok(vec![row(m::P99_INTERACTIVE_UNDER_LOAD_MS)
            .status(Status::Skipped, "T13 runs at 20 and 50 Mbit/s only")]);
    }
    let window = Duration::from_secs(if ctx.quick { 5 } else { 20 });
    let f = crate::netlab::load_floor_at(
        &ctx.netlab_dir,
        &ctx.profile.name,
        window,
        Duration::from_millis(100),
    )?;
    Ok(vec![row(m::P99_INTERACTIVE_UNDER_LOAD_MS)
        .value(f.p99_ms, f.samples)
        .detail(format!(
            "link floor: p99 1-byte echo on its own TCP connection during plain-TCP bulk down+up \
             (p50 {:.1} ms, idle RTT {:.1} ms; bulk {:.1} down / {:.1} up Mbit/s)",
            f.p50_ms, f.idle_rtt_ms_p50, f.down_mbit, f.up_mbit
        ))])
}

/// T8 raw ceiling: `cat file` on the VM, bytes read through the same shaped bridge.
fn run_raw(ctx: &Ctx, id: &str) -> Result<Vec<Measurement>> {
    let row = |metric: &str, unit: &str, better| ctx.row(id, metric, System::Raw, unit, better);
    if id == "T13" {
        return raw_t13(ctx);
    }
    if id != "T8" {
        return Ok(vec![
            row("*", "", Better::Lower).status(Status::Skipped, "raw baseline only for T8 and T13")
        ]);
    }
    let (p, size) = t8_file(ctx)?;
    // `cat file` on the VM through the same client path as Unlatch: the raw bridge, or
    // `ssh bench cat file` in `-ssh` profiles (DESIGN §6: "≥ 80% of `ssh host cat file`").
    let rem = super::remote(
        ctx,
        "cat",
        vec!["cat".into(), p.display().to_string()],
        Vec::new(),
    )?;
    let (prog, args) = rem
        .argv
        .split_first()
        .ok_or_else(|| anyhow!("empty client argv"))?;
    let t = Instant::now();
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn raw cat client")?;
    let mut s = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0u64;
    loop {
        let k = s.read(&mut buf)?;
        if k == 0 {
            break;
        }
        got += k as u64;
    }
    let _ = child.wait();
    drop(rem);
    let d = t.elapsed();
    if got != size {
        bail!("raw cat short read {got} != {size}");
    }
    Ok(vec![row(m::THROUGHPUT_MBIT, "Mbit/s", Better::Higher)
        .value(mbit(size, d), 1)
        .detail(format!(
            "`cat` of {} MiB {}, spawn→EOF",
            size >> 20,
            if ctx.ssh.is_some() {
                "via ssh"
            } else {
                "over the raw bridge"
            }
        ))])
}
