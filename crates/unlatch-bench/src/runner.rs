//! `unlatch-bench run`: profiles × scenarios × systems → JSON report + SCORECARD.md.

use crate::measure::{Measurement, System};
use crate::netlab::{self, Netlab, Profile};
use crate::report::{self, RunReport};
use crate::scenarios::{self, Bins, Ctx};
use crate::targets;
use crate::tree::{self, TreeSpec};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct RunOpts {
    pub profiles: Vec<Profile>,
    pub quick: bool,
    /// Target ids to run (`None` = all).
    pub only: Option<Vec<String>>,
    pub systems: Vec<System>,
    pub out: PathBuf,
    pub scorecard: PathBuf,
    pub work: PathBuf,
    /// Mac-side files (see `Ctx::mac`); `None` = under `work`.
    pub mac_dir: Option<PathBuf>,
    pub bins: Bins,
    pub timeout: Duration,
    pub bench_exe: PathBuf,
    pub tree: TreeSpec,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_trim(p: &str) -> String {
    std::fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Locate a binary: explicit path, env var, siblings of `unlatch-bench`, `./target/release`, PATH.
pub fn find_bin(
    explicit: Option<&str>,
    env: &str,
    name: &str,
    bench_exe: &Path,
) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var(env) {
        return Some(PathBuf::from(p));
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(d) = bench_exe.parent() {
        cands.push(d.join(name));
    }
    cands.push(PathBuf::from("target/release").join(name));
    if let Some(path) = std::env::var_os("PATH") {
        cands.extend(std::env::split_paths(&path).map(|d| d.join(name)));
    }
    if let Some(home) = std::env::var_os("HOME") {
        cands.push(PathBuf::from(home).join(".local/bin").join(name));
    }
    cands
        .into_iter()
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
}

pub fn find_sftp_server() -> Option<PathBuf> {
    [
        "/usr/lib/openssh/sftp-server",
        "/usr/libexec/openssh/sftp-server",
        "/usr/libexec/sftp-server",
        "/usr/lib/ssh/sftp-server",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

fn selected(opts: &RunOpts, id: &str) -> bool {
    match &opts.only {
        Some(ids) => ids
            .iter()
            .any(|x| x.eq_ignore_ascii_case(id) || (x.eq_ignore_ascii_case("T12") && id == "T11")),
        None => true,
    }
}

fn log(msg: &str) {
    eprintln!("[unlatch-bench] {msg}");
}

pub fn run(opts: &RunOpts) -> Result<RunReport> {
    let started = now_secs();
    std::fs::create_dir_all(&opts.work)?;
    let cache = opts.work.join("cache");
    let t = Instant::now();
    let tree = tree::ensure(&opts.tree, &cache).context("generate synthetic tree")?;
    log(&format!(
        "tree {} ({} entries + {} lazy) ready in {:.1} s",
        tree.root.display(),
        tree.eager_entries,
        tree.lazy_dirs + tree.lazy_files,
        t.elapsed().as_secs_f64()
    ));
    let run_dir = opts.work.join(format!("run-{}", std::process::id()));
    std::fs::create_dir_all(&run_dir)?;
    let logs = opts.out.parent().unwrap_or(Path::new(".")).join("logs");
    std::fs::create_dir_all(&logs)?;
    let mut rep = RunReport {
        version: report::REPORT_VERSION,
        started,
        host: read_trim("/proc/sys/kernel/hostname"),
        kernel: read_trim("/proc/sys/kernel/osrelease"),
        quick: opts.quick,
        ..Default::default()
    };
    rep.notes.push(format!(
        "binaries: unlatchd={} unlatch={} sshfs={} sftp-server={}",
        disp(&opts.bins.unlatchd),
        disp(&opts.bins.unlatch),
        disp(&opts.bins.sshfs),
        disp(&opts.bins.sftp_server)
    ));
    rep.cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    rep.notes.push(format!(
        "layout: VM side (tree, unlatchd state) under {}; Mac side (engine replica, cache, staging, FUSE state) under {}",
        opts.work.display(),
        opts.mac_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "the same directory".into())
    ));
    rep.notes.push(format!(
        "tree: seed {:#x}, {} eager entries + {} lazy, {:.0} MiB",
        tree.spec.seed,
        tree.eager_entries,
        tree.lazy_dirs + tree.lazy_files,
        tree.total_bytes as f64 / (1 << 20) as f64
    ));

    for (pi, profile) in opts.profiles.iter().enumerate() {
        let ids: Vec<&str> = scenarios::NETWORK_IDS
            .iter()
            .copied()
            .filter(|id| selected(opts, id))
            .collect();
        if ids.is_empty() {
            break;
        }
        rep.profiles.push(profile.clone());
        let nl_dir = PathBuf::from(format!("/tmp/hnl-{}-{pi}", std::process::id()));
        let lab = match Netlab::start(profile, &nl_dir, &opts.bench_exe) {
            Ok(l) => l,
            Err(e) => {
                log(&format!("{}: netlab failed: {e:#}", profile.name));
                rep.notes
                    .push(format!("{}: netlab failed: {e:#}", profile.name));
                continue;
            }
        };
        let cal = netlab::calibrate(&lab, if opts.quick { 2.0 } else { 4.0 }, scenarios::IDLE);
        let cal = match cal {
            Ok(c) => {
                log(&format!(
                    "{}: rtt p50 {:.2} ms, down {:.1} / up {:.1} Mbit/s, 256K warm {:.0} ms / idle {:.0} ms",
                    profile.name, c.rtt_ms_p50, c.down_mbit, c.up_mbit, c.fetch_256k_warm_ms, c.fetch_256k_idle_ms
                ));
                rep.calibrations.push(c.clone());
                Some(c)
            }
            Err(e) => {
                rep.notes
                    .push(format!("{}: calibration failed: {e:#}", profile.name));
                None
            }
        };
        let work = run_dir.join(&profile.name);
        std::fs::create_dir_all(&work)?;
        let mac = match &opts.mac_dir {
            Some(d) => d
                .join(format!("run-{}", std::process::id()))
                .join(&profile.name),
            None => work.clone(),
        };
        std::fs::create_dir_all(&mac)?;
        let load0 = read_loadavg();
        let ssh = if profile.ssh {
            match crate::sshlab::SshLab::setup(&work.join("ssh"), opts.bins.sftp_server.as_deref())
            {
                Ok(s) => Some(s),
                Err(e) => {
                    log(&format!("{}: ssh setup failed: {e:#}", profile.name));
                    rep.notes
                        .push(format!("{}: ssh setup failed: {e:#}", profile.name));
                    continue;
                }
            }
        } else {
            None
        };
        let ctx = Ctx {
            profile: profile.clone(),
            netlab_dir: lab.dir().to_path_buf(),
            bench_exe: opts.bench_exe.clone(),
            work: work.clone(),
            mac: mac.clone(),
            cache: cache.clone(),
            tree: tree.clone(),
            bins: opts.bins.clone(),
            quick: opts.quick,
            calibration: cal,
            log: logs.join(format!("bench-{}.log", profile.name)),
            ssh,
        };
        let ctx_path = work.join("ctx.json");
        std::fs::write(&ctx_path, serde_json::to_vec_pretty(&ctx)?)?;
        let mut rows: Vec<Measurement> = Vec::new();
        for id in ids {
            for sys in scenarios::systems_for(id) {
                if !opts.systems.contains(sys)
                    && !(*sys == System::Raw && opts.systems.contains(&System::Sshfs))
                {
                    continue;
                }
                let t = Instant::now();
                let r =
                    scenarios::run_subprocess(&ctx_path, &ctx, id, *sys, opts.timeout, &ctx.log);
                log(&format!(
                    "{} {id:>3} {:<5} {:>6.1} s  {}",
                    profile.name,
                    sys.as_str(),
                    t.elapsed().as_secs_f64(),
                    summarize(&r)
                ));
                rows.extend(r);
                let _ = tree::clean_scratch(&tree);
            }
        }
        targets::evaluate(&mut rows, profile);
        rep.measurements.extend(rows);
        rep.load.push(report::LoadSample {
            profile: profile.name.clone(),
            start: load0,
            end: read_loadavg(),
        });
        drop(lab);
        let _ = std::fs::remove_dir_all(&nl_dir);
        let _ = std::fs::remove_dir_all(&work);
        if opts.mac_dir.is_some() {
            let _ = std::fs::remove_dir_all(&mac);
        }
    }

    // Daemon-level targets run once, unshaped.
    let dids: Vec<&str> = scenarios::DAEMON_IDS
        .iter()
        .copied()
        .filter(|id| selected(opts, id))
        .collect();
    if !dids.is_empty() && opts.systems.contains(&System::Unlatch) {
        let profile = Profile {
            name: "daemon".into(),
            ..Profile::new(0, None)
        };
        rep.profiles.push(profile.clone());
        let work = run_dir.join("daemon");
        std::fs::create_dir_all(&work)?;
        let ctx = Ctx {
            profile: profile.clone(),
            netlab_dir: PathBuf::new(),
            bench_exe: opts.bench_exe.clone(),
            work: work.clone(),
            mac: work.clone(),
            cache: cache.clone(),
            tree: tree.clone(),
            bins: opts.bins.clone(),
            quick: opts.quick,
            calibration: None,
            log: logs.join("bench-daemon.log"),
            ssh: None,
        };
        let ctx_path = work.join("ctx.json");
        std::fs::write(&ctx_path, serde_json::to_vec_pretty(&ctx)?)?;
        let mut rows = Vec::new();
        for id in dids {
            let t = Instant::now();
            let r = scenarios::run_subprocess(
                &ctx_path,
                &ctx,
                id,
                System::Unlatch,
                opts.timeout,
                &ctx.log,
            );
            log(&format!(
                "daemon {id:>3} unlatch {:>6.1} s  {}",
                t.elapsed().as_secs_f64(),
                summarize(&r)
            ));
            rows.extend(r);
        }
        targets::evaluate(&mut rows, &profile);
        rep.measurements.extend(rows);
        let _ = std::fs::remove_dir_all(&work);
    }
    let _ = std::fs::remove_dir_all(&run_dir);
    rep.finished = now_secs();
    rep.save(&opts.out)?;
    std::fs::write(&opts.scorecard, report::scorecard(&rep))?;
    Ok(rep)
}

/// `/proc/loadavg` (1, 5, 15 min).
fn read_loadavg() -> [f64; 3] {
    let s = read_trim("/proc/loadavg");
    let mut it = s
        .split_whitespace()
        .map(|x| x.parse::<f64>().unwrap_or(0.0));
    [
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
    ]
}

fn disp(p: &Option<PathBuf>) -> String {
    p.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(not found)".into())
}

fn summarize(rows: &[Measurement]) -> String {
    rows.iter()
        .map(|m| match (m.status, m.value) {
            (crate::measure::Status::Ok, Some(v)) => format!("{}={v:.3}{}", m.metric, m.unit),
            (s, _) => format!(
                "{}={:?}({})",
                m.metric,
                s,
                m.detail
                    .as_deref()
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>()
            ),
        })
        .collect::<Vec<_>>()
        .join("  ")
}
