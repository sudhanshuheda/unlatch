//! The XPC bridge end to end through the C ABI: a real engine (offline: its unlatchd command does
//! not exist), a dispatcher, request frames in, reply frames out through the C callback.
//!
//! Set `UNLATCH_FFI_E2E_UNLATCHD=/path/to/unlatchd` to also run against a live `unlatchd stdio` session.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CString};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use unlatch::*;
use unlatch_proto::frame;
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};
use unlatch_proto::ItemId;

#[derive(Default)]
struct Replies {
    frames: Mutex<Vec<Vec<u8>>>,
    cv: Condvar,
}

impl Replies {
    /// Final replies by call id (progress frames skipped), waiting until `calls` are all in.
    fn wait_final(&self, calls: &[u64], timeout: Duration) -> HashMap<u64, IpcResponse> {
        let deadline = Instant::now() + timeout;
        let mut g = self.frames.lock().unwrap();
        loop {
            let decoded: HashMap<u64, IpcResponse> = g
                .iter()
                .map(|b| {
                    frame::decode_body::<IpcFrame<IpcResponse>>(&b[4..])
                        .expect("reply frame decodes")
                })
                .filter(|f| !matches!(f.msg, IpcResponse::Progress { .. }))
                .map(|f| (f.call, f.msg))
                .collect();
            if calls.iter().all(|c| decoded.contains_key(c)) {
                return decoded;
            }
            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for replies to {calls:?}; have {:?}",
                decoded.keys()
            );
            g = self.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }

    fn count(&self) -> usize {
        self.frames.lock().unwrap().len()
    }
}

unsafe extern "C" fn on_reply(ctx: *mut c_void, bytes: *const u8, len: usize, fd: i32) {
    assert_eq!(fd, -1);
    // SAFETY: ctx is the test's `Replies`, alive until the dispatcher is freed.
    let r = unsafe { &*(ctx as *const Replies) };
    if bytes.is_null() {
        return;
    }
    // SAFETY: libunlatch passes `len` readable bytes valid for this call.
    let frame = unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec();
    r.frames.lock().unwrap().push(frame);
    r.cv.notify_all();
}

fn start_engine(cfg: serde_json::Value) -> *mut UnlatchEngine {
    let c = CString::new(cfg.to_string()).unwrap();
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: valid arguments; no event callback.
    let e = unsafe { unlatch_engine_start(c.as_ptr(), None, std::ptr::null_mut(), &mut err) };
    if e.is_null() {
        // SAFETY: error string from libunlatch.
        let msg = unsafe { std::ffi::CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned();
        panic!("engine start failed: {msg}");
    }
    e
}

fn submit(d: *const UnlatchDispatcher, call: u64, msg: IpcRequest) {
    let bytes = frame::encode(&IpcFrame { call, msg }, false).unwrap();
    // SAFETY: live dispatcher, valid buffer, no fd.
    assert!(unsafe { unlatch_dispatcher_submit(d, bytes.as_ptr(), bytes.len(), -1) });
}

#[test]
fn requests_round_trip_through_the_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let engine = start_engine(serde_json::json!({
        "name": "bridge-test",
        "transport": {"command": {"argv": ["/nonexistent/unlatchd", "stdio"]}},
        "remote_root": "/nowhere",
        "state_dir": dir.path(),
        "client_name": "test-mac",
        "list_timeout_ms": 200,
    }));
    let replies = Box::new(Replies::default());
    let ctx = &*replies as *const Replies as *mut c_void;
    // SAFETY: live engine; `replies` outlives the dispatcher.
    let d = unsafe { unlatch_dispatcher_new(engine, Some(on_reply), ctx) };
    assert!(!d.is_null());

    submit(
        d,
        1,
        IpcRequest::Hello {
            proto: unlatch_proto::PROTO_VERSION,
            domain: "bridge-test".into(),
        },
    );
    submit(d, 2, IpcRequest::Status);
    submit(d, 3, IpcRequest::CurrentAnchor);
    submit(d, 4, IpcRequest::Item { id: ItemId::ROOT });
    submit(
        d,
        5,
        IpcRequest::Delete {
            id: ItemId(987_654),
            base: Default::default(),
            recursive: false,
        },
    );
    let got = replies.wait_final(&[1, 2, 3, 4, 5], Duration::from_secs(20));

    assert!(
        matches!(&got[&1], IpcResponse::Hello { domain, .. } if domain == "bridge-test"),
        "{:?}",
        got[&1]
    );
    assert!(matches!(&got[&2], IpcResponse::Status(_)), "{:?}", got[&2]);
    assert!(
        matches!(&got[&3], IpcResponse::Anchor(a) if !a.is_empty()),
        "{:?}",
        got[&3]
    );
    // Offline and never synced: an answer, not a hang (the root item or an error).
    assert!(
        matches!(&got[&4], IpcResponse::Item(_) | IpcResponse::Error { .. }),
        "{:?}",
        got[&4]
    );
    // Every reply is valid JSON through the Swift-facing decoder too.
    for b in replies.frames.lock().unwrap().iter() {
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid buffer and out pointer.
        let json = unsafe { unlatch_ipc_decode_response_json(b.as_ptr(), b.len(), &mut err) };
        assert!(!json.is_null());
        // SAFETY: freeing our result once.
        unsafe { unlatch_free_string(json) };
    }

    // A malformed frame is rejected without disturbing the connection.
    // SAFETY: valid buffer.
    assert!(!unsafe { unlatch_dispatcher_submit(d, [1u8, 0, 0, 0, 9].as_ptr(), 5, -1) });
    submit(d, 6, IpcRequest::Status);
    replies.wait_final(&[6], Duration::from_secs(10));

    // SAFETY: freeing once; after this no callback may run.
    unsafe { unlatch_dispatcher_free(d) };
    let n = replies.count();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        replies.count(),
        n,
        "reply callback ran after unlatch_dispatcher_free"
    );
    // SAFETY: stopping once, after the dispatcher that pointed into it.
    unsafe { unlatch_engine_stop(engine) };
}

#[test]
fn call_json_runs_in_process() {
    let dir = tempfile::tempdir().unwrap();
    let engine = start_engine(serde_json::json!({
        "name": "call-json",
        "transport": {"command": {"argv": ["/nonexistent/unlatchd", "stdio"]}},
        "remote_root": "/nowhere",
        "state_dir": dir.path(),
        "client_name": "test-mac",
    }));
    let req = CString::new(
        r#"{"call":9,"msg":{"MaterializedChanged":{"added":[1],"removed":[],"full":true}}}"#,
    )
    .unwrap();
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: live engine, valid strings.
    let out = unsafe { unlatch_engine_call_json(engine, req.as_ptr(), &mut err) };
    assert!(!out.is_null(), "call_json failed");
    // SAFETY: libunlatch string, freed once.
    let json = unsafe { std::ffi::CStr::from_ptr(out) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: as above.
    unsafe { unlatch_free_string(out) };
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["call"], 9);
    assert!(
        v["msg"] == "Ok" || v["msg"].get("Error").is_some(),
        "{json}"
    );

    // A request that needs a descriptor cannot go through call_json.
    let with_fd = CString::new(
        r#"{"call":1,"msg":{"Modify":{"id":2,"base":{"content":null,"meta":null},"changed_fields":1,"has_content":true,"local":{"xattrs":[],"hidden":false}}}}"#,
    )
    .unwrap();
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: live engine, valid strings.
    assert!(unsafe { unlatch_engine_call_json(engine, with_fd.as_ptr(), &mut err) }.is_null());
    // SAFETY: error string, freed once.
    unsafe { unlatch_free_string(err) };

    // SAFETY: live handle; status JSON is freed; stop once.
    unsafe {
        let s = unlatch_engine_status_json(engine);
        assert!(!s.is_null());
        unlatch_free_string(s);
        unlatch_engine_network_changed(engine);
        unlatch_engine_stop(engine);
    }
}

/// Opt-in: a live `unlatchd stdio` session behind the bridge.
#[test]
fn live_unlatchd_listing() {
    let Some(unlatchd) = std::env::var_os("UNLATCH_FFI_E2E_UNLATCHD") else {
        eprintln!("SKIP: set UNLATCH_FFI_E2E_UNLATCHD=/path/to/unlatchd to run");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("hello.txt"), b"hi").unwrap();
    std::fs::create_dir(root.path().join("sub")).unwrap();
    let dstate = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let engine = start_engine(serde_json::json!({
        "name": "live",
        "transport": {"command": {"argv": [
            unlatchd.to_string_lossy(), "stdio", "--root", root.path(), "--state", dstate.path()]}},
        "remote_root": root.path(),
        "state_dir": state.path(),
        "client_name": "test-mac",
    }));
    let replies = Box::new(Replies::default());
    let ctx = &*replies as *const Replies as *mut c_void;
    // SAFETY: live engine; `replies` outlives the dispatcher.
    let d = unsafe { unlatch_dispatcher_new(engine, Some(on_reply), ctx) };
    submit(
        d,
        1,
        IpcRequest::Hello {
            proto: unlatch_proto::PROTO_VERSION,
            domain: "live".into(),
        },
    );
    submit(
        d,
        2,
        IpcRequest::Enumerate {
            container: ItemId::ROOT,
            cursor: None,
            limit: 100,
            viewer: false,
        },
    );
    let got = replies.wait_final(&[1, 2], Duration::from_secs(30));
    match &got[&2] {
        IpcResponse::Page { items, .. } => {
            let mut names: Vec<_> = items.iter().map(|i| i.display_name.as_str()).collect();
            names.sort();
            assert_eq!(names, ["hello.txt", "sub"]);
        }
        other => panic!("unexpected {other:?}"),
    }
    // SAFETY: free, then stop, once each.
    unsafe {
        unlatch_dispatcher_free(d);
        unlatch_engine_stop(engine);
    }
}
