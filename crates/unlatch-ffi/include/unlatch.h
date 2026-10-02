/*
 * unlatch.h — C ABI of libunlatch (crates/unlatch-ffi), the Rust engine linked into Unlatch.app (engine
 * agent) and UnlatchFileProvider.appex. Hand-written; `cargo test -p unlatch-ffi` checks that every
 * function declared here is exported by the library, and compiles + links a C program against it.
 *
 * Conventions
 *  - No function ever unwinds (panics are caught and reported as errors).
 *  - `char *` results and `UnlatchBytes` results are owned by the caller: release them with
 *    unlatch_free_string() / unlatch_free_bytes(). Pointers passed *into* libunlatch stay owned by the
 *    caller unless a comment says otherwise.
 *  - `char **out_error` (may be NULL) receives NULL on success, or on failure a JSON object
 *    `{"code": "...", "msg": "..."}` the caller frees with unlatch_free_string(). `code` is a
 *    unlatch_proto::ErrorCode variant name ("Offline", "NeedsUser", ...) or one of the FFI's own
 *    codes "InvalidArgument" and "Panic".
 *  - Frames are complete unlatch_proto frames: u32 little-endian length (of what follows), one
 *    flags byte, postcard payload. They carry IpcFrame<IpcRequest> / IpcFrame<IpcResponse>.
 *  - JSON forms are serde's: {"call": 7, "msg": {"Item": {"id": 5}}}, unit variants as bare
 *    strings ("Status"), byte vectors as arrays of numbers, ItemId as a number.
 *  - Callbacks run on libunlatch threads, possibly concurrently; `ctx` must be usable from any
 *    thread. The data pointers they receive are valid only for the duration of the call.
 */
#ifndef UNLATCH_H
#define UNLATCH_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handles. */
typedef struct UnlatchEngine UnlatchEngine;
typedef struct UnlatchDispatcher UnlatchDispatcher;

/* An owned byte buffer. ptr == NULL means "no buffer" (an error occurred). */
typedef struct UnlatchBytes {
    uint8_t *ptr;
    size_t len;
} UnlatchBytes;

/*
 * Engine event: JSON object with "type" one of
 *   "WorkingSetChanged" {domain, anchor: [u8]}   -> signalEnumerator(for: .workingSet)
 *   "ErrorResolved"     {domain}                  -> signalErrorResolved(.serverUnreachable)
 *   "Reimport"          {domain, below: ItemId}   -> reimportItems(below:)
 *   "NeedsUser"         {domain, reason, url}     -> show in the menu bar, no retry
 *   "StatusChanged"     {domain, status: EngineStatus}
 */
typedef void (*UnlatchEventCallback)(void *ctx, const char *event_json);

/* Reply frame from a dispatcher. `fd` is -1 (reserved for replies that carry a descriptor).
 * `frame == NULL && len == 0` means the engine closed the connection (fault injection
 * `die_before_ipc_reply:<kind>`: the op ran, its reply is withheld): the host should invalidate
 * its XPC connection. It is delivered at most once per dispatcher, after every reply frame the
 * dispatcher sent, and never after unlatch_dispatcher_free has returned; no frame follows it. The
 * host may call unlatch_dispatcher_free from inside it. */
typedef void (*UnlatchReplyCallback)(void *ctx, const uint8_t *frame, size_t len, int32_t fd);

/* ---- library info -------------------------------------------------------------------------- */

/* Static string, never freed. */
const char *unlatch_version(void);
/* unlatch_proto::PROTO_VERSION, for IpcRequest::Hello. */
uint32_t unlatch_proto_version(void);

/* ---- engine ---------------------------------------------------------------------------------- */

/*
 * Start an engine. `config_json` (schema in crates/unlatch-ffi/src/config.rs and docs/MACOS.md):
 *   {"name": domain id, "transport": {"ssh": {"destination", "port"?, "identity"?, "extra_args"?}}
 *    | {"command": {"argv": [...], "env"?: {...}}}, "remote_root", "state_dir" (absolute),
 *    "client_name", optional: cache_dir, temp_dir, unlatchd_command, remote_install_dir,
 *    unlatchd_upload, cache_budget, prefetch, default_lazy_names, ssh_env, askpass,
 *    list_timeout_ms, expose_exec, mass_delete_frac, mass_delete_abs, mass_delete_min}
 * Unknown keys are rejected. Loads the replica and starts connecting in the background; never
 * blocks on the network. `event_cb` may be NULL. Returns NULL on failure.
 */
UnlatchEngine *unlatch_engine_start(const char *config_json, UnlatchEventCallback event_cb, void *ctx,
                                char **out_error);

/* Flush, stop and free. After it returns `event_cb` is never called again. NULL is a no-op.
 * Safe to call from inside `event_cb` (the stop then completes on a helper thread). */
void unlatch_engine_stop(UnlatchEngine *engine);

/* EngineStatus as JSON (free with unlatch_free_string). NULL if `engine` is NULL. */
char *unlatch_engine_status_json(const UnlatchEngine *engine);

/* Network path changed or the Mac woke up: reconnect now if not live. */
void unlatch_engine_network_changed(const UnlatchEngine *engine);

/* Connect now with interactive auth (SSH_ASKPASS). BLOCKS until connected or failed. */
bool unlatch_engine_connect_interactive(const UnlatchEngine *engine, char **out_error);

/* Resolve a paused mass deletion: apply = true deletes, false keeps the files. */
bool unlatch_engine_confirm_paused(const UnlatchEngine *engine, bool apply, char **out_error);

/* Run one IPC request in-process (no fd). `request_json` is an IpcFrame<IpcRequest>; the result
 * is the final IpcFrame<IpcResponse> as JSON (progress frames dropped). BLOCKS. */
char *unlatch_engine_call_json(const UnlatchEngine *engine, const char *request_json, char **out_error);

/* ---- XPC bridge ------------------------------------------------------------------------------ */

/* One dispatcher per XPC connection. `engine` must outlive it. NULL on failure. The extension's
 * first frame must be Hello with this engine's domain name ("name" in the config); any other
 * domain is answered Error{code: Protocol}. Create/Modify with content stream Progress frames
 * before their final reply, like Fetch. */
UnlatchDispatcher *unlatch_dispatcher_new(const UnlatchEngine *engine, UnlatchReplyCallback reply_cb, void *ctx);

/* Submit one request frame (non-blocking). `fd` (or -1) is the content file of a Create/Modify
 * with has_content; its ownership passes to libunlatch in every case. false = malformed frame. */
bool unlatch_dispatcher_submit(const UnlatchDispatcher *disp, const uint8_t *frame, size_t len, int32_t fd);

/* Cancel the connection's outstanding calls and free. After it returns `reply_cb` is never
 * called again. NULL is a no-op. Safe to call from inside `reply_cb`. */
void unlatch_dispatcher_free(UnlatchDispatcher *disp);

/* ---- JSON <-> frame helpers ------------------------------------------------------------------ */

UnlatchBytes unlatch_ipc_encode_request_json(const char *json, char **out_error);
UnlatchBytes unlatch_ipc_encode_response_json(const char *json, char **out_error);
char *unlatch_ipc_decode_request_json(const uint8_t *frame, size_t len, char **out_error);
char *unlatch_ipc_decode_response_json(const uint8_t *frame, size_t len, char **out_error);

/* ---- ownership ------------------------------------------------------------------------------- */

void unlatch_free_string(char *s);
void unlatch_free_bytes(UnlatchBytes bytes);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* UNLATCH_H */
