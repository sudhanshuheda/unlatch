//! In-memory [`Backend`] modelling the engine contract closely enough to test the FUSE layer:
//! seq-based versions, conflict copies on stale bases, never-failing creates (`name 2`),
//! `DeletionRejected` on stale deletes / non-empty dirs, and `ReplicaChanged` events for both
//! local and simulated VM-side changes.

use super::EventSink;
use crate::backend::Backend;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use unlatch_core::{
    CreateKind, CreateRequest, EngineEvent, Fetched, Modified, ModifyRequest, Page, Result,
};
use unlatch_proto::ipc::{fields, IpcItem, LocalMeta};
use unlatch_proto::{
    BaseVersion, Entry, ErrorCode, ItemId, Kind, ProtoError, Version, ACCESS_R, ACCESS_W, ACCESS_X,
};

struct Node {
    item: IpcItem,
    content: Vec<u8>,
}

struct State {
    nodes: HashMap<ItemId, Node>,
    seq: u64,
    next_id: u64,
}

pub struct MemBackend {
    st: Mutex<State>,
    sink: Mutex<Option<EventSink>>,
    pub creates: AtomicUsize,
    pub modifies: AtomicUsize,
    pub deletes: AtomicUsize,
    pub reads: AtomicUsize,
    /// Fail every create/modify (VM unreachable).
    pub fail_uploads: std::sync::atomic::AtomicBool,
    pub lists: AtomicUsize,
}

fn err(code: ErrorCode, msg: &str) -> ProtoError {
    ProtoError::new(code, msg)
}

fn now_ns() -> i64 {
    super::attr::now_ns()
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

impl MemBackend {
    pub fn new() -> MemBackend {
        let root = IpcItem {
            entry: Entry {
                id: ItemId::ROOT,
                parent: ItemId::ROOT,
                name: String::new(),
                kind: Kind::Dir,
                size: 0,
                mtime_ns: now_ns(),
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
            display_name: String::new(),
            caps: 0,
            local: LocalMeta::default(),
            user_exec: false,
            symlink_blocked: false,
        };
        let mut nodes = HashMap::new();
        nodes.insert(
            ItemId::ROOT,
            Node {
                item: root,
                content: Vec::new(),
            },
        );
        MemBackend {
            st: Mutex::new(State {
                nodes,
                seq: 1,
                next_id: 2,
            }),
            sink: Mutex::new(None),
            creates: AtomicUsize::new(0),
            modifies: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            fail_uploads: std::sync::atomic::AtomicBool::new(false),
            lists: AtomicUsize::new(0),
        }
    }

    pub fn set_sink(&self, sink: EventSink) {
        *super::lock(&self.sink) = Some(sink);
    }

    fn emit(&self, ids: Vec<ItemId>, parents: Vec<ItemId>) {
        if let Some(s) = super::lock(&self.sink).as_ref() {
            s.on_event(EngineEvent::ReplicaChanged { ids, parents });
        }
    }

    fn child(st: &State, parent: ItemId, name: &str) -> Option<ItemId> {
        st.nodes
            .values()
            .find(|n| {
                n.item.entry.parent == parent
                    && n.item.entry.id != parent
                    && n.item.entry.name == name
            })
            .map(|n| n.item.entry.id)
    }

    fn insert(
        st: &mut State,
        parent: ItemId,
        name: &str,
        kind: Kind,
        content: Vec<u8>,
        target: Option<String>,
    ) -> IpcItem {
        st.seq += 1;
        let id = ItemId(st.next_id);
        st.next_id += 1;
        let size = match kind {
            Kind::File => content.len() as u64,
            Kind::Symlink => target.as_ref().map(|t| t.len() as u64).unwrap_or(0),
            Kind::Dir => 0,
        };
        let item = IpcItem {
            entry: Entry {
                id,
                parent,
                name: name.to_string(),
                kind,
                size,
                mtime_ns: now_ns(),
                mode: if kind == Kind::Dir { 0o755 } else { 0o644 },
                version: Version {
                    content: st.seq,
                    meta: st.seq,
                },
                symlink_target: target,
                lazy: false,
                seq: st.seq,
                access: ACCESS_R | ACCESS_W | ACCESS_X,
            },
            display_name: name.to_string(),
            caps: 0,
            local: LocalMeta::default(),
            user_exec: false,
            symlink_blocked: false,
        };
        st.nodes.insert(
            id,
            Node {
                item: item.clone(),
                content,
            },
        );
        item
    }

    fn free_name(st: &State, parent: ItemId, name: &str) -> String {
        if Self::child(st, parent, name).is_none() {
            return name.to_string();
        }
        let (stem, ext) = split_ext(name);
        (2..)
            .map(|n| format!("{stem} {n}{ext}"))
            .find(|c| Self::child(st, parent, c).is_none())
            .unwrap_or_default()
    }

    fn set_content(st: &mut State, id: ItemId, content: Vec<u8>) {
        st.seq += 1;
        let seq = st.seq;
        if let Some(n) = st.nodes.get_mut(&id) {
            n.item.entry.size = content.len() as u64;
            n.item.entry.version.content = seq;
            n.item.entry.seq = seq;
            n.item.entry.mtime_ns = now_ns();
            n.content = content;
        }
    }

    fn remove_subtree(st: &mut State, id: ItemId) {
        let kids: Vec<ItemId> = st
            .nodes
            .values()
            .filter(|n| n.item.entry.parent == id && n.item.entry.id != id)
            .map(|n| n.item.entry.id)
            .collect();
        for k in kids {
            Self::remove_subtree(st, k);
        }
        st.nodes.remove(&id);
    }

    // ---- simulated VM-side changes (emit events like the engine would) ----------------------

    pub fn vm_path(&self, path: &str) -> Option<ItemId> {
        let st = super::lock(&self.st);
        let mut cur = ItemId::ROOT;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = Self::child(&st, cur, comp)?;
        }
        Some(cur)
    }

    pub fn vm_content(&self, path: &str) -> Option<Vec<u8>> {
        let id = self.vm_path(path)?;
        super::lock(&self.st)
            .nodes
            .get(&id)
            .map(|n| n.content.clone())
    }

    pub fn vm_names(&self, dir: &str) -> Vec<String> {
        let Some(d) = self.vm_path(dir) else {
            return Vec::new();
        };
        let st = super::lock(&self.st);
        let mut v: Vec<String> = st
            .nodes
            .values()
            .filter(|n| n.item.entry.parent == d && n.item.entry.id != d)
            .map(|n| n.item.entry.name.clone())
            .collect();
        v.sort();
        v
    }

    /// Create or overwrite a file on the "VM".
    pub fn vm_write(&self, dir: &str, name: &str, content: &[u8]) -> ItemId {
        let parent = self.vm_path(dir).unwrap_or(ItemId::ROOT);
        let id = {
            let mut st = super::lock(&self.st);
            match Self::child(&st, parent, name) {
                Some(id) => {
                    Self::set_content(&mut st, id, content.to_vec());
                    id
                }
                None => {
                    Self::insert(&mut st, parent, name, Kind::File, content.to_vec(), None)
                        .entry
                        .id
                }
            }
        };
        self.emit(vec![id], vec![parent]);
        id
    }

    pub fn vm_mkdir(&self, dir: &str, name: &str) -> ItemId {
        let parent = self.vm_path(dir).unwrap_or(ItemId::ROOT);
        let id = {
            let mut st = super::lock(&self.st);
            Self::insert(&mut st, parent, name, Kind::Dir, Vec::new(), None)
                .entry
                .id
        };
        self.emit(vec![id], vec![parent]);
        id
    }

    /// Simulate an engine whose replica is still empty (before the first snapshot).
    pub fn vm_remove_root_for_test(&self) {
        super::lock(&self.st).nodes.remove(&ItemId::ROOT);
    }

    pub fn vm_remove(&self, path: &str) {
        let Some(id) = self.vm_path(path) else { return };
        let parent = {
            let mut st = super::lock(&self.st);
            let parent = st
                .nodes
                .get(&id)
                .map(|n| n.item.entry.parent)
                .unwrap_or(ItemId::ROOT);
            Self::remove_subtree(&mut st, id);
            st.seq += 1;
            parent
        };
        self.emit(vec![id], vec![parent]);
    }

    pub fn vm_rename(&self, path: &str, new_dir: &str, new_name: &str) {
        let Some(id) = self.vm_path(path) else { return };
        let np = self.vm_path(new_dir).unwrap_or(ItemId::ROOT);
        let old_parent = {
            let mut st = super::lock(&self.st);
            st.seq += 1;
            let seq = st.seq;
            let Some(n) = st.nodes.get_mut(&id) else {
                return;
            };
            let old = n.item.entry.parent;
            n.item.entry.parent = np;
            n.item.entry.name = new_name.to_string();
            n.item.display_name = new_name.to_string();
            n.item.entry.version.meta = seq;
            n.item.entry.seq = seq;
            old
        };
        self.emit(vec![id], vec![old_parent, np]);
    }
}

impl Backend for MemBackend {
    fn item(&self, id: ItemId) -> Result<IpcItem> {
        super::lock(&self.st)
            .nodes
            .get(&id)
            .map(|n| n.item.clone())
            .ok_or_else(|| err(ErrorCode::NotFound, "no item"))
    }

    fn lookup(&self, parent: ItemId, name: &str) -> Result<IpcItem> {
        let st = super::lock(&self.st);
        let id = Self::child(&st, parent, name)
            .ok_or_else(|| err(ErrorCode::NotFound, "no such name"))?;
        Ok(st.nodes[&id].item.clone())
    }

    fn list(
        &self,
        container: ItemId,
        cursor: Option<&[u8]>,
        limit: u32,
        _viewer: bool,
    ) -> Result<Page> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        let st = super::lock(&self.st);
        if !st.nodes.contains_key(&container) {
            return Err(err(ErrorCode::NotFound, "no container"));
        }
        let mut items: Vec<IpcItem> = st
            .nodes
            .values()
            .filter(|n| n.item.entry.parent == container && n.item.entry.id != container)
            .map(|n| n.item.clone())
            .collect();
        items.sort_by(|a, b| a.display_name.as_bytes().cmp(b.display_name.as_bytes()));
        let start = cursor
            .and_then(|c| std::str::from_utf8(c).ok())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let end = (start + limit as usize).min(items.len());
        let next = (end < items.len()).then(|| end.to_string().into_bytes());
        Ok(Page {
            items: items[start.min(end)..end].to_vec(),
            next,
        })
    }

    fn read(&self, id: ItemId, offset: u64, len: u32) -> Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let st = super::lock(&self.st);
        let n = st
            .nodes
            .get(&id)
            .ok_or_else(|| err(ErrorCode::NotFound, "no item"))?;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(n.content.len());
        let end = start.saturating_add(len as usize).min(n.content.len());
        Ok(n.content[start..end].to_vec())
    }

    fn fetch(&self, id: ItemId, dest_dir: &Path) -> Result<Fetched> {
        let (item, content) = {
            let st = super::lock(&self.st);
            let n = st
                .nodes
                .get(&id)
                .ok_or_else(|| err(ErrorCode::NotFound, "no item"))?;
            (n.item.clone(), n.content.clone())
        };
        let path = dest_dir.join(format!("fetch-{}-{:08x}", id.0, rand::random::<u32>()));
        std::fs::write(&path, &content).map_err(|e| err(ErrorCode::Io, &e.to_string()))?;
        Ok(Fetched { path, item })
    }

    fn create(&self, req: CreateRequest) -> Result<Modified> {
        if self.fail_uploads.load(Ordering::SeqCst) {
            return Err(err(ErrorCode::Offline, "not connected to the VM"));
        }
        self.creates.fetch_add(1, Ordering::SeqCst);
        let mut content = Vec::new();
        if let Some(mut f) = req.content {
            f.read_to_end(&mut content)
                .map_err(|e| err(ErrorCode::Io, &e.to_string()))?;
        }
        let kind = match req.kind {
            CreateKind::File => Kind::File,
            CreateKind::Dir => Kind::Dir,
            CreateKind::Symlink => Kind::Symlink,
            CreateKind::Package | CreateKind::Alias => {
                return Err(err(ErrorCode::ExcludedFromSync, "v1"))
            }
        };
        let item = {
            let mut st = super::lock(&self.st);
            if !st.nodes.contains_key(&req.parent) {
                return Err(err(ErrorCode::NotFound, "no parent"));
            }
            let name = Self::free_name(&st, req.parent, &req.name);
            let item = Self::insert(
                &mut st,
                req.parent,
                &name,
                kind,
                content,
                req.symlink_target,
            );
            let id = item.entry.id;
            if let Some(n) = st.nodes.get_mut(&id) {
                if let Some(t) = req.mtime_ns {
                    n.item.entry.mtime_ns = t;
                }
                if req.user_exec == Some(true) {
                    n.item.user_exec = true;
                    n.item.entry.mode |= 0o100;
                }
            }
            st.nodes[&id].item.clone()
        };
        self.emit(vec![item.entry.id], vec![req.parent]);
        Ok(Modified {
            item,
            still_pending: 0,
            should_fetch_content: false,
            conflict_copy: None,
        })
    }

    fn modify(&self, id: ItemId, base: BaseVersion, req: ModifyRequest) -> Result<Modified> {
        if self.fail_uploads.load(Ordering::SeqCst) {
            return Err(err(ErrorCode::Offline, "not connected to the VM"));
        }
        self.modifies.fetch_add(1, Ordering::SeqCst);
        let mut content = None;
        if let Some(mut f) = req.content {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)
                .map_err(|e| err(ErrorCode::Io, &e.to_string()))?;
            content = Some(buf);
        }
        let mut parents = Vec::new();
        let mut ids = vec![id];
        let result = {
            let mut st = super::lock(&self.st);
            let cur = st
                .nodes
                .get(&id)
                .map(|n| n.item.clone())
                .ok_or_else(|| err(ErrorCode::NotFound, "no item"))?;
            parents.push(cur.entry.parent);
            let mut conflict_copy = None;
            let mut should_fetch_content = false;
            if let Some(c) = content {
                if base.content.is_some_and(|b| b != cur.entry.version.content) {
                    let same = st.nodes.get(&id).is_some_and(|n| n.content == c);
                    if !same {
                        let (stem, ext) = split_ext(&cur.entry.name);
                        let copy_name = Self::free_name(
                            &st,
                            cur.entry.parent,
                            &format!("{stem} (conflict from test){ext}"),
                        );
                        let copy = Self::insert(
                            &mut st,
                            cur.entry.parent,
                            &copy_name,
                            Kind::File,
                            c,
                            None,
                        );
                        ids.push(copy.entry.id);
                        conflict_copy = Some(copy);
                        should_fetch_content = true;
                    }
                } else {
                    Self::set_content(&mut st, id, c);
                }
            }
            if req.changed_fields & (fields::FILENAME | fields::PARENT) != 0 {
                let np = req.new_parent.unwrap_or(cur.entry.parent);
                let nn = req.new_name.clone().unwrap_or(cur.entry.name.clone());
                // RENAME_NOREPLACE: a taken name leaves the item where it is (applied = false).
                if Self::child(&st, np, &nn).is_none() {
                    st.seq += 1;
                    let seq = st.seq;
                    if let Some(n) = st.nodes.get_mut(&id) {
                        n.item.entry.parent = np;
                        n.item.entry.name = nn.clone();
                        n.item.display_name = nn;
                        n.item.entry.version.meta = seq;
                        n.item.entry.seq = seq;
                    }
                    parents.push(np);
                }
            }
            if let Some(n) = st.nodes.get_mut(&id) {
                if let Some(t) = req.mtime_ns {
                    n.item.entry.mtime_ns = t;
                }
                if let Some(x) = req.user_exec {
                    n.item.user_exec = x;
                    n.item.entry.mode = if x {
                        n.item.entry.mode | 0o100
                    } else {
                        n.item.entry.mode & !0o100
                    };
                }
            }
            let item = st.nodes[&id].item.clone();
            Modified {
                item,
                still_pending: 0,
                should_fetch_content,
                conflict_copy,
            }
        };
        self.emit(ids, parents);
        Ok(result)
    }

    fn delete(&self, id: ItemId, base: BaseVersion, recursive: bool) -> Result<()> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        let parent = {
            let mut st = super::lock(&self.st);
            let Some(n) = st.nodes.get(&id) else {
                return Ok(());
            };
            let cur = n.item.entry.clone();
            if base.content.is_some_and(|b| b != cur.version.content)
                || base.meta.is_some_and(|m| m != cur.version.meta)
            {
                return Err(err(ErrorCode::DeletionRejected, "changed since seen"));
            }
            let has_kids = st
                .nodes
                .values()
                .any(|n| n.item.entry.parent == id && n.item.entry.id != id);
            if cur.kind == Kind::Dir && has_kids && !recursive {
                return Err(err(ErrorCode::DeletionRejected, "not empty"));
            }
            Self::remove_subtree(&mut st, id);
            cur.parent
        };
        self.emit(vec![id], vec![parent]);
        Ok(())
    }
}
