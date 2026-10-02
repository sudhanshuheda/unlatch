//! Opening a link to `unlatchd`: spawn ssh (or a command), bootstrap/upload `unlatchd` when missing,
//! exchange preambles, classify failures.
//!
//! Bootstrap (Ssh transport, one `ssh … sh -s` session): the engine writes a POSIX-sh script on
//! stdin that (1) probes the install dir (`$UNLATCH_HOME`, `$XDG_DATA_HOME/unlatch`, `~/.unlatch`,
//! `/var/tmp/unlatch-$UID`, `/tmp/unlatch-$UID`; first that is 0700/owned/not a symlink/local fs/
//! passes an exec test), (2) checks `unlatchd-<version>` exists with the expected sha256
//! (`sha256sum`/`shasum -a 256`), (3) if not, prints a request marker and receives the binary
//! (length-prefixed on the same stdin) into `.tmp`, verifies, fsyncs, renames, (4) `exec`s
//! `unlatchd connect --root <root>`. The binary's sha256 is re-verified on every connect.
//!
//! Details:
//! * The remote file is `unlatchd-<version>-<sha256[..16]>` in the chosen dir; the script exports
//!   `UNLATCH_HOME=<dir>` for `unlatchd`. With no binary configured for the VM's architecture it
//!   falls back to an `unlatchd` on the remote `PATH`.
//! * The "length prefix" is the size in the script's per-arch table; the script asks with a
//!   `UNLATCH-<nonce>-NEED <arch>` line and reads exactly that many bytes with `head -c`.
//! * The client preamble is written only after the script's `UNLATCH-<nonce>-EXEC` line, so no
//!   shell or `head` can consume it.
//! * `EngineConfig::remote_install_dir` names a remote install dir to try first (tests, unusual
//!   VM layouts); it goes through the same checks as the rest of the probe order.
//! * Script failures come back as `UNLATCH-<nonce>-ERR fatal|retry <msg>`: `fatal` → `NeedsUser`
//!   (a human must fix the VM), `retry` → `Offline`.

mod bootstrap;
mod login_env;
mod scan;

use crate::{err, EngineConfig, Result, Transport};
use scan::{Scanner, Seen};
use std::io::Cursor;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Notify;
use unlatch_proto::frame::{negotiate, Preamble};
use unlatch_proto::{ErrorCode, ProtoError, PROTO_VERSION};

/// An open, handshaken link (both preambles exchanged, version negotiated).
pub struct Link {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    /// The spawned process (ssh or command); killed on drop of the Link's owner.
    pub child: Option<tokio::process::Child>,
    pub server: Preamble,
    pub proto: u16,
    /// Bytes the remote shell printed before the preamble (shown in diagnostics).
    pub junk: Vec<u8>,
}

/// Waiting for a preamble from a local command (`unlatchd stdio`/`connect` start in milliseconds).
const COMMAND_HANDSHAKE: Duration = Duration::from_secs(30);
/// Background ssh: connect + auth + probe. A hung ProxyCommand or a login URL that nobody will
/// visit must not wedge the supervisor.
const SSH_HANDSHAKE: Duration = Duration::from_secs(30);
/// Interactive connects wait for a human typing a password or approving 2FA.
const SSH_HANDSHAKE_INTERACTIVE: Duration = Duration::from_secs(300);
/// Upload time allowance on top of the handshake budget (a pessimistic 64 KiB/s link).
const UPLOAD_MIN_BYTES_PER_SEC: u64 = 64 * 1024;
/// Stderr kept for diagnostics/classification (the tail; ssh prints the cause last).
const STDERR_KEEP: usize = 64 * 1024;

fn client_preamble() -> Preamble {
    Preamble {
        proto_min: 1,
        proto_max: PROTO_VERSION as u16,
        build_id: [0; 16],
    }
}

/// Open a link. `interactive` allows askpass prompts (UI-initiated connects); background
/// reconnects use `BatchMode=yes`. Errors: `NeedsUser` (host key, auth, 2FA, "visit URL"),
/// `Offline` (network, timeouts), `Protocol` (no common version / no preamble).
///
/// For `NeedsUser` the message contains the relevant ssh stderr, so
/// `classify_ssh_failure(&e.msg, None)` recovers the login URL, if any.
pub async fn open(cfg: &EngineConfig, interactive: bool) -> Result<Link> {
    match &cfg.transport {
        Transport::Command { argv, env } => open_command(argv, env).await,
        Transport::Ssh { .. } => open_ssh(cfg, interactive).await,
    }
}

// ---- process plumbing ----------------------------------------------------------------------

/// Tail of the child's stderr, drained continuously (a full stderr pipe would block ssh).
#[derive(Clone, Default)]
struct StderrLog {
    buf: Arc<Mutex<Vec<u8>>>,
    done: Arc<Notify>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl StderrLog {
    fn text(&self) -> String {
        let b = self
            .buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&b).trim().to_string()
    }

    /// Wait (bounded) for the stderr stream to reach EOF after the process exited.
    async fn settle(&self, max: Duration) {
        if self.finished.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let _ = tokio::time::timeout(max, self.done.notified()).await;
    }
}

struct Spawned {
    child: Child,
    stdin: ChildStdin,
    stderr: StderrLog,
    what: String,
}

fn spawn(cmd: &mut Command, what: &str) -> Result<(Spawned, ChildStdout)> {
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // SAFETY: setsid is async-signal-safe. Detaching from the controlling terminal keeps ssh from
    // prompting on (or stopping for) a tty the user cannot see: prompts go through askpass or
    // fail fast under BatchMode, and terminal signals aimed at the host do not hit the link.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| err(ErrorCode::Io, format!("cannot run {what}: {e}")))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(err(ErrorCode::Io, format!("{what}: stdio pipes missing")));
    };
    no_sigpipe(&stdin);
    let log = StderrLog::default();
    let sink = log.clone();
    let what_owned = what.to_string();
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tracing::debug!(target: "unlatch::transport", "{what_owned} stderr: {}", String::from_utf8_lossy(&chunk[..n]).trim_end());
                    let mut b = sink
                        .buf
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    b.extend_from_slice(&chunk[..n]);
                    if b.len() > STDERR_KEEP {
                        let cut = b.len() - STDERR_KEEP;
                        b.drain(..cut);
                    }
                }
            }
        }
        sink.finished
            .store(true, std::sync::atomic::Ordering::SeqCst);
        sink.done.notify_waiters();
    });
    Ok((
        Spawned {
            child,
            stdin,
            stderr: log,
            what: what.to_string(),
        },
        stdout,
    ))
}

/// The host app does not ignore SIGPIPE; a write to a dead ssh must be an `EPIPE`, not a kill.
#[cfg(target_os = "macos")]
fn no_sigpipe(stdin: &ChildStdin) {
    use std::os::fd::AsRawFd;
    const F_SETNOSIGPIPE: libc::c_int = 73; // <sys/fcntl.h>; not exported by the libc crate
                                            // SAFETY: fcntl on a pipe fd we own.
    unsafe {
        libc::fcntl(stdin.as_raw_fd(), F_SETNOSIGPIPE, 1);
    }
}

#[cfg(not(target_os = "macos"))]
fn no_sigpipe(_stdin: &ChildStdin) {}

fn status_str(st: Option<ExitStatus>) -> String {
    match st {
        Some(s) => match s.code() {
            Some(c) => format!("exit {c}"),
            None => format!("{s}"),
        },
        None => "still running".into(),
    }
}

fn lossy_tail(b: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(b);
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

/// The link died before the preamble: collect exit status + stderr and turn them into an error.
async fn failure(mut sp: Spawned, junk: &[u8], ssh: bool, cause: &str) -> ProtoError {
    let status = tokio::time::timeout(Duration::from_secs(2), sp.child.wait())
        .await
        .ok()
        .and_then(|r| r.ok());
    if status.is_none() {
        let _ = sp.child.start_kill();
    }
    sp.stderr.settle(Duration::from_millis(500)).await;
    let stderr = sp.stderr.text();
    let printed = lossy_tail(junk, 512);
    let mut msg = format!("{} {cause} ({})", sp.what, status_str(status));
    if !stderr.is_empty() {
        msg.push_str(&format!(": {}", lossy_tail(stderr.as_bytes(), 2048)));
    }
    if !printed.is_empty() {
        msg.push_str(&format!("; remote shell printed: {printed}"));
    }
    let code = if ssh {
        classify_ssh_failure(&stderr, status.and_then(|s| s.code())).0
    } else {
        ErrorCode::Offline
    };
    err(code, msg)
}

async fn timeout_failure(mut sp: Spawned, junk: &[u8], ssh: bool, after: Duration) -> ProtoError {
    let _ = sp.child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), sp.child.wait()).await;
    sp.stderr.settle(Duration::from_millis(200)).await;
    let stderr = sp.stderr.text();
    // A login URL / 2FA prompt blocks forever in the background: that is NeedsUser, not Offline.
    let code = match ssh.then(|| classify_ssh_failure(&stderr, None)) {
        Some((ErrorCode::NeedsUser, _)) => ErrorCode::NeedsUser,
        _ => ErrorCode::Offline,
    };
    let mut msg = format!("{}: no unlatch handshake within {after:?}", sp.what);
    if !stderr.is_empty() {
        msg.push_str(&format!(": {}", lossy_tail(stderr.as_bytes(), 2048)));
    }
    let printed = lossy_tail(junk, 512);
    if !printed.is_empty() {
        msg.push_str(&format!("; remote shell printed: {printed}"));
    }
    err(code, msg)
}

fn finish(sp: Spawned, scanner: Scanner<ChildStdout>, server: Preamble) -> Result<Link> {
    let ours = client_preamble();
    let proto = negotiate(&ours, &server).ok_or_else(|| {
        err(
            ErrorCode::Protocol,
            format!(
                "no common protocol version: client speaks {}..={}, unlatchd {}..={} — update unlatchd",
                ours.proto_min, ours.proto_max, server.proto_min, server.proto_max
            ),
        )
    })?;
    let (rest, stdout, junk) = scanner.into_parts();
    if !junk.is_empty() {
        tracing::info!(
            "remote shell printed before unlatchd started: {}",
            lossy_tail(&junk, 512)
        );
    }
    Ok(Link {
        reader: Box::new(Cursor::new(rest).chain(stdout)),
        writer: Box::new(sp.stdin),
        child: Some(sp.child),
        server,
        proto,
        junk,
    })
}

// ---- Transport::Command ---------------------------------------------------------------------

async fn open_command(argv: &[String], env: &[(String, String)]) -> Result<Link> {
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| err(ErrorCode::Protocol, "empty unlatchd command"))?;
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let (mut sp, stdout) = spawn(&mut cmd, prog)?;
    // A child that dies at once makes this write fail with EPIPE; the scan then reports why.
    let wrote = sp
        .stdin
        .write_all(&client_preamble().to_bytes())
        .await
        .and(sp.stdin.flush().await);
    let mut scanner = Scanner::new(stdout, None);
    let seen = tokio::time::timeout(COMMAND_HANDSHAKE, scanner.next()).await;
    match seen {
        Err(_) => Err(timeout_failure(sp, &scanner.junk, false, COMMAND_HANDSHAKE).await),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
            Err(err(ErrorCode::Protocol, format!("{prog}: {e}")))
        }
        Ok(Err(e)) => Err(failure(sp, &scanner.junk, false, &format!("read failed: {e}")).await),
        Ok(Ok(Seen::Preamble(p))) => {
            if let Err(e) = wrote {
                return Err(err(
                    ErrorCode::Offline,
                    format!("{prog}: writing preamble: {e}"),
                ));
            }
            finish(sp, scanner, p)
        }
        Ok(Ok(_)) => Err(failure(
            sp,
            &scanner.junk,
            false,
            "exited before the unlatch handshake",
        )
        .await),
    }
}

// ---- Transport::Ssh -------------------------------------------------------------------------

fn home_dir(cfg: &EngineConfig) -> Option<std::path::PathBuf> {
    cfg.ssh_env
        .iter()
        .find(|(k, _)| k == "HOME")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("HOME").ok())
        .filter(|h| !h.is_empty())
        .map(Into::into)
}

async fn open_ssh(cfg: &EngineConfig, interactive: bool) -> Result<Link> {
    let argv = ssh_argv(cfg, interactive);
    let nonce = format!("{:016x}", rand::random::<u64>());
    let prefix = format!("UNLATCH-{nonce}-");
    let remote_home = cfg.remote_install_dir.as_deref().filter(|d| !d.is_empty());
    let (script, uploads) = match &cfg.unlatchd_command {
        Some(c) => (
            bootstrap::override_script(&nonce, c, &cfg.remote_root),
            Vec::new(),
        ),
        None => {
            let ups = bootstrap::uploads(cfg)?;
            (
                bootstrap::script(&nonce, &ups, &cfg.remote_root, remote_home),
                ups,
            )
        }
    };

    // ControlPath lives in ~/.ssh; ssh refuses to multiplex if the directory is missing.
    if let Some(home) = home_dir(cfg) {
        let dot_ssh = home.join(".ssh");
        if !dot_ssh.exists() {
            use std::os::unix::fs::DirBuilderExt;
            let _ = std::fs::DirBuilder::new().mode(0o700).create(&dot_ssh);
        }
    }

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).arg("sh -s");
    cmd.envs(cfg.ssh_env.iter().map(|(k, v)| (k, v)));
    if interactive {
        if let Some(askpass) = &cfg.askpass {
            cmd.env("SSH_ASKPASS", askpass)
                .env("SSH_ASKPASS_REQUIRE", "force");
            // OpenSSH < 8.4 ignores SSH_ASKPASS_REQUIRE and only uses askpass with DISPLAY set.
            if std::env::var_os("DISPLAY").is_none()
                && !cfg.ssh_env.iter().any(|(k, _)| k == "DISPLAY")
            {
                cmd.env("DISPLAY", ":0");
            }
        }
    }
    let (mut sp, stdout) = spawn(&mut cmd, "ssh")?;

    let upload_bytes: u64 = uploads.iter().map(|u| u.size).max().unwrap_or(0);
    let budget = if interactive {
        SSH_HANDSHAKE_INTERACTIVE
    } else {
        SSH_HANDSHAKE
    } + Duration::from_secs(upload_bytes / UPLOAD_MIN_BYTES_PER_SEC);
    let deadline = tokio::time::Instant::now() + budget;

    if let Err(e) = sp
        .stdin
        .write_all(script.as_bytes())
        .await
        .and(sp.stdin.flush().await)
    {
        return Err(failure(sp, &[], true, &format!("closed stdin ({e})")).await);
    }
    let mut scanner = Scanner::new(stdout, Some(prefix));
    loop {
        let seen = match tokio::time::timeout_at(deadline, scanner.next()).await {
            Err(_) => return Err(timeout_failure(sp, &scanner.junk, true, budget).await),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
                return Err(err(ErrorCode::Protocol, format!("ssh: {e}")));
            }
            Ok(Err(e)) => {
                return Err(failure(sp, &scanner.junk, true, &format!("read failed: {e}")).await)
            }
            Ok(Ok(s)) => s,
        };
        match seen {
            Seen::Preamble(p) => return finish(sp, scanner, p),
            Seen::Eof => {
                return Err(failure(
                    sp,
                    &scanner.junk,
                    true,
                    "closed the session before unlatchd started",
                )
                .await)
            }
            Seen::Marker(m) => {
                let (word, arg) = m.split_once(' ').unwrap_or((m.as_str(), ""));
                match word {
                    "NEED" => {
                        let Some(up) = uploads.iter().find(|u| u.arch == arg) else {
                            return Err(err(
                                ErrorCode::Protocol,
                                format!("bootstrap asked for unknown arch {arg:?}"),
                            ));
                        };
                        tracing::info!(arch = %up.arch, bytes = up.size, "uploading unlatchd to the VM");
                        if let Err(e) = send_file(&mut sp.stdin, &up.path, up.size).await {
                            return Err(failure(
                                sp,
                                &scanner.junk,
                                true,
                                &format!("upload failed ({e})"),
                            )
                            .await);
                        }
                    }
                    "EXEC" => {
                        let bytes = client_preamble().to_bytes();
                        if let Err(e) = sp.stdin.write_all(&bytes).await.and(sp.stdin.flush().await)
                        {
                            return Err(failure(
                                sp,
                                &scanner.junk,
                                true,
                                &format!("closed stdin ({e})"),
                            )
                            .await);
                        }
                    }
                    "ERR" => {
                        let (class, msg) = arg.split_once(' ').unwrap_or((arg, ""));
                        let code = if class == "fatal" {
                            ErrorCode::NeedsUser
                        } else {
                            ErrorCode::Offline
                        };
                        let _ = sp.child.start_kill();
                        return Err(err(
                            code,
                            format!("unlatchd bootstrap on the VM failed: {msg}"),
                        ));
                    }
                    other => tracing::debug!("ignoring unknown bootstrap marker {other:?}"),
                }
            }
        }
    }
}

/// Stream exactly `size` bytes of `path` (the size the script's `head -c` expects).
async fn send_file(stdin: &mut ChildStdin, path: &Path, size: u64) -> std::io::Result<()> {
    let f = tokio::fs::File::open(path).await?;
    let mut limited = f.take(size);
    let n = tokio::io::copy(&mut limited, stdin).await?;
    stdin.flush().await?;
    if n != size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("{} shrank to {n} bytes during upload", path.display()),
        ));
    }
    Ok(())
}

// ---- ssh argv, classification, names --------------------------------------------------------

/// Classify an ssh exit (usually 255) from its stderr.
///
/// `NeedsUser` (never retried automatically): host key unknown/changed, authentication refused,
/// too many auth failures, keyboard-interactive/2FA, locked key or agent, and any "visit this
/// URL"/device-code login (the URL is returned). `Offline` (retry with backoff): refused,
/// timed out, unreachable, name resolution, reset/closed connections, and any other exit 255.
/// Another exit status means the remote command itself failed: `Protocol`.
pub fn classify_ssh_failure(stderr: &str, exit: Option<i32>) -> (ErrorCode, Option<String>) {
    let lower = stderr.to_ascii_lowercase();
    let url = extract_url(stderr);
    const NEEDS_USER: &[&str] = &[
        "remote host identification has changed",
        "host key verification failed",
        "host key is known for",
        "host key for",
        "authenticity of host",
        "permission denied",
        "too many authentication failures",
        "keyboard-interactive",
        "verification code",
        "one-time password",
        "two-factor",
        "passphrase",
        "sign_and_send_pubkey: signing failed",
        "agent refused operation",
        "ssh_askpass:",
        "no more authentication methods",
        "account is locked",
        "password change required",
    ];
    const URL_WORDS: &[&str] = &[
        "visit",
        "authenticat",
        "log in",
        "login",
        "sign in",
        "open",
        "browser",
        "device",
        "code",
    ];
    if NEEDS_USER.iter().any(|p| lower.contains(p)) {
        return (ErrorCode::NeedsUser, url);
    }
    if url.is_some() && URL_WORDS.iter().any(|w| lower.contains(w)) {
        return (ErrorCode::NeedsUser, url);
    }
    const OFFLINE: &[&str] = &[
        "connection refused",
        "connection timed out",
        "operation timed out",
        "no route to host",
        "network is unreachable",
        "host is down",
        "could not resolve hostname",
        "name or service not known",
        "nodename nor servname",
        "temporary failure in name resolution",
        "no address associated with hostname",
        "connection reset",
        "connection closed by",
        "connection aborted",
        "software caused connection abort",
        "kex_exchange_identification",
        "ssh_exchange_identification",
        "broken pipe",
        "timeout, server",
        "not responding",
        "proxycommand",
        "stdio forwarding failed",
    ];
    if OFFLINE.iter().any(|p| lower.contains(p)) {
        return (ErrorCode::Offline, None);
    }
    match exit {
        None | Some(255) => (ErrorCode::Offline, None),
        Some(_) => (ErrorCode::Protocol, None),
    }
}

fn extract_url(s: &str) -> Option<String> {
    let start = s.find("https://").or_else(|| s.find("http://"))?;
    let rest = &s[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ')', ';', ':']);
    (url.len() > "https://".len()).then(|| url.to_string())
}

/// Build the ssh argv (without the remote command) for `cfg` (exposed for tests/doctor).
///
/// `extra_args` come right after `-T`: ssh keeps the *first* value it sees for an option, so the
/// user's explicit `-o` settings override Unlatch's defaults. The destination follows `--` so a
/// name starting with `-` can never be parsed as an option.
pub fn ssh_argv(cfg: &EngineConfig, interactive: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["ssh".into(), "-T".into()];
    let (destination, port, identity, extra) = match &cfg.transport {
        Transport::Ssh {
            destination,
            port,
            identity,
            extra_args,
        } => (
            destination.as_str(),
            *port,
            identity.as_ref(),
            extra_args.as_slice(),
        ),
        Transport::Command { .. } => ("", None, None, &[][..]),
    };
    a.extend(extra.iter().cloned());
    let mut opt = |o: &str| {
        a.push("-o".into());
        a.push(o.into());
    };
    // LZ4 per frame is ~10x cheaper than ssh's zlib (DESIGN §4).
    opt("Compression=no");
    opt("ServerAliveInterval=15");
    opt("ServerAliveCountMax=3");
    if !interactive {
        opt("BatchMode=yes");
    }
    // One authenticated master per host: the interactive connect creates it, background
    // reconnects reuse it without prompting (D21). Short path: sun_path is 104 bytes on macOS.
    opt("ControlMaster=auto");
    opt("ControlPath=~/.ssh/unlatch-%C");
    opt("ControlPersist=10m");
    if let Some(id) = identity {
        opt("IdentitiesOnly=yes");
        a.push("-i".into());
        a.push(id.to_string_lossy().into_owned());
    }
    if let Some(p) = port {
        a.push("-p".into());
        a.push(p.to_string());
    }
    a.push("--".into());
    a.push(destination.to_string());
    a
}

/// Sanitize a machine name for conflict file names: ≤ 32 bytes (UTF-8 boundary), no '/', NUL
/// or control characters; empty → "mac".
pub fn sanitize_client_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|&c| c != '/' && c != '\0' && !c.is_control())
        .collect();
    let trimmed = cleaned.trim();
    let mut end = trimmed.len().min(32);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let out = trimmed[..end].trim_end();
    if out.is_empty() {
        "mac".to_string()
    } else {
        out.to_string()
    }
}

/// Environment of the user's login shell (`$SHELL -l -i -c 'env -0'`), for spawning ssh from a
/// launchd-started host (review (e)10): its PATH reaches ProxyCommand tools, its agent socket
/// reaches 1Password/Secretive. Empty on failure or timeout (the caller then inherits).
pub fn resolve_login_env(timeout: Duration) -> Vec<(String, String)> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    login_env::resolve(&shell, timeout)
}
