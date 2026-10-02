//! JSON ⇄ frame helpers. The JSON is serde's own representation of the `unlatch_proto::ipc` types
//! (externally tagged enums: `"Status"` for unit variants, `{"Item": {"id": 5}}` otherwise), which
//! the Swift `Codable` mirrors in `mac/UnlatchShared` encode and decode. The bytes are one complete
//! frame (`u32 LE length` + flags + postcard payload), exactly what travels over XPC.

use crate::util::{
    bytes_arg, clear_error, guard, into_c_string, str_arg, write_error, FfiError, UnlatchBytes,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::ffi::c_char;
use unlatch_proto::frame::{self, MAX_FRAME};
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};

/// The body (flags + payload) of one complete frame, after checking its length prefix.
pub(crate) fn frame_body(bytes: &[u8]) -> Result<&[u8], FfiError> {
    if bytes.len() < 5 {
        return Err(FfiError::invalid(format!(
            "frame too short ({} bytes)",
            bytes.len()
        )));
    }
    let declared = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if declared == 0 || declared > MAX_FRAME {
        return Err(FfiError::invalid(format!(
            "frame length {declared} out of range"
        )));
    }
    if declared != bytes.len() - 4 {
        return Err(FfiError::invalid(format!(
            "frame length prefix {declared} does not match buffer ({} bytes after prefix)",
            bytes.len() - 4
        )));
    }
    Ok(&bytes[4..])
}

pub(crate) fn decode_frame<T: DeserializeOwned>(bytes: &[u8]) -> Result<IpcFrame<T>, FfiError> {
    frame::decode_body(frame_body(bytes)?).map_err(|e| FfiError::invalid(format!("frame: {e}")))
}

pub(crate) fn encode_frame<T: Serialize>(f: &IpcFrame<T>) -> Result<Vec<u8>, FfiError> {
    // Local IPC: never compress (CPU for nothing); the decoder accepts both.
    frame::encode(f, false).map_err(|e| FfiError::invalid(format!("frame: {e}")))
}

fn encode_json<T: Serialize + DeserializeOwned>(json: &str) -> Result<Vec<u8>, FfiError> {
    let f: IpcFrame<T> =
        serde_json::from_str(json).map_err(|e| FfiError::invalid(format!("JSON: {e}")))?;
    encode_frame(&f)
}

fn decode_json<T: Serialize + DeserializeOwned>(bytes: &[u8]) -> Result<String, FfiError> {
    let f: IpcFrame<T> = decode_frame(bytes)?;
    serde_json::to_string(&f).map_err(|e| FfiError::invalid(format!("JSON: {e}")))
}

/// `IpcFrame<IpcRequest>` JSON → frame bytes.
pub fn encode_request_frame_json(json: &str) -> Result<Vec<u8>, String> {
    encode_json::<IpcRequest>(json).map_err(|e| e.msg)
}

/// `IpcFrame<IpcResponse>` JSON → frame bytes.
pub fn encode_response_frame_json(json: &str) -> Result<Vec<u8>, String> {
    encode_json::<IpcResponse>(json).map_err(|e| e.msg)
}

/// Frame bytes → `IpcFrame<IpcRequest>` JSON.
pub fn decode_request_frame_json(bytes: &[u8]) -> Result<String, String> {
    decode_json::<IpcRequest>(bytes).map_err(|e| e.msg)
}

/// Frame bytes → `IpcFrame<IpcResponse>` JSON.
pub fn decode_response_frame_json(bytes: &[u8]) -> Result<String, String> {
    decode_json::<IpcResponse>(bytes).map_err(|e| e.msg)
}

unsafe fn encode_extern<T: Serialize + DeserializeOwned>(
    json: *const c_char,
    out_error: *mut *mut c_char,
) -> UnlatchBytes {
    // SAFETY: forwarded caller contract (valid out pointer or NULL).
    unsafe { clear_error(out_error) };
    let r = guard(|| {
        // SAFETY: forwarded caller contract (NUL-terminated string or NULL).
        let json = unsafe { str_arg(json, "json") }?;
        encode_json::<T>(json)
    });
    match r {
        Ok(v) => UnlatchBytes::from_vec(v),
        Err(e) => {
            // SAFETY: forwarded caller contract.
            unsafe { write_error(out_error, &e) };
            UnlatchBytes::null()
        }
    }
}

unsafe fn decode_extern<T: Serialize + DeserializeOwned>(
    bytes: *const u8,
    len: usize,
    out_error: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: forwarded caller contract.
    unsafe { clear_error(out_error) };
    let r = guard(|| {
        // SAFETY: forwarded caller contract (`len` readable bytes or NULL).
        let bytes = unsafe { bytes_arg(bytes, len, "bytes") }?;
        decode_json::<T>(bytes)
    });
    match r {
        Ok(s) => into_c_string(s),
        Err(e) => {
            // SAFETY: forwarded caller contract.
            unsafe { write_error(out_error, &e) };
            std::ptr::null_mut()
        }
    }
}

/// Encode an `IpcFrame<IpcRequest>` given as JSON into one frame. Returns `{NULL, 0}` on error.
///
/// # Safety
/// `json` must be NULL or NUL-terminated; `out_error` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn unlatch_ipc_encode_request_json(
    json: *const c_char,
    out_error: *mut *mut c_char,
) -> UnlatchBytes {
    // SAFETY: caller contract.
    unsafe { encode_extern::<IpcRequest>(json, out_error) }
}

/// Encode an `IpcFrame<IpcResponse>` given as JSON into one frame. Returns `{NULL, 0}` on error.
///
/// # Safety
/// As [`unlatch_ipc_encode_request_json`].
#[no_mangle]
pub unsafe extern "C" fn unlatch_ipc_encode_response_json(
    json: *const c_char,
    out_error: *mut *mut c_char,
) -> UnlatchBytes {
    // SAFETY: caller contract.
    unsafe { encode_extern::<IpcResponse>(json, out_error) }
}

/// Decode one complete request frame into `IpcFrame<IpcRequest>` JSON. NULL on error.
///
/// # Safety
/// `bytes` must point to `len` readable bytes (or be NULL); `out_error` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn unlatch_ipc_decode_request_json(
    bytes: *const u8,
    len: usize,
    out_error: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract.
    unsafe { decode_extern::<IpcRequest>(bytes, len, out_error) }
}

/// Decode one complete response frame into `IpcFrame<IpcResponse>` JSON. NULL on error.
///
/// # Safety
/// As [`unlatch_ipc_decode_request_json`].
#[no_mangle]
pub unsafe extern "C" fn unlatch_ipc_decode_response_json(
    bytes: *const u8,
    len: usize,
    out_error: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract.
    unsafe { decode_extern::<IpcResponse>(bytes, len, out_error) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unlatch_proto::ipc::IpcRequest;

    #[test]
    fn frame_body_checks_prefix() {
        let f = encode_frame(&IpcFrame {
            call: 1,
            msg: IpcRequest::Status,
        })
        .unwrap();
        assert!(frame_body(&f).is_ok());
        assert!(frame_body(&f[..f.len() - 1]).is_err(), "truncated");
        let mut longer = f.clone();
        longer.push(0);
        assert!(frame_body(&longer).is_err(), "trailing bytes");
        assert!(frame_body(&[0, 0, 0, 0, 0]).is_err(), "zero length");
        assert!(frame_body(&[1, 0, 0]).is_err(), "short");
        assert!(frame_body(&[0xff, 0xff, 0xff, 0xff, 0]).is_err(), "huge");
    }

    #[test]
    fn unit_variant_json_is_bare_string() {
        let b = encode_request_frame_json(r#"{"call":3,"msg":"CurrentAnchor"}"#).unwrap();
        assert_eq!(
            decode_request_frame_json(&b).unwrap(),
            r#"{"call":3,"msg":"CurrentAnchor"}"#
        );
    }

    #[test]
    fn request_is_not_a_response() {
        let b = encode_request_frame_json(r#"{"call":3,"msg":{"Item":{"id":7}}}"#).unwrap();
        // Postcard is positional: the same bytes decode as a different response variant or fail.
        // What matters is that the JSON encoders are typed; decoding as the right type round-trips.
        assert!(decode_request_frame_json(&b).is_ok());
        assert!(encode_response_frame_json(r#"{"call":3,"msg":{"Item":{"id":7}}}"#).is_err());
    }
}
