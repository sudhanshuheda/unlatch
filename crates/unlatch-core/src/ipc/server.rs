//! Unix-socket IPC server: 0600 socket, peer-uid check, one reader thread + [`DispInner`] per
//! connection.
//!
//! On macOS production traffic arrives over XPC (review (a)2); this server is the Linux/test
//! transport carrying the identical frames.

use super::dispatch::declares_content;
use super::dispatcher::{lock, DispInner, Handler};
use super::sock::{self, FrameReader};
use crate::{err, Result};
use std::collections::HashMap;
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use unlatch_proto::frame;
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};
use unlatch_proto::ErrorCode;

/// `sizeof(sockaddr_un.sun_path)` minus the NUL: 104 on macOS (the finding that sank the
/// group-container socket path), 108 on Linux.
#[cfg(any(target_os = "linux", target_os = "android"))]
const SUN_PATH_MAX: usize = 107;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SUN_PATH_MAX: usize = 103;

type Conns = Arc<Mutex<HashMap<u64, UnixStream>>>;

pub(crate) struct Server {
    path: PathBuf,
    /// (dev, ino) of the socket we bound, so drop never unlinks a successor's socket.
    ident: (u64, u64),
    stop: Arc<AtomicBool>,
    wake: UnixStream,
    accept: Option<JoinHandle<()>>,
    conns: Conns,
}

fn io_err(what: &str, path: &Path, e: io::Error) -> crate::ProtoError {
    err(ErrorCode::Io, format!("{what} {}: {e}", path.display()))
}

/// Remove a stale socket at `path`; refuse if something is listening or it is not a socket.
fn clear_stale(path: &Path) -> Result<()> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err("stat", path, e)),
    };
    if !md.file_type().is_socket() {
        return Err(err(
            ErrorCode::Exists,
            format!("{} exists and is not a socket", path.display()),
        ));
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(err(
            ErrorCode::Exists,
            format!("another engine is serving {}", path.display()),
        )),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            std::fs::remove_file(path).map_err(|e| io_err("remove stale socket", path, e))
        }
        Err(e) => Err(io_err("probe existing socket", path, e)),
    }
}

pub(crate) fn bind(handler: Handler, path: &Path) -> Result<Server> {
    if path.as_os_str().len() > SUN_PATH_MAX {
        return Err(err(
            ErrorCode::InvalidName,
            format!(
                "socket path is {} bytes, the limit is {SUN_PATH_MAX}: {}",
                path.as_os_str().len(),
                path.display()
            ),
        ));
    }
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file = path
        .file_name()
        .ok_or_else(|| err(ErrorCode::InvalidName, "socket path has no file name"))?;
    clear_stale(path)?;

    // Bind under a private temporary name, restrict it, then rename into place: the socket is
    // never reachable at its public path with the default (umask-derived) mode, and no
    // process-wide umask change is needed.
    let tmp = dir.join(format!(
        ".{}.{}.{:x}.tmp",
        file.to_string_lossy(),
        std::process::id(),
        rand::random::<u32>()
    ));
    if tmp.as_os_str().len() > SUN_PATH_MAX {
        return Err(err(
            ErrorCode::InvalidName,
            format!("socket directory path too long: {}", dir.display()),
        ));
    }
    let listener = UnixListener::bind(&tmp).map_err(|e| io_err("bind", &tmp, e))?;
    let publish = || -> io::Result<(u64, u64)> {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, path)?;
        let md = std::fs::symlink_metadata(path)?;
        Ok((md.dev(), md.ino()))
    };
    let ident = match publish() {
        Ok(id) => id,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_err("publish socket", path, e));
        }
    };
    listener
        .set_nonblocking(true)
        .map_err(|e| io_err("configure", path, e))?;

    let (wake, wake_rx) = UnixStream::pair().map_err(|e| io_err("wake pipe for", path, e))?;
    let stop = Arc::new(AtomicBool::new(false));
    let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
    let accept = {
        let stop = Arc::clone(&stop);
        let conns = Arc::clone(&conns);
        std::thread::Builder::new()
            .name("unlatch-ipc-accept".into())
            .spawn(move || accept_loop(listener, wake_rx, stop, handler, conns))
            .map_err(|e| io_err("spawn accept thread for", path, e))?
    };
    Ok(Server {
        path: path.to_path_buf(),
        ident,
        stop,
        wake,
        accept: Some(accept),
        conns,
    })
}

fn poll_readable(fds: &[i32]) -> io::Result<Vec<bool>> {
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    loop {
        // SAFETY: pfds is a live array of the given length.
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(pfds.iter().map(|p| p.revents != 0).collect());
    }
}

fn accept_loop(
    listener: UnixListener,
    wake: UnixStream,
    stop: Arc<AtomicBool>,
    handler: Handler,
    conns: Conns,
) {
    let next_id = AtomicU64::new(1);
    let me = sock::euid();
    while !stop.load(Ordering::SeqCst) {
        let ready = match poll_readable(&[listener.as_raw_fd(), wake.as_raw_fd()]) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("ipc: poll failed, server stopping: {e}");
                return;
            }
        };
        if ready[1] || stop.load(Ordering::SeqCst) {
            return;
        }
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => {
                // EMFILE and friends: keep serving existing connections, try again later.
                tracing::warn!("ipc: accept failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
        };
        // Accepted sockets inherit O_NONBLOCK on BSD-derived kernels.
        if let Err(e) = stream
            .set_nonblocking(false)
            .and_then(|()| sock::prepare(&stream))
        {
            tracing::warn!("ipc: cannot configure connection: {e}");
            continue;
        }
        match sock::peer_uid(&stream) {
            Ok(uid) if uid == me => {}
            Ok(uid) => {
                tracing::warn!(uid, "ipc: refusing connection from another user");
                continue;
            }
            Err(e) => {
                tracing::warn!("ipc: cannot read peer credentials: {e}");
                continue;
            }
        }
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let handler = Arc::clone(&handler);
        let conns2 = Arc::clone(&conns);
        let registered = stream.try_clone().map(|c| lock(&conns).insert(id, c));
        if let Err(e) = registered {
            tracing::warn!("ipc: cannot track connection: {e}");
            continue;
        }
        let spawned = std::thread::Builder::new()
            .name("unlatch-ipc-conn".into())
            .spawn(move || {
                serve_conn(stream, handler);
                lock(&conns2).remove(&id);
            });
        if let Err(e) = spawned {
            tracing::warn!("ipc: cannot spawn connection thread: {e}");
            if let Some(s) = lock(&conns).remove(&id) {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
    }
}

/// Serve one connection until EOF, a protocol violation, or a deliberate close.
pub(crate) fn serve_conn(stream: UnixStream, handler: Handler) {
    let (writer, closer) = match (stream.try_clone(), stream.try_clone()) {
        (Ok(w), Ok(c)) => (w, c),
        (Err(e), _) | (_, Err(e)) => {
            tracing::warn!("ipc: cannot clone connection: {e}");
            return;
        }
    };
    let send = move |f: IpcFrame<IpcResponse>| {
        // Local socket: compression would only cost CPU.
        let res = frame::encode(&f, false)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
            .and_then(|bytes| sock::send_frame(&writer, &bytes, None));
        if let Err(e) = res {
            tracing::debug!(call = f.call, "ipc: reply not delivered: {e}");
            let _ = writer.shutdown(Shutdown::Both);
        }
    };
    let on_close = move || {
        let _ = closer.shutdown(Shutdown::Both);
    };
    let disp = DispInner::new(handler, Box::new(send), Some(Box::new(on_close)));
    let mut reader = FrameReader::new(stream);
    loop {
        match reader.next() {
            Ok(Some((body, fd))) => match frame::decode_body::<IpcFrame<IpcRequest>>(&body) {
                Ok(req) => {
                    let fd = if declares_content(&req.msg) { fd } else { None };
                    disp.submit(req, fd);
                }
                Err(e) => {
                    // No call id to answer: the stream is unusable from here on.
                    tracing::warn!("ipc: undecodable frame, closing connection: {e}");
                    break;
                }
            },
            Ok(None) => break,
            Err(e) => {
                if e.kind() != io::ErrorKind::ConnectionReset {
                    tracing::debug!("ipc: connection read failed: {e}");
                }
                break;
            }
        }
    }
    disp.close();
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = (&self.wake).write_all(&[1]);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
        for (_, c) in lock(&self.conns).drain() {
            let _ = c.shutdown(Shutdown::Both);
        }
        if let Ok(md) = std::fs::symlink_metadata(&self.path) {
            if (md.dev(), md.ino()) == self.ident {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}
