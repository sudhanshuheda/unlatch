//! C ABI over `unlatch-core`, linked into the macOS app (as the engine agent) and the File Provider
//! extension as the static library `libunlatch.a`. The header is hand-written:
//! `crates/unlatch-ffi/include/unlatch.h` (tests check that every declared function exists here).
//!
//! Three groups of functions:
//! * **Engine** (`unlatch_engine_*`): start/stop one [`unlatch_core::Engine`] per File Provider domain
//!   from a JSON config ([`FfiConfig`]); engine events arrive as JSON on a C callback.
//! * **Dispatcher** (`unlatch_dispatcher_*`): the XPC bridge. One per XPC connection, wrapping
//!   [`unlatch_core::ipc::Dispatcher`]; the payloads are complete `unlatch_proto::frame` frames of
//!   `IpcFrame<IpcRequest>` / `IpcFrame<IpcResponse>`, byte-identical to the unix-socket transport.
//! * **JSON helpers** (`unlatch_ipc_*`): convert between serde's JSON form of the IPC types and frame
//!   bytes, so Swift only needs `Codable` mirrors of the protocol, never postcard.
//!
//! Rules every exported function follows:
//! * No panic ever unwinds into the caller: each body runs under `catch_unwind` and a panic turns
//!   into the documented failure value plus an `out_error` of code `"Panic"`.
//! * Strings and byte buffers returned to the caller are owned by the caller and must be released
//!   with [`unlatch_free_string`] / [`unlatch_free_bytes`]; nothing else frees them.
//! * Errors are reported through an optional `char **out_error` as JSON
//!   `{"code": "<ErrorCode or InvalidArgument/Panic>", "msg": "…"}`.

mod config;
mod dispatcher;
mod engine;
mod event;
mod json;
mod util;

pub use config::{FfiConfig, FfiPrefetch, FfiTransport, FfiUnlatchdBinary};
pub use dispatcher::{
    unlatch_dispatcher_free, unlatch_dispatcher_new, unlatch_dispatcher_submit, UnlatchDispatcher,
    UnlatchReplyCallback,
};
pub use engine::{
    unlatch_engine_call_json, unlatch_engine_confirm_paused, unlatch_engine_connect_interactive,
    unlatch_engine_network_changed, unlatch_engine_start, unlatch_engine_status_json,
    unlatch_engine_stop, UnlatchEngine, UnlatchEventCallback,
};
pub use event::event_json;
pub use json::{
    decode_request_frame_json, decode_response_frame_json, encode_request_frame_json,
    encode_response_frame_json, unlatch_ipc_decode_request_json, unlatch_ipc_decode_response_json,
    unlatch_ipc_encode_request_json, unlatch_ipc_encode_response_json,
};
pub use util::{unlatch_free_bytes, unlatch_free_string, UnlatchBytes};

use std::ffi::c_char;

/// Crate version as a static NUL-terminated string (never freed).
#[no_mangle]
pub extern "C" fn unlatch_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// `unlatch_proto::PROTO_VERSION`, for the `Hello` frames the Swift side builds.
#[no_mangle]
pub extern "C" fn unlatch_proto_version() -> u32 {
    unlatch_proto::PROTO_VERSION
}
