//! How fpsim reaches "the provider": exactly the IPC calls the Swift shim forwards.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use unlatch_core::ipc::IpcClient;
use unlatch_core::{EngineEvent, EventHandler};
use unlatch_proto::ipc::{IpcRequest, IpcResponse};
use unlatch_proto::{ErrorCode, ProtoError};

/// One IPC hop. `Ok(IpcResponse::Error { .. })` is a provider-level error (what the extension
/// would return to fileproviderd); `Err(_)` is a transport failure (the XPC connection
/// invalidated — MQ-073: "cancel my calls", never "engine gone").
pub trait Backend: Send {
    fn call(
        &mut self,
        req: IpcRequest,
        content: Option<OwnedFd>,
    ) -> Result<IpcResponse, ProtoError>;
}

impl<B: Backend + ?Sized> Backend for Box<B> {
    fn call(
        &mut self,
        req: IpcRequest,
        content: Option<OwnedFd>,
    ) -> Result<IpcResponse, ProtoError> {
        (**self).call(req, content)
    }
}

/// The production path: [`unlatch_core::ipc::IpcClient`] over the engine's unix socket.
///
/// A transport failure drops the client; the next call connects afresh, like the fresh extension
/// instance fileproviderd launches per signal and per retry (MQ-003).
pub struct IpcBackend {
    socket: PathBuf,
    domain: String,
    timeout: Duration,
    client: Option<IpcClient>,
}

impl IpcBackend {
    pub fn new(socket: &Path, domain: &str, timeout: Duration) -> IpcBackend {
        IpcBackend {
            socket: socket.to_path_buf(),
            domain: domain.to_string(),
            timeout,
            client: None,
        }
    }

    /// Forget the current connection (simulates fileproviderd killing the extension).
    pub fn disconnect(&mut self) {
        self.client = None;
    }
}

impl Backend for IpcBackend {
    fn call(
        &mut self,
        req: IpcRequest,
        content: Option<OwnedFd>,
    ) -> Result<IpcResponse, ProtoError> {
        if self.client.is_none() {
            self.client = Some(IpcClient::connect(
                &self.socket,
                &self.domain,
                self.timeout,
            )?);
        }
        let Some(client) = self.client.as_ref() else {
            return Err(ProtoError::new(ErrorCode::Offline, "no ipc client"));
        };
        match client.call(req, content, None) {
            Ok(r) => Ok(r),
            Err(e) => {
                self.client = None;
                Err(e)
            }
        }
    }
}

/// An [`EventHandler`] for `Engine::start` plus the receiving end fpsim drains. This is the
/// stand-in for the host app turning engine events into `signalEnumerator` /
/// `signalErrorResolved` / `reimportItems` calls.
pub fn event_channel() -> (EventHandler, Receiver<EngineEvent>) {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let handler: EventHandler = Arc::new(move |ev: EngineEvent| {
        if let Ok(tx) = tx.lock() {
            // A closed receiver only means the simulator is gone.
            let _ = tx.send(ev);
        }
    });
    (handler, rx)
}

/// Short, stable label of a request for logs and call counters.
pub fn request_kind(req: &IpcRequest) -> &'static str {
    match req {
        IpcRequest::Hello { .. } => "hello",
        IpcRequest::Item { .. } => "item",
        IpcRequest::Enumerate { .. } => "enumerate",
        IpcRequest::CurrentAnchor => "anchor",
        IpcRequest::ChangesSince { .. } => "changes",
        IpcRequest::MaterializedChanged { .. } => "materialized",
        IpcRequest::Fetch { .. } => "fetch",
        IpcRequest::Create { .. } => "create",
        IpcRequest::Modify { .. } => "modify",
        IpcRequest::Delete { .. } => "delete",
        IpcRequest::Cancel { .. } => "cancel",
        IpcRequest::Status => "status",
        IpcRequest::ConfirmPaused { .. } => "confirm_paused",
    }
}
