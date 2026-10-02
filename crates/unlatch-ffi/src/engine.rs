//! `unlatch_engine_*`: one engine per File Provider domain, hosted by the macOS engine agent.

use crate::config;
use crate::event::event_json;
use crate::util::{
    clear_error, guard, guard_or, in_callback, into_c_string, str_arg, write_error, CallbackSlot,
    CtxPtr, FfiError,
};
use std::ffi::{c_char, c_void, CString};
use std::sync::Arc;
use unlatch_core::{Engine, EngineEvent, EventHandler};
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};
use unlatch_proto::ErrorCode;

/// `void (*)(void *ctx, const char *event_json)`. `event_json` is valid only during the call.
pub type UnlatchEventCallback =
    Option<unsafe extern "C" fn(ctx: *mut c_void, event_json: *const c_char)>;

struct EventSink {
    cb: unsafe extern "C" fn(*mut c_void, *const c_char),
    ctx: CtxPtr,
}

/// Opaque engine handle (`UnlatchEngine *` in C).
pub struct UnlatchEngine {
    pub(crate) engine: Engine,
    events: Arc<CallbackSlot<EventSink>>,
}

fn event_handler(domain: String, slot: Arc<CallbackSlot<EventSink>>) -> EventHandler {
    Arc::new(move |ev: EngineEvent| {
        let Some(json) = event_json(&domain, &ev) else {
            return;
        };
        // serde_json output never contains NUL.
        let Ok(c) = CString::new(json) else { return };
        slot.with(|sink| {
            // SAFETY: the host registered `cb` for exactly this signature; `c` outlives the call.
            unsafe { (sink.cb)(sink.ctx.0, c.as_ptr()) }
        });
    })
}

/// Start an engine from a JSON config (see `config.rs` / `unlatch.h`). Never blocks on the network.
/// `event_cb` may be NULL. Returns NULL on error (and sets `*out_error`).
///
/// # Safety
/// `config_json` NUL-terminated or NULL; `out_error` NULL or writable; `event_cb`/`ctx` must be
/// callable from any thread until `unlatch_engine_stop` returns.
#[no_mangle]
pub unsafe extern "C" fn unlatch_engine_start(
    config_json: *const c_char,
    event_cb: UnlatchEventCallback,
    ctx: *mut c_void,
    out_error: *mut *mut c_char,
) -> *mut UnlatchEngine {
    // SAFETY: caller contract.
    unsafe { clear_error(out_error) };
    let r = guard(|| {
        // SAFETY: caller contract.
        let json = unsafe { str_arg(config_json, "config_json") }?;
        let cfg = config::parse(json)?;
        let slot = Arc::new(CallbackSlot::new(event_cb.map(|cb| EventSink {
            cb,
            ctx: CtxPtr(ctx),
        })));
        let handler = event_handler(cfg.name.clone(), slot.clone());
        match Engine::start(cfg, Some(handler)) {
            Ok(engine) => Ok(Box::into_raw(Box::new(UnlatchEngine {
                engine,
                events: slot,
            }))),
            Err(e) => {
                // A failed start may already have emitted events; make sure none follow.
                slot.revoke();
                Err(FfiError::from(e))
            }
        }
    });
    match r {
        Ok(p) => p,
        Err(e) => {
            // SAFETY: caller contract.
            unsafe { write_error(out_error, &e) };
            std::ptr::null_mut()
        }
    }
}

fn stop_now(handle: Box<UnlatchEngine>) {
    guard_or((), || handle.engine.shutdown());
    handle.events.revoke();
    drop(handle);
}

/// Flush and stop the engine and free the handle. After it returns the event callback is never
/// invoked again, so `ctx` may be released. Called from inside the event callback, the stop is
/// completed on a helper thread instead (waiting there would deadlock). NULL is a no-op.
///
/// # Safety
/// `engine` must be NULL or a live handle from `unlatch_engine_start`, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn unlatch_engine_stop(engine: *mut UnlatchEngine) {
    if engine.is_null() {
        return;
    }
    // SAFETY: caller contract: live handle, ownership transferred to us.
    let handle = unsafe { Box::from_raw(engine) };
    if in_callback() {
        let spawned = std::thread::Builder::new()
            .name("unlatch-engine-stop".into())
            .spawn(move || stop_now(handle));
        if let Err(e) = spawned {
            tracing::error!(target: "unlatch_ffi", "cannot spawn stop thread: {e}");
        }
    } else {
        stop_now(handle);
    }
}

/// # Safety
/// `engine` must be NULL or a live handle.
unsafe fn engine_ref<'a>(engine: *const UnlatchEngine) -> Result<&'a UnlatchEngine, FfiError> {
    if engine.is_null() {
        return Err(FfiError::invalid("engine is NULL"));
    }
    // SAFETY: caller contract.
    Ok(unsafe { &*engine })
}

/// `EngineStatus` as JSON (free with `unlatch_free_string`); NULL if `engine` is NULL.
///
/// # Safety
/// `engine` must be NULL or a live handle.
#[no_mangle]
pub unsafe extern "C" fn unlatch_engine_status_json(engine: *const UnlatchEngine) -> *mut c_char {
    let r = guard(|| {
        // SAFETY: caller contract.
        let h = unsafe { engine_ref(engine) }?;
        serde_json::to_string(&h.engine.status()).map_err(|e| FfiError::invalid(e.to_string()))
    });
    r.map(into_c_string).unwrap_or(std::ptr::null_mut())
}

/// Network path changed / woke from sleep: reconnect now if not live.
///
/// # Safety
/// `engine` must be NULL or a live handle.
#[no_mangle]
pub unsafe extern "C" fn unlatch_engine_network_changed(engine: *const UnlatchEngine) {
    let _ = guard(|| {
        // SAFETY: caller contract.
        unsafe { engine_ref(engine) }?.engine.network_changed();
        Ok(())
    });
}

/// Connect now allowing interactive auth (askpass). **Blocks** until connected or failed; call
/// it off the main thread. Returns true on success.
///
/// # Safety
/// `engine` NULL or live; `out_error` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn unlatch_engine_connect_interactive(
    engine: *const UnlatchEngine,
    out_error: *mut *mut c_char,
) -> bool {
    // SAFETY: caller contract.
    unsafe { clear_error(out_error) };
    let r = guard(|| {
        // SAFETY: caller contract.
        unsafe { engine_ref(engine) }?
            .engine
            .connect_interactive()
            .map_err(FfiError::from)
    });
    // SAFETY: caller contract.
    unsafe { report(r, out_error) }
}

/// Resolve a paused mass deletion (`ConnState::Paused`): `apply` = delete, else keep.
///
/// # Safety
/// `engine` NULL or live; `out_error` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn unlatch_engine_confirm_paused(
    engine: *const UnlatchEngine,
    apply: bool,
    out_error: *mut *mut c_char,
) -> bool {
    // SAFETY: caller contract.
    unsafe { clear_error(out_error) };
    let r = guard(|| {
        // SAFETY: caller contract.
        unsafe { engine_ref(engine) }?
            .engine
            .confirm_paused(apply)
            .map_err(FfiError::from)
    });
    // SAFETY: caller contract.
    unsafe { report(r, out_error) }
}

/// Run one IPC request in-process (the agent uses it for its own `MaterializedChanged` walks):
/// `request_json` is an `IpcFrame<IpcRequest>`, the result the final `IpcFrame<IpcResponse>`
/// (progress frames are dropped). Blocking. Requests that need an fd are rejected.
///
/// # Safety
/// `engine` NULL or live; `request_json` NUL-terminated or NULL; `out_error` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn unlatch_engine_call_json(
    engine: *const UnlatchEngine,
    request_json: *const c_char,
    out_error: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract.
    unsafe { clear_error(out_error) };
    let r = guard(|| {
        // SAFETY: caller contract.
        let h = unsafe { engine_ref(engine) }?;
        // SAFETY: caller contract.
        let json = unsafe { str_arg(request_json, "request_json") }?;
        let req: IpcFrame<IpcRequest> =
            serde_json::from_str(json).map_err(|e| FfiError::invalid(format!("JSON: {e}")))?;
        if needs_fd(&req.msg) {
            return Err(FfiError::invalid(
                "requests with has_content need an fd; use a dispatcher",
            ));
        }
        let call = req.call;
        let mut last: Option<IpcFrame<IpcResponse>> = None;
        unlatch_core::ipc::dispatch(&h.engine, req, None, &mut |resp| {
            if !matches!(resp.msg, IpcResponse::Progress { .. }) {
                last = Some(resp);
            }
        });
        let resp = last.unwrap_or(IpcFrame {
            call,
            msg: IpcResponse::Error {
                code: ErrorCode::Protocol,
                msg: "no reply".into(),
                current: None,
            },
        });
        serde_json::to_string(&resp).map_err(|e| FfiError::invalid(e.to_string()))
    });
    match r {
        Ok(s) => into_c_string(s),
        Err(e) => {
            // SAFETY: caller contract.
            unsafe { write_error(out_error, &e) };
            std::ptr::null_mut()
        }
    }
}

pub(crate) fn needs_fd(req: &IpcRequest) -> bool {
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

/// # Safety
/// `out_error` must be NULL or writable.
unsafe fn report(r: Result<(), FfiError>, out_error: *mut *mut c_char) -> bool {
    match r {
        Ok(()) => true,
        Err(e) => {
            // SAFETY: caller contract.
            unsafe { write_error(out_error, &e) };
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{decode_frame, encode_frame};

    #[test]
    fn needs_fd_only_for_content() {
        assert!(!needs_fd(&IpcRequest::Status));
        assert!(needs_fd(&IpcRequest::Modify {
            id: unlatch_proto::ItemId(2),
            base: Default::default(),
            changed_fields: 1,
            new_parent: None,
            new_name: None,
            has_content: true,
            mtime_ns: None,
            user_exec: None,
            local: Default::default(),
        }));
    }

    #[test]
    fn frame_helpers_roundtrip() {
        let f = IpcFrame {
            call: 9,
            msg: IpcRequest::Status,
        };
        let b = encode_frame(&f).unwrap();
        assert_eq!(decode_frame::<IpcRequest>(&b).unwrap(), f);
    }
}
