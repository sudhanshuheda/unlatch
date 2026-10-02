//! Mutations: create / modify / delete with replay-stable op ids (rule 1, D1), create never
//! surfacing `Exists` (rule 2, MQ-014), conflicts that never error (rules 3–4, MQ-013),
//! Mac-only metadata kept local (rule 5), and the delete contract (rule 6, D7).

use super::names::{is_bounce_of, numbered};
use super::replica::{DeleteSeen, Source};
use super::session::{Reply, SessionHandle, Sink};
use super::Shared;
use crate::{err, CancelToken, CreateKind, CreateRequest, Modified, ModifyRequest, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc as smpsc;
use std::time::{Duration, Instant};
use unlatch_proto::frame::BULK_CHUNK;
use unlatch_proto::ipc::{fields, IpcItem, LocalMeta};
use unlatch_proto::wire::{Request, Response};
use unlatch_proto::{BaseVersion, Entry, ErrorCode, ItemId, Kind, OpId, Version};

/// Fields kept on the Mac only (never sent to the VM).
pub(crate) const LOCAL_FIELDS: u32 = fields::LAST_USED_DATE
    | fields::TAG_DATA
    | fields::FAVORITE_RANK
    | fields::CREATION_DATE
    | fields::EXTENDED_ATTRIBUTES
    | fields::TYPE_AND_CREATOR;
/// Every field the engine handles; the rest come back in `still_pending`.
pub(crate) const KNOWN_FIELDS: u32 = LOCAL_FIELDS
    | fields::CONTENTS
    | fields::FILENAME
    | fields::PARENT
    | fields::CONTENT_MODIFICATION_DATE
    | fields::FILE_SYSTEM_FLAGS;
const MAX_NAME_ATTEMPTS: u32 = 100;

// ---- op ids -----------------------------------------------------------------------------------

fn op_hash(parts: &[&[u8]]) -> OpId {
    let mut h = blake3::Hasher::new_derive_key("unlatch 2026 op id v1");
    for p in parts {
        h.update(&(p.len() as u64).to_le_bytes());
        h.update(p);
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}

fn base_bytes(b: BaseVersion) -> Vec<u8> {
    let mut v = Vec::with_capacity(18);
    for c in [b.content, b.meta] {
        match c {
            Some(x) => {
                v.push(1);
                v.extend_from_slice(&x.to_le_bytes());
            }
            None => v.push(0),
        }
    }
    v
}

/// Create: `H(domain, template_id)`; later name candidates (`name 2.ext`, …) add the name so
/// each attempt is its own idempotent op.
pub(crate) fn create_op(domain: &str, template_id: &str, candidate: Option<&str>) -> OpId {
    match candidate {
        None => op_hash(&[b"create", domain.as_bytes(), template_id.as_bytes()]),
        Some(n) => op_hash(&[
            b"create",
            domain.as_bytes(),
            template_id.as_bytes(),
            n.as_bytes(),
        ]),
    }
}

/// Modify: `H(id, base, changed_fields, blake3(content))`, plus the sub-op (`rename`, `write`,
/// `setattr`) and its argument, since one modify may issue several wire ops.
pub(crate) fn modify_op(
    id: ItemId,
    base: BaseVersion,
    changed: u32,
    hash: Option<&[u8; 32]>,
    sub: &str,
    arg: &str,
) -> OpId {
    let h: &[u8] = hash.map_or(&[], |h| h.as_slice());
    op_hash(&[
        b"modify",
        &id.0.to_le_bytes(),
        &base_bytes(base),
        &changed.to_le_bytes(),
        h,
        sub.as_bytes(),
        arg.as_bytes(),
    ])
}

/// Delete: `H(id, base)` plus `recursive` and `seen_seq`. A directory's base does not move when
/// its children change, so with `H(id, base)` alone a new delete after a partial one (`kept`)
/// would be answered from the daemon's ops table with the stale refusal for 7 days. A replay of
/// the same system call reuses the `seen_seq` of its first attempt (`DeleteSeen`, persisted), so
/// it is the same op; a new call after the system re-enumerated the folder gets a fresh one.
pub(crate) fn delete_op(id: ItemId, base: BaseVersion, recursive: bool, seen_seq: u64) -> OpId {
    op_hash(&[
        b"delete",
        &id.0.to_le_bytes(),
        &base_bytes(base),
        &[recursive as u8],
        &seen_seq.to_le_bytes(),
    ])
}

// ---- upload staging ---------------------------------------------------------------------------

/// Progress + cancellation of one create/modify upload (`Engine::create_with`/`modify_with`).
pub(crate) struct Xfer<'a> {
    pub progress: &'a dyn Fn(u64, u64),
    pub cancel: &'a CancelToken,
}

impl Xfer<'_> {
    fn check(&self) -> Result<()> {
        if self.cancel.is_cancelled() {
            Err(err(ErrorCode::Cancelled, "cancelled"))
        } else {
            Ok(())
        }
    }
}

/// How often a cancelled upload is noticed while waiting for unlatchd's final reply.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Upload content copied into `temp_dir` (blake3 computed) before anything is sent, so the
/// bytes can't change under the hash and a replay has the same op id.
pub(crate) struct Staged {
    path: PathBuf,
    pub size: u64,
    pub hash: [u8; 32],
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn io(what: &str, e: std::io::Error) -> unlatch_proto::ProtoError {
    let code = if e.raw_os_error() == Some(libc::ENOSPC) {
        ErrorCode::NoSpace
    } else {
        ErrorCode::Io
    };
    err(code, format!("{what}: {e}"))
}

pub(crate) fn stage(sh: &Shared, file: Option<std::fs::File>, x: &Xfer<'_>) -> Result<Staged> {
    x.check()?;
    let path = sh
        .cfg
        .temp_dir
        .join(format!("stage-{:032x}", rand::random::<u128>()));
    let mut out = std::fs::File::create(&path).map_err(|e| io("stage upload", e))?;
    let mut staged = Staged {
        path,
        size: 0,
        hash: [0; 32],
    };
    let mut hasher = blake3::Hasher::new();
    let mut size = 0u64;
    if let Some(mut f) = file {
        // The fd may have been read from already; stage the whole file when seekable.
        let _ = f.seek(SeekFrom::Start(0));
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io("read upload content", e)),
            };
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| io("stage upload", e))?;
            size += n as u64;
            x.check()?;
        }
    }
    out.flush().map_err(|e| io("stage upload", e))?;
    staged.size = size;
    staged.hash = *hasher.finalize().as_bytes();
    Ok(staged)
}

/// Put an uploaded file's bytes into the cache as `(id, ver)` (a fetch right after a save is local).
fn adopt(sh: &Shared, staged: Staged, id: ItemId, ver: u64) {
    if sh.cache.adopt(id, ver, &staged.path).is_err() {
        tracing::debug!("could not cache uploaded content of {id}");
    }
}

/// Send a `Write` and stream its chunks on the bulk lane.
fn upload(
    sh: &Shared,
    s: &SessionHandle,
    req: Request,
    staged: &Staged,
    x: &Xfer<'_>,
) -> Result<Response> {
    x.check()?;
    sh.uploads.fetch_add(1, Ordering::Relaxed);
    let r = upload_inner(sh, s, req, staged, x);
    sh.uploads.fetch_sub(1, Ordering::Relaxed);
    r
}

fn map_reply(r: Reply) -> Result<Response> {
    match r {
        Reply::Resp(r) => Ok(r),
        Reply::Err(e) => Err(e),
        Reply::Chunk { .. } => Err(err(ErrorCode::Protocol, "unexpected ReadChunk")),
    }
}

fn upload_inner(
    sh: &Shared,
    s: &SessionHandle,
    req: Request,
    staged: &Staged,
    x: &Xfer<'_>,
) -> Result<Response> {
    let mut f = std::fs::File::open(&staged.path).map_err(|e| io("open staged upload", e))?;
    let (tx, rx) = smpsc::channel();
    let rid = s.request(req, Sink::Chan(tx))?;
    let abort = |e: unlatch_proto::ProtoError| {
        s.cancel(rid);
        s.forget(rid);
        e
    };
    (x.progress)(0, staged.size);
    let mut buf = vec![0u8; BULK_CHUNK];
    let mut sent = 0u64;
    loop {
        // The server may answer before all chunks (e.g. a replayed op or NotFound).
        match rx.try_recv() {
            Ok(r) => return map_reply(r),
            Err(smpsc::TryRecvError::Disconnected) => {
                return Err(err(ErrorCode::Offline, "connection lost"))
            }
            Err(smpsc::TryRecvError::Empty) => {}
        }
        // Checked between chunks: a chunk already queued on the bulk lane still goes out, then
        // the `Cancel` makes unlatchd discard the staged data (and grant its credit back).
        x.check().map_err(abort)?;
        let mut n = 0;
        let want = s.upload_chunk_size().min(buf.len());
        while n < want && sent + (n as u64) < staged.size {
            match f.read(&mut buf[n..want]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(abort(io("read staged upload", e))),
            }
        }
        sent += n as u64;
        let last = sent >= staged.size || n == 0;
        s.send_chunk(rid, buf[..n].to_vec(), last)?;
        (x.progress)(sent, staged.size);
        if last {
            break;
        }
    }
    let deadline = Instant::now() + sh.timing.reply_timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            s.forget(rid);
            return Err(err(
                ErrorCode::Timeout,
                "unlatchd did not confirm the upload",
            ));
        }
        match rx.recv_timeout(left.min(CANCEL_POLL)) {
            Ok(r) => return map_reply(r),
            Err(smpsc::RecvTimeoutError::Timeout) => x.check().map_err(abort)?,
            Err(smpsc::RecvTimeoutError::Disconnected) => {
                return Err(err(ErrorCode::Offline, "connection lost"))
            }
        }
    }
}

// ---- helpers ------------------------------------------------------------------------------------

/// Merge the Mac-only fields named in `changed` from `new` into `cur`.
pub(crate) fn merge_local(cur: &LocalMeta, new: &LocalMeta, changed: u32) -> LocalMeta {
    let mut m = cur.clone();
    if changed & fields::TAG_DATA != 0 {
        m.tag_data = new.tag_data.clone();
    }
    if changed & fields::LAST_USED_DATE != 0 {
        m.last_used_ns = new.last_used_ns;
    }
    if changed & fields::FAVORITE_RANK != 0 {
        m.favorite_rank = new.favorite_rank;
    }
    if changed & fields::CREATION_DATE != 0 {
        m.creation_ns = new.creation_ns;
    }
    if changed & fields::EXTENDED_ATTRIBUTES != 0 {
        m.xattrs = new.xattrs.clone();
    }
    if changed & fields::TYPE_AND_CREATOR != 0 {
        m.type_creator = new.type_creator;
    }
    if changed & fields::FILE_SYSTEM_FLAGS != 0 {
        m.hidden = new.hidden;
    }
    m
}

fn set_local(sh: &Shared, id: ItemId, meta: LocalMeta) -> Result<()> {
    sh.apply_msg_wait(|done| super::applier::ApplyMsg::LocalMeta { id, meta, done })
}

fn same_kind(it: &IpcItem, k: CreateKind) -> bool {
    match k {
        CreateKind::File => it.entry.kind == Kind::File && !it.symlink_blocked,
        CreateKind::Dir | CreateKind::Package => it.entry.kind == Kind::Dir,
        CreateKind::Symlink => it.entry.kind == Kind::Symlink && !it.symlink_blocked,
        CreateKind::Alias => it.entry.kind == Kind::File,
    }
}

/// blake3 of the cached content of `it` at its current version, if cached.
fn cached_hash(sh: &Shared, it: &IpcItem) -> Option<[u8; 32]> {
    let ver = it.entry.version.content;
    let p = sh.cache.get_pinned(it.entry.id, ver)?;
    let r = (|| {
        let mut f = std::fs::File::open(&p).ok()?;
        let mut h = blake3::Hasher::new();
        std::io::copy(&mut f, &mut h).ok()?;
        Some(*h.finalize().as_bytes())
    })();
    sh.cache.unpin(it.entry.id, ver);
    r
}

fn finish_existing(
    sh: &Shared,
    ex: IpcItem,
    local: &LocalMeta,
    still_pending: u32,
    should_fetch: bool,
) -> Result<Modified> {
    let id = ex.entry.id;
    if *local != LocalMeta::default() && *local != ex.local {
        set_local(sh, id, local.clone())?;
    }
    let item = sh.item(id).unwrap_or(ex);
    Ok(Modified {
        item,
        still_pending,
        should_fetch_content: should_fetch,
        conflict_copy: None,
    })
}

/// A create that may already exist found a file at its path with other bytes: the file stays as
/// it is on the VM and the Mac's bytes land as its conflict copy (the daemon's unknown-base
/// write keeps both). The reply is the existing file at the server's version with
/// `should_fetch_content`; the copy arrives through the working set (rule 3).
fn keep_as_conflict(
    sh: &Shared,
    index: Option<unlatch_proto::IndexId>,
    ex: IpcItem,
    req: &CreateRequest,
    staged: Staged,
    still_pending: u32,
    x: &Xfer<'_>,
) -> Result<Modified> {
    let s = sh.session_for(index, sh.cfg.list_timeout)?;
    let w = Request::Write {
        op: op_hash(&[
            b"create-conflict",
            sh.cfg.name.as_bytes(),
            req.template_id.as_bytes(),
            &ex.entry.id.0.to_le_bytes(),
        ]),
        parent: ex.entry.parent,
        name: ex.entry.name.clone(),
        target: Some(ex.entry.id),
        // Never matches a live seq: the daemon keeps both unless the bytes are identical.
        base: Some(0),
        size: staged.size,
        content_hash: staged.hash,
        mtime_ns: req.mtime_ns,
        exec: None,
        move_to: None,
        may_exist: false,
    };
    match upload(sh, &s, w, &staged, x)? {
        Response::Written {
            entry,
            conflict_copy,
        } => {
            let id = entry.id;
            let mut ups = vec![(entry, Source::Other)];
            if let Some(c) = &conflict_copy {
                ups.push((c.clone(), Source::LocalCreate));
            }
            sh.apply_local(ups, vec![])?;
            let copy = match conflict_copy {
                Some(c) => {
                    adopt(sh, staged, c.id, c.version.content);
                    sh.signal_working_set_now();
                    sh.item(c.id).ok()
                }
                None => None,
            };
            if req.local != LocalMeta::default() {
                set_local(sh, id, req.local.clone())?;
            }
            Ok(Modified {
                item: sh.item(id)?,
                still_pending,
                // Identical bytes after all (no copy): the Mac holds the VM's content.
                should_fetch_content: copy.is_some(),
                conflict_copy: copy,
            })
        }
        _ => Err(err(ErrorCode::Protocol, "unexpected reply to Write")),
    }
}

fn expect_entry(r: Response) -> Result<Entry> {
    match r {
        Response::Entry(e) => Ok(e),
        Response::Written { entry, .. } => Ok(entry),
        Response::Renamed { entry, .. } => Ok(entry),
        _ => Err(err(ErrorCode::Protocol, "unexpected reply")),
    }
}

/// Errors that must reach the system as errors (retried by it later): never "resolved" into
/// returning server state.
fn is_transient(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::Offline | ErrorCode::Timeout | ErrorCode::IndexChanged | ErrorCode::Cancelled
    )
}

// ---- create -------------------------------------------------------------------------------------

pub(crate) fn create(sh: &Shared, mut req: CreateRequest, x: &Xfer<'_>) -> Result<Modified> {
    let _g = sh.begin_mutation(&[req.parent]);
    let still_pending = req.changed_fields & !KNOWN_FIELDS;
    if !unlatch_proto::valid_name(&req.name) {
        return Err(err(
            ErrorCode::InvalidName,
            format!("invalid name {:?}", req.name),
        ));
    }
    let (index, parent_known, existing) = {
        let st = sh.read_state();
        let ex = st
            .lookup_id(req.parent, &req.name)
            .and_then(|id| st.ipc_item(id, sh.cfg.expose_exec));
        (st.index, st.nodes.contains_key(&req.parent), ex)
    };
    if !parent_known {
        return Err(err(
            ErrorCode::NotFound,
            format!("parent {} not found", req.parent),
        ));
    }
    // v1: packages, aliases and Finder droppings stay on the Mac — unless the VM already has
    // an item there, which is then simply returned (D8).
    let excluded = matches!(req.kind, CreateKind::Package | CreateKind::Alias)
        || unlatch_proto::is_mac_local_name(&req.name);
    if excluded {
        return match existing {
            Some(ex) => {
                let fetch = ex.entry.kind == Kind::File;
                finish_existing(sh, ex, &req.local, still_pending, fetch)
            }
            None => Err(err(
                ErrorCode::ExcludedFromSync,
                format!("{} stays on this Mac", req.name),
            )),
        };
    }
    let staged = match req.kind {
        CreateKind::File => Some(stage(sh, req.content.take(), x)?),
        _ => None,
    };
    // Rule 2: an existing same-kind item at (parent, name) is the answer when the system says it
    // may already exist (reimport) or the content is identical.
    if let Some(ex) = existing.clone() {
        if same_kind(&ex, req.kind) {
            let content_eq = match req.kind {
                CreateKind::File => staged
                    .as_ref()
                    .is_some_and(|s| cached_hash(sh, &ex) == Some(s.hash)),
                CreateKind::Symlink => {
                    let raw = sh
                        .read_state()
                        .nodes
                        .get(&ex.entry.id)
                        .and_then(|n| n.entry.symlink_target.clone());
                    raw.is_some() && raw == req.symlink_target
                }
                _ => true,
            };
            if req.may_already_exist && req.kind == CreateKind::File && !content_eq {
                // The system holds other bytes for this file than the VM has (rule 7: a
                // pending edit re-offered after a reimport). Never drop them: the existing
                // file stays, the Mac's bytes land as its conflict copy (rule 3).
                if let Some(st) = staged {
                    return keep_as_conflict(sh, index, ex, &req, st, still_pending, x);
                }
            }
            if req.may_already_exist || content_eq {
                let fetch = req.kind == CreateKind::File && !content_eq;
                return finish_existing(sh, ex, &req.local, still_pending, fetch);
            }
        }
    }
    let s = sh.session_for(index, sh.cfg.list_timeout)?;
    let exec = if sh.cfg.expose_exec {
        req.user_exec
    } else {
        None
    };
    let mut created: Option<Entry> = None;
    for n in 1..=MAX_NAME_ATTEMPTS {
        let name = numbered(&req.name, n);
        let op = create_op(
            &sh.cfg.name,
            &req.template_id,
            (n > 1).then_some(name.as_str()),
        );
        let r = match req.kind {
            CreateKind::File => {
                let st = staged
                    .as_ref()
                    .ok_or_else(|| err(ErrorCode::Io, "missing staged content"))?;
                let w = Request::Write {
                    op,
                    parent: req.parent,
                    name: name.clone(),
                    target: None,
                    base: None,
                    size: st.size,
                    content_hash: st.hash,
                    mtime_ns: req.mtime_ns,
                    exec,
                    move_to: None,
                    // Same bytes already there (our own earlier attempt) → that file.
                    may_exist: true,
                };
                upload(sh, &s, w, st, x).and_then(expect_entry)
            }
            CreateKind::Dir => sh
                .call(
                    &s,
                    Request::Mkdir {
                        op,
                        parent: req.parent,
                        name: name.clone(),
                        may_exist: n == 1,
                    },
                    sh.timing.reply_timeout,
                )
                .and_then(expect_entry),
            CreateKind::Symlink => {
                let target = req.symlink_target.clone().unwrap_or_default();
                sh.call(
                    &s,
                    Request::Symlink {
                        op,
                        parent: req.parent,
                        name: name.clone(),
                        target,
                    },
                    sh.timing.reply_timeout,
                )
                .and_then(expect_entry)
            }
            CreateKind::Package | CreateKind::Alias => unreachable_kind(),
        };
        match r {
            Ok(e) => {
                created = Some(e);
                break;
            }
            Err(e) if e.code == ErrorCode::Exists => {
                if n == 1 && req.may_already_exist {
                    // Reimport: match by path against the VM's current listing.
                    let _ = sh.ensure_listed(req.parent, sh.cfg.list_timeout);
                    let found = {
                        let st = sh.read_state();
                        st.lookup_id(req.parent, &req.name)
                            .and_then(|id| st.ipc_item(id, sh.cfg.expose_exec))
                    };
                    if let Some(ex) = found.filter(|ex| same_kind(ex, req.kind)) {
                        // A file with these very bytes would have been answered by the
                        // daemon (may_exist): these differ — keep them as a conflict copy.
                        if let Some(st) = staged {
                            return keep_as_conflict(sh, index, ex, &req, st, still_pending, x);
                        }
                        return finish_existing(sh, ex, &req.local, still_pending, false);
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }
    let e = created.ok_or_else(|| err(ErrorCode::CannotSync, "no free name after 100 attempts"))?;
    let (id, ver) = (e.id, e.version.content);
    sh.apply_local(vec![(e, Source::LocalCreate)], vec![])?;
    let uploaded = staged.is_some();
    if let Some(st) = staged {
        adopt(sh, st, id, ver);
    }
    if req.local != LocalMeta::default() {
        set_local(sh, id, req.local.clone())?;
    }
    let item = sh.item(id)?;
    // The reply's version names our bytes; the item can already be newer (an agent wrote
    // right after unlatchd published them, and its Events came first): fetch, as for modify.
    let should_fetch =
        uploaded && item.entry.kind == Kind::File && item.entry.version.content != ver;
    Ok(Modified {
        item,
        still_pending,
        should_fetch_content: should_fetch,
        conflict_copy: None,
    })
}

fn unreachable_kind() -> Result<Entry> {
    Err(err(
        ErrorCode::ExcludedFromSync,
        "packages and aliases stay on this Mac",
    ))
}

// ---- modify -------------------------------------------------------------------------------------

pub(crate) fn modify(
    sh: &Shared,
    id: ItemId,
    base: BaseVersion,
    mut req: ModifyRequest,
    x: &Xfer<'_>,
) -> Result<Modified> {
    let _g = sh.begin_mutation(&[id]);
    let cf = req.changed_fields;
    let still_pending = cf & !KNOWN_FIELDS;
    let (index, cur, item, mapped) = {
        let st = sh.read_state();
        let n = st
            .nodes
            .get(&id)
            .ok_or_else(|| err(ErrorCode::NotFound, format!("no item {id}")))?;
        let it = st
            .ipc_item(id, sh.cfg.expose_exec)
            .ok_or_else(|| err(ErrorCode::NotFound, format!("no item {id}")))?;
        let mapped = it.display_name != n.entry.name || st.collides(id);
        (st.index, n.entry.clone(), it, mapped)
    };
    // Rule 5: Mac-only fields live in local_meta; touching only them costs zero network traffic.
    if cf & (LOCAL_FIELDS | fields::FILE_SYSTEM_FLAGS) != 0 {
        let merged = merge_local(&item.local, &req.local, cf);
        if merged != item.local {
            set_local(sh, id, merged)?;
        }
    }
    // Rule 6: a reparent to the trash is a delete (trash is unsupported, D7).
    if cf & fields::PARENT != 0 && req.new_parent == Some(ItemId::TRASH) {
        return match delete(sh, id, base, true) {
            Ok(()) => Err(err(ErrorCode::NotFound, "deleted on the VM")),
            Err(e) if e.code == ErrorCode::DeletionRejected => {
                server_state(sh, id, still_pending, false, None)
            }
            Err(e) => Err(e),
        };
    }
    let new_parent = req
        .new_parent
        .filter(|_| cf & fields::PARENT != 0)
        .unwrap_or(cur.parent);
    let new_display = req
        .new_name
        .clone()
        .filter(|_| cf & fields::FILENAME != 0)
        .unwrap_or_else(|| item.display_name.clone());
    let wants_rename = new_parent != cur.parent || new_display != item.display_name;
    let is_file = cur.kind == Kind::File && !item.symlink_blocked;
    let wants_content = cf & fields::CONTENTS != 0 && req.content.is_some() && is_file;
    let mut should_fetch = cf & fields::CONTENTS != 0 && item.symlink_blocked;
    let wants_mtime = cf & fields::CONTENT_MODIFICATION_DATE != 0
        && req.mtime_ns.is_some_and(|m| m != cur.mtime_ns)
        && !item.symlink_blocked
        && cur.kind != Kind::Symlink;
    let wants_exec = cf & fields::FILE_SYSTEM_FLAGS != 0
        && sh.cfg.expose_exec
        && is_file
        && req.user_exec.is_some_and(|x| x != item.user_exec);

    let mut move_to: Option<(ItemId, String)> = None;
    if wants_rename {
        if new_parent == cur.parent && mapped && is_bounce_of(&item.display_name, &new_display) {
            // Rule 11 / MQ-016: the system bounced a mapped name locally; keep it local.
            sh.apply_msg_wait(|done| super::applier::ApplyMsg::Override {
                id,
                display: Some(new_display.clone()),
                done,
            })?;
        } else if base.meta.is_some_and(|m| m != cur.version.meta)
            || !unlatch_proto::valid_name(&new_display)
        {
            // Rule 4: the VM renamed/moved it too (or the name is unusable): metadata never
            // errors — the reply carries the server's state, which the system applies.
        } else {
            // Rule 11: display → real for every outgoing op. The item's own display name means
            // "name unchanged" — also for a pure move, where the system sends no filename and
            // the display is all we have: a twin shown as `readme (Unlatch 2).md` dragged into
            // another folder lands there as `readme.md`, never under its generated name.
            let real = if new_display == item.display_name {
                cur.name.clone()
            } else {
                new_display
            };
            if new_parent == cur.parent && real == cur.name {
                // Renamed back to its real name (e.g. un-bouncing a mapped item): only the
                // display changes; the VM already has this name.
                sh.apply_msg_wait(|done| super::applier::ApplyMsg::Override {
                    id,
                    display: None,
                    done,
                })?;
            } else if wants_content {
                move_to = Some((new_parent, real));
            } else {
                rename(sh, index, id, base, cf, &cur, new_parent, &real)?;
            }
        }
    }
    let mut conflict_copy = None;
    let mut wrote = false;
    // The content version whose bytes the Mac holds once this reply lands: its base, or what
    // we just uploaded. MQ-013: the system believes the reply's version with those bytes, so
    // if the item's content is newer by the time we answer (an agent write the VM had and the
    // engine learnt with the rename/setattr reply, or right after our upload) it must re-fetch.
    let mut mac_content = base.content;
    if wants_content {
        wrote = true;
        let staged = stage(sh, req.content.take(), x)?;
        let s = sh.session_for(index, sh.cfg.list_timeout)?;
        let cur = sh
            .read_state()
            .nodes
            .get(&id)
            .map(|n| n.entry.clone())
            .unwrap_or(cur);
        let w = Request::Write {
            op: modify_op(id, base, cf, Some(&staged.hash), "write", ""),
            parent: cur.parent,
            name: cur.name.clone(),
            target: Some(id),
            // Unknown base (beforeFirstSync) never matches a live seq: the daemon keeps both
            // versions unless the bytes are identical.
            base: Some(base.content.unwrap_or(0)),
            size: staged.size,
            content_hash: staged.hash,
            mtime_ns: if wants_mtime { req.mtime_ns } else { None },
            exec: if wants_exec { req.user_exec } else { None },
            move_to,
            may_exist: false,
        };
        match upload(sh, &s, w, &staged, x) {
            Ok(Response::Written {
                entry,
                conflict_copy: copy,
            }) => {
                let ver = entry.version.content;
                let mut ups = vec![(entry, Source::Other)];
                if let Some(c) = &copy {
                    ups.push((c.clone(), Source::LocalCreate));
                }
                sh.apply_local(ups, vec![])?;
                match copy {
                    Some(c) => {
                        // Rule 3: the original at the server's version, re-download, and the
                        // conflict copy shows up through the working set.
                        should_fetch = true;
                        adopt(sh, staged, c.id, c.version.content);
                        conflict_copy = sh.item(c.id).ok();
                        sh.signal_working_set_now();
                    }
                    None => {
                        mac_content = Some(ver);
                        adopt(sh, staged, id, ver);
                    }
                }
            }
            Ok(_) => return Err(err(ErrorCode::Protocol, "unexpected reply to Write")),
            Err(e) if e.code == ErrorCode::NotFound => {
                sh.apply_local(vec![], vec![id])?;
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    }
    if !wrote && (wants_mtime || wants_exec) {
        let s = sh.session_for(index, sh.cfg.list_timeout)?;
        let sa = Request::SetAttr {
            op: modify_op(id, base, cf, None, "setattr", ""),
            id,
            exec: if wants_exec { req.user_exec } else { None },
            mtime_ns: if wants_mtime { req.mtime_ns } else { None },
        };
        match sh.call(&s, sa, sh.timing.reply_timeout) {
            Ok(r) => {
                let e = expect_entry(r)?;
                sh.apply_local(vec![(e, Source::Other)], vec![])?;
            }
            Err(e) if e.code == ErrorCode::NotFound => {
                sh.apply_local(vec![], vec![id])?;
                return Err(e);
            }
            Err(e) if is_transient(e.code) => return Err(e),
            Err(e) => tracing::debug!("setattr on {id} not applied: {e}"),
        }
    }
    let item = sh.item(id)?;
    if item.entry.kind == Kind::File && mac_content.is_some_and(|c| c != item.entry.version.content)
    {
        should_fetch = true;
    }
    Ok(Modified {
        item,
        still_pending,
        should_fetch_content: should_fetch,
        conflict_copy,
    })
}

fn server_state(
    sh: &Shared,
    id: ItemId,
    still_pending: u32,
    should_fetch: bool,
    conflict_copy: Option<IpcItem>,
) -> Result<Modified> {
    let item = sh.item(id)?;
    Ok(Modified {
        item,
        still_pending,
        should_fetch_content: should_fetch,
        conflict_copy,
    })
}

#[allow(clippy::too_many_arguments)]
fn rename(
    sh: &Shared,
    index: Option<unlatch_proto::IndexId>,
    id: ItemId,
    base: BaseVersion,
    cf: u32,
    cur: &Entry,
    new_parent: ItemId,
    real: &str,
) -> Result<()> {
    let s = sh.session_for(index, sh.cfg.list_timeout)?;
    for n in 1..=MAX_NAME_ATTEMPTS {
        let name = numbered(real, n);
        let op = modify_op(
            id,
            base,
            cf,
            None,
            "rename",
            &format!("{}/{}", new_parent.0, name),
        );
        let r = Request::Rename {
            op,
            id,
            base_parent: cur.parent,
            base_name: cur.name.clone(),
            new_parent,
            new_name: name,
        };
        match sh.call(&s, r, sh.timing.reply_timeout) {
            // applied=false → base mismatch, `entry` is the server's state: apply and return it.
            Ok(Response::Renamed { entry, .. }) => {
                return sh.apply_local(vec![(entry, Source::Other)], vec![])
            }
            Ok(_) => return Err(err(ErrorCode::Protocol, "unexpected reply to Rename")),
            Err(e) if e.code == ErrorCode::Exists => continue,
            Err(e) if e.code == ErrorCode::NotFound => {
                sh.apply_local(vec![], vec![id])?;
                return Err(e);
            }
            Err(e) if is_transient(e.code) => return Err(e),
            Err(e) => {
                // Metadata never errors (rule 4): the system gets the server's state back.
                tracing::debug!("rename of {id} not applied: {e}");
                return Ok(());
            }
        }
    }
    Ok(())
}

// ---- delete -------------------------------------------------------------------------------------

pub(crate) fn delete(sh: &Shared, id: ItemId, base: BaseVersion, recursive: bool) -> Result<()> {
    if id == ItemId::ROOT {
        return Err(err(
            ErrorCode::DeletionRejected,
            "the root cannot be deleted",
        ));
    }
    let _g = sh.begin_mutation(&[id]);
    let key = delete_key(base, recursive);
    let (index, cur, children, seen, frozen) = {
        let st = sh.read_state();
        let Some(n) = st.nodes.get(&id) else {
            // Unknown id: already gone (replayed delete) → success.
            return Ok(());
        };
        let frozen = st
            .delete_seen
            .get(&id)
            .filter(|e| e.key == key)
            .map(|e| e.seen);
        let seen = frozen.unwrap_or_else(|| st.server_seq_at(st.consumed));
        (
            st.index,
            n.entry.clone(),
            st.child_count(id),
            seen,
            frozen.is_some(),
        )
    };
    let is_dir = cur.kind == Kind::Dir;
    // Whether the system's base pins the content it holds (else only the anchor counts).
    let content_seen = is_dir || base.content.is_some();
    // Rule 6: what the system has seen is fixed by the first attempt of this call. A retry
    // (reply lost, engine restarted, the system's backoff elapsed) must not widen it to anchors
    // the system consumed since: the user deleted the item locally before those, so nothing
    // they carried below it was ever shown. Durable before the daemon can act on it. Only
    // where `seen` decides anything: a recursive folder delete, or a file of unknown base.
    if !frozen && ((is_dir && recursive) || !content_seen) {
        sh.apply_msg_wait(|done| super::applier::ApplyMsg::DeleteSeen {
            id,
            entry: Some(DeleteSeen { key, seen }),
            done,
        })?;
    }
    let release = || {
        let _ = sh.apply_msg_wait(|done| super::applier::ApplyMsg::DeleteSeen {
            id,
            entry: None,
            done,
        });
    };
    let rejected = |why: &str| Err(err(ErrorCode::DeletionRejected, why.to_string()));
    let meta_mismatch = base.meta.is_some_and(|m| m != cur.version.meta);
    let content_mismatch = !is_dir && base.content.is_some_and(|c| c != cur.version.content);
    if meta_mismatch || content_mismatch {
        return rejected("the item changed on the VM");
    }
    // An unknown content base (`beforeFirstSyncComponent`, or no version at all) proves
    // nothing about which bytes the system holds: never fall back to the replica's version
    // (which may be newer than anything the system was shown) — only the anchor it consumed
    // counts. Content changed since → rejected; the item comes back at its current version.
    if !content_seen && cur.seq > seen {
        return rejected("the item changed on the VM since the system last saw it");
    }
    if is_dir && !recursive && children > 0 {
        return rejected("directory not empty");
    }
    // For a file whose content base matched, the system saw exactly this version (the daemon
    // checks it against the live file); descendants are covered by the anchor (rule 6).
    let seen_seq = if content_seen && !is_dir {
        seen.max(cur.seq)
    } else {
        seen
    };
    let s = sh.session_for(index, sh.cfg.list_timeout)?;
    let req = Request::Remove {
        op: delete_op(id, base, recursive, seen_seq),
        id,
        base: Version {
            content: cur.version.content,
            meta: cur.version.meta,
        },
        recursive,
        seen_seq,
    };
    match sh.call(&s, req, sh.timing.reply_timeout) {
        Ok(Response::Removed { kept }) if kept.is_empty() => sh.apply_local(vec![], vec![id]),
        Ok(Response::Removed { .. }) => {
            // Partial: things newer than what the system saw survive; they arrive as events.
            let _ = sh.refresh(id);
            rejected("items changed on the VM since you last saw this folder")
        }
        Ok(_) => Err(err(ErrorCode::Protocol, "unexpected reply to Remove")),
        Err(e) if e.code == ErrorCode::NotFound => {
            release();
            sh.apply_local(vec![], vec![id])
        }
        Err(e) if matches!(e.code, ErrorCode::VersionMismatch | ErrorCode::NotEmpty) => {
            let _ = sh.refresh(id);
            rejected(&e.msg)
        }
        Err(e) => Err(e),
    }
}

/// What identifies one system delete call of an item besides its id: the base version it
/// carried and whether it is recursive.
fn delete_key(base: BaseVersion, recursive: bool) -> [u8; 16] {
    op_hash(&[b"delete-call", &base_bytes(base), &[recursive as u8]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_ids_are_stable_and_distinct() {
        let b = BaseVersion {
            content: Some(3),
            meta: Some(4),
        };
        let h = [9u8; 32];
        assert_eq!(create_op("d", "t1", None), create_op("d", "t1", None));
        assert_ne!(create_op("d", "t1", None), create_op("d", "t2", None));
        assert_ne!(create_op("d", "t1", None), create_op("e", "t1", None));
        assert_ne!(
            create_op("d", "t1", None),
            create_op("d", "t1", Some("a 2"))
        );
        assert_eq!(
            modify_op(ItemId(5), b, 1, Some(&h), "write", ""),
            modify_op(ItemId(5), b, 1, Some(&h), "write", "")
        );
        assert_ne!(
            modify_op(ItemId(5), b, 1, Some(&h), "write", ""),
            modify_op(ItemId(5), b, 1, Some(&[8; 32]), "write", "")
        );
        let nb = BaseVersion {
            content: None,
            meta: Some(4),
        };
        assert_ne!(
            modify_op(ItemId(5), b, 1, None, "w", ""),
            modify_op(ItemId(5), nb, 1, None, "w", "")
        );
        assert_ne!(
            modify_op(ItemId(5), b, 1, None, "rename", "1/a"),
            modify_op(ItemId(5), b, 1, None, "setattr", "")
        );
        assert_eq!(
            delete_op(ItemId(5), b, true, 9),
            delete_op(ItemId(5), b, true, 9)
        );
        assert_ne!(
            delete_op(ItemId(5), b, true, 9),
            delete_op(ItemId(6), b, true, 9)
        );
        assert_ne!(
            delete_op(ItemId(5), b, true, 9),
            delete_op(ItemId(5), b, true, 10)
        );
        // Length-prefixing: ("ab","c") ≠ ("a","bc").
        assert_ne!(create_op("ab", "c", None), create_op("a", "bc", None));
    }

    #[test]
    fn local_merge_only_touches_named_fields() {
        let cur = LocalMeta {
            tag_data: Some(vec![1]),
            favorite_rank: Some(3),
            ..Default::default()
        };
        let new = LocalMeta {
            tag_data: Some(vec![2]),
            favorite_rank: Some(9),
            hidden: true,
            ..Default::default()
        };
        let m = merge_local(&cur, &new, fields::TAG_DATA);
        assert_eq!(m.tag_data, Some(vec![2]));
        assert_eq!(m.favorite_rank, Some(3));
        assert!(!m.hidden);
        let m = merge_local(&cur, &new, fields::FILE_SYSTEM_FLAGS);
        assert!(m.hidden);
    }
}
