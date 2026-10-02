//! Command-line front end of `unlatch-bench`.

use crate::measure::System;
use crate::netlab::{self, Profile};
use crate::report::{self, RunReport};
use crate::runner::{self, RunOpts};
use anyhow::{anyhow, bail, Result};
use std::path::PathBuf;
use std::time::Duration;

/// Small argv cursor: `--flag value` options plus positionals.
pub struct Args {
    items: Vec<String>,
}

impl Args {
    pub fn new(items: &[String]) -> Args {
        Args {
            items: items.to_vec(),
        }
    }

    /// Remove `--name value` and return the value.
    pub fn opt(&mut self, name: &str) -> Result<Option<String>> {
        let flag = format!("--{name}");
        let prefix = format!("--{name}=");
        if let Some(i) = self.items.iter().position(|a| a.starts_with(&prefix)) {
            let v = self.items.remove(i)[prefix.len()..].to_string();
            return Ok(Some(v));
        }
        match self.items.iter().position(|a| *a == flag) {
            Some(i) => {
                if i + 1 >= self.items.len() {
                    bail!("{flag} needs a value");
                }
                self.items.remove(i);
                Ok(Some(self.items.remove(i)))
            }
            None => Ok(None),
        }
    }

    /// Remove every `--name value` occurrence.
    pub fn opts(&mut self, name: &str) -> Result<Vec<String>> {
        let mut v = Vec::new();
        while let Some(x) = self.opt(name)? {
            v.push(x);
        }
        Ok(v)
    }

    /// Remove `--name` and report whether it was present.
    pub fn flag(&mut self, name: &str) -> bool {
        let flag = format!("--{name}");
        match self.items.iter().position(|a| *a == flag) {
            Some(i) => {
                self.items.remove(i);
                true
            }
            None => false,
        }
    }

    pub fn rest(self) -> Vec<String> {
        self.items
    }

    pub fn finish(self) -> Result<Vec<String>> {
        if let Some(bad) = self.items.iter().find(|a| a.starts_with("--")) {
            bail!("unknown option {bad}");
        }
        Ok(self.items)
    }
}

const USAGE: &str = "usage: unlatch-bench <command>
  run [--profile P,..] [--quick|--full] [--only T3,T4] [--systems unlatch,sshfs,local]
      [--out target/bench/<name>.json] [--scorecard target/bench/SCORECARD.md] [--work DIR]
      [--mac-dir DIR]   (engine replica/cache/staging here, e.g. tmpfs; VM side stays in --work)
      [--unlatchd PATH] [--unlatch PATH] [--sshfs PATH] [--timeout SECS]
  compare BASE.json NEW.json [--threshold 0.10]
  fpsim ARGS..        (unlatch_bench::fpsim::run_cli)
  fuzz ARGS..         (unlatch_bench::fuzz::run_cli)
  gen-tree --out DIR [--tiny] [--seed N]
  netlab calibrate [--profile P,..] [--json]
  netlab floor [--profile P,..] [--secs N] [--json]   (probe RTT under plain-TCP bulk down+up)
  netlab up --profile P [--dir DIR] [--serve NAME=CMD ...]
  netlab connect CLIENT.SOCK SERVICE [ignored..]
  netlab holder --dir DIR --profile P [--slow-start-after-idle 0|1]   (internal)
profiles: rtt<ms>-bw<mbit> (e.g. rtt40-bw50) or rtt<ms>; see bench/README.md";

pub fn main(args: &[String]) -> Result<i32> {
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    match cmd.as_str() {
        "fpsim" => crate::fpsim::run_cli(rest),
        "fuzz" => crate::fuzz::run_cli(rest),
        "netlab" => netlab_cmd(rest),
        "run" => run_cmd(rest),
        "compare" => compare_cmd(rest),
        "scenario" => {
            let mut a = Args::new(rest);
            let ctx = PathBuf::from(a.opt("ctx")?.ok_or_else(|| anyhow!("--ctx required"))?);
            let id = a.opt("id")?.ok_or_else(|| anyhow!("--id required"))?;
            let sys = a
                .opt("system")?
                .ok_or_else(|| anyhow!("--system required"))?;
            let system = System::parse(&sys).ok_or_else(|| anyhow!("bad system {sys:?}"))?;
            let out = PathBuf::from(a.opt("out")?.ok_or_else(|| anyhow!("--out required"))?);
            a.finish()?;
            crate::scenarios::scenario_main(&ctx, &id, system, &out)
        }
        "gen-tree" => {
            let mut a = Args::new(rest);
            let out = PathBuf::from(a.opt("out")?.ok_or_else(|| anyhow!("--out DIR required"))?);
            let mut spec = if a.flag("tiny") {
                crate::tree::TreeSpec::tiny()
            } else {
                crate::tree::TreeSpec::full()
            };
            if let Some(seed) = a.opt("seed")? {
                spec.seed = seed.parse()?;
            }
            a.finish()?;
            let t = std::time::Instant::now();
            let m = crate::tree::ensure(&spec, &out)?;
            println!(
                "tree at {}: eager {} entries (+{} lazy), {:.1} MiB, {:.1} s",
                m.root.display(),
                m.eager_entries,
                m.lazy_dirs + m.lazy_files,
                m.total_bytes as f64 / (1 << 20) as f64,
                t.elapsed().as_secs_f64()
            );
            Ok(0)
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(0)
        }
        other => {
            eprintln!("unknown command {other:?}\n{USAGE}");
            Ok(2)
        }
    }
}

/// Path of the running `unlatch-bench` binary (the netlab needs it inside the namespace).
pub fn bench_exe() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("UNLATCH_BENCH_BIN") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe()?;
    if exe.file_name().is_some_and(|n| n == "unlatch-bench") {
        return Ok(exe);
    }
    // Test binaries live in target/<profile>/deps/.
    for cand in [exe.parent().and_then(|p| p.parent()), exe.parent()]
        .into_iter()
        .flatten()
    {
        let p = cand.join("unlatch-bench");
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "cannot locate the unlatch-bench binary; set UNLATCH_BENCH_BIN"
    ))
}

fn netlab_cmd(args: &[String]) -> Result<i32> {
    let Some((sub, rest)) = args.split_first() else {
        bail!("netlab: missing subcommand\n{USAGE}");
    };
    match sub.as_str() {
        "holder" => {
            let mut a = Args::new(rest);
            let dir = PathBuf::from(a.opt("dir")?.ok_or_else(|| anyhow!("--dir required"))?);
            let profile = Profile::parse(
                &a.opt("profile")?
                    .ok_or_else(|| anyhow!("--profile required"))?,
            )?;
            let ssai = a
                .opt("slow-start-after-idle")?
                .map(|v| v.parse())
                .transpose()?;
            a.finish()?;
            netlab::holder_main(netlab::HolderArgs {
                dir,
                profile,
                slow_start_after_idle: ssai,
            })?;
            Ok(0)
        }
        "connect" => {
            // Positional only; trailing args (sshfs appends ssh options) are ignored.
            let (Some(sock), Some(svc)) = (rest.first(), rest.get(1)) else {
                bail!("netlab connect CLIENT.SOCK SERVICE");
            };
            netlab::connect_main(std::path::Path::new(sock), svc)
        }
        "calibrate" => {
            let mut a = Args::new(rest);
            let profiles = match a.opt("profile")? {
                Some(p) => Profile::parse_list(&p)?,
                None => Profile::standard(),
            };
            let json = a.flag("json");
            a.finish()?;
            let exe = bench_exe()?;
            let mut out = Vec::new();
            for p in &profiles {
                let dir = tempfile::Builder::new().prefix("hnl-").tempdir_in("/tmp")?;
                let lab = netlab::Netlab::start(p, dir.path(), &exe)?;
                let c = netlab::calibrate(&lab, 2.0, Duration::from_secs(2))?;
                if !json {
                    println!(
                        "{:<12} rtt p50 {:>7.2} ms (min {:>7.2})  down {:>7.1} Mbit/s  up {:>7.1} Mbit/s  256KiB warm {:>7.1} ms  after-idle {:>7.1} ms",
                        c.profile, c.rtt_ms_p50, c.rtt_ms_min, c.down_mbit, c.up_mbit,
                        c.fetch_256k_warm_ms, c.fetch_256k_idle_ms
                    );
                }
                out.push(c);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            Ok(0)
        }
        "floor" => {
            let mut a = Args::new(rest);
            let profiles = match a.opt("profile")? {
                Some(p) => Profile::parse_list(&p)?,
                None => Profile::parse_list("rtt40-bw50,rtt100-bw20")?,
            };
            let secs: f64 = a
                .opt("secs")?
                .map(|v| v.parse())
                .transpose()?
                .unwrap_or(10.0);
            let json = a.flag("json");
            a.finish()?;
            let exe = bench_exe()?;
            let mut out = Vec::new();
            for p in &profiles {
                let dir = tempfile::Builder::new().prefix("hnl-").tempdir_in("/tmp")?;
                let lab = netlab::Netlab::start(p, dir.path(), &exe)?;
                let f = netlab::load_floor(
                    &lab,
                    Duration::from_secs_f64(secs),
                    Duration::from_millis(100),
                )?;
                if !json {
                    println!(
                        "{:<22} idle rtt {:>7.2} ms | under bulk down+up: p50 {:>7.1} p99 {:>7.1} max {:>7.1} ms (n={}) | bulk down {:>5.1} up {:>5.1} Mbit/s",
                        f.profile, f.idle_rtt_ms_p50, f.p50_ms, f.p99_ms, f.max_ms, f.samples,
                        f.down_mbit, f.up_mbit
                    );
                }
                out.push(f);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            Ok(0)
        }
        "up" => {
            let mut a = Args::new(rest);
            let profile = Profile::parse(
                &a.opt("profile")?
                    .ok_or_else(|| anyhow!("--profile required"))?,
            )?;
            let dir = match a.opt("dir")? {
                Some(d) => PathBuf::from(d),
                None => std::env::temp_dir().join(format!("hnl-{}", std::process::id())),
            };
            let serves = a.opts("serve")?;
            a.finish()?;
            let exe = bench_exe()?;
            let lab = netlab::Netlab::start(&profile, &dir, &exe)?;
            let mut hosts = Vec::new();
            for s in &serves {
                let (name, cmd) = s
                    .split_once('=')
                    .ok_or_else(|| anyhow!("--serve NAME=CMD, got {s:?}"))?;
                let argv: Vec<String> = cmd.split_whitespace().map(String::from).collect();
                hosts.push(netlab::ServiceHost::start(
                    &dir,
                    name,
                    netlab::Service::command(argv),
                )?);
                println!("service {name}: {}", lab.connect_argv(name).join(" "));
            }
            println!(
                "netlab {} up: holder pid {} (nsenter --user --net -t {} --preserve-credentials …), client socket {}",
                profile.name,
                lab.holder_pid(),
                lab.holder_pid(),
                lab.client_sock().display()
            );
            println!("press Ctrl-C to stop");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        other => bail!("netlab: unknown subcommand {other:?}"),
    }
}

fn run_cmd(args: &[String]) -> Result<i32> {
    let mut a = Args::new(args);
    let quick = a.flag("quick");
    let full = a.flag("full");
    let profiles = match a.opt("profile")? {
        Some(p) => Profile::parse_list(&p)?,
        None if full => Profile::standard(),
        None => vec![Profile::new(40, Some(50))],
    };
    let only = a.opt("only")?.map(|s| {
        s.split(',')
            .map(|x| x.trim().to_uppercase())
            .filter(|x| !x.is_empty())
            .collect()
    });
    let systems = match a.opt("systems")? {
        Some(s) => s
            .split(',')
            .map(|x| System::parse(x.trim()).ok_or_else(|| anyhow!("bad system {x:?}")))
            .collect::<Result<Vec<_>>>()?,
        None => vec![System::Unlatch, System::Sshfs, System::Local, System::Raw],
    };
    let out = PathBuf::from(
        a.opt("out")?
            .unwrap_or_else(|| "target/bench/latest.json".into()),
    );
    let scorecard = match a.opt("scorecard")? {
        Some(s) => PathBuf::from(s),
        None => out
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("SCORECARD.md"),
    };
    let work = PathBuf::from(
        a.opt("work")?
            .or_else(|| std::env::var("UNLATCH_BENCH_WORK").ok())
            .unwrap_or_else(|| "target/unlatch-bench-work".into()),
    );
    let mac_dir = a
        .opt("mac-dir")?
        .or_else(|| std::env::var("UNLATCH_BENCH_MAC_DIR").ok())
        .map(PathBuf::from)
        .map(|p| std::path::absolute(&p))
        .transpose()?;
    let bench_exe = bench_exe()?;
    let unlatchd = a.opt("unlatchd")?;
    let unlatch = a.opt("unlatch")?;
    let sshfs = a.opt("sshfs")?;
    let timeout = a
        .opt("timeout")?
        .map(|t| t.parse::<u64>())
        .transpose()?
        .unwrap_or(if quick { 180 } else { 900 });
    let tiny = a.flag("tiny-tree");
    a.finish()?;
    let bins = crate::scenarios::Bins {
        unlatchd: runner::find_bin(unlatchd.as_deref(), "UNLATCHD_BIN", "unlatchd", &bench_exe),
        unlatch: runner::find_bin(unlatch.as_deref(), "UNLATCH_BIN", "unlatch", &bench_exe),
        sshfs: runner::find_bin(sshfs.as_deref(), "SSHFS_BIN", "sshfs", &bench_exe),
        sftp_server: runner::find_sftp_server(),
    };
    let opts = RunOpts {
        profiles,
        quick,
        only,
        systems,
        out: out.clone(),
        scorecard: scorecard.clone(),
        work: std::path::absolute(&work)?,
        mac_dir,
        bins,
        timeout: Duration::from_secs(timeout),
        bench_exe,
        tree: if tiny {
            crate::tree::TreeSpec::tiny()
        } else {
            crate::tree::TreeSpec::full()
        },
    };
    let rep = runner::run(&opts)?;
    let failed = rep.failed();
    println!(
        "wrote {} and {} — {} unlatch rows judged, {} failed",
        out.display(),
        scorecard.display(),
        rep.measurements.iter().filter(|m| m.pass.is_some()).count(),
        failed.len()
    );
    for f in &failed {
        println!(
            "  FAIL {} {} {}: {:?} (target {})",
            f.profile,
            f.id,
            f.metric,
            f.value,
            f.target.as_deref().unwrap_or("")
        );
    }
    Ok(if failed.is_empty() { 0 } else { 1 })
}

fn compare_cmd(args: &[String]) -> Result<i32> {
    let mut a = Args::new(args);
    let threshold = a
        .opt("threshold")?
        .map(|t| t.parse::<f64>())
        .transpose()?
        .unwrap_or(0.10);
    let pos = a.finish()?;
    let [base, new] = pos.as_slice() else {
        bail!("compare BASE.json NEW.json [--threshold 0.10]");
    };
    let base = RunReport::load(std::path::Path::new(base))?;
    let new = RunReport::load(std::path::Path::new(new))?;
    let regs = report::compare(&base, &new, threshold);
    if regs.is_empty() {
        println!("no regressions (threshold {:.0}%)", threshold * 100.0);
        return Ok(0);
    }
    for r in &regs {
        println!(
            "REGRESSION {}: {} → {} ({})",
            r.key, r.base, r.new, r.reason
        );
    }
    Ok(1)
}
