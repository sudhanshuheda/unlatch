//! Benchmark scenarios. Each `(target id, system)` pair runs in its own `unlatch-bench scenario`
//! subprocess so that a panicking (`todo!()`) or hanging system under test cannot take the run
//! down: the orchestrator kills it at the deadline and records `timeout`/`error`.

pub mod daemon;
pub mod local;
pub mod sshfs;
pub mod unlatch;

use crate::measure::{Better, Measurement, Status, System};
use crate::netlab::{Calibration, Profile};
use crate::tree::TreeManifest;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Paths of the binaries under test and the baseline tools.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Bins {
    pub unlatchd: Option<PathBuf>,
    pub unlatch: Option<PathBuf>,
    pub sshfs: Option<PathBuf>,
    pub sftp_server: Option<PathBuf>,
}

/// Everything a scenario subprocess needs (serialized to a file).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ctx {
    pub profile: Profile,
    /// Netlab socket dir (`client.sock`, `svc/`); empty for daemon-level scenarios.
    pub netlab_dir: PathBuf,
    pub bench_exe: PathBuf,
    /// Per-profile scratch space (mount points, daemon state, VM-side files).
    pub work: PathBuf,
    /// Per-profile "Mac side": engine replica, content cache, upload staging, FUSE client
    /// state and downloaded files (`--mac-dir`; defaults to `work`). Lets a run separate the
    /// client disk's fsync cost from the protocol (e.g. tmpfs ≈ a quiet laptop SSD).
    #[serde(default)]
    pub mac: PathBuf,
    /// Tree/fixture cache.
    pub cache: PathBuf,
    pub tree: TreeManifest,
    pub bins: Bins,
    pub quick: bool,
    pub calibration: Option<Calibration>,
    /// Log file for child stderr (daemons, sshfs).
    pub log: PathBuf,
    /// Present for `-ssh` profiles: sessions go through real ssh ⇄ sshd.
    #[serde(default)]
    pub ssh: Option<crate::sshlab::SshLab>,
}

impl Ctx {
    pub fn client_sock(&self) -> PathBuf {
        self.netlab_dir.join("client.sock")
    }

    pub fn rtt(&self) -> Duration {
        Duration::from_millis(u64::from(self.profile.rtt_ms))
    }

    /// Row factory for this context.
    pub fn row(
        &self,
        id: &str,
        metric: &str,
        system: System,
        unit: &str,
        better: Better,
    ) -> Measurement {
        Measurement::new(id, metric, system, &self.profile.name, unit, better)
    }

    /// Bytes to move in a bulk test so it takes about `secs` at the nominal rate
    /// (≤ 256 MiB, ≥ 4 MiB).
    pub fn bulk_bytes(&self, secs: f64) -> u64 {
        match self.profile.rate_bytes_per_sec() {
            Some(bps) => ((bps as f64 * secs) as u64).clamp(4 << 20, 256 << 20),
            None => 256 << 20,
        }
    }

    /// A unique scratch directory on the VM side (removed on drop).
    pub fn vm_scratch(&self, tag: &str) -> Result<ScratchDir> {
        let p = self
            .tree
            .root
            .join("scratch")
            .join(format!("{tag}-{}", std::process::id()));
        if p.exists() {
            std::fs::remove_dir_all(&p)?;
        }
        std::fs::create_dir_all(&p)?;
        Ok(ScratchDir { path: p })
    }

    /// Root of the Mac-side files (see [`Ctx::mac`]).
    pub fn mac_root(&self) -> PathBuf {
        if self.mac.as_os_str().is_empty() {
            self.work.clone()
        } else {
            self.mac.clone()
        }
    }

    /// A unique Mac-side scratch directory (removed on drop).
    pub fn mac_scratch(&self, tag: &str) -> Result<ScratchDir> {
        let p = self
            .mac_root()
            .join(format!("{tag}-{}", std::process::id()));
        if p.exists() {
            std::fs::remove_dir_all(&p)?;
        }
        std::fs::create_dir_all(&p)?;
        Ok(ScratchDir { path: p })
    }

    /// A unique local scratch directory under `work` (removed on drop).
    pub fn local_scratch(&self, tag: &str) -> Result<ScratchDir> {
        let p = self.work.join(format!("{tag}-{}", std::process::id()));
        if p.exists() {
            std::fs::remove_dir_all(&p)?;
        }
        std::fs::create_dir_all(&p)?;
        Ok(ScratchDir { path: p })
    }
}

/// A server on the VM side of the shaped link: `cmd` runs per connection, reached either through
/// the raw netlab bridge or — in `-ssh` profiles — as the remote command of a real ssh session.
pub struct Remote {
    pub host: crate::netlab::ServiceHost,
    /// Netlab service name (byte counters in `@stats`).
    pub service: String,
    /// Client argv whose stdin/stdout reach `cmd`.
    pub argv: Vec<String>,
    /// `ssh -F <cfg>` (ssh profiles): sshfs's `ssh_command`, host `bench`.
    pub ssh_command: Option<Vec<String>>,
}

/// Start a [`Remote`] named `name` (unique per scenario process) running `cmd` with `env`.
pub fn remote(
    ctx: &Ctx,
    name: &str,
    cmd: Vec<String>,
    env: Vec<(String, String)>,
) -> Result<Remote> {
    use crate::netlab::{connect_argv, Service, ServiceHost};
    let service = format!("{name}-{}", std::process::id());
    match &ctx.ssh {
        None => {
            let host = ServiceHost::start(
                &ctx.netlab_dir,
                &service,
                Service::Command {
                    argv: cmd,
                    env,
                    cwd: None,
                    stderr: Some(ctx.log.clone()),
                },
            )?;
            let argv = connect_argv(&ctx.bench_exe, &ctx.client_sock(), &service);
            Ok(Remote {
                host,
                service,
                argv,
                ssh_command: None,
            })
        }
        Some(ssh) => {
            let host = ServiceHost::start(
                &ctx.netlab_dir,
                &service,
                Service::Command {
                    argv: ssh.sshd_argv(),
                    env: Vec::new(),
                    cwd: None,
                    stderr: Some(ctx.log.clone()),
                },
            )?;
            let proxy = connect_argv(&ctx.bench_exe, &ctx.client_sock(), &service);
            let cfg = ssh.client_config(&service, &proxy)?;
            let base = vec![
                ssh.ssh.display().to_string(),
                "-F".to_string(),
                cfg.display().to_string(),
            ];
            let mut line: Vec<String> = vec!["exec".into()];
            if !env.is_empty() {
                line.push("env".into());
                line.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
            }
            line.extend(cmd);
            let mut argv = base.clone();
            argv.push("bench".into());
            argv.push(crate::sshlab::shell_quote(&line));
            Ok(Remote {
                host,
                service,
                argv,
                ssh_command: Some(base),
            })
        }
    }
}

/// PIDs of processes whose command line contains every one of `needles` (e.g. the `unlatchd`
/// serving one scenario, whether spawned by the netlab directly or by sshd).
pub fn pids_with_cmdline(needles: &[&str]) -> Vec<u32> {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut v = Vec::new();
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        if needles.iter().all(|n| args.iter().any(|a| a == n)) {
            v.push(pid);
        }
    }
    v
}

pub struct ScratchDir {
    pub path: PathBuf,
}

impl ScratchDir {
    /// Path relative to the VM root (for VM scratch dirs).
    pub fn rel(&self, root: &Path) -> String {
        self.path
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Which systems implement which target ids. `T12` is produced by the `T11` Unlatch scenario;
/// T15–T17 are daemon-level (run once, profile `daemon`).
pub fn systems_for(id: &str) -> &'static [System] {
    use System::*;
    match id {
        "T1" | "T2" => &[Local, Unlatch],
        "T3" => &[Local, Sshfs, Unlatch],
        "T4" | "T5" | "T7" => &[Sshfs, Unlatch],
        // Raw = the link's own floor (plain-TCP bulk down+up, probe on its own connection).
        "T13" => &[Raw, Sshfs, Unlatch],
        "T6" | "T9" => &[Local, Sshfs, Unlatch],
        "T8" => &[Local, Raw, Sshfs, Unlatch],
        "T10" | "T14" => &[Unlatch],
        "T11" => &[Local, Sshfs, Unlatch],
        "T12" => &[],
        "T15" | "T16" | "T17" => &[Unlatch],
        _ => &[],
    }
}

pub const NETWORK_IDS: &[&str] = &[
    "T1", "T2", "T3", "T4", "T5", "T6", "T7", "T8", "T9", "T10", "T11", "T13", "T14",
];
pub const DAEMON_IDS: &[&str] = &["T15", "T16", "T17"];

/// Run one scenario in-process (inside the `scenario` subprocess).
pub fn run_in_process(ctx: &Ctx, id: &str, system: System) -> Vec<Measurement> {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match system {
        System::Local => local::run(ctx, id),
        System::Sshfs | System::Raw => sshfs::run(ctx, id, system),
        System::Unlatch if DAEMON_IDS.contains(&id) => daemon::run(ctx, id),
        System::Unlatch => unlatch::run(ctx, id),
        System::Net => Ok(Vec::new()),
    }));
    match r {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => vec![ctx
            .row(id, "*", system, "", Better::Lower)
            .status(Status::Error, format!("{e:#}"))],
        Err(p) => {
            let msg = panic_message(&p);
            let status = if msg.contains("not yet implemented") || msg.contains("not implemented") {
                Status::Unavailable
            } else {
                Status::Error
            };
            vec![ctx
                .row(id, "*", system, "", Better::Lower)
                .status(status, format!("panicked: {msg}"))]
        }
    }
}

pub fn panic_message(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    }
}

/// Entry point of `unlatch-bench scenario --ctx F --id T3 --system sshfs --out F2`.
pub fn scenario_main(ctx_path: &Path, id: &str, system: System, out: &Path) -> Result<i32> {
    let ctx: Ctx = serde_json::from_slice(&std::fs::read(ctx_path)?).context("parse ctx")?;
    // Keep panic output short in the logs; the message is recorded in the row.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("scenario panic: {info}");
    }));
    note(format_args!(
        "scenario {id} {} on {} start",
        system.as_str(),
        ctx.profile.name
    ));
    let rows = run_in_process(&ctx, id, system);
    note(format_args!(
        "scenario {id} {} done ({} rows)",
        system.as_str(),
        rows.len()
    ));
    std::fs::write(out, serde_json::to_vec_pretty(&rows)?)?;
    Ok(0)
}

/// Run a scenario subprocess with a deadline (orchestrator side).
pub fn run_subprocess(
    ctx_path: &Path,
    ctx: &Ctx,
    id: &str,
    system: System,
    timeout: Duration,
    log: &Path,
) -> Vec<Measurement> {
    let out = ctx.work.join(format!("rows-{id}-{}.json", system.as_str()));
    let _ = std::fs::remove_file(&out);
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log);
    let mut cmd = Command::new(&ctx.bench_exe);
    cmd.arg("scenario")
        .arg("--ctx")
        .arg(ctx_path)
        .arg("--id")
        .arg(id)
        .arg("--system")
        .arg(system.as_str())
        .arg("--out")
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    match stderr {
        Ok(f) => cmd.stderr(Stdio::from(f)),
        Err(_) => cmd.stderr(Stdio::null()),
    };
    // Own process group so a timeout kills everything the scenario spawned.
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let fail = |status: Status, why: String| {
        vec![ctx
            .row(id, "*", system, "", Better::Lower)
            .status(status, why)]
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(Status::Error, format!("spawn scenario: {e}")),
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                return match std::fs::read(&out)
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                {
                    Some(rows) => rows,
                    None => fail(
                        Status::Error,
                        format!(
                            "scenario exited with {st} and no result (see {})",
                            log.display()
                        ),
                    ),
                };
            }
            Ok(None) => {}
            Err(e) => return fail(Status::Error, format!("wait: {e}")),
        }
        if start.elapsed() > timeout {
            // SAFETY: plain syscall on the child's process group id.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            let last = last_note(log, child.id())
                .map(|l| format!("; last step: {l}"))
                .unwrap_or_default();
            return fail(
                Status::Timeout,
                format!("no result within {timeout:?}{last}"),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- shared helpers ------------------------------------------------------------------------

/// Last [`note`] line the scenario process `pid` wrote to `log` (for timeout diagnostics).
pub fn last_note(log: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(log).ok()?;
    let tag = format!(" pid {pid}] ");
    text.lines()
        .rev()
        .find_map(|l| l.split_once(&tag).map(|(_, rest)| rest.to_string()))
}

/// Timestamped progress line on stderr (the scenario's log file).
pub fn note(msg: impl std::fmt::Display) {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let t = START.get_or_init(Instant::now).elapsed();
    eprintln!(
        "[{:>8.3}s pid {}] {msg}",
        t.as_secs_f64(),
        std::process::id()
    );
}

/// Run `argv`, discard stdout, and return the elapsed wall time.
pub fn time_cmd(argv: &[&str]) -> Result<Duration> {
    let (prog, args) = argv.split_first().context("empty argv")?;
    let t = Instant::now();
    let st = Command::new(prog)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    let d = t.elapsed();
    if !st.status.success() {
        bail!(
            "{} failed: {}",
            argv.join(" "),
            String::from_utf8_lossy(&st.stderr).trim()
        );
    }
    Ok(d)
}

/// `ls -la` of `dir`, output discarded.
pub fn ls_la(dir: &Path) -> Result<Duration> {
    let d = dir.to_str().context("non-utf8 path")?;
    time_cmd(&["ls", "-la", "--color=never", d])
}

/// Read a whole file, returning (bytes, elapsed).
pub fn read_file(p: &Path) -> Result<(u64, Duration)> {
    use std::io::Read;
    let t = Instant::now();
    let mut f = std::fs::File::open(p).with_context(|| format!("open {}", p.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        n += k as u64;
    }
    Ok((n, t.elapsed()))
}

/// Write the first `n` bytes of `src` to `dst` (T8 test file).
pub fn copy_prefix(src: &Path, dst: &Path, n: u64) -> Result<()> {
    use std::io::Read;
    let f = std::fs::File::open(src)?;
    let mut out = std::fs::File::create(dst)?;
    std::io::copy(&mut f.take(n), &mut out)?;
    out.sync_all()?;
    Ok(())
}

/// Poll `f` every `every` until it returns true or `timeout` passes. Returns time to success.
pub fn poll_until(
    timeout: Duration,
    every: Duration,
    mut f: impl FnMut() -> bool,
) -> Option<Duration> {
    let t = Instant::now();
    loop {
        if f() {
            return Some(t.elapsed());
        }
        if t.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(every);
    }
}

/// The T8 test file on the VM: `scratch/t8/bulk-<n>.bin` (first `n` bytes of `big.bin`).
pub fn t8_file(ctx: &Ctx) -> Result<(PathBuf, u64)> {
    let n = ctx.bulk_bytes(if ctx.quick { 3.0 } else { 20.0 });
    let dir = ctx.tree.root.join("scratch").join("t8");
    std::fs::create_dir_all(&dir)?;
    let p = dir.join(format!("bulk-{n}.bin"));
    let ok = std::fs::metadata(&p).map(|m| m.len() == n).unwrap_or(false);
    if !ok {
        let tmp = dir.join(format!(".tmp-{}", std::process::id()));
        copy_prefix(&ctx.tree.path(&ctx.tree.big), &tmp, n)?;
        std::fs::rename(&tmp, &p)?;
    }
    Ok((p, n))
}

pub fn mbit(bytes: u64, d: Duration) -> f64 {
    bytes as f64 * 8.0 / d.as_secs_f64().max(1e-9) / 1e6
}

/// Sleep long enough for Linux to collapse the congestion window (RTO ≥ 200 ms; design review D24
/// measured with 2 s).
pub const IDLE: Duration = Duration::from_secs(2);
