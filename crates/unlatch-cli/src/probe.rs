//! `unlatch probe`: the user-side verification probe (review §2(f)8).
//!
//! Cross-platform on purpose: it needs only POSIX file APIs on the local side (an `unlatch mount`
//! on Linux, `~/Library/CloudStorage/<domain>` on macOS) and a non-interactive ssh session to
//! the VM. "Visible" is detected by polling `stat` every 1 ms. VM-side completion is detected by
//! a remote shell loop that reports back over ssh; one-way latency is estimated as the observed
//! time minus half the measured ssh round trip (`rtt/2`), and both numbers are recorded.
//!
//! Everything is created under a unique `unlatch-probe-<pid>-<rand>` directory, removed at the end.

use crate::remote::{remote_path, sq, RemoteShell};
use crate::stats::{round3, summarize, Summary};
use anyhow::{bail, Context};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SCHEMA: &str = "unlatch-probe/1";

#[derive(Clone, Debug)]
pub struct ProbeArgs {
    pub local: PathBuf,
    pub ssh: String,
    pub remote: String,
    pub json: Option<PathBuf>,
    pub n: usize,
    pub timeout: Duration,
    pub ssh_args: Vec<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct OsInfo {
    pub family: String,
    pub version: String,
    pub kernel: String,
    pub arch: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckResult {
    pub id: String,
    pub title: String,
    /// `None` = could not run.
    pub ok: Option<bool>,
    /// Latency samples in ms (one-way estimate where the definition says so).
    pub samples_ms: Vec<f64>,
    pub summary_ms: Option<Summary>,
    pub extra: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProbeReport {
    pub schema: String,
    pub unlatch_version: String,
    pub started_at: String,
    pub os: OsInfo,
    /// macOS only: whether `/usr/bin/fileproviderctl` exists. `null` elsewhere.
    pub fileproviderctl: Option<bool>,
    pub local: String,
    pub ssh: String,
    pub remote: String,
    pub subdir: String,
    pub rtt_ms: Option<Summary>,
    pub poll_interval_ms: f64,
    pub checks: Vec<CheckResult>,
    pub cleanup_ok: bool,
}

fn uname() -> (String, String, String) {
    // SAFETY: utsname is plain old data; uname fills it with NUL-terminated strings.
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return (String::new(), String::new(), String::new());
    }
    let s = |f: &[libc::c_char]| {
        let bytes: Vec<u8> = f
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    (s(&u.sysname), s(&u.release), s(&u.machine))
}

pub fn os_info() -> OsInfo {
    let (_sys, release, machine) = uname();
    let version = if cfg!(target_os = "macos") {
        std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .map(|o| format!("macOS {}", String::from_utf8_lossy(&o.stdout).trim()))
            .unwrap_or_else(|| "macOS (unknown)".into())
    } else {
        std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|t| {
                t.lines().find_map(|l| {
                    l.strip_prefix("PRETTY_NAME=")
                        .map(|v| v.trim_matches('"').to_string())
                })
            })
            .unwrap_or_else(|| std::env::consts::OS.to_string())
    };
    OsInfo {
        family: std::env::consts::OS.into(),
        version,
        kernel: release,
        arch: machine,
    }
}

pub fn fileproviderctl_present() -> Option<bool> {
    cfg!(target_os = "macos").then(|| Path::new("/usr/bin/fileproviderctl").exists())
}

/// Deterministic text content of exactly `len` bytes for file `i` (verifiable after transfer).
pub fn content_for(i: usize, len: usize) -> String {
    let unit = format!("unlatch-probe file {i} / ");
    unit.chars().cycle().take(len).collect()
}

/// A remote loop that prints `READY`, polls `cond` about every millisecond for up to
/// `limit_ms` iterations and prints `OK=1` or `OK=0`.
pub fn remote_wait_script(cond: &str, limit_ms: u64) -> String {
    format!(
        "printf 'READY\\n'; n=0; ok=0; while [ $n -lt {limit_ms} ]; do if {cond}; then ok=1; break; fi; \
         sleep 0.001 2>/dev/null || :; n=$((n+1)); done; printf 'OK=%s\\n' \"$ok\""
    )
}

/// Poll `pred` every 1 ms until true (returns the elapsed time) or `timeout`.
pub fn poll_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> Option<Duration> {
    let t0 = Instant::now();
    loop {
        if pred() {
            return Some(t0.elapsed());
        }
        if t0.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn result(id: &str, title: &str) -> CheckResult {
    CheckResult {
        id: id.into(),
        title: title.into(),
        ok: None,
        samples_ms: Vec::new(),
        summary_ms: None,
        extra: json!({}),
        error: None,
    }
}

fn finish(mut r: CheckResult, samples: Vec<f64>, ok: bool) -> CheckResult {
    r.samples_ms = samples.iter().copied().map(round3).collect();
    r.summary_ms = summarize(&r.samples_ms).map(|s| Summary {
        n: s.n,
        min: round3(s.min),
        p50: round3(s.p50),
        p95: round3(s.p95),
        max: round3(s.max),
        mean: round3(s.mean),
    });
    r.ok = Some(ok);
    r
}

struct Probe {
    sh: RemoteShell,
    root: String,
    sub: String,
    local_dir: PathBuf,
    half_rtt_ms: f64,
    n: usize,
    timeout: Duration,
}

impl Probe {
    /// Shell word for a path inside the probe directory on the VM.
    fn rp(&self, rel: &str) -> String {
        let base = format!("{}/{}", self.root.trim_end_matches('/'), self.sub);
        if rel.is_empty() {
            remote_path(&base)
        } else {
            remote_path(&format!("{base}/{rel}"))
        }
    }

    fn lp(&self, rel: &str) -> PathBuf {
        self.local_dir.join(rel)
    }

    fn vm(&mut self, cmd: &str) -> anyhow::Result<String> {
        self.sh.check(cmd, self.timeout)
    }

    fn wait_local(&self, rel: &str, len: Option<u64>) -> anyhow::Result<Duration> {
        let p = self.lp(rel);
        poll_until(self.timeout, || match std::fs::metadata(&p) {
            Ok(m) => !matches!(len, Some(l) if m.len() != l),
            Err(_) => false,
        })
        .with_context(|| {
            format!(
                "{} did not become visible within {:?}",
                p.display(),
                self.timeout
            )
        })
    }

    /// Start a remote wait for `cond`, run `action` locally once the loop is running, and return
    /// `(ok, raw_ms, one_way_ms)` measured from just before `action`.
    fn measure_vm_side(
        &mut self,
        cond: &str,
        action: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<(bool, f64, f64)> {
        let limit = self.timeout.as_millis() as u64;
        let seq = self.sh.send(&remote_wait_script(cond, limit))?;
        loop {
            if self.sh.next_line(self.timeout)?.trim() == "READY" {
                break;
            }
        }
        let t0 = Instant::now();
        action()?;
        let line = self.sh.next_line(self.timeout + Duration::from_secs(5))?;
        let raw = ms(t0.elapsed());
        self.sh.finish(seq, self.timeout)?;
        let ok = line.trim() == "OK=1";
        Ok((ok, raw, (raw - self.half_rtt_ms).max(0.0)))
    }

    fn vm_write_visible(&mut self) -> CheckResult {
        let mut r = result("vm_write_visible", "VM write (ssh) → visible locally");
        let mut samples = Vec::new();
        let mut raw = Vec::new();
        for i in 0..self.n {
            let name = format!("w{i}");
            let seq = match self.sh.send(&format!("printf %s x > {}", self.rp(&name))) {
                Ok(s) => s,
                Err(e) => return error(r, e),
            };
            let t0 = Instant::now();
            let seen = poll_until(self.timeout, || {
                std::fs::metadata(self.lp(&name))
                    .map(|m| m.len() == 1)
                    .unwrap_or(false)
            });
            let elapsed = ms(t0.elapsed());
            if let Err(e) = self.sh.finish(seq, self.timeout) {
                return error(r, e);
            }
            match seen {
                Some(_) => {
                    raw.push(round3(elapsed));
                    samples.push((elapsed - self.half_rtt_ms).max(0.0));
                }
                None => {
                    r.error = Some(format!("{name} not visible within {:?}", self.timeout));
                    return finish(r, samples, false);
                }
            }
        }
        r.extra = json!({ "raw_ms": raw, "definition": "time from sending the write over ssh until stat sees it, minus rtt/2" });
        finish(r, samples, true)
    }

    fn first_ls_1k(&mut self) -> CheckResult {
        let mut r = result(
            "first_ls_1k",
            "first ls of a never-listed 1000-entry directory",
        );
        let cmd = format!(
            "mkdir {d} && cd {d} && i=0; while [ $i -lt 1000 ]; do : > \"f$i\"; i=$((i+1)); done",
            d = self.rp("big")
        );
        if let Err(e) = self.vm(&cmd) {
            return error(r, e);
        }
        if let Err(e) = self.wait_local("big", None) {
            return error(r, e);
        }
        let dir = self.lp("big");
        let count = |d: &Path| std::fs::read_dir(d).map(|rd| rd.count()).unwrap_or(0);
        let t0 = Instant::now();
        let first = count(&dir);
        let first_ms = ms(t0.elapsed());
        let complete = poll_until(self.timeout, || count(&dir) == 1000);
        let complete_ms = complete.map(|_| ms(t0.elapsed()));
        let t1 = Instant::now();
        let mut stat_ok = 0usize;
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if std::fs::symlink_metadata(e.path()).is_ok() {
                    stat_ok += 1;
                }
            }
        }
        let ls_la_ms = ms(t1.elapsed());
        // A first listing may be partial when the directory's own event overtook its children's
        // (VM-side batching); it is reported, and `ok` means the listing completed in time.
        r.extra = json!({
            "first_ls_ms": round3(first_ms),
            "first_ls_entries": first,
            "complete_ms": complete_ms.map(round3),
            "ls_la_ms": round3(ls_la_ms),
            "ls_la_entries": stat_ok,
        });
        finish(r, vec![first_ms], complete.is_some())
    }

    fn cat_cold_warm(&mut self) -> CheckResult {
        let mut r = result("cat_small", "cold and second cat of a 4 KiB file");
        let mut cold = Vec::new();
        let mut warm = Vec::new();
        let mut mismatches = 0;
        for i in 0..self.n {
            let name = format!("c{i}");
            let content = content_for(i, 4096);
            if let Err(e) = self.vm(&format!("printf %s {} > {}", sq(&content), self.rp(&name))) {
                return error(r, e);
            }
            if let Err(e) = self.wait_local(&name, Some(4096)) {
                return error(r, e);
            }
            let t0 = Instant::now();
            let a = std::fs::read(self.lp(&name));
            cold.push(ms(t0.elapsed()));
            let t1 = Instant::now();
            let b = std::fs::read(self.lp(&name));
            warm.push(ms(t1.elapsed()));
            if a.ok().as_deref() != Some(content.as_bytes())
                || b.ok().as_deref() != Some(content.as_bytes())
            {
                mismatches += 1;
            }
        }
        r.extra = json!({
            "cold": summarize(&cold),
            "warm": summarize(&warm),
            "warm_samples_ms": warm.iter().copied().map(round3).collect::<Vec<_>>(),
            "content_mismatches": mismatches,
        });
        finish(r, cold, mismatches == 0)
    }

    fn rename_round_trip(&mut self) -> CheckResult {
        let r = result("rename_round_trip", "local rename → visible on the VM");
        let mut samples = Vec::new();
        let mut all_ok = true;
        for i in 0..self.n {
            let (a, b) = (format!("r{i}"), format!("r{i}-renamed"));
            if let Err(e) = self.vm(&format!("printf %s r > {}", self.rp(&a))) {
                return error(r, e);
            }
            if let Err(e) = self.wait_local(&a, Some(1)) {
                return error(r, e);
            }
            let cond = format!("[ -e {} ] && [ ! -e {} ]", self.rp(&b), self.rp(&a));
            let (la, lb) = (self.lp(&a), self.lp(&b));
            match self.measure_vm_side(&cond, || std::fs::rename(&la, &lb).context("local rename"))
            {
                Ok((ok, _raw, one_way)) => {
                    all_ok &= ok;
                    samples.push(one_way);
                }
                Err(e) => return error(r, e),
            }
        }
        finish(r, samples, all_ok)
    }

    fn save_round_trip(&mut self) -> CheckResult {
        let mut r = result(
            "save_round_trip",
            "local save → identical content on the VM",
        );
        let mut samples = Vec::new();
        let mut close_ms = Vec::new();
        let mut all_ok = true;
        for i in 0..self.n {
            let name = format!("s{i}");
            if let Err(e) = self.vm(&format!("printf %s old > {}", self.rp(&name))) {
                return error(r, e);
            }
            if let Err(e) = self.wait_local(&name, Some(3)) {
                return error(r, e);
            }
            let token = format!("saved-{i}-{:016x}", rand::random::<u64>());
            let cond = format!(
                "[ \"$(cat {} 2>/dev/null)\" = {} ]",
                self.rp(&name),
                sq(&token)
            );
            let path = self.lp(&name);
            let mut write_ms = 0.0;
            let res = self.measure_vm_side(&cond, || {
                let t = Instant::now();
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&path)?;
                f.write_all(token.as_bytes())?;
                drop(f);
                write_ms = ms(t.elapsed());
                Ok(())
            });
            match res {
                Ok((ok, _raw, one_way)) => {
                    all_ok &= ok;
                    samples.push(one_way);
                    close_ms.push(write_ms);
                }
                Err(e) => return error(r, e),
            }
        }
        r.extra = json!({ "local_write_close": summarize(&close_ms) });
        finish(r, samples, all_ok)
    }

    fn conflict(&mut self) -> CheckResult {
        let mut r = result("conflict", "edit on both sides → both versions preserved");
        let run = |p: &mut Probe| -> anyhow::Result<Value> {
            p.vm(&format!("printf %s base > {}", p.rp("k.txt")))?;
            p.wait_local("k.txt", Some(4))?;
            if std::fs::read(p.lp("k.txt"))? != b"base" {
                bail!("local copy does not read \"base\"");
            }
            // The Mac opens the file; the agent edits it on the VM; the Mac then saves.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(p.lp("k.txt"))?;
            p.vm(&format!("printf %s vm-edit > {}", p.rp("k.txt")))?;
            std::thread::sleep(
                Duration::from_millis(300).max(Duration::from_secs_f64(4.0 * p.half_rtt_ms / 1e3)),
            );
            f.write_all(b"mac-edit")?;
            // fsync reports an upload failure that a plain close would swallow.
            let close_result = f.sync_all();
            drop(f);
            std::thread::sleep(
                Duration::from_millis(500).max(Duration::from_secs_f64(4.0 * p.half_rtt_ms / 1e3)),
            );
            let listing = p.vm(&format!(
                "for f in {}/k*; do [ -f \"$f\" ] || continue; printf '%s\\t' \"$(basename \"$f\")\"; cat \"$f\"; echo; done",
                p.rp("")
            ))?;
            let files: Vec<(String, String)> = listing
                .lines()
                .filter_map(|l| l.split_once('\t'))
                .map(|(n, c)| (n.to_string(), c.to_string()))
                .collect();
            let canonical = files
                .iter()
                .find(|(n, _)| n == "k.txt")
                .map(|(_, c)| c.clone());
            let vm_kept = files.iter().any(|(_, c)| c == "vm-edit");
            let mac_kept = files.iter().any(|(_, c)| c == "mac-edit");
            let copies: Vec<&String> = files
                .iter()
                .filter(|(n, _)| n != "k.txt")
                .map(|(n, _)| n)
                .collect();
            let converged = canonical.as_ref().and_then(|c| {
                poll_until(p.timeout, || {
                    std::fs::read(p.lp("k.txt"))
                        .map(|b| b == c.as_bytes())
                        .unwrap_or(false)
                })
            });
            let winner = match canonical.as_deref() {
                Some("vm-edit") => "vm",
                Some("mac-edit") => "mac",
                _ => "neither",
            };
            Ok(json!({
                "both_preserved": vm_kept && mac_kept,
                "vm_version_preserved": vm_kept,
                "mac_version_preserved": mac_kept,
                "canonical_name_holds": winner,
                "conflict_copies": copies,
                "local_converged_ms": converged.map(|d| round3(ms(d))),
                "local_close_error": close_result.err().map(|e| e.to_string()),
            }))
        };
        match run(self) {
            Ok(extra) => {
                let ok = extra["both_preserved"].as_bool().unwrap_or(false);
                r.extra = extra;
                finish(r, Vec::new(), ok)
            }
            Err(e) => error(r, e),
        }
    }

    fn delete_round_trip(&mut self) -> CheckResult {
        let r = result("delete_round_trip", "local delete → gone on the VM");
        let mut samples = Vec::new();
        let mut all_ok = true;
        for i in 0..self.n {
            let name = format!("d{i}");
            if let Err(e) = self.vm(&format!("printf %s d > {}", self.rp(&name))) {
                return error(r, e);
            }
            if let Err(e) = self.wait_local(&name, Some(1)) {
                return error(r, e);
            }
            let cond = format!("[ ! -e {} ]", self.rp(&name));
            let path = self.lp(&name);
            match self.measure_vm_side(&cond, || {
                std::fs::remove_file(&path).context("local delete")
            }) {
                Ok((ok, _raw, one_way)) => {
                    all_ok &= ok;
                    samples.push(one_way);
                }
                Err(e) => return error(r, e),
            }
        }
        finish(r, samples, all_ok)
    }
}

fn error(mut r: CheckResult, e: anyhow::Error) -> CheckResult {
    r.ok = Some(false);
    r.error = Some(format!("{e:#}"));
    r
}

pub fn run(args: &ProbeArgs) -> anyhow::Result<ProbeReport> {
    if !args.local.is_dir() {
        bail!(
            "{} is not a directory (the local view of --remote)",
            args.local.display()
        );
    }
    let started_at = crate::timefmt::now_rfc3339();
    let sh = RemoteShell::connect(&args.ssh, &args.ssh_args).with_context(|| {
        format!(
            "ssh {} (run `unlatch doctor --host {} --root {}`)",
            args.ssh, args.ssh, args.remote
        )
    })?;
    let sub = format!(
        "unlatch-probe-{}-{:08x}",
        std::process::id(),
        rand::random::<u32>()
    );
    let mut p = Probe {
        sh,
        root: args.remote.clone(),
        local_dir: args.local.join(&sub),
        sub: sub.clone(),
        half_rtt_ms: 0.0,
        n: args.n.max(1),
        timeout: args.timeout,
    };
    let rtt = p.sh.rtt_samples(10)?;
    p.half_rtt_ms = crate::stats::median(&rtt).unwrap_or(0.0) / 2.0;
    p.vm(&format!("mkdir -p {}", p.rp("")))?;

    let mut checks = Vec::new();
    let mut setup = result("dir_visible", "VM mkdir → visible locally");
    let t0 = Instant::now();
    match p.wait_local("", None) {
        Ok(_) => {
            let v = (ms(t0.elapsed()) - p.half_rtt_ms).max(0.0);
            checks.push(finish(setup, vec![v], true));
        }
        Err(e) => {
            setup = error(setup, e);
            checks.push(setup);
        }
    }
    if checks[0].ok == Some(true) {
        type Step = fn(&mut Probe) -> CheckResult;
        let steps: [Step; 7] = [
            Probe::vm_write_visible,
            Probe::first_ls_1k,
            Probe::cat_cold_warm,
            Probe::rename_round_trip,
            Probe::save_round_trip,
            Probe::conflict,
            Probe::delete_round_trip,
        ];
        for step in steps {
            let r = step(&mut p);
            eprintln!(
                "  {:<18} {}",
                r.id,
                if r.ok == Some(true) { "done" } else { "FAILED" }
            );
            checks.push(r);
        }
    }

    let cleanup_ok = p.vm(&format!("rm -rf {}", p.rp(""))).is_ok()
        && poll_until(Duration::from_secs(10).max(p.timeout), || {
            !p.local_dir.exists()
        })
        .is_some();

    Ok(ProbeReport {
        schema: SCHEMA.into(),
        unlatch_version: env!("CARGO_PKG_VERSION").into(),
        started_at,
        os: os_info(),
        fileproviderctl: fileproviderctl_present(),
        local: args.local.display().to_string(),
        ssh: args.ssh.clone(),
        remote: args.remote.clone(),
        subdir: sub,
        rtt_ms: summarize(&rtt).map(|s| Summary {
            p50: round3(s.p50),
            p95: round3(s.p95),
            ..s
        }),
        poll_interval_ms: 1.0,
        checks,
        cleanup_ok,
    })
}

pub fn scorecard(r: &ProbeReport) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "unlatch probe  {} ({}, {} {})\n  local  {}\n  remote {}:{}\n",
        r.started_at, r.os.version, r.os.family, r.os.kernel, r.local, r.ssh, r.remote
    ));
    if let Some(rtt) = &r.rtt_ms {
        s.push_str(&format!(
            "  ssh rtt p50 {:.2} ms (one-way estimates subtract {:.2} ms)\n",
            rtt.p50,
            rtt.p50 / 2.0
        ));
    }
    if let Some(f) = r.fileproviderctl {
        s.push_str(&format!(
            "  fileproviderctl: {}\n",
            if f { "present" } else { "missing" }
        ));
    }
    s.push_str(&format!(
        "\n  {:<18} {:>4} {:>10} {:>10}  {}\n",
        "check", "n", "p50 ms", "p95 ms", "result"
    ));
    for c in &r.checks {
        let (n, p50, p95) = match &c.summary_ms {
            Some(m) => (
                m.n.to_string(),
                format!("{:.2}", m.p50),
                format!("{:.2}", m.p95),
            ),
            None => ("-".into(), "-".into(), "-".into()),
        };
        let res = match c.ok {
            Some(true) => "ok".to_string(),
            Some(false) => format!(
                "FAIL{}",
                c.error
                    .as_ref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            ),
            None => "not run".into(),
        };
        s.push_str(&format!(
            "  {:<18} {:>4} {:>10} {:>10}  {}\n",
            c.id, n, p50, p95, res
        ));
        match c.id.as_str() {
            "first_ls_1k" => s.push_str(&format!(
                "  {:<18} first ls {} entries; complete after {} ms; ls -la {} ms\n",
                "", c.extra["first_ls_entries"], c.extra["complete_ms"], c.extra["ls_la_ms"]
            )),
            "cat_small" => {
                if let Some(w) = c.extra["warm"].as_object() {
                    s.push_str(&format!(
                        "  {:<18} second cat p50 {} ms\n",
                        "",
                        w.get("p50").cloned().unwrap_or(Value::Null)
                    ));
                }
            }
            "conflict" => s.push_str(&format!(
                "  {:<18} canonical name holds the {} version; copies: {}\n",
                "", c.extra["canonical_name_holds"], c.extra["conflict_copies"]
            )),
            _ => {}
        }
    }
    s.push_str(&format!(
        "\n  cleanup: {}\n",
        if r.cleanup_ok {
            "ok"
        } else {
            "INCOMPLETE (remove the unlatch-probe-* dir by hand)"
        }
    ));
    s
}

pub fn all_ok(r: &ProbeReport) -> bool {
    r.cleanup_ok && r.checks.iter().all(|c| c.ok == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_is_deterministic_and_sized() {
        assert_eq!(content_for(3, 4096).len(), 4096);
        assert_eq!(content_for(3, 10), content_for(3, 10));
        assert_ne!(content_for(3, 64), content_for(4, 64));
        assert!(
            !content_for(1, 4096).contains('\''),
            "must survive single quoting cheaply"
        );
    }

    #[test]
    fn remote_wait_script_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x");
        let cond = format!("[ -e {} ]", sq(&f.to_string_lossy()));
        let run = |limit| {
            let o = std::process::Command::new("sh")
                .arg("-c")
                .arg(remote_wait_script(&cond, limit))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        assert_eq!(run(5), "READY\nOK=0\n");
        std::fs::write(&f, "").unwrap();
        assert_eq!(run(5), "READY\nOK=1\n");
    }

    #[test]
    fn poll_until_times_out_and_succeeds() {
        assert!(poll_until(Duration::from_millis(5), || false).is_none());
        let mut n = 0;
        assert!(poll_until(Duration::from_secs(1), || {
            n += 1;
            n > 3
        })
        .is_some());
    }

    #[test]
    fn os_info_is_filled() {
        let o = os_info();
        assert_eq!(o.family, std::env::consts::OS);
        assert!(!o.kernel.is_empty());
        assert!(!o.arch.is_empty());
        if cfg!(target_os = "linux") {
            assert_eq!(fileproviderctl_present(), None);
        }
    }

    #[test]
    fn report_json_shape_and_scorecard() {
        let r = ProbeReport {
            schema: SCHEMA.into(),
            unlatch_version: "0".into(),
            started_at: "2026-09-30T00:00:00Z".into(),
            os: os_info(),
            fileproviderctl: None,
            local: "/mnt".into(),
            ssh: "vm".into(),
            remote: "/srv".into(),
            subdir: "unlatch-probe-1-0".into(),
            rtt_ms: summarize(&[1.0, 2.0]),
            poll_interval_ms: 1.0,
            checks: vec![
                finish(result("vm_write_visible", "t"), vec![1.0, 2.0, 3.0], true),
                error(result("conflict", "t"), anyhow::anyhow!("boom")),
            ],
            cleanup_ok: true,
        };
        let v: Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["schema"], "unlatch-probe/1");
        assert_eq!(v["checks"][0]["summary_ms"]["p50"], 2.0);
        assert_eq!(v["checks"][1]["ok"], false);
        assert_eq!(v["checks"][1]["error"], "boom");
        assert!(v["fileproviderctl"].is_null());
        for k in [
            "os",
            "rtt_ms",
            "local",
            "ssh",
            "remote",
            "subdir",
            "cleanup_ok",
            "started_at",
        ] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
        let card = scorecard(&r);
        assert!(card.contains("vm_write_visible"));
        assert!(card.contains("FAIL: boom"));
        assert!(!all_ok(&r));
    }

    /// End-to-end probe mechanics with the VM side being this machine and "local" being the
    /// same directory (no Unlatch in between, so everything is immediately visible).
    #[test]
    #[ignore = "needs ssh localhost"]
    fn probe_against_identity_mount() {
        let dir = tempfile::tempdir().unwrap();
        let args = ProbeArgs {
            local: dir.path().to_path_buf(),
            ssh: "localhost".into(),
            remote: dir.path().to_string_lossy().into_owned(),
            json: None,
            n: 3,
            timeout: Duration::from_secs(10),
            ssh_args: vec![],
        };
        let r = run(&args).unwrap();
        eprintln!("{}", scorecard(&r));
        for c in &r.checks {
            if c.id == "conflict" {
                // Without Unlatch there is no conflict handling: plain overwrite, one version lost.
                assert_eq!(c.extra["both_preserved"], false);
            } else {
                assert_eq!(c.ok, Some(true), "{c:?}");
            }
        }
        assert!(r.cleanup_ok);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
