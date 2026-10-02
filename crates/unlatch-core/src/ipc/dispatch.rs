//! Mapping of every [`IpcRequest`] onto the engine's public API.
//!
//! Written against the [`EngineApi`] trait (implemented by [`Engine`]) so the mapping itself —
//! error translation, content fds, progress frames, fault injection — is unit-tested with a mock
//! while the engine internals are developed separately.

use crate::{
    CancelToken, Changes, CreateRequest, Engine, Fetched, Modified, ModifyRequest, Page, Result,
};
use std::cell::RefCell;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::{Duration, Instant};
use unlatch_proto::ipc::{EngineStatus, IpcFrame, IpcItem, IpcRequest, IpcResponse};
use unlatch_proto::{BaseVersion, ErrorCode, ItemId, ProtoError, PROTO_VERSION};

/// What the connection should do after a handler returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// The handler replied (or the dispatcher will synthesize a missing final reply).
    Continue,
    /// Drop the final reply and close the connection (fault injection: `die_before_ipc_reply`).
    CloseConnection,
}

/// Progress frames are coalesced to at most one per this interval (plus the final 100% one), so
/// a fast local clone does not flood the extension with frames.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// The subset of [`Engine`] the dispatcher drives.
pub(crate) trait EngineApi: Send + Sync + 'static {
    /// Domain name; `Hello { domain }` must match it.
    fn name(&self) -> &str;
    fn item(&self, id: ItemId) -> Result<IpcItem>;
    fn list(
        &self,
        container: ItemId,
        cursor: Option<&[u8]>,
        limit: u32,
        viewer: bool,
    ) -> Result<Page>;
    fn anchor(&self) -> Vec<u8>;
    fn changes_since(&self, anchor: &[u8], limit: u32) -> Result<Changes>;
    fn materialized_changed(&self, added: &[ItemId], removed: &[ItemId], full: bool) -> Result<()>;
    fn fetch(
        &self,
        id: ItemId,
        version: Option<u64>,
        dest_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Fetched>;
    fn create_with(
        &self,
        req: CreateRequest,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Modified>;
    fn modify_with(
        &self,
        id: ItemId,
        base: BaseVersion,
        req: ModifyRequest,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Modified>;
    fn delete(&self, id: ItemId, base: BaseVersion, recursive: bool) -> Result<()>;
    fn status(&self) -> EngineStatus;
    fn confirm_paused(&self, apply: bool) -> Result<()>;
}

impl EngineApi for Engine {
    fn name(&self) -> &str {
        Engine::name(self)
    }
    fn item(&self, id: ItemId) -> Result<IpcItem> {
        Engine::item(self, id)
    }
    fn list(
        &self,
        container: ItemId,
        cursor: Option<&[u8]>,
        limit: u32,
        viewer: bool,
    ) -> Result<Page> {
        Engine::list(self, container, cursor, limit, viewer)
    }
    fn anchor(&self) -> Vec<u8> {
        Engine::anchor(self)
    }
    fn changes_since(&self, anchor: &[u8], limit: u32) -> Result<Changes> {
        Engine::changes_since(self, anchor, limit)
    }
    fn materialized_changed(&self, added: &[ItemId], removed: &[ItemId], full: bool) -> Result<()> {
        Engine::materialized_changed(self, added, removed, full)
    }
    fn fetch(
        &self,
        id: ItemId,
        version: Option<u64>,
        dest_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Fetched> {
        Engine::fetch(self, id, version, dest_dir, progress, cancel)
    }
    fn create_with(
        &self,
        req: CreateRequest,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Modified> {
        Engine::create_with(self, req, progress, cancel)
    }
    fn modify_with(
        &self,
        id: ItemId,
        base: BaseVersion,
        req: ModifyRequest,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Modified> {
        Engine::modify_with(self, id, base, req, progress, cancel)
    }
    fn delete(&self, id: ItemId, base: BaseVersion, recursive: bool) -> Result<()> {
        Engine::delete(self, id, base, recursive)
    }
    fn status(&self) -> EngineStatus {
        Engine::status(self)
    }
    fn confirm_paused(&self, apply: bool) -> Result<()> {
        Engine::confirm_paused(self, apply)
    }
}

/// Does this request carry a content fd out of band?
pub(crate) fn declares_content(req: &IpcRequest) -> bool {
    matches!(
        req,
        IpcRequest::Create {
            has_content: true,
            ..
        } | IpcRequest::Modify {
            has_content: true,
            ..
        }
    )
}

/// Fault-injection kind (`die_before_ipc_reply:<kind>`) of a request, if it has one.
fn fault_kind(req: &IpcRequest) -> Option<&'static str> {
    match req {
        IpcRequest::Create { .. } => Some("create"),
        IpcRequest::Modify { .. } => Some("modify"),
        IpcRequest::Delete { .. } => Some("delete"),
        IpcRequest::Fetch { .. } => Some("fetch"),
        _ => None,
    }
}

pub(crate) fn error_response(e: ProtoError, current: Option<IpcItem>) -> IpcResponse {
    IpcResponse::Error {
        code: e.code,
        msg: e.msg,
        current,
    }
}

fn done(m: Modified) -> IpcResponse {
    IpcResponse::Done {
        item: m.item,
        still_pending: m.still_pending,
        should_fetch_content: m.should_fetch_content,
        conflict_copy: m.conflict_copy,
    }
}

fn missing_fd() -> IpcResponse {
    error_response(
        ProtoError::new(
            ErrorCode::Protocol,
            "has_content set but no content fd attached",
        ),
        None,
    )
}

/// Run one request against `api`. Sends zero or more `Progress` frames and then exactly one final
/// frame through `reply`, unless the fault `die_before_ipc_reply:<kind>` is armed, in which case
/// the op still runs to completion but no final frame is sent and the caller must close the
/// connection (crash-matrix contract, review (f)3).
pub(crate) fn dispatch_api(
    api: &dyn EngineApi,
    frame: IpcFrame<IpcRequest>,
    fd: Option<OwnedFd>,
    reply: &mut dyn FnMut(IpcFrame<IpcResponse>),
    cancel: &CancelToken,
    fault: &dyn Fn(&str) -> bool,
) -> Disposition {
    let call = frame.call;
    let kind = fault_kind(&frame.msg);
    // Content arrives as an fd only when the request declares it; anything else is closed here.
    let content = if declares_content(&frame.msg) {
        fd.map(File::from)
    } else {
        None
    };
    let reply = RefCell::new(reply);
    // `Progress` frames for Fetch and for Create/Modify uploads: coalesced to one per
    // PROGRESS_INTERVAL (plus completion), none once the call is cancelled.
    let last = RefCell::new(None::<Instant>);
    let progress = |done: u64, total: u64| {
        let now = Instant::now();
        let mut last = last.borrow_mut();
        let due =
            !matches!(*last, Some(t) if now.duration_since(t) < PROGRESS_INTERVAL) || done >= total;
        if due && !cancel.is_cancelled() {
            *last = Some(now);
            (reply.borrow_mut())(IpcFrame {
                call,
                msg: IpcResponse::Progress { done, total },
            });
        }
    };

    let resp = match frame.msg {
        IpcRequest::Hello { proto, domain } => {
            if proto != PROTO_VERSION {
                error_response(
                    ProtoError::new(
                        ErrorCode::Protocol,
                        format!(
                            "ipc protocol {proto} not supported (engine speaks {PROTO_VERSION})"
                        ),
                    ),
                    None,
                )
            } else if domain != api.name() {
                // An extension talking to the wrong engine must fail loudly, never browse
                // another VM.
                error_response(
                    ProtoError::new(
                        ErrorCode::Protocol,
                        format!("this engine serves domain {:?}, not {domain:?}", api.name()),
                    ),
                    None,
                )
            } else {
                IpcResponse::Hello {
                    proto: PROTO_VERSION,
                    domain,
                }
            }
        }
        IpcRequest::Item { id } => match api.item(id) {
            Ok(it) => IpcResponse::Item(it),
            Err(e) => error_response(e, None),
        },
        IpcRequest::Enumerate {
            container,
            cursor,
            limit,
            viewer,
        } => match api.list(container, cursor.as_deref(), limit, viewer) {
            Ok(p) => IpcResponse::Page {
                items: p.items,
                next: p.next,
            },
            Err(e) => error_response(e, None),
        },
        IpcRequest::CurrentAnchor => IpcResponse::Anchor(api.anchor()),
        IpcRequest::ChangesSince { anchor, limit } => match api.changes_since(&anchor, limit) {
            Ok(c) => IpcResponse::Changes {
                updated: c.updated,
                removed: c.removed,
                anchor: c.anchor,
                more: c.more,
            },
            Err(e) => error_response(e, None),
        },
        IpcRequest::MaterializedChanged {
            added,
            removed,
            full,
        } => match api.materialized_changed(&added, &removed, full) {
            Ok(()) => IpcResponse::Ok,
            Err(e) => error_response(e, None),
        },
        IpcRequest::Fetch {
            id,
            version,
            dest_dir,
        } => match api.fetch(id, version, Path::new(&dest_dir), &progress, cancel) {
            Ok(f) => match f.path.into_os_string().into_string() {
                Ok(path) => IpcResponse::Fetched { path, item: f.item },
                Err(p) => error_response(
                    ProtoError::new(ErrorCode::Io, format!("fetched path is not UTF-8: {p:?}")),
                    None,
                ),
            },
            Err(e) => error_response(e, None),
        },
        IpcRequest::Create {
            template_id,
            parent,
            name,
            kind,
            has_content,
            symlink_target,
            mtime_ns,
            user_exec,
            changed_fields,
            local,
            may_already_exist,
            deletion_conflicted,
        } => {
            if has_content && content.is_none() {
                missing_fd()
            } else {
                let req = CreateRequest {
                    template_id,
                    parent,
                    name,
                    kind: kind.into(),
                    content,
                    symlink_target,
                    mtime_ns,
                    user_exec,
                    changed_fields,
                    local,
                    may_already_exist,
                    deletion_conflicted,
                };
                match api.create_with(req, &progress, cancel) {
                    Ok(m) => done(m),
                    Err(e) => error_response(e, None),
                }
            }
        }
        IpcRequest::Modify {
            id,
            base,
            changed_fields,
            new_parent,
            new_name,
            has_content,
            mtime_ns,
            user_exec,
            local,
        } => {
            if has_content && content.is_none() {
                missing_fd()
            } else {
                let req = ModifyRequest {
                    changed_fields,
                    new_parent,
                    new_name,
                    content,
                    mtime_ns,
                    user_exec,
                    local,
                };
                match api.modify_with(id, base, req, &progress, cancel) {
                    Ok(m) => done(m),
                    Err(e) => error_response(e, None),
                }
            }
        }
        IpcRequest::Delete {
            id,
            base,
            recursive,
        } => match api.delete(id, base, recursive) {
            Ok(()) => IpcResponse::Deleted,
            Err(e) => {
                // The shim needs the item as it is now for fileProviderErrorForRejectedDeletion.
                let current = (e.code == ErrorCode::DeletionRejected)
                    .then(|| api.item(id).ok())
                    .flatten();
                error_response(e, current)
            }
        },
        // Cancellation is routed by the Dispatcher; reaching here means there is nothing to cancel.
        IpcRequest::Cancel { .. } => IpcResponse::Ok,
        IpcRequest::Status => IpcResponse::Status(api.status()),
        IpcRequest::ConfirmPaused { apply } => match api.confirm_paused(apply) {
            Ok(()) => IpcResponse::Ok,
            Err(e) => error_response(e, None),
        },
    };

    if let Some(kind) = kind {
        if fault(&format!("die_before_ipc_reply:{kind}")) {
            tracing::warn!(
                call,
                kind,
                "fault injection: dropping IPC reply and closing the connection"
            );
            return Disposition::CloseConnection;
        }
    }
    (reply.into_inner())(IpcFrame { call, msg: resp });
    Disposition::Continue
}
