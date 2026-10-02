//! fpsim against the real engine and `unlatchd` on this machine.
//!
//! Layout of one world (all under a fresh temp dir):
//! * `root/` — the VM root the agent (fuzzer) writes into;
//! * `outside/` — a sentinel tree next to the root; it must never change;
//! * `state/` — engine replica/cache; `hstate/` — `unlatchd stdio` index;
//! * `ctl/` — `offline` (link cut) and `wire-fault` (one-shot `UNLATCH_FAULT` for the next unlatchd);
//! * `unlatchd.sh` — the `Transport::Command` the engine spawns; it honours `ctl/`.
//!
//! The engine runs in-process by default. Engine-side faults (`die_before_ipc_reply:<kind>`)
//! need their own process, so arming one restarts the engine as `unlatch-bench fpsim engine-host`
//! with `UNLATCH_FAULT` set; the first IPC transport error afterwards restarts it clean (the
//! "extension/agent kill").

use super::backend::{event_channel, Backend, IpcBackend};
use super::check::{fingerprint, fingerprint_diff, materialize_and_check_realpaths, Fingerprint};
use super::host::parse_event_line;
use super::scenarios::{self, Verdict};
use super::scripted::ReplyFault;
use super::sim::{FpSim, SimConfig};
use super::vmfs::{RealFs, VmFs};
use super::world::World;
use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use unlatch_core::ipc::IpcServerHandle;
use unlatch_core::{Engine, EngineConfig, EventHandler, Transport};
use unlatch_proto::ipc::{ConnState, IpcRequest, IpcResponse};
use unlatch_proto::ProtoError;

/// Where the binaries are.
#[derive(Clone, Debug)]
pub struct E2eConfig {
    pub unlatchd: PathBuf,
    /// `unlatch-bench` binary for the out-of-process engine host (engine-side faults).
    pub engine_host: Option<PathBuf>,
    pub verbose: bool,
    /// Parent of the per-world temp dirs (short: unix socket paths are ≤ 108 bytes).
    pub tmp_base: PathBuf,
    /// How the engine reaches the daemon.
    pub daemon: DaemonMode,
}

/// `unlatchd connect` (default) attaches to a per-root background `unlatchd serve` that outlives
/// engine restarts and link cuts, as in production. `unlatchd stdio` runs one in-process session
/// per connection (every reconnect is a daemon restart). `$UNLATCH_E2E_DAEMON=stdio` selects it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonMode {
    Connect,
    Stdio,
}

impl E2eConfig {
    /// Resolve binaries from flags, then `$UNLATCHD_BIN` / `$UNLATCH_ENGINE_HOST`, then siblings of
    /// the running executable (cargo's `target/<profile>/`).
    pub fn discover(
        unlatchd: Option<PathBuf>,
        engine_host: Option<PathBuf>,
        verbose: bool,
    ) -> Result<E2eConfig> {
        let unlatchd = unlatchd
            .or_else(|| std::env::var_os("UNLATCHD_BIN").map(PathBuf::from))
            .or_else(|| sibling_binary("unlatchd"))
            .ok_or_else(|| {
                anyhow!("cannot find unlatchd: pass --unlatchd PATH or set UNLATCHD_BIN")
            })?;
        if !unlatchd.is_file() {
            bail!("unlatchd binary {} does not exist", unlatchd.display());
        }
        let engine_host = engine_host
            .or_else(|| std::env::var_os("UNLATCH_ENGINE_HOST").map(PathBuf::from))
            .or_else(|| crate::cli::bench_exe().ok())
            .filter(|p| p.is_file());
        let tmp_base = std::env::var_os("UNLATCH_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let daemon = match std::env::var("UNLATCH_E2E_DAEMON").as_deref() {
            Ok("stdio") => DaemonMode::Stdio,
            _ => DaemonMode::Connect,
        };
        Ok(E2eConfig {
            daemon,
            unlatchd,
            engine_host,
            verbose,
            tmp_base,
        })
    }
}

/// The `unlatchd` binary for `cargo test` (the e2e suites run by default, not `#[ignore]`):
///
/// 1. `$UNLATCHD_BIN`, which must exist (an explicit choice is never second-guessed);
/// 2. otherwise `cargo build -p unlatchd` into the running test's own target dir and profile
///    (cargo does not hold the build lock while tests run; a no-op build costs ~1 s), so the
///    daemon under test is always built from this checkout — never a stale leftover;
/// 3. if that build fails (no cargo, offline), an existing `target/{<profile>,release,debug}/
///    unlatchd`, with a loud staleness warning;
/// 4. otherwise an error that says how to provide one. Callers panic with it: a missing daemon
///    must fail the suite, never skip it silently.
///
/// `$UNLATCH_E2E_NO_BUILD=1` skips step 2.
pub fn unlatchd_for_tests() -> std::result::Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("UNLATCHD_BIN").map(PathBuf::from) {
        return if p.is_file() {
            Ok(p)
        } else {
            Err(format!(
                "UNLATCHD_BIN={} does not exist; build it with `cargo build -p unlatchd` or unset it",
                p.display()
            ))
        };
    }
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // Test binaries live in <target>/<profile>/deps/.
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("unexpected test binary location {}", exe.display()))?
        .to_path_buf();
    let target_dir = profile_dir
        .parent()
        .ok_or_else(|| format!("unexpected test binary location {}", exe.display()))?
        .to_path_buf();
    let profile = profile_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut build_err = String::from("skipped ($UNLATCH_E2E_NO_BUILD)");
    if std::env::var_os("UNLATCH_E2E_NO_BUILD").is_none() {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut cmd = Command::new(&cargo);
        cmd.current_dir(&workspace)
            .args(["build", "--quiet", "-p", "unlatchd", "--target-dir"])
            .arg(&target_dir);
        match profile.as_str() {
            "debug" => {}
            "release" => {
                cmd.arg("--release");
            }
            p => {
                cmd.args(["--profile", p]);
            }
        }
        match cmd.stdin(Stdio::null()).output() {
            Ok(o) if o.status.success() && profile_dir.join("unlatchd").is_file() => {
                return Ok(profile_dir.join("unlatchd"));
            }
            Ok(o) => {
                build_err = format!(
                    "`cargo build -p unlatchd` failed ({}): {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                )
            }
            Err(e) => build_err = format!("cannot run {}: {e}", Path::new(&cargo).display()),
        }
    }
    let mut candidates = vec![profile_dir.join("unlatchd")];
    for p in ["release", "debug"].map(|p| target_dir.join(p).join("unlatchd")) {
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    }
    if let Some(p) = candidates.iter().find(|p| p.is_file()) {
        eprintln!(
            "WARNING: could not build unlatchd ({build_err}); testing the existing, possibly STALE {}",
            p.display()
        );
        return Ok(p.clone());
    }
    Err(format!(
        "no unlatchd binary for the end-to-end tests: {build_err}. Build one with \
         `cargo build -p unlatchd` (or `cargo build --release -p unlatchd`) or point UNLATCHD_BIN at a \
         built unlatchd; looked in {}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn sibling_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join(name), dir.parent()?.join(name)]
        .into_iter()
        .find(|p| p.is_file())
}

/// `$FPSIM_SLOW_HOST_MS`: the host takes this long to handle each engine event (a busy
/// extension main thread, a loaded CI runner). The engine must not depend on how fast its events
/// reach fileproviderd: whatever a reply leaves to the working set (a conflict copy, MQ-013) has to
/// be signalled before the reply goes out. With this set, a reply that overtakes its signal fails
/// the scenario every time instead of once in a few hundred runs.
fn slow_host(inner: EventHandler) -> EventHandler {
    let ms = std::env::var("FPSIM_SLOW_HOST_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if ms == 0 {
        return inner;
    }
    Arc::new(move |ev| {
        std::thread::sleep(Duration::from_millis(ms));
        inner(ev)
    })
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn fault_token(f: ReplyFault) -> &'static str {
    match f {
        ReplyFault::Create => "die_before_ipc_reply:create",
        ReplyFault::Modify => "die_before_ipc_reply:modify",
        ReplyFault::Delete => "die_before_ipc_reply:delete",
        ReplyFault::Fetch => "die_before_ipc_reply:fetch",
    }
}

fn perr(e: ProtoError) -> anyhow::Error {
    anyhow!("{e}")
}

struct HostProc {
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<String>,
}

enum Side {
    None,
    InProc {
        engine: Engine,
        _ipc: IpcServerHandle,
    },
    Child(HostProc),
}

/// Owns the engine (in-process or child) and restarts it on demand.
pub struct EngineCtl {
    cfg: E2eConfig,
    socket: PathBuf,
    root: String,
    state: PathBuf,
    wrapper: PathBuf,
    handler: EventHandler,
    side: Side,
    /// A `die_before_ipc_reply` fault is armed in the current child.
    reply_fault_armed: bool,
    /// Bumped on every engine (re)start: IPC connections to an older engine are invalid.
    generation: u64,
}

const LIVE_TIMEOUT: Duration = Duration::from_secs(30);
const BARRIER_TIMEOUT: Duration = Duration::from_secs(15);

/// `$UNLATCH_E2E_LIST_TIMEOUT_MS`: how long the engine waits for a connection before answering
/// `Offline` (default: the engine's own, 20 s). The fuzzer shortens it: while it cuts the link
/// on purpose, every Mac op would otherwise wait the full 20 s, and an offline-heavy seed takes
/// 20+ minutes.
pub fn apply_list_timeout(cfg: &mut EngineConfig) {
    if let Some(ms) = std::env::var("UNLATCH_E2E_LIST_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        cfg.list_timeout = Duration::from_millis(ms);
    }
}

impl EngineCtl {
    fn engine_config(&self) -> EngineConfig {
        let argv = vec![
            "/bin/sh".to_string(),
            self.wrapper.to_string_lossy().into_owned(),
        ];
        let mut cfg = EngineConfig::new(
            "fpsim",
            Transport::Command {
                argv,
                env: Vec::new(),
            },
            &self.root,
            self.state.clone(),
            "fpsim",
        );
        apply_list_timeout(&mut cfg);
        cfg
    }

    fn start_inproc(&mut self) -> Result<()> {
        self.start_inproc_opts(true)
    }

    fn start_inproc_opts(&mut self, wait_live: bool) -> Result<()> {
        let engine =
            Engine::start(self.engine_config(), Some(self.handler.clone())).map_err(perr)?;
        let _ = std::fs::remove_file(&self.socket);
        let ipc = engine.serve_ipc(&self.socket).map_err(perr)?;
        if wait_live {
            engine
                .wait_live(LIVE_TIMEOUT)
                .map_err(perr)
                .context("engine never became live")?;
        }
        self.side = Side::InProc { engine, _ipc: ipc };
        self.generation += 1;
        Ok(())
    }

    fn start_child(&mut self, fault: Option<&str>) -> Result<()> {
        let exe = self
            .cfg
            .engine_host
            .clone()
            .ok_or_else(|| anyhow!("no engine-host binary"))?;
        let mut cmd = Command::new(exe);
        cmd.args(["fpsim", "engine-host", "--socket"])
            .arg(&self.socket)
            .args(["--root", &self.root, "--state"])
            .arg(&self.state)
            .args(["--argv", "/bin/sh", "--argv"])
            .arg(&self.wrapper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if self.cfg.verbose {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .env_remove("UNLATCH_FAULT");
        if let Some(f) = fault {
            cmd.env("UNLATCH_FAULT", f);
        }
        let mut child = cmd.spawn().context("spawning engine host")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let (tx, replies) = channel::<String>();
        let handler = self.handler.clone();
        std::thread::Builder::new()
            .name("engine-host-out".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    match parse_event_line(&line) {
                        Some(ev) => handler(ev),
                        None => {
                            if tx.send(line).is_err() {
                                return;
                            }
                        }
                    }
                }
            })?;
        match replies.recv_timeout(LIVE_TIMEOUT) {
            Ok(l) if l == "READY" => {}
            Ok(l) => bail!("engine host said {l:?}"),
            Err(_) => bail!("engine host did not start"),
        }
        self.generation += 1;
        self.side = Side::Child(HostProc {
            child,
            stdin,
            replies,
        });
        self.command("live", LIVE_TIMEOUT)?;
        Ok(())
    }

    fn stop(&mut self) {
        match std::mem::replace(&mut self.side, Side::None) {
            Side::None => {}
            // The IPC server handle goes out of scope after the engine stopped.
            Side::InProc { engine, _ipc } => engine.shutdown(),
            Side::Child(mut h) => {
                let _ = writeln!(h.stdin, "quit");
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = h.child.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = h.child.kill();
                let _ = h.child.wait();
            }
        }
    }

    fn command(&mut self, cmd: &str, t: Duration) -> Result<()> {
        match &mut self.side {
            Side::Child(h) => {
                writeln!(h.stdin, "{cmd} {}", t.as_millis())?;
                h.stdin.flush()?;
                match h.replies.recv_timeout(t + Duration::from_secs(5)) {
                    Ok(l) if l == "OK" => Ok(()),
                    Ok(l) => Err(anyhow!("engine host {cmd}: {l}")),
                    Err(RecvTimeoutError::Timeout) => Err(anyhow!("engine host {cmd}: no answer")),
                    Err(RecvTimeoutError::Disconnected) => Err(anyhow!("engine host exited")),
                }
            }
            Side::InProc { engine, .. } => {
                let r = match cmd {
                    "barrier" => engine.server_barrier(t),
                    "idle" => engine.wait_idle(t),
                    "live" => engine.wait_live(t),
                    "drop" => {
                        engine.drop_connection();
                        Ok(())
                    }
                    "network" => {
                        engine.network_changed();
                        Ok(())
                    }
                    "interactive" => engine.connect_interactive(),
                    other => return Err(anyhow!("unknown engine command {other}")),
                };
                r.map_err(perr)
            }
            Side::None => Err(anyhow!("engine not running")),
        }
    }

    /// The IPC connection died; if an injected reply fault did it, restart the engine clean.
    fn after_transport_error(&mut self) {
        if self.reply_fault_armed {
            self.reply_fault_armed = false;
            self.stop();
            if let Err(e) = self.start_child(None) {
                if self.cfg.verbose {
                    eprintln!("fpsim: engine host restart failed: {e:#}");
                }
            }
        }
    }
}

fn lock(ctl: &Arc<Mutex<EngineCtl>>) -> MutexGuard<'_, EngineCtl> {
    match ctl.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// [`IpcBackend`] that tells the [`EngineCtl`] about transport failures.
pub struct E2eBackend {
    ipc: IpcBackend,
    ctl: Arc<Mutex<EngineCtl>>,
    /// Engine generation the current IPC connection belongs to.
    generation: u64,
}

impl Backend for E2eBackend {
    fn call(
        &mut self,
        req: IpcRequest,
        content: Option<OwnedFd>,
    ) -> Result<IpcResponse, ProtoError> {
        // An engine restart invalidates the extension's connection (a new XPC peer).
        let current = lock(&self.ctl).generation;
        if current != self.generation {
            self.ipc.disconnect();
            self.generation = current;
        }
        let r = self.ipc.call(req, content);
        if r.is_err() {
            let mut c = lock(&self.ctl);
            c.after_transport_error();
            self.generation = c.generation;
        }
        r
    }
}

/// fpsim + real engine + real `unlatchd` + the real VM root.
pub struct RealWorld {
    dir: tempfile::TempDir,
    keep: bool,
    root: PathBuf,
    outside: PathBuf,
    ctl_dir: PathBuf,
    hstate: PathBuf,
    ctl: Arc<Mutex<EngineCtl>>,
    sim: FpSim<E2eBackend>,
    vm: RealFs,
    sentinel: Fingerprint,
    /// `settle` answers a mass-deletion pause with "apply" (see [`World::set_auto_confirm`]).
    auto_confirm: bool,
}

impl RealWorld {
    pub fn start(cfg: &E2eConfig) -> Result<RealWorld> {
        std::fs::create_dir_all(&cfg.tmp_base)?;
        let dir = tempfile::Builder::new()
            .prefix("hfp-")
            .tempdir_in(&cfg.tmp_base)?;
        let base = dir.path().to_path_buf();
        let (root, outside, state, hstate, ctl_dir, scratch) = (
            base.join("root"),
            base.join("outside"),
            base.join("state"),
            base.join("hstate"),
            base.join("ctl"),
            base.join("scratch"),
        );
        for d in [&root, &outside, &state, &hstate, &ctl_dir, &scratch] {
            std::fs::create_dir_all(d)?;
        }
        // Sentinel content the root may link to but Unlatch must never touch.
        std::fs::create_dir_all(outside.join("dir"))?;
        std::fs::write(outside.join("secret.txt"), b"outside the root")?;
        std::fs::write(outside.join("dir/inner.txt"), b"also outside")?;
        let sentinel = fingerprint(&outside)?;
        let wrapper = base.join("unlatchd.sh");
        let ctl_s = ctl_dir.to_string_lossy().into_owned();
        let script = format!(
            "#!/bin/sh\n\
             # Written by fpsim: link cut + one-shot unlatchd fault injection.\n\
             if [ -e {c}/offline ]; then echo 'fpsim: link cut' >&2; exit 75; fi\n\
             if [ -e {c}/wire-fault ]; then UNLATCH_FAULT=$(cat {c}/wire-fault); rm -f {c}/wire-fault; export UNLATCH_FAULT; else unset UNLATCH_FAULT; fi\n\
             UNLATCHD_LOG={log}; export UNLATCHD_LOG\n\
             exec {h} {mode} --root {r} --state {s}\n",
            mode = match cfg.daemon {
                DaemonMode::Connect => "connect",
                DaemonMode::Stdio => "stdio",
            },
            c = sh_quote(&ctl_s),
            log = sh_quote(&ctl_dir.join("unlatchd.log").to_string_lossy()),
            h = sh_quote(&cfg.unlatchd.to_string_lossy()),
            r = sh_quote(&root.to_string_lossy()),
            s = sh_quote(&hstate.to_string_lossy()),
        );
        std::fs::write(&wrapper, script)?;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
        let (handler, rx) = event_channel();
        let handler = slow_host(handler);
        let socket = base.join("e.sock");
        let mut ctl = EngineCtl {
            cfg: cfg.clone(),
            socket: socket.clone(),
            root: root.to_string_lossy().into_owned(),
            state,
            wrapper,
            handler,
            side: Side::None,
            reply_fault_armed: false,
            generation: 0,
        };
        ctl.start_inproc()?;
        let ctl = Arc::new(Mutex::new(ctl));
        let backend = E2eBackend {
            ipc: IpcBackend::new(&socket, "fpsim", Duration::from_secs(20)),
            ctl: ctl.clone(),
            generation: 0,
        };
        let sim = FpSim::new(SimConfig::new("fpsim", &scratch), backend, rx);
        let vm = RealFs::new(&root);
        Ok(RealWorld {
            dir,
            keep: false,
            auto_confirm: true,
            root,
            outside,
            ctl_dir,
            hstate,
            ctl,
            sim,
            vm,
            sentinel,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sentinel directory next to the root (targets of out-of-root symlinks).
    pub fn outside(&self) -> &Path {
        &self.outside
    }

    pub fn base(&self) -> &Path {
        self.dir.path()
    }

    /// Keep the world directory (logs, state) after the run, for debugging.
    pub fn dir_keep(&mut self) -> PathBuf {
        self.keep = true;
        self.dir.path().to_path_buf()
    }

    /// `Engine::drop_connection()`: the engine reconnects on its own.
    pub fn drop_connection(&mut self) -> Result<()> {
        let mut c = lock(&self.ctl);
        c.command("drop", Duration::from_secs(1))?;
        c.command("live", LIVE_TIMEOUT)
    }

    /// SIGKILL every `unlatchd` serving this world; the engine respawns it.
    pub fn kill_unlatchd(&mut self) -> Result<usize> {
        self.kill_daemon_procs(&["stdio", "connect", "serve"])
    }

    /// SIGKILL this world's `unlatchd <mode>` processes of the given modes.
    fn kill_daemon_procs(&mut self, modes: &[&str]) -> Result<usize> {
        let needle = self.hstate.to_string_lossy().into_owned();
        let mut killed = 0;
        for ent in std::fs::read_dir("/proc")?.flatten() {
            let Some(pid) = ent.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            let Ok(cmd) = std::fs::read(ent.path().join("cmdline")) else {
                continue;
            };
            let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
            if cmd.contains(&needle) && modes.iter().any(|m| cmd.contains(&format!(" {m} "))) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
                killed += 1;
            }
        }
        Ok(killed)
    }

    /// Arm `die_after_commit:<op>` for the *next* unlatchd process and make the engine reconnect so
    /// the fault applies to the ops that follow.
    pub fn arm_wire_fault(&mut self, op: &str) -> Result<()> {
        self.arm_daemon_fault(&format!("die_after_commit:{op}"))
    }

    /// Restart the daemon with `UNLATCH_FAULT=<token>` (one-shot: the next daemon only).
    pub fn arm_daemon_fault(&mut self, token: &str) -> Result<()> {
        std::fs::write(self.ctl_dir.join("wire-fault"), token)?;
        self.kill_unlatchd()?;
        self.wait_live()
    }

    pub fn wait_live(&mut self) -> Result<()> {
        let mut c = lock(&self.ctl);
        if c.command("live", LIVE_TIMEOUT).is_ok() {
            return Ok(());
        }
        c.command("network", Duration::from_secs(1))?;
        if c.command("live", LIVE_TIMEOUT).is_ok() {
            return Ok(());
        }
        c.command("interactive", LIVE_TIMEOUT)?;
        c.command("live", LIVE_TIMEOUT)
    }

    /// Restart the engine in-process from an empty state dir: a full snapshot follows. Returns
    /// without waiting for it (the caller races VM changes against the snapshot).
    pub fn restart_engine_fresh(&mut self) -> Result<()> {
        let mut c = lock(&self.ctl);
        c.stop();
        let state = c.state.clone();
        if state.exists() {
            std::fs::remove_dir_all(&state)?;
        }
        std::fs::create_dir_all(&state)?;
        c.start_inproc_opts(false)
    }

    /// Everything outside the root is exactly as it was.
    pub fn outside_changes(&self) -> Result<Vec<String>> {
        Ok(fingerprint_diff(
            &self.sentinel,
            &fingerprint(&self.outside)?,
        ))
    }

    /// Materialize the Mac's tree and check every realpath stays inside the domain (D12).
    pub fn realpath_problems(&self) -> Result<Vec<String>> {
        let dest = self
            .dir
            .path()
            .join(format!("mat-{}", self.sim.now().as_nanos()));
        let p = materialize_and_check_realpaths(&self.sim.visible(), &dest)?;
        let _ = std::fs::remove_dir_all(&dest);
        Ok(p)
    }

    fn paused(&mut self) -> bool {
        self.sim
            .engine_status()
            .is_some_and(|s| matches!(s.state, ConnState::Paused { .. }))
    }
}

impl Drop for RealWorld {
    fn drop(&mut self) {
        lock(&self.ctl).stop();
        let _ = Command::new(&lock(&self.ctl).cfg.unlatchd)
            .args(["stop", "--state"])
            .arg(&self.hstate)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.kill_unlatchd();
        if self.keep {
            // Leak the directory on purpose: it holds the logs of a failed run.
            let dir = std::mem::replace(
                &mut self.dir,
                match tempfile::tempdir() {
                    Ok(d) => d,
                    Err(_) => return,
                },
            );
            let _ = dir.keep();
        }
    }
}

impl World for RealWorld {
    type B = E2eBackend;

    fn name(&self) -> String {
        format!("e2e:{}", self.dir.path().display())
    }

    fn sim(&mut self) -> &mut FpSim<E2eBackend> {
        &mut self.sim
    }

    fn vm(&mut self) -> &mut dyn VmFs {
        &mut self.vm
    }

    fn settle(&mut self) -> Result<()> {
        if self.ctl_dir.join("offline").exists() {
            return Ok(());
        }
        for attempt in 0..3 {
            let mut r = lock(&self.ctl).command("barrier", BARRIER_TIMEOUT);
            // The barrier has seen every VM change applied — or held by the mass-deletion guard
            // (rule 8), whose batch stays queued, so "idle" would wait for the user. Accept it
            // (the fuzzer's agent really did delete that much) unless a scenario is testing
            // the pause itself.
            if r.is_ok() && self.paused() {
                if !self.auto_confirm {
                    return Ok(());
                }
                self.sim
                    .confirm_paused(true)
                    .map_err(|e| anyhow!("confirm_paused: {e}"))?;
            }
            if r.is_ok() {
                r = lock(&self.ctl).command("idle", BARRIER_TIMEOUT);
            }
            match r {
                Ok(()) => return Ok(()),
                Err(e) if attempt == 2 => return Err(e.context("settle")),
                Err(_) => self.wait_live()?,
            }
        }
        Ok(())
    }

    fn set_online(&mut self, online: bool) -> Result<()> {
        let marker = self.ctl_dir.join("offline");
        if online {
            let _ = std::fs::remove_file(&marker);
            self.wait_live()?;
            self.settle()
        } else {
            std::fs::write(&marker, b"")?;
            // The link dies; the VM-side daemon (`serve`) keeps running, as over a laptop sleep.
            self.kill_daemon_procs(&["stdio", "connect"])?;
            // Let the engine notice before the scenario continues.
            std::thread::sleep(Duration::from_millis(200));
            Ok(())
        }
    }

    fn restart_engine(&mut self) -> Result<()> {
        let mut c = lock(&self.ctl);
        let child = matches!(c.side, Side::Child(_));
        c.stop();
        if child {
            c.start_child(None)
        } else {
            c.start_inproc()
        }
    }

    fn arm_reply_fault(&mut self, f: ReplyFault) -> Result<bool> {
        let mut c = lock(&self.ctl);
        if c.cfg.engine_host.is_none() {
            return Ok(false);
        }
        c.stop();
        c.start_child(Some(fault_token(f)))?;
        c.reply_fault_armed = true;
        Ok(true)
    }

    fn set_auto_confirm(&mut self, on: bool) -> Result<bool> {
        self.auto_confirm = on;
        Ok(true)
    }

    fn wipe_daemon_index(&mut self, reimage: bool) -> Result<bool> {
        // Every daemon of this world goes (serve keeps the index in memory), then its state —
        // only once they are gone, so no dying daemon checkpoints over the wipe.
        let modes = ["stdio", "connect", "serve"];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while self.kill_daemon_procs(&modes)? > 0 {
            if std::time::Instant::now() > deadline {
                return Err(anyhow!("unlatchd processes of this world do not die"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        for ent in std::fs::read_dir(&self.hstate)?.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let index_file = name == "index.bin" || name.starts_with("journal.");
            if index_file || (reimage && !name.ends_with(".lock")) {
                let p = ent.path();
                if p.is_dir() {
                    std::fs::remove_dir_all(&p)?;
                } else {
                    std::fs::remove_file(&p)?;
                }
            }
        }
        Ok(true)
    }
}

/// Run one fpsim scenario end to end; adds the outside-root and realpath invariants.
pub fn run_scenario(cfg: &E2eConfig, name: &str) -> Result<Verdict> {
    let mut w = RealWorld::start(cfg)?;
    let mut v = scenarios::run(name, &mut w)?;
    if v.skipped.is_none() {
        v.problems.extend(w.outside_changes()?);
        v.problems.extend(w.realpath_problems()?);
    }
    if cfg.verbose && !v.problems.is_empty() {
        eprintln!("--- {name}: provider calls ---");
        for c in &w.sim().history {
            eprintln!(
                "{:>10.3}s {:<12} id={:?} fields={:#x} -> {:?}",
                c.at.as_secs_f64(),
                c.kind,
                c.id,
                c.fields,
                c.outcome
            );
        }
        eprintln!("--- stats: {:?}", w.sim().stats);
        eprintln!("--- world dir kept at {}", w.base().display());
        let _ = w.dir_keep();
    }
    Ok(v)
}
