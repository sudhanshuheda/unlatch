//! Process lifecycle (D18, review §2(d)10): `stdio`, `serve`, `connect`, `status`, `stop`, `gc`.
//!
//! * `serve` holds `flock(<state>/serve.lock)` for its whole life and listens on the abstract
//!   socket `\0unlatch/<uid>/p<major>/<hash>` (never stale, no unlink); peers are checked with
//!   SO_PEERCRED in both directions. The hash covers a random nonce kept in the 0700 state dir
//!   (`sock.nonce`, 0600), re-rolled whenever the name is taken: the abstract namespace has no
//!   permissions, so a predictable name could be pre-bound by another user to deny service.
//! * `connect` never unlinks anything; it spawns a server only while holding
//!   `<state>/spawn.lock`, and only after `LOCK_NB` on `serve.lock` succeeded (server dead).
//! * The background server is properly daemonized (double fork, setsid, stdio → /dev/null + log
//!   file, other fds closed) so it never holds the ssh session open.

use crate::config::Config;
use crate::core::Core;
use crate::sys;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const EXIT_USAGE: i32 = 2;

/// Install directory: `$UNLATCH_HOME`, else the directory holding this binary.
pub fn install_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("UNLATCH_HOME") {
        return PathBuf::from(h);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The install directory when it is unlatchd's own: `$UNLATCH_HOME` (the bootstrap and `npx
/// unlatch` always export the directory they probed), or — without it — the directory holding
/// this binary if it is one of the directories the bootstrap probes (`$XDG_DATA_HOME/unlatch`,
/// `~/.unlatch`, `/var/tmp/unlatch-$UID`, `/tmp/unlatch-$UID`, matched by identity). `None`: the
/// binary sits in a directory of the user's (`~/bin`, `~/.local/bin`, `~/.cargo/bin`, …), of
/// which only the binary itself is unlatchd's.
pub fn dedicated_install_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("UNLATCH_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    use std::os::unix::fs::MetadataExt;
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let id = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()));
    let mine = id(&exe_dir)?;
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut probed = vec![
        PathBuf::from(format!("/var/tmp/unlatch-{uid}")),
        PathBuf::from(format!("/tmp/unlatch-{uid}")),
    ];
    for (var, sub) in [("XDG_DATA_HOME", "unlatch"), ("HOME", ".unlatch")] {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            probed.push(Path::new(&v).join(sub));
        }
    }
    probed
        .iter()
        .any(|p| id(p) == Some(mine))
        .then_some(exe_dir)
}

fn hex16(b: &[u8]) -> String {
    blake3::hash(b).as_bytes()[..8]
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

pub fn root_hash(root: &Path) -> String {
    hex16(root.as_os_str().as_encoded_bytes())
}

pub fn default_state_dir(root: &Path) -> PathBuf {
    install_dir().join("state").join(root_hash(root))
}

/// Abstract socket name. The hash covers the state dir too, so two state dirs for one root
/// (tests) never share a server, and the state dir's private nonce (see [`current_socket_name`]);
/// `None` is the nonce-less name of unlatchd versions before it (still used while no nonce exists).
pub fn socket_name(root: &Path, state: &Path, nonce: Option<&str>) -> Vec<u8> {
    let mut key = root.as_os_str().as_encoded_bytes().to_vec();
    key.push(0);
    key.extend_from_slice(state.as_os_str().as_encoded_bytes());
    if let Some(n) = nonce {
        key.push(0);
        key.extend_from_slice(n.as_bytes());
    }
    format!(
        "unlatch/{}/p{}/{}",
        sys::geteuid(),
        unlatch_proto::PROTO_VERSION,
        hex16(&key)
    )
    .into_bytes()
}

const NONCE_FILE: &str = "sock.nonce";

fn read_nonce(state: &Path) -> Option<String> {
    let s = std::fs::read_to_string(state.join(NONCE_FILE)).ok()?;
    let s = s.trim();
    (s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_string())
}

/// A fresh random nonce, written 0600 (atomically) into the state dir. Only `serve`, holding
/// serve.lock, writes it.
fn new_nonce(state: &Path) -> io::Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let n: u128 = rand::random();
    let n = format!("{n:032x}");
    let tmp = state.join(format!("{NONCE_FILE}.tmp{}", sys::getpid()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let r = f
        .write_all(n.as_bytes())
        .and_then(|_| f.sync_all())
        .and_then(|_| std::fs::rename(&tmp, state.join(NONCE_FILE)));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.map(|_| n)
}

/// The name the server of `root`/`state` listens on now (it re-reads the state dir's nonce).
pub fn current_socket_name(root: &Path, state: &Path) -> Vec<u8> {
    socket_name(root, state, read_nonce(state).as_deref())
}

fn prepare_state(state: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(state)?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700));
    std::fs::canonicalize(state)
}

fn lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

// ---- signals ---------------------------------------------------------------------------

static SIG_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_sig: libc::c_int) {
    let fd = SIG_PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = [1u8];
        // SAFETY: write(2) is async-signal-safe.
        unsafe { libc::write(fd, b.as_ptr() as *const libc::c_void, 1) };
    }
}

/// Run `f` once on SIGTERM/SIGINT/SIGHUP (on a normal thread, not in the handler).
fn on_termination(f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    let (r, w) = sys::pipe()?;
    SIG_PIPE_W.store(w.as_raw_fd(), Ordering::Relaxed);
    std::mem::forget(w);
    for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: installing a handler that only calls write(2).
        unsafe { libc::signal(s, on_signal as *const () as libc::sighandler_t) };
    }
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            loop {
                match sys::poll_fds(&[(r.as_raw_fd(), libc::POLLIN)], -1) {
                    Ok(v) if v[0] & libc::POLLIN != 0 => break,
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
            f();
        })?;
    Ok(())
}

// ---- stdio -----------------------------------------------------------------------------

fn dup_file(fd: RawFd) -> io::Result<File> {
    // SAFETY: dup of a standard fd.
    let d = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if d < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: d is a fresh fd we own.
    Ok(unsafe { File::from_raw_fd(d) })
}

/// `unlatchd stdio --root R --state DIR`: one wire session on stdin/stdout, in-process.
pub fn stdio(root: &Path, state: Option<&Path>) -> i32 {
    crate::log::init(None);
    let root = match std::fs::canonicalize(root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("unlatchd: root {}: {e}", root.display());
            return 1;
        }
    };
    let state = state
        .map(|s| s.to_path_buf())
        .unwrap_or_else(|| default_state_dir(&root));
    let state = match prepare_state(&state) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("unlatchd: state {}: {e}", state.display());
            return 1;
        }
    };
    // Exclusive use of the state dir (a background serve for it would corrupt the index).
    let lock = match lock_file(&state.join("serve.lock")) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("unlatchd: {e}");
            return 1;
        }
    };
    if let Err(e) = acquire_serve_lock(&lock, Duration::from_secs(3)) {
        eprintln!(
            "unlatchd: state dir {} is in use by another unlatchd: {e}",
            state.display()
        );
        return 1;
    }
    let core = match Core::new(&root, &state, Config::from_env()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("unlatchd: {e}");
            return 1;
        }
    };
    {
        let c = core.clone();
        let _ = on_termination(move || {
            c.stop();
            std::process::exit(0);
        });
    }
    let (inp, out) = match (dup_file(0), dup_file(1)) {
        (Ok(i), Ok(o)) => (i, o),
        _ => {
            eprintln!("unlatchd: cannot use stdio");
            return 1;
        }
    };
    let r = crate::session::run(
        core.clone(),
        BufReader::with_capacity(256 * 1024, inp),
        BufWriter::with_capacity(256 * 1024, out),
        Box::new(|| {}),
    );
    core.stop();
    drop(lock);
    match r {
        Ok(()) => 0,
        Err(e) => {
            crate::log!("stdio session: {e}");
            0
        }
    }
}

// ---- serve -----------------------------------------------------------------------------

fn acquire_serve_lock(f: &File, patience: Duration) -> io::Result<()> {
    // Connectors hold serve.lock only for an instant (their liveness probe): retry briefly
    // before concluding another server owns it.
    let t0 = Instant::now();
    loop {
        match sys::flock(f.as_raw_fd(), true, true) {
            Ok(()) => return Ok(()),
            Err(e) if sys::is_would_block(&e) && t0.elapsed() < patience => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Double fork + setsid; stdio → /dev/null, stderr → log; every other fd closed; cwd /.
/// Returns in the grandchild only; the original process exits 0 once the child has forked.
fn daemonize(log: &Path) -> io::Result<()> {
    // SAFETY: single-threaded at this point (called before any thread is spawned).
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        pid => {
            let mut status = 0;
            // SAFETY: reap the intermediate child.
            unsafe { libc::waitpid(pid, &mut status, 0) };
            std::process::exit(0);
        }
    }
    // SAFETY: plain syscalls in the child.
    unsafe {
        if libc::setsid() < 0 {
            libc::_exit(1);
        }
        match libc::fork() {
            -1 => libc::_exit(1),
            0 => {}
            _ => libc::_exit(0),
        }
    }
    let devnull = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let logf = OpenOptions::new().create(true).append(true).open(log)?;
    // SAFETY: dup2 onto the standard fds.
    unsafe {
        libc::dup2(devnull.as_raw_fd(), 0);
        libc::dup2(devnull.as_raw_fd(), 1);
        libc::dup2(logf.as_raw_fd(), 2);
    }
    drop(devnull);
    drop(logf);
    let fds: Vec<i32> = std::fs::read_dir("/proc/self/fd")
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok()))
                .collect()
        })
        .unwrap_or_default();
    for fd in fds {
        if fd > 2 {
            // SAFETY: closing inherited fds (the read_dir fd is already closed).
            unsafe { libc::close(fd) };
        }
    }
    let _ = std::env::set_current_dir("/");
    Ok(())
}

pub fn serve(root: &Path, state: Option<&Path>, foreground: bool) -> i32 {
    let root = match std::fs::canonicalize(root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("unlatchd: root {}: {e}", root.display());
            return 1;
        }
    };
    let state = state
        .map(|s| s.to_path_buf())
        .unwrap_or_else(|| default_state_dir(&root));
    let state = match prepare_state(&state) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("unlatchd: state {}: {e}", state.display());
            return 1;
        }
    };
    let log = std::env::var_os("UNLATCHD_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|| state.join("serve.log"));
    if !foreground {
        if let Err(e) = daemonize(&log) {
            eprintln!("unlatchd: daemonize: {e}");
            return 1;
        }
    }
    crate::log::init(Some(&log));
    let lock = match lock_file(&state.join("serve.lock")) {
        Ok(f) => f,
        Err(e) => {
            crate::log!("serve.lock: {e}");
            return 1;
        }
    };
    if acquire_serve_lock(&lock, Duration::from_secs(3)).is_err() {
        crate::log!("another serve owns {}; exiting", state.display());
        return 0;
    }
    let _ = std::fs::write(state.join("serve.pid"), format!("{}\n", sys::getpid()));
    let _ = std::fs::write(state.join("root"), root.as_os_str().as_encoded_bytes());
    let listener = match bind_listener(&root, &state) {
        Ok(l) => l,
        Err(e) => {
            crate::log!("bind: {e}");
            return 1;
        }
    };
    crate::log!(
        "serve {} (state {}) pid {}",
        root.display(),
        state.display(),
        sys::getpid()
    );
    let core = match Core::new(&root, &state, Config::from_env()) {
        Ok(c) => c,
        Err(e) => {
            crate::log!("core: {e}");
            return 1;
        }
    };
    {
        let c = core.clone();
        let _ = on_termination(move || {
            c.shutdown.store(true, Ordering::Relaxed);
        });
    }
    let _ = listener.set_nonblocking(true);
    let uid = sys::geteuid();
    let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    loop {
        if core.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let idle = conns.load(Ordering::Relaxed) == 0
            && core
                .last_client
                .lock()
                .map(|t| t.elapsed() >= core.cfg.idle_exit)
                .unwrap_or(false);
        if idle {
            crate::log!("no clients for {:?}; exiting", core.cfg.idle_exit);
            break;
        }
        match sys::poll_fds(&[(listener.as_raw_fd(), libc::POLLIN)], 500) {
            Ok(v) if v[0] & libc::POLLIN != 0 => {}
            _ => continue,
        }
        let (stream, _) = match listener.accept() {
            Ok(x) => x,
            Err(_) => continue,
        };
        match sys::peer_cred(stream.as_raw_fd()) {
            Ok((_, peer_uid, _)) if peer_uid == uid => {}
            other => {
                crate::log!("rejecting peer {other:?}");
                continue;
            }
        }
        let _ = stream.set_nonblocking(false);
        let c = core.clone();
        // Idle accounting counts connections, not handshaken sessions, so a client in the
        // middle of its handshake keeps the server alive.
        conns.fetch_add(1, Ordering::Relaxed);
        let n = conns.clone();
        let spawned = std::thread::Builder::new()
            .name("session".into())
            .spawn(move || {
                serve_conn(c.clone(), stream);
                n.fetch_sub(1, Ordering::Relaxed);
                if let Ok(mut t) = c.last_client.lock() {
                    *t = Instant::now();
                }
            });
        if let Err(e) = spawned {
            conns.fetch_sub(1, Ordering::Relaxed);
            crate::log!("spawn session: {e}");
        }
    }
    core.stop();
    let _ = std::fs::remove_file(state.join("serve.pid"));
    drop(lock);
    0
}

/// Bind the per-root name. Taken (by another user — the abstract namespace has no permissions —
/// or by a previous server still exiting): re-roll the nonce and bind the new name, which
/// connectors pick up from the state dir. A squatter cannot learn a name before it is bound.
fn bind_listener(root: &Path, state: &Path) -> io::Result<UnixListener> {
    let mut nonce = read_nonce(state).or_else(|| match new_nonce(state) {
        Ok(n) => Some(n),
        Err(e) => {
            // Degraded (state dir not writable): the nonce-less name, as before nonces.
            crate::log!("{NONCE_FILE}: {e}; using the default socket name");
            None
        }
    });
    let mut tries = 0;
    loop {
        let name = socket_name(root, state, nonce.as_deref());
        match SocketAddr::from_abstract_name(&name).and_then(|a| UnixListener::bind_addr(&a)) {
            Ok(l) => return Ok(l),
            Err(e) if e.raw_os_error() == Some(libc::EADDRINUSE) && tries < 8 => {
                tries += 1;
                crate::log!(
                    "socket name {} is taken; choosing a new one",
                    String::from_utf8_lossy(&name)
                );
                nonce = Some(new_nonce(state)?);
            }
            Err(e) => return Err(e),
        }
    }
}

fn serve_conn(core: Arc<Core>, stream: UnixStream) {
    let (Ok(r), Ok(w), Ok(c)) = (stream.try_clone(), stream.try_clone(), stream.try_clone()) else {
        return;
    };
    drop(stream);
    let _ = crate::session::run(
        core,
        BufReader::with_capacity(256 * 1024, r),
        BufWriter::with_capacity(256 * 1024, w),
        Box::new(move || {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }),
    );
}

// ---- connect ---------------------------------------------------------------------------

fn try_connect(name: &[u8]) -> io::Result<UnixStream> {
    let s = connect_abstract_nonblocking(name)?;
    let (_, uid, _) = sys::peer_cred(s.as_raw_fd())?;
    if uid != sys::geteuid() {
        // Someone else squatted the name (§2(a)9: SO_PEERCRED both ways). Never talk to it;
        // the caller treats the name as unserved (a server it starts moves to a new name).
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("socket owned by uid {uid}"),
        ));
    }
    Ok(s)
}

/// Connect to an abstract unix socket without ever blocking: a name owned by someone who never
/// accepts (a squatter that let its accept queue fill) would otherwise park `connect(2)` in the
/// kernel forever and deny service. A full queue is reported as `PermissionDenied`, the same
/// "not served by us" answer as a foreign owner, so `attach` moves on to a server it starts.
/// The connected stream is switched back to blocking.
fn connect_abstract_nonblocking(name: &[u8]) -> io::Result<UnixStream> {
    // SAFETY: a fresh socket fd, owned by the UnixStream from here on.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a valid socket we just created and nobody else owns.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    // Abstract namespace: sun_path[0] = NUL, then the name (no terminator).
    if name.len() + 1 > addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket name too long",
        ));
    }
    for (dst, src) in addr.sun_path[1..].iter_mut().zip(name) {
        *dst = *src as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + 1 + name.len()) as libc::socklen_t;
    // SAFETY: `addr` is a valid sockaddr_un and `len` covers exactly the bytes we filled in.
    let r = unsafe {
        libc::connect(
            fd,
            (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            len,
        )
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EAGAIN) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "socket name owned by a process that is not accepting",
            ));
        }
        return Err(e);
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn spawn_serve(root: &Path, state: &Path) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe)
        .arg("serve")
        .arg("--root")
        .arg(root)
        .arg("--state")
        .arg(state)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // The first process exits as soon as the daemon has forked away.
    child.wait()?;
    Ok(())
}

/// Attach to (or start) the per-root server and return a connected stream.
pub fn attach(root: &Path, state: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let t0 = Instant::now();
    let mut squatted = false;
    loop {
        // Re-read every round: a starting server may have moved to a new name.
        let name = current_socket_name(root, state);
        match try_connect(&name) {
            Ok(s) => {
                crate::log!("connect: attached after {:?}", t0.elapsed());
                return Ok(s);
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                if !squatted {
                    crate::log!("connect: {e}; not ours, ignoring it");
                    squatted = true;
                }
            }
            Err(_) => {}
        }
        if t0.elapsed() > timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "unlatchd serve did not come up",
            ));
        }
        let serve_lock = lock_file(&state.join("serve.lock"))?;
        match sys::flock(serve_lock.as_raw_fd(), true, true) {
            Ok(()) => {
                // Nobody holds serve.lock → no server. Release at once (a starting server
                // retries briefly), then spawn under spawn.lock.
                drop(serve_lock);
                let spawn_lock = lock_file(&state.join("spawn.lock"))?;
                sys::flock(spawn_lock.as_raw_fd(), true, false)?;
                if let Ok(s) = try_connect(&current_socket_name(root, state)) {
                    return Ok(s);
                }
                let probe = lock_file(&state.join("serve.lock"))?;
                let dead = sys::flock(probe.as_raw_fd(), true, true).is_ok();
                drop(probe);
                if dead {
                    crate::log!("connect: no server; spawning one");
                    spawn_serve(root, state)?;
                    let t1 = Instant::now();
                    while t1.elapsed() < Duration::from_secs(10) {
                        if let Ok(s) = try_connect(&current_socket_name(root, state)) {
                            return Ok(s);
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                drop(spawn_lock);
            }
            Err(e) if sys::is_would_block(&e) => {
                // A server holds the lock but is not accepting yet.
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
}

/// `unlatchd connect --root R [--state DIR]`: attach/spawn and bridge stdio ⇄ socket. Exits as
/// soon as either side closes, so ssh never lingers (T: ssh exits within 1 s).
pub fn connect(root: &Path, state: Option<&Path>) -> i32 {
    crate::log::init(None);
    let root = match std::fs::canonicalize(root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("unlatchd: root {}: {e}", root.display());
            return 1;
        }
    };
    let state = state
        .map(|s| s.to_path_buf())
        .unwrap_or_else(|| default_state_dir(&root));
    let state = match prepare_state(&state) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("unlatchd: state {}: {e}", state.display());
            return 1;
        }
    };
    // A server that was just killed can still accept for an instant (its thread group is
    // tearing down): only bridge once the server's preamble arrived, else attach again.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut stream = match attach(&root, &state, left) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("unlatchd: connect: {e}");
                return 1;
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
        let mut pre = [0u8; unlatch_proto::frame::PREAMBLE_LEN];
        match stream.read_exact(&mut pre) {
            Ok(()) if pre[..8] == unlatch_proto::frame::MAGIC => {
                let _ = stream.set_read_timeout(None);
                if sys::write_all_fd(1, &pre).is_err() {
                    return 0;
                }
                return bridge(stream);
            }
            other => {
                crate::log!("connect: server closed before its preamble ({other:?}); retrying");
                if Instant::now() >= deadline {
                    eprintln!("unlatchd: connect: server keeps closing the connection");
                    return 1;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn bridge(stream: UnixStream) -> i32 {
    let Ok(mut up) = stream.try_clone() else {
        return 1;
    };
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        // SAFETY: fd 0 is ours for the life of the process.
        let mut stdin = unsafe { File::from_raw_fd(0) };
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if up.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        std::mem::forget(stdin);
        let _ = up.shutdown(std::net::Shutdown::Write);
        // The server closes promptly on EOF; never keep ssh around longer than this.
        std::thread::sleep(Duration::from_millis(800));
        std::process::exit(0);
    });
    let mut down = stream;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match down.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if sys::write_all_fd(1, &buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    std::process::exit(0);
}

// ---- status / stop / gc ------------------------------------------------------------------

struct ServerState {
    state: PathBuf,
    root: String,
    /// The `unlatchd serve` holding serve.lock (verified, see [`serve_holder`]).
    pid: Option<i32>,
    /// serve.lock is held (by a serve, a `stdio` session or, for an instant, a connector).
    running: bool,
}

fn inspect(state: &Path) -> ServerState {
    let root = std::fs::read_to_string(state.join("root")).unwrap_or_default();
    let running = match lock_file(&state.join("serve.lock")) {
        Ok(f) => sys::flock(f.as_raw_fd(), true, true).is_err(),
        Err(_) => false,
    };
    let pid = if running { serve_holder(state) } else { None };
    if !running {
        // Left by a serve that died uncleanly: its pid may name another process by now.
        let _ = std::fs::remove_file(state.join("serve.pid"));
    }
    ServerState {
        state: state.to_path_buf(),
        root,
        pid,
        running,
    }
}

/// The pid of the `unlatchd serve` that holds `<state>/serve.lock`, verified from /proc rather
/// than trusted from serve.pid (stale after an unclean exit, and its pid can be reused by an
/// unrelated process): the flock's owner per /proc/locks, which must be a process of ours
/// whose executable is an unlatchd binary, whose argv[1] is `serve`, and which has serve.lock open.
fn serve_holder(state: &Path) -> Option<i32> {
    // /proc/locks is a seq_file read in chunks; a line can be missed while other processes take
    // and release locks between chunks (seen once in 480 stops under 25× CPU oversubscription).
    // Only a miss is retried: a found holder is verified on its own.
    for attempt in 0..5 {
        if let Some(pid) = serve_holder_once(state) {
            return Some(pid);
        }
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    None
}

fn serve_holder_once(state: &Path) -> Option<i32> {
    use std::os::unix::fs::MetadataExt;
    let lock = std::fs::metadata(state.join("serve.lock")).ok()?;
    let locks = std::fs::read_to_string("/proc/locks").ok()?;
    // "1: FLOCK  ADVISORY  WRITE 4242 08:02:1234567 0 EOF"
    let owners = locks.lines().filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        let (kind, pid, file) = (f.get(1)?, f.get(4)?, f.get(5)?);
        let ino: u64 = file.rsplit(':').next()?.parse().ok()?;
        (*kind == "FLOCK" && ino == lock.ino()).then(|| pid.parse::<i32>().ok())?
    });
    owners
        .filter(|&pid| pid > 0 && pid != sys::getpid())
        .find(|&pid| is_serve_holding(pid, (lock.dev(), lock.ino())))
}

fn is_serve_holding(pid: i32, lock: (u64, u64)) -> bool {
    use std::os::unix::fs::MetadataExt;
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let ours = std::fs::metadata(&proc)
        .map(|m| m.uid() == sys::geteuid())
        .unwrap_or(false);
    let exe_ok = std::fs::read_link(proc.join("exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|n| n.starts_with("unlatchd"));
    let serve = std::fs::read(proc.join("cmdline"))
        .map(|c| c.split(|&b| b == 0).nth(1) == Some(&b"serve"[..]))
        .unwrap_or(false);
    let holds = std::fs::read_dir(proc.join("fd"))
        .map(|rd| {
            rd.flatten().any(|e| {
                std::fs::metadata(e.path())
                    .map(|m| (m.dev(), m.ino()) == lock)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    ours && exe_ok && serve && holds
}

fn states(state: Option<&Path>) -> Vec<PathBuf> {
    match state {
        Some(s) => vec![s.to_path_buf()],
        None => std::fs::read_dir(install_dir().join("state"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

pub fn status(state: Option<&Path>) -> i32 {
    let mut any = false;
    for s in states(state) {
        let st = inspect(&s);
        any |= st.running;
        let idx = std::fs::metadata(s.join("index.bin"))
            .map(|m| m.len())
            .unwrap_or(0);
        println!(
            "{}\troot={}\tpid={}\trunning={}\tindex_bytes={}",
            st.state.display(),
            st.root,
            st.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            if st.running { "yes" } else { "no" },
            idx
        );
    }
    if any {
        0
    } else {
        3
    }
}

/// Stop the `unlatchd serve` of each state dir: SIGTERM, then SIGKILL after 5 s — only ever to the
/// verified lock-holding serve, never to whatever pid serve.pid names.
pub fn stop(state: Option<&Path>) -> i32 {
    let mut rc = 0;
    for s in states(state) {
        let st = inspect(&s);
        if !st.running {
            continue;
        }
        let Some(pid) = st.pid else {
            // An `unlatchd stdio` session (or a connector's instant probe) holds it: not a server.
            eprintln!(
                "unlatchd: {} is in use by an unlatchd process that is not a server; not stopping it",
                s.display()
            );
            rc = 1;
            continue;
        };
        let alive = |s: &Path| inspect(s).pid == Some(pid);
        // SAFETY: signalling the verified serve.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let t0 = Instant::now();
        while alive(&s) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        if alive(&s) {
            // SAFETY: as above (re-verified just now).
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let t1 = Instant::now();
            while alive(&s) && t1.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(20));
            }
            rc = 1;
        }
        if alive(&s) {
            eprintln!("unlatchd: {} (pid {pid}) did not stop", s.display());
            rc = 1;
        } else {
            println!("stopped {} (pid {pid})", s.display());
        }
    }
    rc
}

pub fn gc(state: Option<&Path>) -> i32 {
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    let mut in_use: Vec<PathBuf> = Vec::new();
    for s in states(state) {
        let st = inspect(&s);
        if let (true, Some(pid)) = (st.running, st.pid) {
            if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
                in_use.push(exe);
            }
        } else if !st.root.is_empty()
            && !Path::new(&st.root).exists()
            && std::fs::remove_dir_all(&s).is_ok()
        {
            println!(
                "removed state of vanished root {} ({})",
                st.root,
                s.display()
            );
        }
    }
    if let Ok(rd) = std::fs::read_dir(install_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("unlatchd-") {
                continue;
            }
            let canon = std::fs::canonicalize(&p).ok();
            if canon.is_some() && (canon == me || in_use.iter().any(|u| Some(u) == canon.as_ref()))
            {
                continue;
            }
            if std::fs::remove_file(&p).is_ok() {
                println!("removed {}", p.display());
            }
        }
    }
    0
}
