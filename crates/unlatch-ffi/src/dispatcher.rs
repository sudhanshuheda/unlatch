//! `unlatch_dispatcher_*`: the XPC bridge. The agent creates one dispatcher per XPC connection
//! (after reading that connection's `Hello` to pick the domain's engine), submits every frame the
//! extension sends, and forwards every reply frame back through the connection.

use crate::engine::UnlatchEngine;
use crate::json::{decode_frame, encode_frame};
use crate::util::{bytes_arg, guard, guard_or, in_callback, CallbackSlot, CtxPtr, FfiError};
use std::ffi::c_void;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use unlatch_core::ipc::Dispatcher;
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};
use unlatch_proto::ErrorCode;

/// `void (*)(void *ctx, const uint8_t *frame, size_t len, int32_t fd)`. `frame` is valid only
/// during the call. `fd` is -1 (reserved: a future reply carrying a descriptor transfers it).
/// `frame == NULL && len == 0` means the engine closed the connection (fault injection,
/// `die_before_ipc_reply`): the host should invalidate the XPC connection. Sent at most once,
/// after every reply frame, never after `unlatch_dispatcher_free` returns (it is wired to
/// `unlatch_core::ipc::Dispatcher::with_close`; the free revokes the callback before dropping the
/// core dispatcher, so the drop's own close does not reach the host).
pub type UnlatchReplyCallback =
    Option<unsafe extern "C" fn(ctx: *mut c_void, frame: *const u8, len: usize, fd: i32)>;

struct ReplySink {
    cb: unsafe extern "C" fn(*mut c_void, *const u8, usize, i32),
    ctx: CtxPtr,
}

/// Opaque dispatcher handle (`UnlatchDispatcher *` in C).
pub struct UnlatchDispatcher {
    inner: Dispatcher,
    replies: Arc<CallbackSlot<ReplySink>>,
}

fn reply_bytes(resp: &IpcFrame<IpcResponse>) -> Vec<u8> {
    match encode_frame(resp) {
        Ok(b) => b,
        Err(e) => {
            // Only possible for a reply above MAX_FRAME; the caller still needs a final answer.
            tracing::error!(target: "unlatch_ffi", "reply for call {} not encodable: {}", resp.call, e.msg);
            let fallback = IpcFrame {
                call: resp.call,
                msg: IpcResponse::Error {
                    code: ErrorCode::Protocol,
                    msg: format!("reply not encodable: {}", e.msg),
                    current: None,
                },
            };
            encode_frame(&fallback).unwrap_or_default()
        }
    }
}

/// Create a dispatcher for one connection. Returns NULL if `engine` or `reply_cb` is NULL or the
/// engine cannot create one.
///
/// # Safety
/// `engine` must be NULL or a live handle that outlives the dispatcher; `reply_cb`/`ctx` must be
/// callable from any thread until `unlatch_dispatcher_free` returns.
#[no_mangle]
pub unsafe extern "C" fn unlatch_dispatcher_new(
    engine: *const UnlatchEngine,
    reply_cb: UnlatchReplyCallback,
    ctx: *mut c_void,
) -> *mut UnlatchDispatcher {
    let r = guard(|| {
        if engine.is_null() {
            return Err(FfiError::invalid("engine is NULL"));
        }
        let cb = reply_cb.ok_or_else(|| FfiError::invalid("reply_cb is NULL"))?;
        // SAFETY: caller contract (live handle).
        let engine = unsafe { &*engine }.engine.clone();
        let replies = Arc::new(CallbackSlot::new(Some(ReplySink {
            cb,
            ctx: CtxPtr(ctx),
        })));
        let send = {
            let replies = replies.clone();
            Box::new(move |resp: IpcFrame<IpcResponse>| {
                let bytes = reply_bytes(&resp);
                replies.with(|sink| {
                    // SAFETY: the host registered `cb` for this signature; `bytes` outlives the call.
                    unsafe { (sink.cb)(sink.ctx.0, bytes.as_ptr(), bytes.len(), -1) }
                });
            })
        };
        let on_close = {
            let replies = replies.clone();
            Box::new(move || {
                replies.with(|sink| {
                    // SAFETY: the reserved "connection closed" form of the same callback.
                    unsafe { (sink.cb)(sink.ctx.0, std::ptr::null(), 0, -1) }
                });
            })
        };
        let inner = Dispatcher::with_close(engine, send, on_close);
        Ok(Box::into_raw(Box::new(UnlatchDispatcher {
            inner,
            replies,
        })))
    });
    r.unwrap_or(std::ptr::null_mut())
}

/// Submit one complete request frame (non-blocking). `fd` (or -1) is the content descriptor for
/// a `Create`/`Modify` with `has_content: true`; ownership passes to libunlatch in every case (it
/// is closed if the frame is rejected). Returns false if the frame is malformed.
///
/// # Safety
/// `disp` NULL or live; `bytes` points to `len` readable bytes; `fd` is -1 or an open descriptor
/// the caller no longer uses.
#[no_mangle]
pub unsafe extern "C" fn unlatch_dispatcher_submit(
    disp: *const UnlatchDispatcher,
    bytes: *const u8,
    len: usize,
    fd: i32,
) -> bool {
    // Take ownership first so the descriptor is closed on every failure path.
    // SAFETY: caller contract: `fd` is open and ours from here on.
    let fd = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) });
    let r = guard(move || {
        if disp.is_null() {
            return Err(FfiError::invalid("dispatcher is NULL"));
        }
        // SAFETY: caller contract (live handle).
        let d = unsafe { &*disp };
        // SAFETY: caller contract.
        let bytes = unsafe { bytes_arg(bytes, len, "bytes") }?;
        let req: IpcFrame<IpcRequest> = decode_frame(bytes)?;
        d.inner.submit(req, fd);
        Ok(())
    });
    match r {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(target: "unlatch_ffi", "dispatcher submit rejected: {}", e.msg);
            false
        }
    }
}

fn free_now(d: Box<UnlatchDispatcher>) {
    // Revoke first so nothing reaches the host after we return, then drop the core dispatcher:
    // that is "the connection is gone", which cancels every call still in flight (review
    // §2(e)1 — an invalidated connection means "cancel my calls", never "engine gone").
    d.replies.revoke();
    guard_or((), || drop(d));
}

/// Cancel this connection's outstanding calls and free the dispatcher. After it returns
/// `reply_cb` is never invoked again. Called from inside `reply_cb`, the free completes on a
/// helper thread. NULL is a no-op.
///
/// # Safety
/// `disp` must be NULL or a live handle, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn unlatch_dispatcher_free(disp: *mut UnlatchDispatcher) {
    if disp.is_null() {
        return;
    }
    // SAFETY: caller contract: live handle, ownership transferred.
    let d = unsafe { Box::from_raw(disp) };
    if in_callback() {
        let spawned = std::thread::Builder::new()
            .name("unlatch-dispatcher-free".into())
            .spawn(move || free_now(d));
        if let Err(e) = spawned {
            tracing::error!(target: "unlatch_ffi", "cannot spawn free thread: {e}");
        }
    } else {
        free_now(d);
    }
}
