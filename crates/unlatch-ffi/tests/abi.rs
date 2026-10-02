//! The C surface: header ⇄ exports, a real C program linked against libunlatch.a, NULL-safety,
//! panic containment at the boundary, and descriptor ownership.

use std::collections::BTreeSet;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Command;
use unlatch::*;

fn header() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("include/unlatch.h"))
        .expect("read unlatch.h")
}

/// Function names declared in unlatch.h (`… unlatch_xxx(` at the start of a declaration).
fn declared_functions() -> BTreeSet<String> {
    let text = header();
    let mut out = BTreeSet::new();
    let mut in_comment = false;
    for line in text.lines() {
        let l = line.trim();
        if in_comment {
            in_comment = !l.contains("*/");
            continue;
        }
        if l.starts_with("/*") {
            in_comment = !l.contains("*/");
            continue;
        }
        if l.starts_with("typedef") || l.starts_with('#') {
            continue;
        }
        if let Some(open) = l.find('(') {
            let head = &l[..open];
            if let Some(name) = head
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .rfind(|s| !s.is_empty())
            {
                if name.starts_with("unlatch_") {
                    out.insert(name.to_owned());
                }
            }
        }
    }
    out
}

/// Every exported function, by address — this fails to compile if one is renamed on the Rust side.
fn rust_exports() -> Vec<(&'static str, usize)> {
    vec![
        ("unlatch_version", unlatch_version as *const () as usize),
        (
            "unlatch_proto_version",
            unlatch_proto_version as *const () as usize,
        ),
        (
            "unlatch_engine_start",
            unlatch_engine_start as *const () as usize,
        ),
        (
            "unlatch_engine_stop",
            unlatch_engine_stop as *const () as usize,
        ),
        (
            "unlatch_engine_status_json",
            unlatch_engine_status_json as *const () as usize,
        ),
        (
            "unlatch_engine_network_changed",
            unlatch_engine_network_changed as *const () as usize,
        ),
        (
            "unlatch_engine_connect_interactive",
            unlatch_engine_connect_interactive as *const () as usize,
        ),
        (
            "unlatch_engine_confirm_paused",
            unlatch_engine_confirm_paused as *const () as usize,
        ),
        (
            "unlatch_engine_call_json",
            unlatch_engine_call_json as *const () as usize,
        ),
        (
            "unlatch_dispatcher_new",
            unlatch_dispatcher_new as *const () as usize,
        ),
        (
            "unlatch_dispatcher_submit",
            unlatch_dispatcher_submit as *const () as usize,
        ),
        (
            "unlatch_dispatcher_free",
            unlatch_dispatcher_free as *const () as usize,
        ),
        (
            "unlatch_ipc_encode_request_json",
            unlatch_ipc_encode_request_json as *const () as usize,
        ),
        (
            "unlatch_ipc_encode_response_json",
            unlatch_ipc_encode_response_json as *const () as usize,
        ),
        (
            "unlatch_ipc_decode_request_json",
            unlatch_ipc_decode_request_json as *const () as usize,
        ),
        (
            "unlatch_ipc_decode_response_json",
            unlatch_ipc_decode_response_json as *const () as usize,
        ),
        (
            "unlatch_free_string",
            unlatch_free_string as *const () as usize,
        ),
        (
            "unlatch_free_bytes",
            unlatch_free_bytes as *const () as usize,
        ),
    ]
}

#[test]
fn header_matches_rust_exports() {
    let declared = declared_functions();
    let exported: BTreeSet<String> = rust_exports()
        .iter()
        .map(|(n, p)| {
            assert_ne!(*p, 0);
            n.to_string()
        })
        .collect();
    assert_eq!(declared, exported);
}

/// `target/<profile>/libunlatch.a` next to this test binary (`target/<profile>/deps/abi-*`).
fn staticlib() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?;
    let p = profile_dir.join("libunlatch.a");
    p.exists().then_some(p)
}

fn tool(name: &str) -> bool {
    Command::new(name).arg("--version").output().is_ok()
}

#[test]
fn staticlib_exports_every_declared_symbol() {
    let Some(lib) = staticlib() else {
        eprintln!("SKIP: libunlatch.a not found next to the test binary");
        return;
    };
    if !tool("nm") {
        eprintln!("SKIP: nm not available");
        return;
    }
    let out = Command::new("nm")
        .arg("-g")
        .arg("--defined-only")
        .arg(&lib)
        .output()
        .expect("run nm");
    assert!(
        out.status.success(),
        "nm failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let defined: BTreeSet<&str> = text
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace().rev();
            let sym = parts.next()?;
            let kind = parts.next()?;
            (kind == "T").then(|| sym.trim_start_matches('_'))
        })
        .collect();
    for f in declared_functions() {
        assert!(
            defined.contains(f.as_str()),
            "{f} declared in unlatch.h but not exported by libunlatch.a"
        );
    }
}

const C_PROGRAM: &str = r#"
#include "unlatch.h"
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>

static int events = 0;
static void on_event(void *ctx, const char *json) { (void)ctx; (void)json; events++; }
static void on_reply(void *ctx, const uint8_t *f, size_t n, int32_t fd) { (void)ctx; (void)f; (void)n; (void)fd; }

int main(void) {
    char *err = NULL;
    if (strlen(unlatch_version()) == 0 || unlatch_proto_version() == 0) return 10;

    UnlatchBytes b = unlatch_ipc_encode_request_json("{\"call\":5,\"msg\":{\"Item\":{\"id\":12}}}", &err);
    if (b.ptr == NULL || err != NULL) return 11;
    char *json = unlatch_ipc_decode_request_json(b.ptr, b.len, &err);
    if (json == NULL || strcmp(json, "{\"call\":5,\"msg\":{\"Item\":{\"id\":12}}}") != 0) return 12;
    unlatch_free_string(json);
    unlatch_free_bytes(b);

    b = unlatch_ipc_encode_response_json("{\"call\":5,\"msg\":\"Deleted\"}", NULL);
    json = unlatch_ipc_decode_response_json(b.ptr, b.len, NULL);
    if (json == NULL || strstr(json, "Deleted") == NULL) return 13;
    unlatch_free_string(json);
    unlatch_free_bytes(b);

    b = unlatch_ipc_encode_request_json("{\"call\":", &err);
    if (b.ptr != NULL || err == NULL || strstr(err, "InvalidArgument") == NULL) return 14;
    unlatch_free_string(err);
    err = NULL;

    UnlatchEngine *e = unlatch_engine_start("{\"name\":\"x\"}", on_event, NULL, &err);
    if (e != NULL || err == NULL) return 15;
    unlatch_free_string(err);
    err = NULL;

    /* NULL handles are errors, never crashes. */
    unlatch_engine_stop(NULL);
    unlatch_engine_network_changed(NULL);
    if (unlatch_engine_status_json(NULL) != NULL) return 16;
    if (unlatch_engine_connect_interactive(NULL, &err)) return 17;
    unlatch_free_string(err);
    err = NULL;
    if (unlatch_engine_confirm_paused(NULL, true, NULL)) return 18;
    if (unlatch_engine_call_json(NULL, "{}", NULL) != NULL) return 19;
    if (unlatch_dispatcher_new(NULL, on_reply, NULL) != NULL) return 20;
    if (unlatch_dispatcher_submit(NULL, NULL, 0, -1)) return 21;
    unlatch_dispatcher_free(NULL);
    unlatch_free_string(NULL);
    UnlatchBytes none = {0};
    unlatch_free_bytes(none);
    puts("ok");
    return 0;
}
"#;

#[test]
fn c_program_compiles_links_and_runs() {
    let Some(lib) = staticlib() else {
        eprintln!("SKIP: libunlatch.a not found next to the test binary");
        return;
    };
    if !tool("cc") {
        eprintln!("SKIP: no C compiler");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("main.c");
    std::fs::write(&src, C_PROGRAM).expect("write C source");
    let exe = dir.path().join("main");
    let include = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
    let mut cc = Command::new("cc");
    cc.args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&exe)
        .arg(&src)
        .arg("-I")
        .arg(&include)
        .arg(&lib);
    if cfg!(target_os = "macos") {
        cc.args([
            "-framework",
            "CoreFoundation",
            "-framework",
            "Security",
            "-liconv",
        ]);
    } else {
        cc.args(["-lpthread", "-ldl", "-lm"]);
    }
    let out = cc.output().expect("run cc");
    assert!(
        out.status.success(),
        "cc failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = Command::new(&exe).output().expect("run C program");
    assert!(
        run.status.success(),
        "C program exited {:?}\nstdout: {}\nstderr: {}",
        run.status.code(),
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "ok");
}

fn take(p: *mut c_char) -> String {
    assert!(!p.is_null());
    // SAFETY: libunlatch string, freed once.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    // SAFETY: as above.
    unsafe { unlatch_free_string(p) };
    s
}

unsafe extern "C" fn count_event(ctx: *mut c_void, _json: *const c_char) {
    // SAFETY: ctx is the AtomicUsize the test passed and keeps alive.
    let n = unsafe { &*(ctx as *const std::sync::atomic::AtomicUsize) };
    n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

#[test]
fn engine_start_rejects_bad_config_with_json_error() {
    for bad in [
        "",
        "{}",
        "[1]",
        r#"{"name":"x","transport":{"ssh":{"destination":"a"}},"remote_root":"/r","state_dir":"rel","client_name":"m"}"#,
    ] {
        let c = CString::new(bad).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid arguments; NULL callback allowed.
        let e = unsafe { unlatch_engine_start(c.as_ptr(), None, std::ptr::null_mut(), &mut err) };
        assert!(e.is_null(), "{bad}");
        let v: serde_json::Value = serde_json::from_str(&take(err)).unwrap();
        assert_eq!(v["code"], "InvalidArgument", "{bad}");
    }
}

/// With a valid config the call must return either a live handle or NULL + error — whatever the
/// engine does inside (including panicking while unlatch-core is still being written), nothing may
/// unwind across the boundary.
#[test]
fn engine_start_never_unwinds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = serde_json::json!({
        "name": "t",
        "transport": {"command": {"argv": ["/nonexistent/unlatchd", "stdio"]}},
        "remote_root": "/tmp/nowhere",
        "state_dir": dir.path(),
        "client_name": "test",
    });
    let c = CString::new(cfg.to_string()).unwrap();
    let n = std::sync::atomic::AtomicUsize::new(0);
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: valid arguments; `n` outlives the engine (stopped below before `n` drops).
    let e = unsafe {
        unlatch_engine_start(
            c.as_ptr(),
            Some(count_event),
            &n as *const _ as *mut c_void,
            &mut err,
        )
    };
    if e.is_null() {
        let v: serde_json::Value = serde_json::from_str(&take(err)).unwrap();
        assert!(v["code"].is_string() && v["msg"].is_string());
    } else {
        assert!(err.is_null());
        // SAFETY: live handle.
        let status = unsafe { unlatch_engine_status_json(e) };
        if !status.is_null() {
            let v: serde_json::Value = serde_json::from_str(&take(status)).unwrap();
            assert!(v.get("state").is_some());
        }
        // SAFETY: live handle; not used afterwards.
        unsafe { unlatch_engine_stop(e) };
        let after = n.load(std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            n.load(std::sync::atomic::Ordering::SeqCst),
            after,
            "no events after stop"
        );
    }
}

#[test]
fn submit_takes_ownership_of_fd_even_on_rejection() {
    use std::io::Read;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;
    // Hand libunlatch one end of a socket pair: once it closes that end, reading the other end
    // sees EOF. (Checking /proc/self/fd/N would race with other tests reusing the number.)
    let (mine, theirs) = UnixStream::pair().expect("socketpair");
    let fd = theirs.into_raw_fd();
    // SAFETY: NULL dispatcher is an error; `fd` is ours to give away.
    assert!(!unsafe { unlatch_dispatcher_submit(std::ptr::null(), [0u8; 3].as_ptr(), 3, fd) });
    let mut mine = mine;
    mine.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 1];
    let n = mine
        .read(&mut buf)
        .expect("peer end must be closed, not left open");
    assert_eq!(n, 0, "fd must be closed after a rejected submit");
}

#[test]
fn null_and_garbage_are_rejected_without_crashing() {
    // SAFETY: every call below passes NULL or valid pointers, which the API documents as errors.
    unsafe {
        assert!(unlatch_dispatcher_new(std::ptr::null(), None, std::ptr::null_mut()).is_null());
        assert!(!unlatch_dispatcher_submit(
            std::ptr::null(),
            std::ptr::null(),
            0,
            -1
        ));
        unlatch_dispatcher_free(std::ptr::null_mut());
        unlatch_engine_stop(std::ptr::null_mut());
        unlatch_engine_network_changed(std::ptr::null());
        assert!(unlatch_engine_status_json(std::ptr::null()).is_null());
        let mut err: *mut c_char = std::ptr::null_mut();
        assert!(!unlatch_engine_connect_interactive(
            std::ptr::null(),
            &mut err
        ));
        assert!(take(err).contains("InvalidArgument"));
        assert!(!unlatch_engine_confirm_paused(
            std::ptr::null(),
            false,
            std::ptr::null_mut()
        ));
        let req = CString::new(r#"{"call":1,"msg":"Status"}"#).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        assert!(unlatch_engine_call_json(std::ptr::null(), req.as_ptr(), &mut err).is_null());
        assert!(take(err).contains("engine is NULL"));
        assert!(!CStr::from_ptr(unlatch_version()).to_bytes().is_empty());
        assert_eq!(unlatch_proto_version(), unlatch_proto::PROTO_VERSION);
    }
}
