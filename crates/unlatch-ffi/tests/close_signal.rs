//! The reserved "connection closed" reply (`frame == NULL, len == 0`, see include/unlatch.h and
//! docs/PROTOCOL-NOTES.md): with `UNLATCH_FAULT=die_before_ipc_reply:delete` the engine runs the delete, withholds
//! its reply and closes the connection, and the host's reply callback hears about it exactly once.
//!
//! Its own test binary (own process): `UNLATCH_FAULT` is read once per process, so it is set here
//! before the engine consults it and cannot leak into other tests.

use std::ffi::{c_char, c_void, CString};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use unlatch::*;
use unlatch_proto::frame;
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};
use unlatch_proto::{ErrorCode, ItemId};

#[derive(Default)]
struct Seen {
    /// Decoded frames, and `None` for each NULL (closed) callback, in arrival order.
    events: Mutex<Vec<Option<IpcFrame<IpcResponse>>>>,
    cv: Condvar,
}

impl Seen {
    fn wait(&self, what: &str, f: impl Fn(&[Option<IpcFrame<IpcResponse>>]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut g = self.events.lock().unwrap();
        while !f(&g) {
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for {what}: {:?}", *g);
            g = self.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }
}

unsafe extern "C" fn on_reply(ctx: *mut c_void, bytes: *const u8, len: usize, fd: i32) {
    assert_eq!(fd, -1);
    // SAFETY: ctx is the test's `Seen`, alive until the dispatcher is freed.
    let s = unsafe { &*(ctx as *const Seen) };
    let ev = if bytes.is_null() {
        assert_eq!(len, 0, "NULL frame must come with len 0");
        None
    } else {
        // SAFETY: libunlatch passes `len` readable bytes valid for this call.
        let b = unsafe { std::slice::from_raw_parts(bytes, len) };
        Some(frame::decode_body::<IpcFrame<IpcResponse>>(&b[4..]).expect("reply decodes"))
    };
    s.events.lock().unwrap().push(ev);
    s.cv.notify_all();
}

fn submit(d: *const UnlatchDispatcher, call: u64, msg: IpcRequest) {
    let bytes = frame::encode(&IpcFrame { call, msg }, false).unwrap();
    // SAFETY: live dispatcher, valid buffer, no fd.
    assert!(unsafe { unlatch_dispatcher_submit(d, bytes.as_ptr(), bytes.len(), -1) });
}

fn final_for(ev: &[Option<IpcFrame<IpcResponse>>], call: u64) -> Option<&IpcResponse> {
    ev.iter()
        .flatten()
        .find(|f| f.call == call && !matches!(f.msg, IpcResponse::Progress { .. }))
        .map(|f| &f.msg)
}

#[test]
fn fault_close_reaches_the_host_as_a_null_frame() {
    std::env::set_var("UNLATCH_FAULT", "die_before_ipc_reply:delete");
    let dir = tempfile::tempdir().unwrap();
    let cfg = serde_json::json!({
        "name": "close-test",
        "transport": {"command": {"argv": ["/nonexistent/unlatchd", "stdio"]}},
        "remote_root": "/nowhere",
        "state_dir": dir.path(),
        "client_name": "test-mac",
    });
    let c = CString::new(cfg.to_string()).unwrap();
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: valid arguments; no event callback.
    let engine = unsafe { unlatch_engine_start(c.as_ptr(), None, std::ptr::null_mut(), &mut err) };
    assert!(!engine.is_null(), "engine start failed");

    let seen = Box::new(Seen::default());
    let ctx = &*seen as *const Seen as *mut c_void;
    // SAFETY: live engine; `seen` outlives the dispatcher.
    let d = unsafe { unlatch_dispatcher_new(engine, Some(on_reply), ctx) };
    assert!(!d.is_null());

    // Hello is checked against the engine's domain.
    submit(
        d,
        1,
        IpcRequest::Hello {
            proto: unlatch_proto::PROTO_VERSION,
            domain: "some-other-vm".into(),
        },
    );
    submit(
        d,
        2,
        IpcRequest::Hello {
            proto: unlatch_proto::PROTO_VERSION,
            domain: "close-test".into(),
        },
    );
    seen.wait("hello replies", |ev| {
        final_for(ev, 1).is_some() && final_for(ev, 2).is_some()
    });
    {
        let ev = seen.events.lock().unwrap();
        assert!(
            matches!(
                final_for(&ev, 1),
                Some(IpcResponse::Error {
                    code: ErrorCode::Protocol,
                    ..
                })
            ),
            "{ev:?}"
        );
        assert!(
            matches!(final_for(&ev, 2), Some(IpcResponse::Hello { domain, .. }) if domain == "close-test"),
            "{ev:?}"
        );
    }

    // The armed kind: the op runs (unknown id → Ok), its reply is withheld, the connection closes.
    submit(
        d,
        3,
        IpcRequest::Delete {
            id: ItemId(987_654),
            base: Default::default(),
            recursive: false,
        },
    );
    seen.wait("NULL frame", |ev| ev.iter().any(Option::is_none));
    // Nothing more arrives: later submissions are dropped and the close is signalled once.
    submit(d, 4, IpcRequest::Status);
    std::thread::sleep(Duration::from_millis(200));
    {
        let ev = seen.events.lock().unwrap();
        assert_eq!(ev.iter().filter(|e| e.is_none()).count(), 1, "{ev:?}");
        assert!(
            ev.last().unwrap().is_none(),
            "NULL frame is the last: {ev:?}"
        );
        assert!(final_for(&ev, 3).is_none(), "{ev:?}");
        assert!(final_for(&ev, 4).is_none(), "{ev:?}");
    }

    // SAFETY: freeing once; the free's own close must not signal again.
    unsafe { unlatch_dispatcher_free(d) };
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        seen.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.is_none())
            .count(),
        1
    );

    // A dispatcher that is freed without a fault never sees a NULL frame.
    let seen2 = Box::new(Seen::default());
    let ctx2 = &*seen2 as *const Seen as *mut c_void;
    // SAFETY: live engine; `seen2` outlives the dispatcher.
    let d2 = unsafe { unlatch_dispatcher_new(engine, Some(on_reply), ctx2) };
    submit(d2, 1, IpcRequest::Status);
    seen2.wait("status", |ev| final_for(ev, 1).is_some());
    // SAFETY: freeing once.
    unsafe { unlatch_dispatcher_free(d2) };
    std::thread::sleep(Duration::from_millis(50));
    assert!(seen2.events.lock().unwrap().iter().all(Option::is_some));

    // SAFETY: stopping once, after the dispatchers that pointed into it.
    unsafe { unlatch_engine_stop(engine) };
}
