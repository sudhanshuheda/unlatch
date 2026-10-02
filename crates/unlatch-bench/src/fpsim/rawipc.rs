//! A minimal, frame-level implementation of the IPC transport (unix socket, `IpcFrame` postcard
//! frames, one `SCM_RIGHTS` fd attached to the first byte of the frame that carries content).
//!
//! It exists so fpsim's own tests exercise the real codec and fd passing against the scripted
//! engine while `unlatch_core::ipc` is being written, and so the scripted engine can be served to
//! any IPC client. It is deliberately tiny: one call in flight per client connection.

use super::backend::Backend;
use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr};
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use unlatch_proto::frame::{decode_body, encode, MAX_FRAME};
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};
use unlatch_proto::{ErrorCode, ProtoError, PROTO_VERSION};

fn nix_io(e: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

/// Read one frame body plus the fd that arrived with its first bytes. `Ok(None)` on clean EOF.
pub fn recv_frame(sock: &UnixStream) -> io::Result<Option<(Vec<u8>, Option<OwnedFd>)>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    let mut fd: Option<OwnedFd> = None;
    while got < len.len() {
        let mut cmsg = nix::cmsg_space!([RawFd; 1]);
        let (n, fds) = {
            let mut iov = [IoSliceMut::new(&mut len[got..])];
            let msg = recvmsg::<UnixAddr>(
                sock.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg),
                MsgFlags::MSG_CMSG_CLOEXEC,
            )
            .map_err(nix_io)?;
            let mut fds: Vec<RawFd> = Vec::new();
            for c in msg.cmsgs().map_err(nix_io)? {
                if let ControlMessageOwned::ScmRights(v) = c {
                    fds.extend(v);
                }
            }
            (msg.bytes, fds)
        };
        for raw in fds {
            // SAFETY: SCM_RIGHTS just installed `raw` in this process and nothing else owns it.
            let owned = unsafe { OwnedFd::from_raw_fd(raw) };
            if fd.is_none() {
                fd = Some(owned);
            }
        }
        if n == 0 {
            return if got == 0 {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        got += n;
    }
    let n = u32::from_le_bytes(len) as usize;
    if n == 0 || n > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad frame length {n}"),
        ));
    }
    let mut body = vec![0u8; n];
    (&*sock).read_exact(&mut body)?;
    Ok(Some((body, fd)))
}

/// Write one encoded frame, attaching `fd` to its first byte.
pub fn send_frame(sock: &UnixStream, frame: &[u8], fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
    let Some(fd) = fd else {
        return (&*sock).write_all(frame);
    };
    let fds = [fd.as_raw_fd()];
    let cmsg = [ControlMessage::ScmRights(&fds)];
    let iov = [IoSlice::new(frame)];
    let n = sendmsg::<UnixAddr>(sock.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)
        .map_err(nix_io)?;
    (&*sock).write_all(&frame[n..])
}

/// A running scripted IPC server. Dropping it stops accepting new connections.
pub struct RawServer {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl RawServer {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RawServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the stop flag.
        let _ = UnixStream::connect(&self.path);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serve the IPC protocol on `path`; each connection gets a backend from `make`. A backend
/// transport error closes the connection without a reply (`die_before_ipc_reply`).
pub fn serve<B, F>(path: &Path, make: F) -> io::Result<RawServer>
where
    B: Backend + 'static,
    F: Fn() -> B + Send + 'static,
{
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let accept = std::thread::Builder::new()
        .name("rawipc-accept".into())
        .spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let backend = make();
                let _ = std::thread::Builder::new()
                    .name("rawipc-conn".into())
                    .spawn(move || serve_conn(conn, backend));
            }
        })?;
    Ok(RawServer {
        path: path.to_path_buf(),
        stop,
        accept: Some(accept),
    })
}

fn serve_conn<B: Backend>(conn: UnixStream, mut backend: B) {
    while let Ok(Some((body, fd))) = recv_frame(&conn) {
        let Ok(frame) = decode_body::<IpcFrame<IpcRequest>>(&body) else {
            return;
        };
        let Ok(resp) = backend.call(frame.msg, fd) else {
            return;
        };
        let Ok(bytes) = encode(
            &IpcFrame {
                call: frame.call,
                msg: resp,
            },
            false,
        ) else {
            return;
        };
        if send_frame(&conn, &bytes, None).is_err() {
            return;
        }
    }
}

/// Blocking client over the raw transport; reconnects (and re-sends `Hello`) after a transport
/// failure, like a freshly launched extension instance (MQ-003).
pub struct RawIpcClient {
    path: PathBuf,
    domain: String,
    timeout: Duration,
    conn: Option<UnixStream>,
    next_call: u64,
}

impl RawIpcClient {
    pub fn new(path: &Path, domain: &str, timeout: Duration) -> RawIpcClient {
        RawIpcClient {
            path: path.to_path_buf(),
            domain: domain.to_string(),
            timeout,
            conn: None,
            next_call: 1,
        }
    }

    fn roundtrip(&mut self, req: IpcRequest, fd: Option<OwnedFd>) -> io::Result<IpcResponse> {
        if self.conn.is_none() {
            let c = UnixStream::connect(&self.path)?;
            c.set_read_timeout(Some(self.timeout))?;
            self.conn = Some(c);
            let hello = IpcRequest::Hello {
                proto: PROTO_VERSION,
                domain: self.domain.clone(),
            };
            match self.exchange(hello, None)? {
                IpcResponse::Hello { .. } => {}
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("hello: {other:?}"),
                    ))
                }
            }
        }
        self.exchange(req, fd)
    }

    fn exchange(&mut self, req: IpcRequest, fd: Option<OwnedFd>) -> io::Result<IpcResponse> {
        let call = self.next_call;
        self.next_call += 1;
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
        let bytes = encode(&IpcFrame { call, msg: req }, false)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        send_frame(conn, &bytes, fd.as_ref().map(|f| f.as_fd()))?;
        loop {
            let Some((body, _)) = recv_frame(conn)? else {
                return Err(io::ErrorKind::UnexpectedEof.into());
            };
            let frame: IpcFrame<IpcResponse> = decode_body(&body)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            if frame.call != call || matches!(frame.msg, IpcResponse::Progress { .. }) {
                continue;
            }
            return Ok(frame.msg);
        }
    }
}

impl Backend for RawIpcClient {
    fn call(
        &mut self,
        req: IpcRequest,
        content: Option<OwnedFd>,
    ) -> Result<IpcResponse, ProtoError> {
        match self.roundtrip(req, content) {
            Ok(r) => Ok(r),
            Err(e) => {
                self.conn = None;
                Err(ProtoError::new(
                    ErrorCode::Offline,
                    format!("ipc transport: {e}"),
                ))
            }
        }
    }
}
