//! Test-only fake `unlatchd`: an in-memory tree speaking the wire protocol's server side over a
//! `tokio::io::duplex` pipe (Welcome/Snapshot/Resume, Events, ListDir with lazy dirs, Read under
//! credit, Write + WriteChunk, Mkdir/Symlink/Rename/Remove/SetAttr with an ops table, Pong).

use super::session::LinkParts;
use super::Opener;
use crate::{err, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Notify};
use unlatch_proto::frame::{self, aio};
use unlatch_proto::wire::{
    Change, ClientMsg, Request, Response, ServerInfo, ServerMsg, WelcomeMode,
};
use unlatch_proto::{
    Entry, ErrorCode, IndexId, ItemId, Kind, OpId, ProtoError, Version, ACCESS_R, ACCESS_W,
    ACCESS_X,
};

pub(crate) const ROOT_PATH: &str = "/vm/root";

#[derive(Clone, Debug)]
pub(crate) struct FNode {
    pub entry: Entry,
    pub content: Vec<u8>,
    pub children: BTreeMap<String, ItemId>,
    /// Lazy dir whose children are hidden until a ListDir.
    pub hidden: bool,
}

#[derive(Default)]
pub(crate) struct Faults {
    /// Execute + record the op, then kill the connection instead of replying (once).
    pub die_after_commit: Option<&'static str>,
    /// Ignore pings (black-hole liveness test).
    pub ignore_pings: bool,
    /// Openers fail with this error.
    pub connect_error: Option<ProtoError>,
    /// Snapshot: send this Events batch before the chunks (LWW interleaving test).
    pub event_before_snapshot: Option<Vec<Change>>,
    /// Delay ListDir replies by this long.
    pub listdir_delay_ms: u64,
    /// After the next Write that publishes the client's bytes: an agent appends these to the
    /// file before the reply, as unlatchd answers it — the reply keeps the version of the
    /// client's bytes, the item moves on to a newer one whose Events precede the reply.
    pub agent_append_after_write: Option<Vec<u8>>,
}

pub(crate) struct Fs {
    pub index: IndexId,
    pub seq: u64,
    pub nodes: HashMap<ItemId, FNode>,
    next_id: u64,
    ops: HashMap<OpId, Response>,
    pub tombs: Vec<(ItemId, u64)>,
    subs: Vec<mpsc::UnboundedSender<ServerMsg>>,
    pub requests: Vec<Request>,
    pub hellos: Vec<ClientMsg>,
    pub faults: Faults,
    pub sessions: u64,
    pub max_outstanding_read: i64,
    /// Largest credit balance ever granted to this fake (what it could send without waiting).
    pub max_credit_balance: i64,
    /// Credit charged for `ReadChunk`s (frame body length as sent) and granted by the client.
    pub read_wire_bytes: u64,
    pub credit_granted: u64,
}

fn blake(b: &[u8]) -> [u8; 32] {
    *blake3::hash(b).as_bytes()
}

impl Fs {
    fn new() -> Fs {
        let mut nodes = HashMap::new();
        nodes.insert(
            ItemId::ROOT,
            FNode {
                entry: Entry {
                    id: ItemId::ROOT,
                    parent: ItemId::ROOT,
                    name: "root".into(),
                    kind: Kind::Dir,
                    size: 0,
                    mtime_ns: 0,
                    mode: 0o755,
                    version: Version {
                        content: 1,
                        meta: 1,
                    },
                    symlink_target: None,
                    lazy: false,
                    seq: 1,
                    access: ACCESS_R | ACCESS_W | ACCESS_X,
                },
                content: vec![],
                children: BTreeMap::new(),
                hidden: false,
            },
        );
        Fs {
            index: IndexId(0xABCD),
            seq: 1,
            nodes,
            next_id: 100,
            ops: HashMap::new(),
            tombs: Vec::new(),
            subs: Vec::new(),
            requests: Vec::new(),
            hellos: Vec::new(),
            faults: Faults::default(),
            sessions: 0,
            max_outstanding_read: 0,
            max_credit_balance: 0,
            read_wire_bytes: 0,
            credit_granted: 0,
        }
    }

    fn bump(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn broadcast(&mut self, changes: Vec<Change>) {
        if changes.is_empty() {
            return;
        }
        let seq = self.seq;
        self.subs.retain(|s| {
            s.send(ServerMsg::Events {
                seq,
                changes: changes.clone(),
                batch_end: true,
            })
            .is_ok()
        });
    }

    pub fn resolve(&self, path: &str) -> Option<ItemId> {
        let mut cur = ItemId::ROOT;
        for c in path.split('/').filter(|c| !c.is_empty()) {
            cur = *self.nodes.get(&cur)?.children.get(c)?;
        }
        Some(cur)
    }

    fn split(path: &str) -> (&str, &str) {
        match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        }
    }

    fn add(
        &mut self,
        parent: ItemId,
        name: &str,
        kind: Kind,
        content: Vec<u8>,
        target: Option<String>,
    ) -> Result<Entry> {
        let p = self
            .nodes
            .get(&parent)
            .ok_or_else(|| err(ErrorCode::NotFound, "no parent"))?;
        if p.entry.kind != Kind::Dir {
            return Err(err(ErrorCode::NotDir, "not a dir"));
        }
        if p.children.contains_key(name) {
            return Err(err(ErrorCode::Exists, format!("{name} exists")));
        }
        let seq = self.bump();
        self.next_id += 1;
        let id = ItemId(self.next_id);
        let size = match kind {
            Kind::File => content.len() as u64,
            Kind::Symlink => target.as_ref().map_or(0, |t| t.len() as u64),
            Kind::Dir => 0,
        };
        let e = Entry {
            id,
            parent,
            name: name.into(),
            kind,
            size,
            mtime_ns: 1_000,
            mode: if kind == Kind::Dir { 0o755 } else { 0o644 },
            version: Version {
                content: seq,
                meta: seq,
            },
            symlink_target: target,
            lazy: false,
            seq,
            access: ACCESS_R | ACCESS_W | ACCESS_X,
        };
        self.nodes.insert(
            id,
            FNode {
                entry: e.clone(),
                content,
                children: BTreeMap::new(),
                hidden: false,
            },
        );
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children.insert(name.into(), id);
        }
        Ok(e)
    }

    fn subtree(&self, id: ItemId) -> Vec<ItemId> {
        let mut out = Vec::new();
        let mut st = vec![id];
        while let Some(x) = st.pop() {
            out.push(x);
            if let Some(n) = self.nodes.get(&x) {
                st.extend(n.children.values().copied());
            }
        }
        out
    }

    fn remove_id(&mut self, id: ItemId) -> u64 {
        let seq = self.bump();
        for x in self.subtree(id) {
            if let Some(n) = self.nodes.remove(&x) {
                if let Some(p) = self.nodes.get_mut(&n.entry.parent) {
                    if p.children.get(&n.entry.name) == Some(&x) {
                        p.children.remove(&n.entry.name);
                    }
                }
            }
            self.tombs.push((x, seq));
        }
        seq
    }

    fn visible(&self, id: ItemId) -> bool {
        // Hidden under a lazy, never-listed ancestor?
        let mut cur = self.nodes.get(&id).map(|n| n.entry.parent);
        let mut guard = 0;
        while let Some(p) = cur {
            if p == id || guard > 1000 {
                break;
            }
            let Some(n) = self.nodes.get(&p) else {
                return false;
            };
            if n.hidden {
                return false;
            }
            if p == ItemId::ROOT {
                break;
            }
            cur = Some(n.entry.parent);
            guard += 1;
        }
        true
    }

    // ---- VM-side operations (the "agent") -------------------------------------------------------

    pub fn vm_write(&mut self, path: &str, content: &[u8]) -> ItemId {
        let (dir, name) = Self::split(path);
        let parent = self.resolve(dir).expect("vm_write: parent");
        if let Some(&id) = self.nodes.get(&parent).and_then(|p| p.children.get(name)) {
            let seq = self.bump();
            let n = self.nodes.get_mut(&id).expect("node");
            n.content = content.to_vec();
            n.entry.size = content.len() as u64;
            n.entry.version.content = seq;
            n.entry.seq = seq;
            let e = n.entry.clone();
            let vis = self.visible(id);
            if vis {
                self.broadcast(vec![Change::Upsert(e)]);
            }
            return id;
        }
        let e = self
            .add(parent, name, Kind::File, content.to_vec(), None)
            .expect("vm_write");
        if self.visible(e.id) {
            self.broadcast(vec![Change::Upsert(e.clone())]);
        }
        e.id
    }

    /// Create a file without telling any client (the replica cannot know it).
    pub fn vm_write_silent(&mut self, path: &str, content: &[u8]) -> ItemId {
        let (dir, name) = Self::split(path);
        let parent = self.resolve(dir).expect("vm_write_silent: parent");
        self.add(parent, name, Kind::File, content.to_vec(), None)
            .expect("vm_write_silent")
            .id
    }

    pub fn vm_mkdir(&mut self, path: &str) -> ItemId {
        let (dir, name) = Self::split(path);
        let parent = self.resolve(dir).expect("vm_mkdir: parent");
        let e = self
            .add(parent, name, Kind::Dir, vec![], None)
            .expect("vm_mkdir");
        if self.visible(e.id) {
            self.broadcast(vec![Change::Upsert(e.clone())]);
        }
        e.id
    }

    pub fn vm_lazy_dir(&mut self, path: &str) -> ItemId {
        let id = self.vm_mkdir(path);
        let seq = self.bump();
        let n = self.nodes.get_mut(&id).expect("node");
        n.entry.lazy = true;
        n.entry.seq = seq;
        n.hidden = true;
        let e = n.entry.clone();
        self.broadcast(vec![Change::Upsert(e)]);
        id
    }

    pub fn vm_symlink(&mut self, path: &str, target: &str) -> ItemId {
        let (dir, name) = Self::split(path);
        let parent = self.resolve(dir).expect("vm_symlink: parent");
        let e = self
            .add(parent, name, Kind::Symlink, vec![], Some(target.into()))
            .expect("vm_symlink");
        self.broadcast(vec![Change::Upsert(e.clone())]);
        e.id
    }

    pub fn vm_rm(&mut self, path: &str) {
        let id = self.resolve(path).expect("vm_rm");
        let seq = self.remove_id(id);
        self.broadcast(vec![Change::Remove { id, seq }]);
    }

    pub fn vm_rename(&mut self, from: &str, to: &str) {
        let id = self.resolve(from).expect("vm_rename");
        let (dir, name) = Self::split(to);
        let np = self.resolve(dir).expect("vm_rename: parent");
        let e = self.do_rename(id, np, name).expect("rename");
        self.broadcast(vec![Change::Upsert(e)]);
    }

    fn do_rename(&mut self, id: ItemId, np: ItemId, name: &str) -> Result<Entry> {
        if self
            .nodes
            .get(&np)
            .is_some_and(|p| p.children.contains_key(name))
        {
            return Err(err(ErrorCode::Exists, "exists"));
        }
        let seq = self.bump();
        let (op, oname) = {
            let n = self
                .nodes
                .get(&id)
                .ok_or_else(|| err(ErrorCode::NotFound, "gone"))?;
            (n.entry.parent, n.entry.name.clone())
        };
        if let Some(p) = self.nodes.get_mut(&op) {
            p.children.remove(&oname);
        }
        if let Some(p) = self.nodes.get_mut(&np) {
            p.children.insert(name.into(), id);
        }
        let n = self
            .nodes
            .get_mut(&id)
            .ok_or_else(|| err(ErrorCode::NotFound, "gone"))?;
        n.entry.parent = np;
        n.entry.name = name.into();
        n.entry.version.meta = seq;
        n.entry.seq = seq;
        Ok(n.entry.clone())
    }

    pub fn content_of(&self, path: &str) -> Option<Vec<u8>> {
        self.resolve(path)
            .and_then(|id| self.nodes.get(&id))
            .map(|n| n.content.clone())
    }

    pub fn names_in(&self, path: &str) -> Vec<String> {
        let id = self.resolve(path).expect("names_in");
        self.nodes[&id].children.keys().cloned().collect()
    }

    pub fn count(&self, pred: impl Fn(&Request) -> bool) -> usize {
        self.requests.iter().filter(|r| pred(r)).count()
    }

    /// Server semantics of one mutating request (wire.rs); `Ok` responses are recorded by op id.
    fn exec(
        &mut self,
        req: &Request,
        upload: Option<Vec<u8>>,
        client: &str,
    ) -> std::result::Result<(Response, Vec<Change>), ProtoError> {
        match req {
            Request::Mkdir {
                parent,
                name,
                may_exist,
                ..
            } => {
                if let Some(&ex) = self.nodes.get(parent).and_then(|p| p.children.get(name)) {
                    let n = &self.nodes[&ex];
                    if *may_exist && n.entry.kind == Kind::Dir {
                        return Ok((Response::Entry(n.entry.clone()), vec![]));
                    }
                    return Err(err(ErrorCode::Exists, "exists"));
                }
                let e = self.add(*parent, name, Kind::Dir, vec![], None)?;
                Ok((Response::Entry(e.clone()), vec![Change::Upsert(e)]))
            }
            Request::Symlink {
                parent,
                name,
                target,
                ..
            } => {
                let e = self.add(*parent, name, Kind::Symlink, vec![], Some(target.clone()))?;
                Ok((Response::Entry(e.clone()), vec![Change::Upsert(e)]))
            }
            Request::Write {
                parent,
                name,
                target,
                base,
                content_hash,
                may_exist,
                move_to,
                mtime_ns,
                exec,
                ..
            } => {
                let data = upload.unwrap_or_default();
                if blake(&data) != *content_hash {
                    return Err(err(ErrorCode::Io, "hash mismatch"));
                }
                match target {
                    None => {
                        if let Some(&ex) = self.nodes.get(parent).and_then(|p| p.children.get(name))
                        {
                            let n = &self.nodes[&ex];
                            if *may_exist
                                && n.entry.kind == Kind::File
                                && blake(&n.content) == *content_hash
                            {
                                return Ok((
                                    Response::Written {
                                        entry: n.entry.clone(),
                                        conflict_copy: None,
                                    },
                                    vec![],
                                ));
                            }
                            return Err(err(ErrorCode::Exists, "exists"));
                        }
                        let mut e = self.add(*parent, name, Kind::File, data, None)?;
                        if let Some(m) = mtime_ns {
                            e.mtime_ns = *m;
                            if let Some(n) = self.nodes.get_mut(&e.id) {
                                n.entry.mtime_ns = *m;
                            }
                        }
                        Ok((
                            Response::Written {
                                entry: e.clone(),
                                conflict_copy: None,
                            },
                            vec![Change::Upsert(e)],
                        ))
                    }
                    Some(id) => {
                        let cur = self
                            .nodes
                            .get(id)
                            .ok_or_else(|| err(ErrorCode::NotFound, "gone"))?
                            .clone();
                        if base.is_some_and(|b| b != cur.entry.version.content) {
                            if blake(&cur.content) == *content_hash {
                                return Ok((
                                    Response::Written {
                                        entry: cur.entry,
                                        conflict_copy: None,
                                    },
                                    vec![],
                                ));
                            }
                            let (stem, ext) = super::names::split_ext(&cur.entry.name);
                            let cname =
                                format!("{stem} (conflict from {client} 2026-09-30 12.00){ext}");
                            let c = self.add(cur.entry.parent, &cname, Kind::File, data, None)?;
                            return Ok((
                                Response::Written {
                                    entry: cur.entry,
                                    conflict_copy: Some(c.clone()),
                                },
                                vec![Change::Upsert(c)],
                            ));
                        }
                        let mut changes = vec![];
                        if let Some((np, nn)) = move_to {
                            self.do_rename(*id, *np, nn)?;
                        }
                        let seq = self.bump();
                        let n = self
                            .nodes
                            .get_mut(id)
                            .ok_or_else(|| err(ErrorCode::NotFound, "gone"))?;
                        n.entry.size = data.len() as u64;
                        n.content = data;
                        n.entry.version.content = seq;
                        n.entry.seq = seq;
                        if let Some(m) = mtime_ns {
                            n.entry.mtime_ns = *m;
                        }
                        if let Some(x) = exec {
                            n.entry.mode = if *x {
                                n.entry.mode | 0o111
                            } else {
                                n.entry.mode & !0o111
                            };
                            n.entry.version.meta = seq;
                        }
                        let e = n.entry.clone();
                        changes.push(Change::Upsert(e.clone()));
                        Ok((
                            Response::Written {
                                entry: e,
                                conflict_copy: None,
                            },
                            changes,
                        ))
                    }
                }
            }
            Request::Rename {
                id,
                base_parent,
                base_name,
                new_parent,
                new_name,
                ..
            } => {
                let cur = self
                    .nodes
                    .get(id)
                    .ok_or_else(|| err(ErrorCode::NotFound, "gone"))?
                    .entry
                    .clone();
                if cur.parent != *base_parent || cur.name != *base_name {
                    return Ok((
                        Response::Renamed {
                            entry: cur,
                            applied: false,
                        },
                        vec![],
                    ));
                }
                let e = self.do_rename(*id, *new_parent, new_name)?;
                Ok((
                    Response::Renamed {
                        entry: e.clone(),
                        applied: true,
                    },
                    vec![Change::Upsert(e)],
                ))
            }
            Request::Remove {
                id,
                base,
                recursive,
                seen_seq,
                ..
            } => {
                let cur = self
                    .nodes
                    .get(id)
                    .ok_or_else(|| err(ErrorCode::NotFound, "gone"))?
                    .clone();
                let bad = if cur.entry.kind == Kind::Dir {
                    base.meta != cur.entry.version.meta
                } else {
                    *base != cur.entry.version
                };
                if bad {
                    return Err(err(ErrorCode::VersionMismatch, "changed"));
                }
                if cur.entry.kind == Kind::Dir && !cur.children.is_empty() && !recursive {
                    return Err(err(ErrorCode::NotEmpty, "not empty"));
                }
                let sub = self.subtree(*id);
                let newer: Vec<ItemId> = sub
                    .iter()
                    .copied()
                    .filter(|x| self.nodes.get(x).is_some_and(|n| n.entry.seq > *seen_seq))
                    .collect();
                if newer.is_empty() {
                    let seq = self.remove_id(*id);
                    return Ok((
                        Response::Removed { kept: vec![] },
                        vec![Change::Remove { id: *id, seq }],
                    ));
                }
                // Keep newer items and their ancestors; delete the rest bottom-up.
                let mut keep: HashSet<ItemId> = HashSet::new();
                for n in &newer {
                    let mut x = *n;
                    while keep.insert(x) && x != *id {
                        x = self.nodes[&x].entry.parent;
                    }
                }
                let mut changes = vec![];
                for x in sub {
                    if !keep.contains(&x) && self.nodes.contains_key(&x) {
                        let seq = self.remove_id(x);
                        changes.push(Change::Remove { id: x, seq });
                    }
                }
                Ok((Response::Removed { kept: newer }, changes))
            }
            Request::SetAttr {
                id, exec, mtime_ns, ..
            } => {
                let seq = self.bump();
                let n = self
                    .nodes
                    .get_mut(id)
                    .ok_or_else(|| err(ErrorCode::NotFound, "gone"))?;
                if let Some(m) = mtime_ns {
                    n.entry.mtime_ns = *m;
                }
                if let Some(x) = exec {
                    n.entry.mode = if *x {
                        n.entry.mode | 0o111
                    } else {
                        n.entry.mode & !0o111
                    };
                    n.entry.version.meta = seq;
                }
                n.entry.seq = seq;
                let e = n.entry.clone();
                Ok((Response::Entry(e.clone()), vec![Change::Upsert(e)]))
            }
            _ => Err(err(ErrorCode::Unsupported, "not a mutation")),
        }
    }
}

fn op_of(r: &Request) -> Option<(OpId, &'static str)> {
    match r {
        Request::Write { op, .. } => Some((*op, "write")),
        Request::Mkdir { op, .. } => Some((*op, "mkdir")),
        Request::Symlink { op, .. } => Some((*op, "symlink")),
        Request::Rename { op, .. } => Some((*op, "rename")),
        Request::Remove { op, .. } => Some((*op, "remove")),
        Request::SetAttr { op, .. } => Some((*op, "setattr")),
        _ => None,
    }
}

#[derive(Clone)]
pub(crate) struct FakeServer {
    pub fs: Arc<Mutex<Fs>>,
}

impl FakeServer {
    pub fn new() -> FakeServer {
        FakeServer {
            fs: Arc::new(Mutex::new(Fs::new())),
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Fs) -> R) -> R {
        let mut g = self.fs.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut g)
    }

    pub fn opener(&self) -> Opener {
        let me = self.clone();
        Arc::new(move |_interactive| {
            let me = me.clone();
            Box::pin(async move {
                if let Some(e) = me.with(|fs| fs.faults.connect_error.clone()) {
                    return Err(e);
                }
                let (a, b) = tokio::io::duplex(256 * 1024);
                tokio::spawn(serve(me.fs.clone(), b));
                let (r, w) = tokio::io::split(a);
                Ok(LinkParts {
                    reader: Box::new(r),
                    writer: Box::new(w),
                    child: None,
                })
            })
        })
    }
}

struct Upload {
    req: Request,
    data: Vec<u8>,
}

async fn serve(fs: Arc<Mutex<Fs>>, stream: tokio::io::DuplexStream) {
    let (mut r, mut w) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMsg>();
    // Pre-encoded frames (bulk data charged by its encoded size, like unlatchd).
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        let mut raw_open = true;
        loop {
            let f = tokio::select! {
                m = rx.recv() => match m {
                    Some(m) => match frame::encode(&m, true) {
                        Ok(f) => f,
                        Err(_) => return,
                    },
                    None => break,
                },
                f = raw_rx.recv(), if raw_open => match f {
                    Some(f) => f,
                    None => {
                        raw_open = false;
                        continue;
                    }
                },
            };
            if w.write_all(&f).await.is_err() {
                return;
            }
        }
        let _ = w.shutdown().await;
    });
    // Hello
    let hello: ClientMsg = match aio::read(&mut r).await {
        Ok(Some(m)) => m,
        _ => return,
    };
    let (resume, client) = match &hello {
        ClientMsg::Hello {
            resume,
            client_name,
            ..
        } => (resume.clone(), client_name.clone()),
        _ => return,
    };
    {
        let mut g = fs.lock().unwrap_or_else(|p| p.into_inner());
        g.hellos.push(hello.clone());
        g.sessions += 1;
        let info = ServerInfo {
            version: "fake".into(),
            hostname: "vm".into(),
            root_path: ROOT_PATH.into(),
            entries: g.nodes.len() as u64,
            watches: 0,
            polled: false,
            warnings: vec![],
        };
        let root = g.nodes[&ItemId::ROOT].entry.clone();
        let resumable = resume.as_ref().is_some_and(|rs| rs.index == g.index);
        let mode = if resumable {
            WelcomeMode::Resume
        } else {
            WelcomeMode::Snapshot
        };
        let _ = tx.send(ServerMsg::Welcome {
            proto: unlatch_proto::PROTO_VERSION,
            index: g.index,
            seq: g.seq,
            mode: mode.clone(),
            root,
            info,
            lazy_names: vec![],
        });
        if resumable {
            let since = resume.as_ref().map_or(0, |r| r.seq);
            let mut changes: Vec<Change> = Vec::new();
            let mut ups: Vec<Entry> = g
                .nodes
                .values()
                .filter(|n| n.entry.seq > since && n.entry.id != ItemId::ROOT)
                .filter(|n| g.visible(n.entry.id))
                .map(|n| n.entry.clone())
                .collect();
            ups.sort_by_key(|e| e.seq);
            changes.extend(ups.into_iter().map(Change::Upsert));
            changes.extend(
                g.tombs
                    .iter()
                    .filter(|(_, s)| *s > since)
                    .map(|(id, s)| Change::Remove { id: *id, seq: *s }),
            );
            let _ = tx.send(ServerMsg::Events {
                seq: g.seq,
                changes,
                batch_end: true,
            });
        } else {
            if let Some(ev) = g.faults.event_before_snapshot.take() {
                let _ = tx.send(ServerMsg::Events {
                    seq: g.seq,
                    changes: ev,
                    batch_end: true,
                });
            }
            // Breadth-first chunks of ≤ 50 entries.
            let mut queue = vec![ItemId::ROOT];
            let mut chunk = Vec::new();
            let mut complete = Vec::new();
            while let Some(d) = queue.pop() {
                let n = &g.nodes[&d];
                if n.hidden {
                    continue;
                }
                complete.push(d);
                for c in n.children.values() {
                    let cn = &g.nodes[c];
                    chunk.push(cn.entry.clone());
                    if cn.entry.kind == Kind::Dir {
                        queue.insert(0, *c);
                    }
                    if chunk.len() >= 50 {
                        let _ = tx.send(ServerMsg::SnapshotChunk {
                            entries: std::mem::take(&mut chunk),
                            complete_dirs: vec![],
                        });
                    }
                }
            }
            let _ = tx.send(ServerMsg::SnapshotChunk {
                entries: chunk,
                complete_dirs: complete,
            });
            let _ = tx.send(ServerMsg::SnapshotDone { seq: g.seq });
        }
        g.subs.push(tx.clone());
    }
    let credit = Arc::new((
        AtomicI64::new(super::session::INITIAL_CREDIT),
        Notify::new(),
    ));
    let mut uploads: HashMap<u32, Upload> = HashMap::new();
    let cancelled: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
    loop {
        let m: ClientMsg = match aio::read(&mut r).await {
            Ok(Some(m)) => m,
            _ => break,
        };
        match m {
            ClientMsg::Hello { .. } => break,
            ClientMsg::Credit { bulk_bytes } => {
                let bal =
                    credit.0.fetch_add(bulk_bytes as i64, Ordering::AcqRel) + bulk_bytes as i64;
                {
                    let mut g = fs.lock().unwrap_or_else(|p| p.into_inner());
                    g.max_credit_balance = g.max_credit_balance.max(bal);
                    g.credit_granted += bulk_bytes as u64;
                }
                credit.1.notify_waiters();
                credit.1.notify_one();
            }
            ClientMsg::Cancel { req_id } => {
                cancelled
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(req_id);
                if uploads.remove(&req_id).is_some() {
                    let _ = tx.send(ServerMsg::Error {
                        req_id: Some(req_id),
                        err: err(ErrorCode::Cancelled, "cancelled"),
                    });
                }
            }
            ClientMsg::WriteChunk { req_id, data, last } => {
                let n = data.len() as u32;
                if let Some(u) = uploads.get_mut(&req_id) {
                    u.data.extend_from_slice(&data);
                }
                if n > 0 {
                    let _ = tx.send(ServerMsg::Credit { bulk_bytes: n });
                }
                if last {
                    if let Some(u) = uploads.remove(&req_id) {
                        if !mutate(&fs, &tx, req_id, &u.req, Some(u.data), &client) {
                            break;
                        }
                    }
                }
            }
            ClientMsg::Request { req_id, req } => {
                fs.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .requests
                    .push(req.clone());
                match req {
                    Request::Ping { nonce } => {
                        let (ignore, seq) = {
                            let g = fs.lock().unwrap_or_else(|p| p.into_inner());
                            (g.faults.ignore_pings, g.seq)
                        };
                        if !ignore {
                            let _ = tx.send(ServerMsg::Response {
                                req_id,
                                resp: Response::Pong { nonce, seq },
                            });
                        }
                    }
                    Request::Stat { id } => {
                        let g = fs.lock().unwrap_or_else(|p| p.into_inner());
                        let msg = match g.nodes.get(&id) {
                            Some(n) => ServerMsg::Response {
                                req_id,
                                resp: Response::Entry(n.entry.clone()),
                            },
                            None => ServerMsg::Error {
                                req_id: Some(req_id),
                                err: err(ErrorCode::NotFound, "gone"),
                            },
                        };
                        let _ = tx.send(msg);
                    }
                    Request::ListDir { dir } => {
                        let delay = fs
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .faults
                            .listdir_delay_ms;
                        if delay > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        }
                        let mut g = fs.lock().unwrap_or_else(|p| p.into_inner());
                        let Some(n) = g.nodes.get(&dir).cloned() else {
                            let _ = tx.send(ServerMsg::Error {
                                req_id: Some(req_id),
                                err: err(ErrorCode::NotFound, "gone"),
                            });
                            continue;
                        };
                        let mut dir_e = n.entry.clone();
                        if n.hidden {
                            let seq = g.bump();
                            if let Some(m) = g.nodes.get_mut(&dir) {
                                m.hidden = false;
                                m.entry.lazy = false;
                                m.entry.seq = seq;
                                dir_e = m.entry.clone();
                            }
                        }
                        let kids: Vec<Entry> = n
                            .children
                            .values()
                            .map(|c| g.nodes[c].entry.clone())
                            .collect();
                        let parts: Vec<Vec<Entry>> = kids.chunks(40).map(|c| c.to_vec()).collect();
                        if parts.is_empty() {
                            let _ = tx.send(ServerMsg::Response {
                                req_id,
                                resp: Response::ListingPart {
                                    dir: dir_e.clone(),
                                    entries: vec![],
                                    last: true,
                                },
                            });
                        }
                        let np = parts.len();
                        for (i, p) in parts.into_iter().enumerate() {
                            let _ = tx.send(ServerMsg::Response {
                                req_id,
                                resp: Response::ListingPart {
                                    dir: dir_e.clone(),
                                    entries: p,
                                    last: i + 1 == np,
                                },
                            });
                        }
                    }
                    Request::Unwatch { .. } => {}
                    Request::Read {
                        id,
                        offset,
                        len,
                        expect,
                    } => {
                        let (data, ver) = {
                            let g = fs.lock().unwrap_or_else(|p| p.into_inner());
                            match g.nodes.get(&id) {
                                None => {
                                    let _ = tx.send(ServerMsg::Error {
                                        req_id: Some(req_id),
                                        err: err(ErrorCode::NotFound, "gone"),
                                    });
                                    continue;
                                }
                                Some(n) => {
                                    if expect.is_some_and(|e| e != n.entry.version.content) {
                                        let _ = tx.send(ServerMsg::Error {
                                            req_id: Some(req_id),
                                            err: err(ErrorCode::VersionMismatch, "changed"),
                                        });
                                        continue;
                                    }
                                    let s = (offset as usize).min(n.content.len());
                                    let e = len.map_or(n.content.len(), |l| {
                                        (s + l as usize).min(n.content.len())
                                    });
                                    (n.content[s..e].to_vec(), n.entry.version.content)
                                }
                            }
                        };
                        let tx = raw_tx.clone();
                        let credit = credit.clone();
                        let cancelled = cancelled.clone();
                        let fs2 = fs.clone();
                        tokio::spawn(async move {
                            let chunks: Vec<&[u8]> = if data.is_empty() {
                                vec![&[][..]]
                            } else {
                                data.chunks(frame::BULK_CHUNK).collect()
                            };
                            let n = chunks.len();
                            let mut off = offset;
                            for (i, c) in chunks.into_iter().enumerate() {
                                let Ok(f) = frame::encode(
                                    &ServerMsg::ReadChunk {
                                        req_id,
                                        offset: off,
                                        data: c.to_vec(),
                                        last: i + 1 == n,
                                        version: ver,
                                    },
                                    true,
                                ) else {
                                    return;
                                };
                                // Bulk data only against the client's credit, charged by the
                                // frame body as sent (wire.rs "Credit measure"): wait for a
                                // positive balance, then take it below zero by at most this frame.
                                let cost = (f.len() - 4) as i64;
                                loop {
                                    let notified = credit.1.notified();
                                    let avail = credit.0.load(Ordering::Acquire);
                                    if avail > 0 {
                                        credit.0.fetch_sub(cost, Ordering::AcqRel);
                                        break;
                                    }
                                    if tokio::time::timeout(
                                        std::time::Duration::from_secs(10),
                                        notified,
                                    )
                                    .await
                                    .is_err()
                                    {
                                        return;
                                    }
                                }
                                {
                                    let mut g = fs2.lock().unwrap_or_else(|p| p.into_inner());
                                    let used = super::session::INITIAL_CREDIT
                                        - credit.0.load(Ordering::Acquire);
                                    g.max_outstanding_read = g.max_outstanding_read.max(used);
                                    g.read_wire_bytes += cost as u64;
                                }
                                if cancelled
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .contains(&req_id)
                                {
                                    return;
                                }
                                let _ = tx.send(f);
                                off += c.len() as u64;
                                tokio::task::yield_now().await;
                            }
                        });
                    }
                    Request::Write { .. } => {
                        uploads.insert(
                            req_id,
                            Upload {
                                req,
                                data: Vec::new(),
                            },
                        );
                    }
                    other => {
                        if !mutate(&fs, &tx, req_id, &other, None, &client) {
                            break;
                        }
                    }
                }
            }
        }
    }
    {
        let mut g = fs.lock().unwrap_or_else(|p| p.into_inner());
        g.subs.retain(|s| !s.same_channel(&tx));
    }
    drop(tx);
    writer.abort();
}

/// Execute a mutation with the ops table; returns false when the fault says "die now".
fn mutate(
    fs: &Arc<Mutex<Fs>>,
    tx: &mpsc::UnboundedSender<ServerMsg>,
    req_id: u32,
    req: &Request,
    upload: Option<Vec<u8>>,
    client: &str,
) -> bool {
    let mut g = fs.lock().unwrap_or_else(|p| p.into_inner());
    let Some((op, kind)) = op_of(req) else {
        return true;
    };
    if let Some(resp) = g.ops.get(&op).cloned() {
        let _ = tx.send(ServerMsg::Response { req_id, resp });
        return true;
    }
    match g.exec(req, upload, client) {
        Ok((resp, changes)) => {
            g.ops.insert(op, resp.clone());
            g.broadcast(changes);
            if let Response::Written {
                entry,
                conflict_copy: None,
            } = &resp
            {
                if let Some(extra) = g.faults.agent_append_after_write.take() {
                    let seq = g.bump();
                    if let Some(n) = g.nodes.get_mut(&entry.id) {
                        n.content.extend_from_slice(&extra);
                        n.entry.size = n.content.len() as u64;
                        n.entry.version.content = seq;
                        n.entry.seq = seq;
                        let e = n.entry.clone();
                        g.broadcast(vec![Change::Upsert(e)]);
                    }
                }
            }
            if g.faults.die_after_commit == Some(kind) {
                g.faults.die_after_commit = None;
                g.subs.retain(|s| !s.same_channel(tx));
                return false;
            }
            let _ = tx.send(ServerMsg::Response { req_id, resp });
        }
        Err(e) => {
            let _ = tx.send(ServerMsg::Error {
                req_id: Some(req_id),
                err: e,
            });
        }
    }
    true
}
