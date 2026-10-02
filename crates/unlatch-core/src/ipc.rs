//! IPC: dispatcher (shared by the unix-socket server here and the macOS XPC bridge in
//! `unlatch-ffi`), unix-socket server, and client (used by fpsim, the CLI and tests).
//!
//! Frames: `unlatch_proto::frame` encoding of `IpcFrame<IpcRequest>` / `IpcFrame<IpcResponse>`.
//! File descriptors travel via `SCM_RIGHTS` on the unix socket (one fd, attached to the frame
//! whose request has `has_content: true`).
//!
//! Guarantees (all transports):
//! * every request frame gets exactly one final reply with its `call` id, preceded by any number
//!   of `Progress` frames; replies to different calls may be interleaved and out of order;
//! * `Cancel { call }` is answered with `Ok` and makes the target's final reply
//!   `Error { code: Cancelled }` immediately, while the engine op observes its [`CancelToken`];
//! * a closed connection cancels every call it had in flight (MQ-073: an invalidated connection
//!   means "cancel my calls", not "engine gone");
//! * metadata calls run on their own worker pool and never queue behind transfers;
//! * `Hello { proto, domain }` is answered `Error { code: Protocol }` unless `proto` is
//!   [`unlatch_proto::PROTO_VERSION`] and `domain` is this engine's [`Engine::name`];
//! * `Fetch`, and `Create`/`Modify` that upload content, stream `Progress { done, total }`
//!   (coalesced to ≤ 1 per 50 ms plus completion) and pass the call's [`CancelToken`] to the
//!   engine ([`Engine::fetch`], [`Engine::create_with`], [`Engine::modify_with`]).
//!
//! Fault injection (`UNLATCH_FAULT=die_before_ipc_reply:<create|modify|delete|fetch>`): the op runs
//! to completion, then its reply is dropped and the connection closed (the unix-socket server
//! closes the socket; an XPC host learns it through [`Dispatcher::with_close`]).

mod client;
mod dispatch;
mod dispatcher;
mod server;
mod sock;
#[cfg(test)]
mod tests;

use crate::{CancelToken, Engine, Result};
use dispatch::dispatch_api;
use dispatcher::{DispInner, Handler};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};

/// Handle one request against `engine`, calling `reply` zero or more times with `Progress` and
/// exactly once with the final response (same `call`). Blocking; run on a worker thread.
/// Cancellation: `IpcRequest::Cancel { call }` is handled by [`Dispatcher`], not here.
///
/// With `die_before_ipc_reply:<kind>` armed, the final reply is withheld (callers that own a
/// connection should use [`Dispatcher`], which also closes it).
pub fn dispatch(
    engine: &Engine,
    req: IpcFrame<IpcRequest>,
    fd: Option<OwnedFd>,
    reply: &mut dyn FnMut(IpcFrame<IpcResponse>),
) {
    let _ = dispatch_api(
        engine,
        req,
        fd,
        reply,
        &CancelToken::new(),
        &crate::engine::fault,
    );
}

fn engine_handler(engine: Engine) -> Handler {
    Arc::new(move |req, fd, reply, cancel| {
        dispatch_api(&engine, req, fd, reply, cancel, &crate::engine::fault)
    })
}

/// Multiplexing dispatcher for one connection: runs calls concurrently on a bounded worker pool,
/// tracks CancelTokens per call id, and serializes replies through `send`.
/// Used by the unix-socket server and by the FFI XPC bridge (one per XPC connection).
///
/// Dropping it is "the connection is gone": every in-flight call is cancelled and no further
/// frames are sent.
pub struct Dispatcher {
    inner: Arc<DispInner>,
}

impl Dispatcher {
    pub fn new(
        engine: Engine,
        send: Box<dyn Fn(IpcFrame<IpcResponse>) + Send + Sync>,
    ) -> Dispatcher {
        Dispatcher::with_handler(engine_handler(engine), send, None)
    }

    /// Like [`Dispatcher::new`], plus `on_close`, which runs **once** when the connection is
    /// shut: when the dispatcher closes it itself (fault injection `die_before_ipc_reply:<kind>`
    /// — the op ran, its reply is withheld) or when the dispatcher is dropped. After it has run
    /// `send` is never called again. A host that owns the transport (XPC) invalidates the
    /// connection in `on_close`, which is what a killed agent looks like to the extension
    /// (MQ-073). It runs on a dispatcher worker thread (fault) or on the thread dropping the
    /// dispatcher.
    pub fn with_close(
        engine: Engine,
        send: Box<dyn Fn(IpcFrame<IpcResponse>) + Send + Sync>,
        on_close: Box<dyn Fn() + Send + Sync>,
    ) -> Dispatcher {
        Dispatcher::with_handler(engine_handler(engine), send, Some(on_close))
    }

    /// Dispatcher over an arbitrary handler (tests, alternative engines). `on_close` runs once
    /// when the dispatcher shuts the connection (fault injection) or is dropped.
    pub(crate) fn with_handler(
        handler: Handler,
        send: Box<dyn Fn(IpcFrame<IpcResponse>) + Send + Sync>,
        on_close: Option<Box<dyn Fn() + Send + Sync>>,
    ) -> Dispatcher {
        Dispatcher {
            inner: DispInner::new(handler, send, on_close),
        }
    }

    /// Submit one decoded request frame (non-blocking).
    pub fn submit(&self, req: IpcFrame<IpcRequest>, fd: Option<OwnedFd>) {
        self.inner.submit(req, fd)
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        self.inner.close();
    }
}

/// Running unix-socket server; dropping it stops accepting, closes live connections (cancelling
/// their calls) and removes the socket file (only if it is still the one this server bound).
pub struct IpcServerHandle {
    _server: server::Server,
}

pub(crate) fn serve(engine: Engine, socket_path: &Path) -> Result<IpcServerHandle> {
    serve_with_handler(engine_handler(engine), socket_path)
}

pub(crate) fn serve_with_handler(handler: Handler, socket_path: &Path) -> Result<IpcServerHandle> {
    server::bind(handler, socket_path).map(|s| IpcServerHandle { _server: s })
}

/// Blocking IPC client over one unix-socket connection. Thread-safe: concurrent `call`s are
/// multiplexed by call id.
pub struct IpcClient {
    inner: client::Client,
}

impl IpcClient {
    /// Connect and perform `Hello`.
    pub fn connect(socket: &Path, domain: &str, timeout: Duration) -> Result<IpcClient> {
        client::Client::connect(socket, domain, timeout).map(|inner| IpcClient { inner })
    }

    /// Send one request (optionally with an fd), wait for its final response (progress frames
    /// are passed to `progress`). `IpcResponse::Error` is returned as `Ok`; transport failures
    /// are `Err(Offline)`.
    ///
    /// An fd must be attached exactly when the request sets `has_content` (else `Err(Protocol)`).
    pub fn call(
        &self,
        req: IpcRequest,
        fd: Option<OwnedFd>,
        progress: Option<&dyn Fn(u64, u64)>,
    ) -> Result<IpcResponse> {
        self.inner.call_inner(req, fd, progress, None)
    }

    /// Returns the call id `call` will use next (so a test can `Cancel` it from another thread).
    pub fn next_call_id(&self) -> u64 {
        self.inner.next_call_id()
    }
}
