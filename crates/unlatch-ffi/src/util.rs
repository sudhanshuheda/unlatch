//! Shared FFI plumbing: panic containment, string/byte ownership, error reporting, callback slots.

use serde::Serialize;
use std::any::Any;
use std::cell::Cell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::RwLock;

/// Error codes the FFI adds on top of `unlatch_proto::ErrorCode` (which covers engine errors).
pub(crate) const CODE_INVALID_ARGUMENT: &str = "InvalidArgument";
pub(crate) const CODE_PANIC: &str = "Panic";

/// The error object written to `out_error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct FfiError {
    pub code: String,
    pub msg: String,
}

impl FfiError {
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Self {
            code: CODE_INVALID_ARGUMENT.into(),
            msg: msg.into(),
        }
    }

    pub(crate) fn panic(payload: &(dyn Any + Send)) -> Self {
        Self {
            code: CODE_PANIC.into(),
            msg: format!("internal panic: {}", panic_message(payload)),
        }
    }
}

impl From<unlatch_proto::ProtoError> for FfiError {
    fn from(e: unlatch_proto::ProtoError) -> Self {
        // `ErrorCode` serializes as its bare variant name, the same string Swift switches on.
        let code = serde_json::to_value(e.code)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", e.code));
        Self { code, msg: e.msg }
    }
}

pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Run `f`, converting a panic into `Err(FfiError{code:"Panic"})`. Never unwinds.
pub(crate) fn guard<T>(f: impl FnOnce() -> Result<T, FfiError>) -> Result<T, FfiError> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let e = FfiError::panic(payload.as_ref());
            tracing::error!(target: "unlatch_ffi", "{}", e.msg);
            Err(e)
        }
    }
}

/// Like [`guard`] for functions without an error channel: a panic yields `default`.
pub(crate) fn guard_or<T>(default: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(payload) => {
            tracing::error!(target: "unlatch_ffi", "internal panic: {}", panic_message(payload.as_ref()));
            default
        }
    }
}

/// Borrow a caller-supplied C string as UTF-8.
///
/// # Safety
/// `p` must be null or point to a NUL-terminated string valid for the returned lifetime.
pub(crate) unsafe fn str_arg<'a>(p: *const c_char, what: &str) -> Result<&'a str, FfiError> {
    if p.is_null() {
        return Err(FfiError::invalid(format!("{what} is NULL")));
    }
    // SAFETY: non-null and NUL-terminated per the caller contract.
    let c = unsafe { CStr::from_ptr(p) };
    c.to_str()
        .map_err(|_| FfiError::invalid(format!("{what} is not valid UTF-8")))
}

/// Borrow a caller-supplied byte buffer.
///
/// # Safety
/// `ptr` must be null (only with `len == 0`) or point to `len` readable bytes.
pub(crate) unsafe fn bytes_arg<'a>(
    ptr: *const u8,
    len: usize,
    what: &str,
) -> Result<&'a [u8], FfiError> {
    if ptr.is_null() {
        if len == 0 {
            return Ok(&[]);
        }
        return Err(FfiError::invalid(format!("{what} is NULL")));
    }
    // SAFETY: non-null and `len` readable bytes per the caller contract.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Hand a Rust string to C (caller frees with `unlatch_free_string`). Interior NULs, which JSON
/// never produces but error messages might, are replaced so the string is never truncated
/// silently into something else.
pub(crate) fn into_c_string(s: String) -> *mut c_char {
    let c = CString::new(s).unwrap_or_else(|e| {
        let mut bytes = e.into_vec();
        for b in bytes.iter_mut().filter(|b| **b == 0) {
            *b = b'?';
        }
        // No NULs remain, so this cannot fail; fall back to an empty string rather than panic.
        CString::new(bytes).unwrap_or_default()
    });
    c.into_raw()
}

/// Store `err` as JSON in `*out` when `out` is non-null. Always overwrites (never frees) `*out`.
///
/// # Safety
/// `out` must be null or valid for one pointer write.
pub(crate) unsafe fn write_error(out: *mut *mut c_char, err: &FfiError) {
    if out.is_null() {
        return;
    }
    let json = serde_json::to_string(err).unwrap_or_else(|_| {
        format!(
            "{{\"code\":\"{}\",\"msg\":\"unserializable error\"}}",
            err.code
        )
    });
    // SAFETY: `out` is non-null and writable per the caller contract.
    unsafe { *out = into_c_string(json) };
}

/// Clear `*out` (set to NULL) so callers can rely on "NULL = no error".
///
/// # Safety
/// `out` must be null or valid for one pointer write.
pub(crate) unsafe fn clear_error(out: *mut *mut c_char) {
    if !out.is_null() {
        // SAFETY: as above.
        unsafe { *out = std::ptr::null_mut() };
    }
}

/// Owned byte buffer handed to C. `ptr == NULL` means "no buffer" (an error).
#[repr(C)]
#[derive(Debug)]
pub struct UnlatchBytes {
    pub ptr: *mut u8,
    pub len: usize,
}

impl UnlatchBytes {
    pub(crate) fn null() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            len: 0,
        }
    }

    pub(crate) fn from_vec(v: Vec<u8>) -> Self {
        let boxed = v.into_boxed_slice();
        let len = boxed.len();
        let ptr = Box::into_raw(boxed).cast::<u8>();
        Self { ptr, len }
    }
}

/// Free a string returned by any `unlatch_*` function. NULL is a no-op.
///
/// # Safety
/// `s` must be NULL or a pointer previously returned by libunlatch and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn unlatch_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    guard_or((), || {
        // SAFETY: produced by `CString::into_raw` in `into_c_string`, not freed yet.
        drop(unsafe { CString::from_raw(s) });
    });
}

/// Free a buffer returned by any `unlatch_*` function. A NULL `ptr` is a no-op.
///
/// # Safety
/// `bytes` must be a value previously returned by libunlatch and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn unlatch_free_bytes(bytes: UnlatchBytes) {
    if bytes.ptr.is_null() {
        return;
    }
    guard_or((), || {
        let slice = std::ptr::slice_from_raw_parts_mut(bytes.ptr, bytes.len);
        // SAFETY: produced by `Box::<[u8]>::into_raw` in `UnlatchBytes::from_vec` with this length.
        drop(unsafe { Box::from_raw(slice) });
    });
}

/// A caller-supplied context pointer. The C contract says the callback and its context may be
/// used from any thread, which is what makes these impls sound.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CtxPtr(pub *mut c_void);
// SAFETY: see type docs; libunlatch never dereferences the pointer, it only passes it back.
unsafe impl Send for CtxPtr {}
// SAFETY: as above.
unsafe impl Sync for CtxPtr {}

thread_local! {
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

/// True while this thread is inside a C callback invoked by libunlatch.
pub(crate) fn in_callback() -> bool {
    IN_CALLBACK.with(Cell::get)
}

/// A callback that can be revoked. After [`CallbackSlot::revoke`] returns, the callback is not
/// running on any thread and will never be called again — so the C side may free `ctx` then.
pub(crate) struct CallbackSlot<T> {
    inner: RwLock<Option<T>>,
}

impl<T> CallbackSlot<T> {
    pub(crate) fn new(v: Option<T>) -> Self {
        Self {
            inner: RwLock::new(v),
        }
    }

    /// Invoke `f` with the callback, unless revoked. Concurrent invocations are allowed.
    pub(crate) fn with(&self, f: impl FnOnce(&T)) {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if let Some(cb) = g.as_ref() {
            IN_CALLBACK.with(|c| {
                let prev = c.replace(true);
                f(cb);
                c.set(prev);
            });
        }
    }

    /// Revoke the callback, waiting for in-flight invocations. Must not be called from inside
    /// the callback on the same thread (it would wait for itself); callers check [`in_callback`].
    pub(crate) fn revoke(&self) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *g = None;
    }

    #[cfg(test)]
    pub(crate) fn is_live(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn guard_contains_panics() {
        let r: Result<(), FfiError> = guard(|| panic!("boom"));
        let e = r.unwrap_err();
        assert_eq!(e.code, CODE_PANIC);
        assert!(e.msg.contains("boom"));
        assert_eq!(guard_or(7, || panic!("x")), 7);
        assert_eq!(guard_or(7, || 3), 3);
    }

    #[test]
    fn c_string_interior_nul_is_replaced() {
        let p = into_c_string("a\0b".to_owned());
        // SAFETY: fresh pointer from into_c_string.
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned();
        assert_eq!(s, "a?b");
        // SAFETY: freeing our own allocation once.
        unsafe { unlatch_free_string(p) };
    }

    #[test]
    fn bytes_roundtrip_and_null_free() {
        let b = UnlatchBytes::from_vec(vec![1, 2, 3]);
        // SAFETY: b.ptr points to 3 bytes we own.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(b.ptr, b.len) },
            &[1, 2, 3]
        );
        // SAFETY: freeing once.
        unsafe { unlatch_free_bytes(b) };
        // SAFETY: null is documented as a no-op.
        unsafe { unlatch_free_bytes(UnlatchBytes::null()) };
        // SAFETY: empty buffers are valid allocations too.
        unsafe { unlatch_free_bytes(UnlatchBytes::from_vec(Vec::new())) };
        // SAFETY: null is documented as a no-op.
        unsafe { unlatch_free_string(std::ptr::null_mut()) };
    }

    #[test]
    fn proto_error_code_is_variant_name() {
        let e: FfiError =
            unlatch_proto::ProtoError::new(unlatch_proto::ErrorCode::DeletionRejected, "x").into();
        assert_eq!(e.code, "DeletionRejected");
    }

    #[test]
    fn revoked_slot_never_calls() {
        let slot = Arc::new(CallbackSlot::new(Some(())));
        let n = Arc::new(AtomicUsize::new(0));
        slot.with(|_| {
            assert!(in_callback());
            n.fetch_add(1, Ordering::SeqCst);
        });
        assert!(!in_callback());
        assert!(slot.is_live());
        slot.revoke();
        slot.with(|_| {
            n.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn revoke_waits_for_inflight_callback() {
        let slot = Arc::new(CallbackSlot::new(Some(())));
        let entered = Arc::new(std::sync::Barrier::new(2));
        let done = Arc::new(AtomicUsize::new(0));
        let t = {
            let (slot, entered, done) = (slot.clone(), entered.clone(), done.clone());
            std::thread::spawn(move || {
                slot.with(|_| {
                    entered.wait();
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    done.store(1, Ordering::SeqCst);
                })
            })
        };
        entered.wait();
        slot.revoke();
        // revoke() could only return after the in-flight callback finished.
        assert_eq!(done.load(Ordering::SeqCst), 1);
        t.join().unwrap();
    }
}
