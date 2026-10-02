//! `unlatch mount`: engine + Linux FUSE frontend, optionally daemonized.

use anyhow::{bail, Context};
use std::path::{Path, PathBuf};
use std::time::Duration;
use unlatch_core::{EngineConfig, Transport};

#[derive(Clone, Debug)]
pub struct MountArgs {
    pub mountpoint: PathBuf,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<PathBuf>,
    pub ssh_args: Vec<String>,
    pub command: Vec<String>,
    pub root: String,
    pub state: Option<PathBuf>,
    pub name: Option<String>,
    pub foreground: bool,
    pub ttl: Duration,
    pub prefetch: bool,
    pub exec: bool,
    pub workers: usize,
    pub wait_live: Duration,
    pub unlatchd_command: Option<String>,
}

/// Printed on stdout by a daemonized mount once the kernel accepted the mount.
pub const READY_LINE: &str = "UNLATCH_MOUNT_READY";
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DAEMON_ENV: &str = "UNLATCH_MOUNT_DAEMON_CHILD";
/// How long a mount with no replica yet waits for the first snapshot.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const FIRST_SYNC_WAIT: Duration = Duration::from_secs(60);

impl MountArgs {
    pub fn transport(&self) -> anyhow::Result<Transport> {
        match (&self.host, self.command.is_empty()) {
            (Some(_), false) => bail!("--host and --command are mutually exclusive"),
            (None, true) => {
                bail!("one of --host <ssh destination> or --command <argv…> is required")
            }
            (Some(h), true) => {
                if h.is_empty() || h.starts_with('-') {
                    bail!("invalid ssh destination {h:?}");
                }
                Ok(Transport::Ssh {
                    destination: h.clone(),
                    port: self.port,
                    identity: self.identity.clone(),
                    extra_args: self.ssh_args.clone(),
                })
            }
            (None, false) => Ok(Transport::Command {
                argv: split_command(&self.command),
                env: Vec::new(),
            }),
        }
    }

    /// A short, stable description of what is mounted (keys the default state dir).
    fn source_key(&self) -> String {
        match (&self.host, self.command.first()) {
            (Some(h), _) => format!("ssh:{h}:{}", self.root),
            (None, Some(_)) => format!("cmd:{}:{}", self.command.join(" "), self.root),
            _ => format!("?:{}", self.root),
        }
    }

    pub fn state_dir(&self) -> PathBuf {
        match &self.state {
            Some(s) => s.clone(),
            None => crate::home_dir()
                .join(".unlatch/mount")
                .join(crate::short_hash(&[&self.source_key()])),
        }
    }

    pub fn domain_name(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        let base = self
            .root
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty() && *s != "~");
        match (&self.host, base) {
            (Some(h), Some(b)) => format!("{h}-{b}"),
            (Some(h), None) => h.clone(),
            (None, Some(b)) => b.to_string(),
            (None, None) => "unlatch".to_string(),
        }
    }
}

/// `--command` takes the rest of the line; a single value with spaces is split on whitespace
/// (so `--command "unlatchd stdio --root /r"` works too).
pub fn split_command(argv: &[String]) -> Vec<String> {
    if argv.len() == 1 && argv[0].contains(char::is_whitespace) {
        argv[0].split_whitespace().map(str::to_string).collect()
    } else {
        argv.to_vec()
    }
}

/// Is `path` a mount point already (per /proc/self/mountinfo)?
pub fn is_mounted(path: &Path) -> bool {
    let Ok(canon) = path.canonicalize() else {
        return false;
    };
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    info.lines().any(|l| {
        l.split(' ').nth(4).map(unescape_mountinfo).as_deref()
            == Some(canon.as_os_str().to_string_lossy().as_ref())
    })
}

/// mountinfo escapes space, tab, newline and backslash as `\ooo`.
fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = u32::from(b[i + 1] - b'0') * 64
                + u32::from(b[i + 2] - b'0') * 8
                + u32::from(b[i + 3] - b'0');
            match u8::try_from(v) {
                Ok(v) => {
                    out.push(v);
                    i += 4;
                }
                Err(_) => {
                    out.push(b[i]);
                    i += 1;
                }
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn check_mountpoint(p: &Path) -> anyhow::Result<()> {
    match std::fs::metadata(p) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => bail!("{} is not a directory", p.display()),
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => bail!(
            "{} is a dead FUSE mount (\"transport endpoint is not connected\"); clear it with `fusermount3 -u {}`",
            p.display(),
            p.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("mount point {}", p.display())),
    }
    if is_mounted(p) {
        bail!(
            "{} is already a mount point (unmount it first: `fusermount3 -u {}`)",
            p.display(),
            p.display()
        );
    }
    Ok(())
}

/// Re-run ourselves detached (new session, stderr to a log file) and wait until the child
/// reports the mount is up.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn daemonize(args: &MountArgs) -> anyhow::Result<()> {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    let state = args.state_dir();
    std::fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
    let log_path = state.join("mount.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let exe = std::env::current_exe().context("locating the unlatch binary")?;
    let mut cmd = std::process::Command::new(exe);
    // `--foreground` goes before the rest so it cannot be swallowed by a trailing `--command`.
    cmd.arg("mount")
        .arg("--foreground")
        .args(std::env::args_os().skip(2));
    cmd.env(DAEMON_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting the background mount")?;
    let stdout = child.stdout.take().context("child stdout")?;
    let mut ready = false;
    for line in std::io::BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
    {
        if line.trim() == READY_LINE {
            ready = true;
            break;
        }
    }
    if ready {
        println!(
            "mounted {} (pid {}, log {})",
            args.mountpoint.display(),
            child.id(),
            log_path.display()
        );
        return Ok(());
    }
    let _ = child.wait();
    let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
    let tail: Vec<&str> = tail.lines().rev().take(15).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    bail!(
        "mount failed; last log lines ({}):\n{}",
        log_path.display(),
        tail.join("\n")
    )
}

/// Tell a waiting parent we are up, then detach stdout from its pipe.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn signal_ready() {
    use std::io::Write;
    if std::env::var_os(DAEMON_ENV).is_none() {
        return;
    }
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{READY_LINE}");
    let _ = out.flush();
    // SAFETY: plain fd syscalls on our own stdout; /dev/null is always openable read-write.
    unsafe {
        let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, 1);
            libc::close(fd);
        }
    }
}

pub fn engine_config(args: &MountArgs) -> anyhow::Result<EngineConfig> {
    let client = unlatch_core::transport::sanitize_client_name(&crate::machine_name());
    let mut cfg = EngineConfig::new(
        &args.domain_name(),
        args.transport()?,
        &args.root,
        args.state_dir(),
        &client,
    );
    // On Linux the exec bit is ordinary metadata (the Mac-side quarantine concern of D12 does
    // not apply to a FUSE mount), so it is shown unless --no-exec.
    cfg.expose_exec = args.exec;
    cfg.unlatchd_command = args.unlatchd_command.clone();
    Ok(cfg)
}

#[cfg(target_os = "linux")]
pub fn run_mount(args: MountArgs) -> anyhow::Result<()> {
    use crate::fuse::{event_channel, mount, FsOptions};
    use crate::signals::Terminator;
    use std::sync::Arc;
    use tracing::{info, warn};
    use unlatch_core::Engine;

    check_mountpoint(&args.mountpoint)?;
    args.transport()?;
    if !args.foreground {
        return daemonize(&args);
    }
    let term = Terminator::install().context("installing signal handlers")?;
    let cfg = engine_config(&args)?;
    let state = cfg.state_dir.clone();
    std::fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
    let (sink, queue) = event_channel();
    let engine = Engine::start(cfg, Some(sink.handler()))
        .map_err(|e| anyhow::anyhow!("starting the engine: {e}"))?;
    // With a replica from an earlier run the last-known tree mounts immediately; a fresh
    // state dir has nothing to show until the first snapshot, so wait for it.
    let fresh = engine.item(unlatch_proto::ItemId::ROOT).is_err();
    let wait = if fresh {
        args.wait_live.max(FIRST_SYNC_WAIT)
    } else {
        args.wait_live
    };
    if !wait.is_zero() {
        if let Err(e) = engine.wait_live(wait) {
            if fresh {
                let state = engine.status().state;
                engine.shutdown();
                bail!("the first sync did not complete within {wait:?} ({e}; engine state: {state:?}); try `unlatch doctor`");
            }
            warn!(error = %e, "not live yet; mounting the last-known tree anyway");
        }
    }
    // SAFETY: getuid/getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let opts = FsOptions {
        ttl: args.ttl,
        prefetch: args.prefetch,
        workers: args.workers,
        scratch_dir: state.join("fuse-scratch"),
        unsynced_dir: state.join("unsynced"),
        uid,
        gid,
    };
    let fsname = format!("unlatch:{}", args.domain_name());
    let mounted = match mount(
        Arc::new(engine.clone()),
        opts,
        sink,
        queue,
        &args.mountpoint,
        &fsname,
    ) {
        Ok(m) => m,
        Err(e) => {
            engine.shutdown();
            return Err(e).with_context(|| format!("mounting at {}", args.mountpoint.display()));
        }
    };
    info!(mountpoint = %args.mountpoint.display(), root = %args.root, "mounted");
    signal_ready();
    let by_signal = term.wait_until(|| !mounted.is_alive());
    if by_signal {
        info!("signal received; unmounting");
    } else {
        info!("unmounted externally");
    }
    mounted.unmount();
    engine.shutdown();
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn run_mount(_args: MountArgs) -> anyhow::Result<()> {
    bail!("`unlatch mount` is the Linux FUSE frontend; on macOS use Unlatch.app (File Provider) and `unlatch agent`")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> MountArgs {
        MountArgs {
            mountpoint: "/mnt/x".into(),
            host: None,
            port: None,
            identity: None,
            ssh_args: vec![],
            command: vec![],
            root: "/srv/code".into(),
            state: None,
            name: None,
            foreground: true,
            ttl: Duration::from_secs(300),
            prefetch: true,
            exec: true,
            workers: 16,
            wait_live: Duration::ZERO,
            unlatchd_command: None,
        }
    }

    #[test]
    fn transport_choice() {
        let mut a = base();
        assert!(a.transport().is_err());
        a.host = Some("dev".into());
        assert!(matches!(a.transport().unwrap(), Transport::Ssh { .. }));
        a.command = vec!["unlatchd".into()];
        assert!(a.transport().is_err());
        a.host = None;
        a.command = vec!["unlatchd stdio --root /r".into()];
        match a.transport().unwrap() {
            Transport::Command { argv, .. } => {
                assert_eq!(argv, vec!["unlatchd", "stdio", "--root", "/r"])
            }
            other => panic!("{other:?}"),
        }
        a.command = vec![];
        a.host = Some("-oProxyCommand=evil".into());
        assert!(a.transport().is_err());
    }

    #[test]
    fn names_and_state_dirs() {
        let mut a = base();
        a.host = Some("dev".into());
        assert_eq!(a.domain_name(), "dev-code");
        a.root = "~".into();
        assert_eq!(a.domain_name(), "dev");
        a.name = Some("mine".into());
        assert_eq!(a.domain_name(), "mine");
        let s1 = a.state_dir();
        a.root = "/other".into();
        assert_ne!(a.state_dir(), s1, "different roots get different state");
        a.state = Some("/tmp/st".into());
        assert_eq!(a.state_dir(), PathBuf::from("/tmp/st"));
    }

    #[test]
    fn mountinfo_unescape_and_detection() {
        assert_eq!(unescape_mountinfo(r"/a\040b\134c"), "/a b\\c");
        assert!(is_mounted(Path::new("/")));
        let d = tempfile::tempdir().unwrap();
        assert!(!is_mounted(d.path()));
    }

    #[test]
    fn mountpoint_checks() {
        let d = tempfile::tempdir().unwrap();
        check_mountpoint(d.path()).unwrap();
        let f = d.path().join("file");
        std::fs::write(&f, "x").unwrap();
        assert!(check_mountpoint(&f).is_err());
        assert!(check_mountpoint(&d.path().join("missing")).is_err());
    }
}
