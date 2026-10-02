//! Blocking, thread-safe IPC client multiplexed by call id.

use super::dispatch::declares_content;
use super::dispatcher::lock;
use super::sock;
use crate::{err, Result};
use std::collections::HashMap;
use std::net::Shutdown;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unlatch_proto::frame;
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};
use unlatch_proto::{ErrorCode, ProtoError, PROTO_VERSION};

enum Event {
    Progress(u64, u64),
    Final(Box<IpcResponse>),
}

#[derive(Default)]
struct Pending {
    /// `None` once the connection is gone: new calls fail fast with `Offline`.
    waiters: Option<HashMap<u64, Sender<Event>>>,
    /// Why the connection died (surfaced in the `Offline` message).
    reason: String,
}

pub(crate) struct Client {
    sock: UnixStream,
    send_lock: Mutex<()>,
    pending: Arc<Mutex<Pending>>,
    next: AtomicU64,
}

fn offline(msg: impl Into<String>) -> ProtoError {
    err(ErrorCode::Offline, msg)
}

impl Client {
    pub(crate) fn connect(path: &Path, domain: &str, timeout: Duration) -> Result<Client> {
        let sock = UnixStream::connect(path)
            .map_err(|e| offline(format!("connect {}: {e}", path.display())))?;
        sock::prepare(&sock).map_err(|e| offline(format!("configure socket: {e}")))?;
        let reader = sock
            .try_clone()
            .map_err(|e| offline(format!("clone socket: {e}")))?;
        let pending = Arc::new(Mutex::new(Pending {
            waiters: Some(HashMap::new()),
            reason: String::new(),
        }));
        {
            let pending = Arc::clone(&pending);
            std::thread::Builder::new()
                .name("unlatch-ipc-client".into())
                .spawn(move || read_loop(reader, pending))
                .map_err(|e| offline(format!("spawn reader: {e}")))?;
        }
        let client = Client {
            sock,
            send_lock: Mutex::new(()),
            pending,
            next: AtomicU64::new(1),
        };
        let hello = IpcRequest::Hello {
            proto: PROTO_VERSION,
            domain: domain.to_string(),
        };
        match client.call_inner(hello, None, None, Some(timeout))? {
            IpcResponse::Hello { proto, .. } if proto == PROTO_VERSION => Ok(client),
            IpcResponse::Hello { proto, .. } => Err(err(
                ErrorCode::Protocol,
                format!("engine speaks ipc protocol {proto}, client {PROTO_VERSION}"),
            )),
            IpcResponse::Error { code, msg, .. } => Err(ProtoError::new(code, msg)),
            other => Err(err(
                ErrorCode::Protocol,
                format!("unexpected reply to Hello: {other:?}"),
            )),
        }
    }

    pub(crate) fn next_call_id(&self) -> u64 {
        self.next.load(Ordering::SeqCst)
    }

    pub(crate) fn call_inner(
        &self,
        req: IpcRequest,
        fd: Option<OwnedFd>,
        progress: Option<&dyn Fn(u64, u64)>,
        timeout: Option<Duration>,
    ) -> Result<IpcResponse> {
        if declares_content(&req) != fd.is_some() {
            return Err(err(
                ErrorCode::Protocol,
                "a content fd must be attached exactly when the request sets has_content",
            ));
        }
        let call = self.next.fetch_add(1, Ordering::SeqCst);
        let rx = self.register(call)?;
        let bytes = frame::encode(&IpcFrame { call, msg: req }, false).map_err(|e| {
            self.unregister(call);
            err(ErrorCode::Protocol, format!("encode: {e}"))
        })?;
        let sent = {
            let _g = lock(&self.send_lock);
            sock::send_frame(&self.sock, &bytes, fd.as_ref().map(|f| f.as_fd()))
        };
        // Our copy of the fd is no longer needed once the kernel holds a reference.
        drop(fd);
        if let Err(e) = sent {
            self.unregister(call);
            let _ = self.sock.shutdown(Shutdown::Both);
            return Err(offline(format!("send: {e}")));
        }
        self.wait(call, rx, progress, timeout)
    }

    fn register(&self, call: u64) -> Result<Receiver<Event>> {
        let (tx, rx) = mpsc::channel();
        let mut p = lock(&self.pending);
        let reason = p.reason.clone();
        match p.waiters.as_mut() {
            Some(w) => {
                w.insert(call, tx);
                Ok(rx)
            }
            None => Err(offline(format!("connection closed: {reason}"))),
        }
    }

    fn unregister(&self, call: u64) {
        if let Some(w) = lock(&self.pending).waiters.as_mut() {
            w.remove(&call);
        }
    }

    fn wait(
        &self,
        call: u64,
        rx: Receiver<Event>,
        progress: Option<&dyn Fn(u64, u64)>,
        timeout: Option<Duration>,
    ) -> Result<IpcResponse> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            let ev = match deadline {
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
            };
            match ev {
                Ok(Event::Progress(done, total)) => {
                    if let Some(p) = progress {
                        p(done, total);
                    }
                }
                Ok(Event::Final(resp)) => return Ok(*resp),
                Err(RecvTimeoutError::Timeout) => {
                    self.unregister(call);
                    return Err(err(ErrorCode::Timeout, "no reply from engine"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let reason = lock(&self.pending).reason.clone();
                    return Err(offline(format!("connection closed: {reason}")));
                }
            }
        }
    }
}

fn read_loop(sock: UnixStream, pending: Arc<Mutex<Pending>>) {
    let mut r = &sock;
    let reason = loop {
        let body = match frame::read_body_blocking(&mut r) {
            Ok(Some(b)) => b,
            Ok(None) => break "engine closed the connection".to_string(),
            Err(e) => break format!("read: {e}"),
        };
        let f: IpcFrame<IpcResponse> = match frame::decode_body(&body) {
            Ok(f) => f,
            Err(e) => break format!("undecodable reply: {e}"),
        };
        let mut p = lock(&pending);
        let Some(waiters) = p.waiters.as_mut() else {
            break "closed".into();
        };
        match f.msg {
            IpcResponse::Progress { done, total } => {
                if let Some(tx) = waiters.get(&f.call) {
                    let _ = tx.send(Event::Progress(done, total));
                }
            }
            msg => {
                // Late frames for calls that timed out locally are dropped here.
                if let Some(tx) = waiters.remove(&f.call) {
                    let _ = tx.send(Event::Final(Box::new(msg)));
                }
            }
        }
    };
    let _ = sock.shutdown(Shutdown::Both);
    let mut p = lock(&pending);
    p.reason = reason;
    // Dropping every Sender wakes each waiter with Disconnected → Err(Offline).
    p.waiters = None;
}

impl Drop for Client {
    fn drop(&mut self) {
        // Wakes the reader thread, which then fails any (impossible here) remaining waiters.
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}
