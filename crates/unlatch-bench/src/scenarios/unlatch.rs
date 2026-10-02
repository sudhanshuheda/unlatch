//! Unlatch scenarios through the engine API (`unlatch_core::Engine`) and the FUSE frontend
//! (`unlatch mount`). `unlatchd stdio` runs on the far side of the shaped link, spawned per
//! connection by a netlab service; the engine reaches it with `Transport::Command` running the
//! netlab bridge.

use super::sshfs::{burst, is_fuse_mount, t7_row, T7Plan};
use super::{ls_la, mbit, poll_until, t8_file, Ctx, IDLE};
use crate::measure::{Better, Measurement, Samples, Status, System};
use crate::netlab::ServiceHost;
use crate::targets::metric as m;
use crate::wire_client::vm_rss;
use anyhow::{anyhow, bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unlatch_core::{
    CancelToken, CreateKind, CreateRequest, Engine, EngineConfig, EngineEvent, EventHandler,
    Transport,
};
use unlatch_proto::ipc::{IpcItem, LocalMeta};
use unlatch_proto::ItemId;

const LIVE_TIMEOUT: Duration = Duration::from_secs(180);

/// `(when, ids ∪ parents)` of every `ReplicaChanged` event.
type EventLog = Arc<Mutex<Vec<(Instant, Vec<ItemId>)>>>;

pub struct UnlatchOpts {
    /// Fresh daemon index (cold `unlatchd`) instead of the per-profile persisted one.
    pub fresh_daemon: bool,
    /// Fresh engine replica instead of the per-profile persisted one.
    pub fresh_engine: bool,
    /// Fresh (empty) content cache.
    pub fresh_cache: bool,
    pub prefetch: bool,
}

impl Default for UnlatchOpts {
    fn default() -> Self {
        UnlatchOpts {
            fresh_daemon: false,
            fresh_engine: false,
            fresh_cache: true,
            prefetch: true,
        }
    }
}

/// A live engine connected through the netlab to an `unlatchd stdio` service.
pub struct UnlatchEnv {
    pub engine: Engine,
    pub svc: ServiceHost,
    pub svc_name: String,
    /// `unlatchd --state` of this environment (finds the serving `unlatchd`).
    pub daemon_state: PathBuf,
    pub events: EventLog,
    /// Wall time from `Engine::start` to `wait_live` returning.
    pub live_after: Duration,
    _tmp: Vec<super::ScratchDir>,
}

impl Drop for UnlatchEnv {
    fn drop(&mut self) {
        self.engine.shutdown();
    }
}

impl UnlatchEnv {
    pub fn start(ctx: &Ctx, tag: &str, opts: UnlatchOpts) -> Result<UnlatchEnv> {
        let unlatchd = ctx.bins.unlatchd.as_ref().ok_or_else(|| {
            anyhow!("unlatchd binary not found (cargo build --release -p unlatchd)")
        })?;
        let mut tmp = Vec::new();
        let daemon_state = if opts.fresh_daemon {
            let d = ctx.local_scratch(&format!("unlatchd-{tag}"))?;
            let p = d.path.clone();
            tmp.push(d);
            p
        } else {
            ctx.work.join("unlatchd-state")
        };
        let engine_state = if opts.fresh_engine {
            let d = ctx.mac_scratch(&format!("engine-{tag}"))?;
            let p = d.path.clone();
            tmp.push(d);
            p
        } else {
            ctx.mac_root().join("engine-state")
        };
        std::fs::create_dir_all(&daemon_state)?;
        std::fs::create_dir_all(&engine_state)?;
        let rem = super::remote(
            ctx,
            &format!("unlatchd-{tag}"),
            vec![
                unlatchd.display().to_string(),
                "stdio".into(),
                "--root".into(),
                ctx.tree.root.display().to_string(),
                "--state".into(),
                daemon_state.display().to_string(),
            ],
            vec![("UNLATCHD_LOG".into(), ctx.log.display().to_string())],
        )?;
        let argv = rem.argv.clone();
        let mut cfg = EngineConfig::new(
            "bench",
            Transport::Command {
                argv,
                env: Vec::new(),
            },
            &ctx.tree.root.display().to_string(),
            engine_state,
            "bench",
        );
        if opts.fresh_cache {
            let d = ctx.mac_scratch(&format!("cache-{tag}"))?;
            cfg.cache_dir = d.path.clone();
            tmp.push(d);
        }
        let t = ctx.mac_scratch(&format!("tmp-{tag}"))?;
        cfg.temp_dir = t.path.clone();
        tmp.push(t);
        if !opts.prefetch {
            cfg.prefetch.max_file = 0;
        }
        let events: EventLog = Arc::default();
        let handler: EventHandler = {
            let events = events.clone();
            Arc::new(move |e| {
                if let EngineEvent::ReplicaChanged { mut ids, parents } = e {
                    ids.extend(parents);
                    if let Ok(mut v) = events.lock() {
                        v.push((Instant::now(), ids));
                    }
                }
            })
        };
        let t0 = Instant::now();
        super::note(format_args!("engine start ({tag})"));
        let engine =
            Engine::start(cfg, Some(handler)).map_err(|e| anyhow!("Engine::start: {e}"))?;
        super::note("engine started, waiting for live");
        if let Err(e) = engine.wait_live(LIVE_TIMEOUT) {
            engine.shutdown();
            bail!("engine not live within {LIVE_TIMEOUT:?}: {e}");
        }
        let live_after = t0.elapsed();
        super::note(format_args!("engine live after {live_after:?}"));
        Ok(UnlatchEnv {
            engine,
            svc: rem.host,
            svc_name: rem.service,
            daemon_state,
            events,
            live_after,
            _tmp: tmp,
        })
    }

    pub fn resolve(&self, rel: &str) -> Result<ItemId> {
        resolve(&self.engine, rel)
    }

    /// PID of the `unlatchd stdio` serving this environment (spawned by the netlab service
    /// directly, or by sshd in `-ssh` profiles).
    pub fn unlatchd_pid(&self) -> Option<u32> {
        let state = self.daemon_state.display().to_string();
        super::pids_with_cmdline(&["stdio", &state])
            .into_iter()
            .max()
    }

    /// Bytes moved on the shaped link for this environment's service.
    pub fn link_bytes(&self, ctx: &Ctx) -> Result<(u64, u64)> {
        let st = crate::netlab::stats_via(&ctx.client_sock())?;
        Ok(st
            .get(&self.svc_name)
            .map(|s| (s.up, s.down))
            .unwrap_or((0, 0)))
    }
}

pub fn resolve(engine: &Engine, rel: &str) -> Result<ItemId> {
    let mut id = ItemId::ROOT;
    for c in rel.split('/').filter(|c| !c.is_empty()) {
        id = engine
            .lookup(id, c)
            .map_err(|e| anyhow!("lookup {rel} at {c:?}: {e}"))?
            .entry
            .id;
    }
    Ok(id)
}

pub fn list_all(engine: &Engine, dir: ItemId, viewer: bool) -> Result<Vec<IpcItem>> {
    let mut out = Vec::new();
    let mut cursor: Option<Vec<u8>> = None;
    loop {
        let page = engine
            .list(dir, cursor.as_deref(), 5000, viewer)
            .map_err(|e| anyhow!("list: {e}"))?;
        out.extend(page.items);
        match page.next {
            Some(c) => cursor = Some(c),
            None => return Ok(out),
        }
    }
}

fn wait_resolve(engine: &Engine, rel: &str, timeout: Duration) -> Result<ItemId> {
    let mut last = None;
    let found = poll_until(timeout, Duration::from_micros(200), || {
        match resolve(engine, rel) {
            Ok(id) => {
                last = Some(id);
                true
            }
            Err(_) => false,
        }
    });
    match (found, last) {
        (Some(_), Some(id)) => Ok(id),
        _ => bail!("{rel} not visible in the engine within {timeout:?}"),
    }
}

pub fn run(ctx: &Ctx, id: &str) -> Result<Vec<Measurement>> {
    let row = |metric: &str, unit: &str, better| ctx.row(id, metric, System::Unlatch, unit, better);
    if !ctx.bins.unlatchd.as_ref().is_some_and(|p| p.is_file()) {
        return Ok(vec![row("*", "", Better::Lower).status(
            Status::Unavailable,
            "unlatchd binary not found (cargo build --release -p unlatchd)",
        )]);
    }
    Ok(match id {
        "T1" | "T2" => t1_t2(ctx, id)?,
        "T3" => t3(ctx, &row)?,
        "T4" => t4(ctx, &row)?,
        "T5" => t5(ctx, &row)?,
        "T6" => t6(ctx, &row)?,
        "T7" => t7(ctx, &row)?,
        "T8" => t8(ctx, &row)?,
        "T9" => t9(ctx, &row)?,
        "T10" => t10(ctx, &row)?,
        "T11" => t11_t12(ctx)?,
        "T13" => t13(ctx, &row)?,
        "T14" => t14(ctx, &row)?,
        other => vec![row("*", "", Better::Lower).status(
            Status::Skipped,
            format!("{other}: not an Unlatch network scenario"),
        )],
    })
}

type RowFn<'a> = super::sshfs::RowFn<'a>;

fn t1_t2(ctx: &Ctx, id: &str) -> Result<Vec<Measurement>> {
    let env = UnlatchEnv::start(ctx, "t1", UnlatchOpts::default())?;
    let flat = env.resolve("flat")?;
    let items = list_all(&env.engine, flat, false)?;
    if items.len() as u64 != ctx.tree.spec.flat_entries {
        bail!(
            "engine lists {} entries in flat/, expected {}",
            items.len(),
            ctx.tree.spec.flat_entries
        );
    }
    if id == "T1" {
        let mut s = Samples::new();
        for _ in 0..(if ctx.quick { 100 } else { 500 }) {
            let t = Instant::now();
            let v = list_all(&env.engine, flat, false)?;
            s.push(t.elapsed());
            std::hint::black_box(v);
        }
        Ok(vec![ctx
            .row(
                "T1",
                m::LIST_1000_P50_MS,
                System::Unlatch,
                "ms",
                Better::Lower,
            )
            .value(s.p50().unwrap_or(0.0), s.len())
            .detail(format!(
                "p99 {:.3} ms; Engine::list of 1000 entries, all pages",
                s.p99().unwrap_or(0.0)
            ))])
    } else {
        let ids: Vec<ItemId> = items.iter().map(|i| i.entry.id).collect();
        let mut s = Samples::new();
        // Bounded by time as well: a slow `item` must yield a number, not a scenario timeout.
        let deadline = Instant::now() + Duration::from_secs(10);
        for i in 0..20_000 {
            if Instant::now() > deadline {
                super::note(format_args!("T2: stopped after {i} calls (10 s budget)"));
                break;
            }
            let t = Instant::now();
            let it = env
                .engine
                .item(ids[i % ids.len()])
                .map_err(|e| anyhow!("item: {e}"))?;
            s.push(t.elapsed());
            std::hint::black_box(it);
        }
        Ok(vec![ctx
            .row("T2", m::STAT_P50_US, System::Unlatch, "us", Better::Lower)
            .value(s.p50().unwrap_or(0.0) * 1e3, s.len())
            .detail(format!(
                "p99 {:.1} µs; Engine::item",
                s.p99().unwrap_or(0.0) * 1e3
            ))])
    }
}

// ---- FUSE -------------------------------------------------------------------------------------

/// `unlatch mount` argv: matches `unlatch mount --help` (`<MOUNTPOINT> --root --state --command
/// <ARGV>...`). Override with `UNLATCH_BENCH_MOUNT_ARGV` (a JSON array). Placeholders: `{unlatch}`,
/// `{mnt}`, `{root}`, `{state}`, `{cmd}` (client argv joined by spaces), and `{cmd...}` (client
/// argv spliced — the raw bridge, or `ssh -F <cfg> bench '<unlatchd stdio …>'` in `-ssh` profiles).
pub const DEFAULT_MOUNT_ARGV: &[&str] = &[
    "{unlatch}",
    "mount",
    "{mnt}",
    "--root",
    "{root}",
    "--state",
    "{state}",
    "--command",
    "{cmd...}",
];

pub struct UnlatchMount {
    pub mnt: PathBuf,
    child: Child,
    _svc: ServiceHost,
}

impl UnlatchMount {
    pub fn mount(ctx: &Ctx, tag: &str) -> Result<UnlatchMount> {
        let unlatch = ctx
            .bins
            .unlatch
            .as_ref()
            .ok_or_else(|| anyhow!("unlatch CLI binary not found"))?;
        let unlatchd = ctx
            .bins
            .unlatchd
            .as_ref()
            .ok_or_else(|| anyhow!("unlatchd binary not found"))?;
        let daemon_state = ctx.work.join("unlatchd-state");
        std::fs::create_dir_all(&daemon_state)?;
        let rem = super::remote(
            ctx,
            &format!("unlatchd-fuse-{tag}"),
            vec![
                unlatchd.display().to_string(),
                "stdio".into(),
                "--root".into(),
                ctx.tree.root.display().to_string(),
                "--state".into(),
                daemon_state.display().to_string(),
            ],
            Vec::new(),
        )?;
        let mnt = ctx
            .work
            .join(format!("mnt-unlatch-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&mnt)?;
        let state = ctx.mac_root().join(format!("fuse-state-{tag}"));
        std::fs::create_dir_all(&state)?;
        let bridge = rem.argv.clone();
        let template: Vec<String> = match std::env::var("UNLATCH_BENCH_MOUNT_ARGV") {
            Ok(js) => serde_json::from_str(&js)
                .context("UNLATCH_BENCH_MOUNT_ARGV must be a JSON array")?,
            Err(_) => DEFAULT_MOUNT_ARGV.iter().map(|s| s.to_string()).collect(),
        };
        let argv = expand_mount_argv(&template, unlatch, &mnt, &ctx.tree.root, &state, &bridge);
        let (prog, args) = argv
            .split_first()
            .ok_or_else(|| anyhow!("empty mount argv"))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ctx.log)?;
        let mut child = Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .with_context(|| format!("spawn {}", argv.join(" ")))?;
        let up = poll_until(LIVE_TIMEOUT, Duration::from_millis(20), || {
            is_fuse_mount(&mnt) || matches!(child.try_wait(), Ok(Some(st)) if !st.success())
        });
        if up.is_none() || !is_fuse_mount(&mnt) {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{}` did not mount {} (see {})",
                argv.join(" "),
                mnt.display(),
                ctx.log.display()
            );
        }
        Ok(UnlatchMount {
            mnt,
            child,
            _svc: rem.host,
        })
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.mnt.join(rel)
    }
}

impl Drop for UnlatchMount {
    fn drop(&mut self) {
        super::sshfs::unmount(&self.mnt);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir(&self.mnt);
    }
}

pub fn expand_mount_argv(
    template: &[String],
    unlatch: &Path,
    mnt: &Path,
    root: &Path,
    state: &Path,
    bridge: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    for t in template {
        if t == "{cmd...}" {
            out.extend(bridge.iter().cloned());
            continue;
        }
        out.push(
            t.replace("{unlatch}", &unlatch.display().to_string())
                .replace("{mnt}", &mnt.display().to_string())
                .replace("{root}", &root.display().to_string())
                .replace("{state}", &state.display().to_string())
                .replace("{cmd}", &bridge.join(" ")),
        );
    }
    out
}

fn t3(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let mnt = match UnlatchMount::mount(ctx, "t3") {
        Ok(mnt) => mnt,
        Err(e) => {
            return Ok(vec![
                row(m::LS_LA_MS, "ms", Better::Lower).status(Status::Unavailable, format!("{e:#}")),
                row(m::LS_LA_COLD_MS, "ms", Better::Lower)
                    .status(Status::Unavailable, format!("{e:#}")),
            ])
        }
    };
    let flat = mnt.path("flat");
    let cold = ls_la(&flat)?;
    let mut warm = Samples::new();
    for _ in 0..(if ctx.quick { 10 } else { 30 }) {
        warm.push(ls_la(&flat)?);
    }
    Ok(vec![
        row(m::LS_LA_COLD_MS, "ms", Better::Lower)
            .value(cold.as_secs_f64() * 1e3, 1)
            .detail("first `ls -la` after mount (FUSE, replica already synced)"),
        row(m::LS_LA_MS, "ms", Better::Lower)
            .value(warm.p50().unwrap_or(0.0), warm.len())
            .detail("FUSE `ls -la`"),
    ])
}

// ---- change propagation ------------------------------------------------------------------------

fn t4(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let dir = ctx.vm_scratch("t4-unlatch")?;
    let rel = dir.rel(&ctx.tree.root);
    let env = UnlatchEnv::start(ctx, "t4", UnlatchOpts::default())?;
    let dir_id = wait_resolve(&env.engine, &rel, Duration::from_secs(10))?;
    list_all(&env.engine, dir_id, true)?;
    let mut poll = Samples::new();
    let mut event = Samples::new();
    for i in 0..(if ctx.quick { 10 } else { 30 }) {
        let name = format!("n{i}.txt");
        let t0 = Instant::now();
        std::fs::write(dir.path.join(&name), b"x")?;
        let found = poll_until(Duration::from_secs(10), Duration::from_micros(100), || {
            env.engine.lookup(dir_id, &name).is_ok()
        });
        let Some(_) = found else {
            bail!("{name} not visible within 10 s")
        };
        poll.push(t0.elapsed());
        let id = env
            .engine
            .lookup(dir_id, &name)
            .map_err(|e| anyhow!("{e}"))?
            .entry
            .id;
        // The event may land just after `lookup` already sees the change: wait for it. Match
        // the new id only — an event naming the parent can be a late one for the previous file.
        let mut at: Option<Instant> = None;
        poll_until(Duration::from_secs(2), Duration::from_micros(100), || {
            at = env.events.lock().ok().and_then(|ev| {
                ev.iter()
                    .find(|(at, ids)| *at >= t0 && ids.contains(&id))
                    .map(|(at, _)| *at)
            });
            at.is_some()
        });
        if let Some(at) = at {
            event.push(at - t0);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut rows = vec![row(m::VISIBLE_P50_MS, "ms", Better::Lower)
        .value(poll.p50().unwrap_or(0.0), poll.len())
        .detail("VM write → Engine::lookup succeeds (100 µs polling)")];
    rows.push(if event.is_empty() {
        row(m::VISIBLE_EVENT_P50_MS, "ms", Better::Lower).status(
            Status::Error,
            "no ReplicaChanged event named the new id within 2 s",
        )
    } else {
        row(m::VISIBLE_EVENT_P50_MS, "ms", Better::Lower)
            .value(event.p50().unwrap_or(0.0), event.len())
            .detail("VM write → first EngineEvent::ReplicaChanged naming the new id")
    });
    drop(env);
    // FUSE: what a file browser polling the directory sees.
    match UnlatchMount::mount(ctx, "t4") {
        Ok(mnt) => {
            let mdir = mnt.path(&rel);
            std::fs::read_dir(&mdir)?.count();
            let mut rd = Samples::new();
            for i in 0..(if ctx.quick { 5 } else { 20 }) {
                let name = format!("f{i}.txt");
                let t0 = Instant::now();
                std::fs::write(dir.path.join(&name), b"x")?;
                poll_until(Duration::from_secs(10), Duration::from_millis(1), || {
                    std::fs::read_dir(&mdir)
                        .map(|it| it.flatten().any(|e| e.file_name() == name.as_str()))
                        .unwrap_or(false)
                })
                .ok_or_else(|| anyhow!("{name} not listed via FUSE within 10 s"))?;
                rd.push(t0.elapsed());
            }
            rows.push(
                row(m::VISIBLE_READDIR_P50_MS, "ms", Better::Lower)
                    .value(rd.p50().unwrap_or(0.0), rd.len())
                    .detail("poll readdir() on the FUSE mount until the new name appears"),
            );
        }
        Err(e) => rows.push(
            row(m::VISIBLE_READDIR_P50_MS, "ms", Better::Lower)
                .status(Status::Unavailable, format!("{e:#}")),
        ),
    }
    Ok(rows)
}

fn t5(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let dir = ctx.vm_scratch("t5-unlatch")?;
    let rel = dir.rel(&ctx.tree.root);
    let env = UnlatchEnv::start(ctx, "t5", UnlatchOpts::default())?;
    let dir_id = wait_resolve(&env.engine, &rel, Duration::from_secs(10))?;
    list_all(&env.engine, dir_id, true)?;
    let end = burst(&dir.path, 1000, Duration::from_secs(1))?;
    let done = poll_until(Duration::from_secs(30), Duration::from_millis(1), || {
        list_all(&env.engine, dir_id, false)
            .map(|v| v.len() == 1000)
            .unwrap_or(false)
    });
    Ok(vec![match done {
        Some(_) => row(m::BURST_ALL_VISIBLE_MS, "ms", Better::Lower)
            .value(end.elapsed().as_secs_f64() * 1e3, 1)
            .detail("1000 files written over 1 s on the VM; time after the burst until Engine::list shows all"),
        None => row(m::BURST_ALL_VISIBLE_MS, "ms", Better::Lower).status(Status::Timeout, "not all visible within 30 s"),
    }])
}

// ---- content ----------------------------------------------------------------------------------

fn fetch_once(env: &UnlatchEnv, id: ItemId, dest: &Path) -> Result<(Duration, u64)> {
    let t = Instant::now();
    let f = env
        .engine
        .fetch(id, None, dest, &|_, _| {}, &CancelToken::new())
        .map_err(|e| anyhow!("fetch: {e}"))?;
    let d = t.elapsed();
    let n = std::fs::metadata(&f.path)?.len();
    let _ = std::fs::remove_file(&f.path);
    Ok((d, n))
}

fn t6(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let env = UnlatchEnv::start(ctx, "t6", UnlatchOpts::default())?;
    let dest = ctx.mac_scratch("t6-dest")?;
    let flat = env.resolve("flat")?;
    let items = list_all(&env.engine, flat, true)?; // viewer enumeration → prefetch
                                                    // Give prefetch time to pull the small files (bounded by the link rate).
    let small_bytes: u64 = items
        .iter()
        .filter(|i| i.entry.size <= 256 << 10)
        .map(|i| i.entry.size)
        .sum();
    let wait = ctx
        .profile
        .rate_bytes_per_sec()
        .map(|bps| Duration::from_secs_f64(small_bytes as f64 / bps as f64 * 1.5))
        .unwrap_or_default()
        .clamp(Duration::from_secs(1), Duration::from_secs(15));
    std::thread::sleep(wait);
    let mut s = Samples::new();
    for it in items
        .iter()
        .filter(|i| i.entry.size <= 12 << 10)
        .take(if ctx.quick { 50 } else { 200 })
    {
        s.push(fetch_once(&env, it.entry.id, &dest.path)?.0);
    }
    Ok(vec![row(m::OPEN_SMALL_WARM_P50_MS, "ms", Better::Lower)
        .value(s.p50().unwrap_or(0.0), s.len())
        .detail(format!(
            "Engine::fetch after a viewer enumeration + {:.1} s for prefetch",
            wait.as_secs_f64()
        ))])
}

fn t7(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let env = UnlatchEnv::start(
        ctx,
        "t7",
        UnlatchOpts {
            prefetch: false,
            ..UnlatchOpts::default()
        },
    )?;
    let dest = ctx.mac_scratch("t7-dest")?;
    let plan = T7Plan::new(ctx);
    let mut rows = Vec::new();
    for (metric, files, idle) in plan.cases() {
        let mut s = Samples::new();
        for f in files {
            let id = env.resolve(f)?;
            if idle {
                std::thread::sleep(IDLE);
            } else {
                super::note("T7: server_barrier");
                let _ = env.engine.server_barrier(Duration::from_secs(10));
            }
            super::note(format_args!("T7: fetch {f}"));
            s.push(fetch_once(&env, id, &dest.path)?.0);
        }
        rows.push(t7_row(row, metric, idle, &s));
    }
    Ok(rows)
}

fn t8(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let (p, size) = t8_file(ctx)?;
    let rel = p.strip_prefix(&ctx.tree.root)?.display().to_string();
    let env = UnlatchEnv::start(
        ctx,
        "t8",
        UnlatchOpts {
            prefetch: false,
            ..UnlatchOpts::default()
        },
    )?;
    let id = wait_resolve(&env.engine, &rel, Duration::from_secs(10))?;
    let dest = ctx.mac_scratch("t8-dest")?;
    let (d, n) = fetch_once(&env, id, &dest.path)?;
    if n != size {
        bail!("fetched {n} bytes, expected {size}");
    }
    Ok(vec![row(m::THROUGHPUT_MBIT, "Mbit/s", Better::Higher)
        .value(mbit(size, d), 1)
        .detail(format!(
            "Engine::fetch of {} MiB (single streamed Read under credit)",
            size >> 20
        ))])
}

fn t9(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let dir = ctx.vm_scratch("t9-unlatch")?;
    let rel = dir.rel(&ctx.tree.root);
    let env = UnlatchEnv::start(ctx, "t9", UnlatchOpts::default())?;
    let dir_id = wait_resolve(&env.engine, &rel, Duration::from_secs(10))?;
    let src = ctx.mac_scratch("t9-src")?;
    let mut s = Samples::new();
    for i in 0..(if ctx.quick { 10 } else { 30 }) {
        let lp = src.path.join(format!("u{i}.txt"));
        let mut f = std::fs::File::create(&lp)?;
        f.write_all(&vec![b'a' + (i % 26) as u8; 4096])?;
        drop(f);
        let content = std::fs::File::open(&lp)?;
        let name = format!("u{i}.txt");
        let t = Instant::now();
        env.engine
            .create(CreateRequest {
                template_id: format!("bench-t9-{}-{i}", std::process::id()),
                parent: dir_id,
                name: name.clone(),
                kind: CreateKind::File,
                content: Some(content),
                symlink_target: None,
                mtime_ns: None,
                user_exec: None,
                changed_fields: unlatch_proto::ipc::fields::CONTENTS
                    | unlatch_proto::ipc::fields::FILENAME,
                local: LocalMeta::default(),
                may_already_exist: false,
                deletion_conflicted: false,
            })
            .map_err(|e| anyhow!("create: {e}"))?;
        s.push(t.elapsed());
        let vm = dir.path.join(&name);
        if std::fs::metadata(&vm).map(|m| m.len()).unwrap_or(0) != 4096 {
            bail!("create returned but {} is not on the VM", vm.display());
        }
    }
    Ok(vec![row(m::UPLOAD_4K_P50_MS, "ms", Better::Lower)
        .value(s.p50().unwrap_or(0.0), s.len())
        .detail(
            "Engine::create of a 4 KiB file until it returns (durable on the VM)",
        )])
}

fn t10(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let dir = ctx.vm_scratch("t10-unlatch")?;
    let rel = dir.rel(&ctx.tree.root);
    let env = UnlatchEnv::start(ctx, "t10", UnlatchOpts::default())?;
    let dir_id = wait_resolve(&env.engine, &rel, Duration::from_secs(10))?;
    let n = 100;
    let t0 = Instant::now();
    env.engine.drop_connection();
    for i in 0..n {
        std::fs::write(dir.path.join(format!("o{i}.txt")), b"outage")?;
    }
    let done = poll_until(Duration::from_secs(30), Duration::from_millis(1), || {
        list_all(&env.engine, dir_id, false)
            .map(|v| v.len() == n)
            .unwrap_or(false)
    });
    Ok(vec![match done {
        Some(_) => row(m::RECONNECT_CATCHUP_MS, "ms", Better::Lower)
            .value(t0.elapsed().as_secs_f64() * 1e3, 1)
            .detail(format!(
                "drop_connection + {n} VM creates during the outage → all visible"
            )),
        None => row(m::RECONNECT_CATCHUP_MS, "ms", Better::Lower)
            .status(Status::Timeout, "not caught up within 30 s"),
    }])
}

fn t11_t12(ctx: &Ctx) -> Result<Vec<Measurement>> {
    let r11 =
        |metric: &str, unit: &str, better| ctx.row("T11", metric, System::Unlatch, unit, better);
    let mut rows = Vec::new();
    // (a) Cold daemon (no index) + fresh client.
    {
        let env = UnlatchEnv::start(
            ctx,
            "t11c",
            UnlatchOpts {
                fresh_daemon: true,
                fresh_engine: true,
                fresh_cache: true,
                prefetch: false,
            },
        )?;
        rows.push(
            r11(m::INITIAL_SYNC_COLD_DAEMON_S, "s", Better::Lower)
                .value(env.live_after.as_secs_f64(), 1)
                .detail("no daemon index: includes the VM-side scan"),
        );
    }
    // (b) Warm daemon (persisted index) + fresh client: the T11 number.
    let env = UnlatchEnv::start(
        ctx,
        "t11w",
        UnlatchOpts {
            fresh_daemon: false,
            fresh_engine: true,
            fresh_cache: true,
            prefetch: false,
        },
    )?;
    let lazy = ctx.tree.lazy_dirs + ctx.tree.lazy_files;
    rows.push(
        r11(m::FULL_TREE_KNOWN_S, "s", Better::Lower)
            .value(env.live_after.as_secs_f64(), 1)
            .detail(format!(
                "Engine::start → wait_live, fresh replica, {} entries (+{lazy} lazy)",
                ctx.tree.eager_entries
            )),
    );
    let (_, down) = env.link_bytes(ctx)?;
    rows.push(
        r11(m::INITIAL_SYNC_BYTES, "bytes", Better::Lower)
            .value(down as f64, 1)
            .detail("server→client bytes on the link"),
    );
    // T12: RSS of the unlatchd serving this session.
    let r12 = ctx.row(
        "T12",
        m::RSS_BYTES_PER_ENTRY,
        System::Unlatch,
        "B/entry",
        Better::Lower,
    );
    let entries = env.engine.status().server.map(|s| s.entries).unwrap_or(0);
    rows.push(match env.unlatchd_pid() {
        Some(pid) if entries > 0 => {
            let rss = vm_rss(pid)?;
            r12.value(rss as f64 / entries as f64, 1).detail(format!(
                "VmRSS {:.1} MiB / {entries} entries",
                rss as f64 / (1 << 20) as f64
            ))
        }
        _ => r12.status(
            Status::Error,
            "no unlatchd pid or no entry count in ServerInfo",
        ),
    });
    Ok(rows)
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
    let window = Duration::from_secs(if ctx.quick { 5 } else { 20 });
    let dir = ctx.vm_scratch("t13-unlatch")?;
    let rel = dir.rel(&ctx.tree.root);
    // The bulk load must be incompressible: Unlatch LZ4-compresses frames, so a sparse (all-zero)
    // file would cross the link at ~1/250 of its size and load nothing. Download: the 256 MiB
    // random `big.bin` (it never finishes within the window at ≤ 50 Mbit/s). Upload: a random
    // file sized to ~2× the window at line rate.
    let up_bytes = ctx.bulk_bytes(window.as_secs_f64() * 2.0).min(500 << 20);
    let src = ctx.mac_scratch("t13-src")?;
    let up_path = src.path.join("upload.bin");
    super::copy_prefix(&ctx.tree.path(&ctx.tree.big), &up_path, up_bytes)?;
    let env = Arc::new(UnlatchEnv::start(
        ctx,
        "t13",
        UnlatchOpts {
            prefetch: false,
            ..UnlatchOpts::default()
        },
    )?);
    let dir_id = wait_resolve(&env.engine, &rel, Duration::from_secs(10))?;
    let big_id = wait_resolve(&env.engine, &ctx.tree.big, Duration::from_secs(10))?;
    let dest = ctx.mac_scratch("t13-dest")?;
    let cancel = CancelToken::new();
    let dl = {
        let env = env.clone();
        let cancel = cancel.clone();
        let dest = dest.path.clone();
        std::thread::spawn(move || {
            if std::env::var("UNLATCH_BENCH_T13_ONLY").as_deref() == Ok("up") {
                return;
            }
            let _ = env.engine.fetch(big_id, None, &dest, &|_, _| {}, &cancel);
        })
    };
    let ul = {
        let env = env.clone();
        std::thread::spawn(move || -> Result<()> {
            if std::env::var("UNLATCH_BENCH_T13_ONLY").as_deref() == Ok("down") {
                return Ok(());
            }
            let content = std::fs::File::open(&up_path)?;
            env.engine
                .create(CreateRequest {
                    template_id: format!("bench-t13-{}", std::process::id()),
                    parent: dir_id,
                    name: "upload.bin".into(),
                    kind: CreateKind::File,
                    content: Some(content),
                    symlink_target: None,
                    mtime_ns: None,
                    user_exec: None,
                    changed_fields: unlatch_proto::ipc::fields::CONTENTS,
                    local: LocalMeta::default(),
                    may_already_exist: false,
                    deletion_conflicted: false,
                })
                .map_err(|e| anyhow!("{e}"))?;
            Ok(())
        })
    };
    // Start sampling only once both transfers are moving bytes on the link (the upload is
    // staged and hashed locally first), plus a second of ramp-up.
    let (u0, d0) = env.link_bytes(ctx)?;
    let flowing = poll_until(Duration::from_secs(60), Duration::from_millis(50), || {
        env.link_bytes(ctx)
            .map(|(u, d)| {
                let only = std::env::var("UNLATCH_BENCH_T13_ONLY").unwrap_or_default();
                (only == "down" || u > u0 + (1 << 20)) && (only == "up" || d > d0 + (1 << 20))
            })
            .unwrap_or(false)
    });
    if flowing.is_none() {
        bail!("bulk download + upload did not start within 60 s");
    }
    std::thread::sleep(Duration::from_secs(1));
    let (u1, d1) = env.link_bytes(ctx)?;
    let tl = Instant::now();
    // Lazy package dirs: each `list` of a never-listed lazy dir is one priority ListDir.
    let mut probes: Vec<ItemId> = Vec::new();
    for p in &ctx.tree.lazy_probe_dirs {
        if let Ok(id) = resolve(&env.engine, p) {
            probes.push(id);
        }
    }
    let mut pong = Samples::new();
    let mut listdir = Samples::new();
    let t = Instant::now();
    let mut i = 0;
    while t.elapsed() < window {
        let t0 = Instant::now();
        env.engine
            .server_barrier(Duration::from_secs(30))
            .map_err(|e| anyhow!("barrier: {e}"))?;
        pong.push(t0.elapsed());
        let mut ld = None;
        if let Some(pid) = probes.get(i) {
            let t0 = Instant::now();
            list_all(&env.engine, *pid, false)?;
            ld = Some(t0.elapsed());
            listdir.push(t0.elapsed());
        }
        let (u, d) = env.link_bytes(ctx).unwrap_or((0, 0));
        super::note(format_args!(
            "T13 sample {i} t={:.2}s pong {:.1} ms listdir {} ms; link up {} down {}",
            t.elapsed().as_secs_f64(),
            pong.last_ms().unwrap_or(0.0),
            ld.map(|d| format!("{:.1}", d.as_secs_f64() * 1e3))
                .unwrap_or_else(|| "-".into()),
            u,
            d
        ));
        i += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
    let (u2, d2) = env.link_bytes(ctx)?;
    let el = tl.elapsed();
    cancel.cancel();
    let _ = dl.join();
    env.engine.shutdown(); // aborts the upload
    let _ = ul.join();
    let agg = pong.p99().unwrap_or(0.0).max(listdir.p99().unwrap_or(0.0));
    let mut rows = vec![
        row(m::P99_INTERACTIVE_UNDER_LOAD_MS, "ms", Better::Lower)
            .value(agg, pong.len() + listdir.len())
            .detail(format!(
                "max of Pong / ListDir p99 during a download of the 256 MiB random big.bin + a {} MiB random upload; link {:.1} down / {:.1} up Mbit/s during the window",
                up_bytes >> 20,
                mbit(d2 - d1, el),
                mbit(u2 - u1, el),
            )),
        row(m::P99_PONG_UNDER_LOAD_MS, "ms", Better::Lower)
            .value(pong.p99().unwrap_or(0.0), pong.len()),
    ];
    rows.push(if listdir.is_empty() {
        row(m::P99_LISTDIR_UNDER_LOAD_MS, "ms", Better::Lower)
            .status(Status::Error, "no lazy probe dirs resolved")
    } else {
        row(m::P99_LISTDIR_UNDER_LOAD_MS, "ms", Better::Lower)
            .value(listdir.p99().unwrap_or(0.0), listdir.len())
    });
    Ok(rows)
}

fn t14(ctx: &Ctx, row: &RowFn) -> Result<Vec<Measurement>> {
    let dir = ctx.vm_scratch("t14-unlatch")?;
    let rel = dir.rel(&ctx.tree.root);
    let log_path = dir.path.join("app.log");
    {
        let mut f = std::fs::File::create(&log_path)?;
        let line = [b'L'; 1023];
        for _ in 0..(10 << 10) {
            f.write_all(&line)?;
            f.write_all(b"\n")?;
        }
    }
    let env = UnlatchEnv::start(
        ctx,
        "t14",
        UnlatchOpts {
            prefetch: false,
            ..UnlatchOpts::default()
        },
    )?;
    let id = wait_resolve(
        &env.engine,
        &format!("{rel}/app.log"),
        Duration::from_secs(10),
    )?;
    let dest = ctx.mac_scratch("t14-dest")?;
    // Materialize it (as if the user opened it) and tell the engine it is materialized.
    let f = env
        .engine
        .fetch(id, None, &dest.path, &|_, _| {}, &CancelToken::new())
        .map_err(|e| anyhow!("{e}"))?;
    let _ = std::fs::remove_file(&f.path);
    env.engine
        .materialized_changed(&[id], &[], false)
        .map_err(|e| anyhow!("{e}"))?;
    env.engine
        .server_barrier(Duration::from_secs(10))
        .map_err(|e| anyhow!("{e}"))?;
    let (_, down0) = env.link_bytes(ctx)?;
    let window = Duration::from_secs(if ctx.quick { 3 } else { 20 });
    let mut appended = 0u64;
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&log_path)?;
        let line = [b'a'; 1024];
        let t = Instant::now();
        let mut k = 0u32;
        while t.elapsed() < window {
            f.write_all(&line)?;
            appended += 1024;
            k += 1;
            let due = t + Duration::from_millis(10) * k;
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
        }
    }
    std::thread::sleep(Duration::from_secs(1) + ctx.rtt() * 2);
    env.engine
        .server_barrier(Duration::from_secs(10))
        .map_err(|e| anyhow!("{e}"))?;
    let (_, down1) = env.link_bytes(ctx)?;
    let moved = down1.saturating_sub(down0);
    Ok(vec![row(m::BYTES_MOVED_RATIO, "ratio", Better::Lower)
        .value(moved as f64 / appended.max(1) as f64, 1)
        .detail(format!(
            "{moved} bytes down for {appended} bytes appended to a materialized 10 MiB log"
        ))])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_argv_expansion() {
        let t: Vec<String> = DEFAULT_MOUNT_ARGV.iter().map(|s| s.to_string()).collect();
        let v = expand_mount_argv(
            &t,
            Path::new("/b/unlatch"),
            Path::new("/m"),
            Path::new("/r"),
            Path::new("/s"),
            &["/b/unlatch-bench".into(), "netlab".into(), "connect".into()],
        );
        // Exactly the real CLI: `unlatch mount <MNT> --root <R> --state <S> --command <ARGV>...`
        // (`--command` takes the rest of the line, hyphen values included — ssh's `-F cfg`).
        assert_eq!(
            v,
            [
                "/b/unlatch",
                "mount",
                "/m",
                "--root",
                "/r",
                "--state",
                "/s",
                "--command",
                "/b/unlatch-bench",
                "netlab",
                "connect"
            ]
        );
        let t2 = vec!["{unlatch}".to_string(), "--cmd={cmd}".to_string()];
        let v2 = expand_mount_argv(
            &t2,
            Path::new("h"),
            Path::new("m"),
            Path::new("r"),
            Path::new("s"),
            &["a".into(), "b".into()],
        );
        assert_eq!(v2, vec!["h", "--cmd=a b"]);
    }
}
