//! JSON ⇄ frame round trips through the exported C functions, for every IPC variant.

mod common;

use common::*;
use serde_json::Value;
use std::collections::BTreeSet;
use std::ffi::{c_char, CStr, CString};
use unlatch::{
    unlatch_free_bytes, unlatch_free_string, unlatch_ipc_decode_request_json,
    unlatch_ipc_decode_response_json, unlatch_ipc_encode_request_json,
    unlatch_ipc_encode_response_json, UnlatchBytes,
};
use unlatch_proto::frame;
use unlatch_proto::ipc::{ConnState, IpcFrame, IpcRequest, IpcResponse};

type EncodeFn = unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> UnlatchBytes;
type DecodeFn = unsafe extern "C" fn(*const u8, usize, *mut *mut c_char) -> *mut c_char;

fn take_string(p: *mut c_char) -> String {
    assert!(!p.is_null());
    // SAFETY: non-null string returned by libunlatch; freed once below.
    let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned();
    // SAFETY: as above.
    unsafe { unlatch_free_string(p) };
    s
}

fn encode(f: EncodeFn, json: &str) -> Result<Vec<u8>, Value> {
    let c = CString::new(json).unwrap();
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: valid C string and out pointer.
    let b = unsafe { f(c.as_ptr(), &mut err) };
    if b.ptr.is_null() {
        return Err(serde_json::from_str(&take_string(err)).unwrap());
    }
    assert!(err.is_null(), "error must be NULL on success");
    // SAFETY: libunlatch returned `len` bytes at `ptr`.
    let v = unsafe { std::slice::from_raw_parts(b.ptr, b.len) }.to_vec();
    // SAFETY: freed once.
    unsafe { unlatch_free_bytes(b) };
    Ok(v)
}

fn decode(f: DecodeFn, bytes: &[u8]) -> Result<String, Value> {
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: valid buffer and out pointer.
    let p = unsafe { f(bytes.as_ptr(), bytes.len(), &mut err) };
    if p.is_null() {
        return Err(serde_json::from_str(&take_string(err)).unwrap());
    }
    assert!(err.is_null());
    Ok(take_string(p))
}

/// Swift's synthesized `Codable` omits nil optionals; serde must accept that.
fn strip_nulls(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, x| !x.is_null());
            m.values_mut().for_each(strip_nulls);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

#[test]
fn every_request_variant_has_a_sample() {
    let have: BTreeSet<&str> = requests()
        .iter()
        .map(|(n, _, f)| {
            assert_eq!(*n, request_name(&f.msg));
            *n
        })
        .collect();
    let all: BTreeSet<&str> = variants::<IpcRequest>().iter().copied().collect();
    assert_eq!(
        have, all,
        "add samples for new IpcRequest variants in tests/common/mod.rs"
    );
}

#[test]
fn every_response_variant_has_a_sample() {
    let have: BTreeSet<&str> = responses()
        .iter()
        .map(|(n, _, f)| {
            assert_eq!(*n, response_name(&f.msg));
            *n
        })
        .collect();
    let all: BTreeSet<&str> = variants::<IpcResponse>().iter().copied().collect();
    assert_eq!(
        have, all,
        "add samples for new IpcResponse variants in tests/common/mod.rs"
    );
}

#[test]
fn every_conn_state_and_error_code_has_a_sample() {
    let have: BTreeSet<&str> = conn_states().iter().map(|(n, _)| *n).collect();
    assert_eq!(have, variants::<ConnState>().iter().copied().collect());
    let have: BTreeSet<&str> = error_codes()
        .iter()
        .map(|(n, c)| {
            assert_eq!(
                serde_json::to_value(c).unwrap(),
                Value::String(n.to_string())
            );
            *n
        })
        .collect();
    assert_eq!(
        have,
        variants::<unlatch_proto::ErrorCode>()
            .iter()
            .copied()
            .collect()
    );
}

#[test]
fn requests_roundtrip_through_c_abi() {
    for (name, suffix, sample) in requests() {
        let json = serde_json::to_string(&sample).unwrap();
        let bytes = encode(unlatch_ipc_encode_request_json, &json)
            .unwrap_or_else(|e| panic!("{name}{suffix}: {e}"));
        assert_eq!(
            bytes,
            frame::encode(&sample, false).unwrap(),
            "{name}{suffix}: frame bytes"
        );
        let back = decode(unlatch_ipc_decode_request_json, &bytes).unwrap();
        assert_eq!(back, json, "{name}{suffix}: JSON is stable");
        let parsed: IpcFrame<IpcRequest> = serde_json::from_str(&back).unwrap();
        assert_eq!(parsed, sample);

        let mut v: Value = serde_json::from_str(&json).unwrap();
        strip_nulls(&mut v);
        let again = encode(unlatch_ipc_encode_request_json, &v.to_string()).unwrap();
        assert_eq!(again, bytes, "{name}{suffix}: omitted nulls must mean None");
    }
}

#[test]
fn responses_roundtrip_through_c_abi() {
    for (name, suffix, sample) in responses() {
        let json = serde_json::to_string(&sample).unwrap();
        let bytes = encode(unlatch_ipc_encode_response_json, &json)
            .unwrap_or_else(|e| panic!("{name}{suffix}: {e}"));
        assert_eq!(
            bytes,
            frame::encode(&sample, false).unwrap(),
            "{name}{suffix}"
        );
        let back = decode(unlatch_ipc_decode_response_json, &bytes).unwrap();
        assert_eq!(back, json, "{name}{suffix}");
        let mut v: Value = serde_json::from_str(&json).unwrap();
        strip_nulls(&mut v);
        assert_eq!(
            encode(unlatch_ipc_encode_response_json, &v.to_string()).unwrap(),
            bytes,
            "{name}{suffix}"
        );
    }
}

#[test]
fn compressed_frames_decode_too() {
    // The engine may compress large replies; the decoder must accept LZ4 frames.
    let (_, _, big) = responses()
        .into_iter()
        .find(|(n, s, _)| *n == "Page" && s.is_empty())
        .unwrap();
    let mut f = big.clone();
    if let IpcResponse::Page { items, .. } = &mut f.msg {
        let one = items[0].clone();
        for _ in 0..200 {
            items.push(one.clone());
        }
    }
    let bytes = frame::encode(&f, true).unwrap();
    assert_eq!(bytes[4] & frame::FLAG_LZ4, frame::FLAG_LZ4);
    let json = decode(unlatch_ipc_decode_response_json, &bytes).unwrap();
    assert_eq!(
        serde_json::from_str::<IpcFrame<IpcResponse>>(&json).unwrap(),
        f
    );
}

#[test]
fn malformed_input_is_an_error_not_a_crash() {
    for bad in [
        "",
        "{",
        "null",
        r#"{"call":1}"#,
        r#"{"call":1,"msg":"Nope"}"#,
        r#"{"call":-1,"msg":"Status"}"#,
        r#"{"call":1,"msg":{"Item":{"id":"12"}}}"#,
        r#"{"call":1,"msg":{"Item":{"id":1},"Status":null}}"#,
    ] {
        let e = encode(unlatch_ipc_encode_request_json, bad).unwrap_err();
        assert_eq!(e["code"], "InvalidArgument", "{bad}");
        assert!(e["msg"].as_str().unwrap().len() > 3);
    }
    let good = encode(
        unlatch_ipc_encode_request_json,
        r#"{"call":1,"msg":"Status"}"#,
    )
    .unwrap();
    for bad in [
        vec![],
        vec![1, 0, 0, 0],
        good[..good.len() - 1].to_vec(),
        vec![5, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff],
    ] {
        let e = decode(unlatch_ipc_decode_request_json, &bad).unwrap_err();
        assert_eq!(e["code"], "InvalidArgument");
    }
    // NULL arguments.
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: NULL json is documented as an error, not UB.
    let b = unsafe { unlatch_ipc_encode_request_json(std::ptr::null(), &mut err) };
    assert!(b.ptr.is_null());
    assert!(take_string(err).contains("NULL"));
    // SAFETY: NULL buffer with len 0 is an empty input; out_error may be NULL.
    let p = unsafe { unlatch_ipc_decode_response_json(std::ptr::null(), 0, std::ptr::null_mut()) };
    assert!(p.is_null());
}
