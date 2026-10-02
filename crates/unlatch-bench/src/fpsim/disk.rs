//! fileproviderd's local side: the files under `~/Library/CloudStorage/<domain>` plus the item
//! database that maps them to provider identifiers.
//!
//! The namespace is case- and normalization-insensitive like APFS: two siblings can never fold to
//! the same key (see [`super::names::fold`]).

use super::names::fold;
use std::collections::{BTreeMap, HashMap};
use unlatch_proto::ipc::LocalMeta;
use unlatch_proto::{ErrorCode, ItemId, Kind, Version};

/// Local handle of a node (stable for the node's life; unrelated to [`ItemId`]).
pub type NodeKey = u64;

/// One file, directory or symlink on the simulated Mac disk.
#[derive(Clone, Debug)]
pub struct Node {
    pub key: NodeKey,
    /// `None` only for the domain root.
    pub parent: Option<NodeKey>,
    /// The name on the local disk (normally the item's `display_name`).
    pub name: String,
    pub kind: Kind,
    /// Provider identifier; `None` while the item exists only locally (pending create).
    pub id: Option<ItemId>,
    /// `itemTemplate.itemIdentifier` used for creates; kept across replays.
    pub template_id: String,
    /// The version the system believes the item has (MQ-013: whatever the last reply said).
    pub version: Option<Version>,
    pub size: u64,
    /// Files: `Some` when materialized, `None` when dataless.
    pub content: Option<Vec<u8>>,
    pub symlink_target: Option<String>,
    pub symlink_blocked: bool,
    /// Directories: the container enumerator ran (MQ-001: at most once, ever).
    pub enumerated: bool,
    /// Excluded from sync (`.DS_Store` & co, or `ExcludedFromSync` from the provider).
    pub local_only: bool,
    pub user_exec: bool,
    pub caps: u32,
    pub local: LocalMeta,
    pub mtime_ns: i64,
    /// `changed_fields` not yet acknowledged by the provider.
    pub dirty: u32,
    pub pending_create: bool,
    /// Re-offered as a create after the provider reported the item deleted (MQ-080).
    pub deletion_conflicted: bool,
    /// Set on creates replayed after a reimport.
    pub may_already_exist: bool,
    /// Ordering key of the latest local change (uploads go out in user-action order).
    pub dirty_seq: u64,
    /// Creation order on the local disk (MQ-016 renames the *older* item).
    pub born: u64,
    /// When the system renamed this item locally to dodge a collision (MQ-016), the provider
    /// name it is hiding from; later updates carrying that name keep the local rename.
    pub bounced_from: Option<String>,
    /// The (display) name the provider last reported for this item — differs from `name` while
    /// a local rename is pending or a bounce is in effect.
    pub server_name: Option<String>,
    /// Upload failed permanently (`cannotSynchronize`); shown as an item error, not retried.
    pub sync_error: Option<ErrorCode>,
    /// Provider reported this directory deleted while it still had local children
    /// ("a directory stays until its children are deleted").
    pub remove_when_empty: bool,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    /// Something about this node still has to reach the provider.
    pub fn has_pending(&self) -> bool {
        !self.local_only && self.sync_error.is_none() && (self.pending_create || self.dirty != 0)
    }
}

/// Why a namespace operation was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NsError {
    /// Another sibling already folds to the same name.
    Collision(NodeKey),
    NoSuchNode,
    NotADirectory,
    /// Moving a directory into its own subtree.
    Cycle,
}

impl std::fmt::Display for NsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NsError::Collision(k) => write!(f, "name collides with node {k}"),
            NsError::NoSuchNode => write!(f, "no such node"),
            NsError::NotADirectory => write!(f, "not a directory"),
            NsError::Cycle => write!(f, "move into own subtree"),
        }
    }
}

impl std::error::Error for NsError {}

/// The simulated local volume + item database.
#[derive(Clone, Debug)]
pub struct LocalDisk {
    nodes: HashMap<NodeKey, Node>,
    /// parent → (folded name → child).
    children: HashMap<NodeKey, BTreeMap<String, NodeKey>>,
    by_id: HashMap<ItemId, NodeKey>,
    next_key: NodeKey,
    born: u64,
    root: NodeKey,
}

impl LocalDisk {
    pub fn new() -> LocalDisk {
        let mut d = LocalDisk {
            nodes: HashMap::new(),
            children: HashMap::new(),
            by_id: HashMap::new(),
            next_key: 1,
            born: 0,
            root: 0,
        };
        let mut root = d.blank(None, String::new(), Kind::Dir);
        root.id = Some(ItemId::ROOT);
        let key = root.key;
        d.root = key;
        d.by_id.insert(ItemId::ROOT, key);
        d.children.insert(key, BTreeMap::new());
        d.nodes.insert(key, root);
        d
    }

    /// A detached node with defaults; insert it with [`LocalDisk::insert`].
    pub fn blank(&mut self, parent: Option<NodeKey>, name: String, kind: Kind) -> Node {
        let key = self.next_key;
        self.next_key += 1;
        self.born += 1;
        Node {
            key,
            parent,
            name,
            kind,
            id: None,
            template_id: String::new(),
            version: None,
            size: 0,
            content: None,
            symlink_target: None,
            symlink_blocked: false,
            enumerated: false,
            local_only: false,
            user_exec: false,
            caps: 0,
            local: LocalMeta::default(),
            mtime_ns: 0,
            dirty: 0,
            pending_create: false,
            deletion_conflicted: false,
            may_already_exist: false,
            dirty_seq: 0,
            born: self.born,
            bounced_from: None,
            server_name: None,
            sync_error: None,
            remove_when_empty: false,
        }
    }

    pub fn root(&self) -> NodeKey {
        self.root
    }

    pub fn get(&self, key: NodeKey) -> Option<&Node> {
        self.nodes.get(&key)
    }

    pub fn get_mut(&mut self, key: NodeKey) -> Option<&mut Node> {
        self.nodes.get_mut(&key)
    }

    pub fn by_id(&self, id: ItemId) -> Option<NodeKey> {
        self.by_id.get(&id).copied()
    }

    /// Record (or clear) the provider identifier of a node.
    pub fn set_id(&mut self, key: NodeKey, id: Option<ItemId>) {
        let Some(node) = self.nodes.get_mut(&key) else {
            return;
        };
        if let Some(old) = node.id.take() {
            if self.by_id.get(&old) == Some(&key) {
                self.by_id.remove(&old);
            }
        }
        node.id = id;
        if let Some(id) = id {
            self.by_id.insert(id, key);
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Child of `parent` whose name folds equal to `name`.
    pub fn lookup(&self, parent: NodeKey, name: &str) -> Option<NodeKey> {
        self.children.get(&parent)?.get(&fold(name)).copied()
    }

    /// Children of `parent` in folded-name order.
    pub fn children(&self, parent: NodeKey) -> Vec<NodeKey> {
        self.children
            .get(&parent)
            .map(|m| m.values().copied().collect())
            .unwrap_or_default()
    }

    pub fn has_children(&self, parent: NodeKey) -> bool {
        self.children.get(&parent).is_some_and(|m| !m.is_empty())
    }

    /// Insert a detached node under `node.parent`.
    pub fn insert(&mut self, node: Node) -> Result<NodeKey, NsError> {
        let parent = node.parent.ok_or(NsError::NoSuchNode)?;
        if !self.nodes.get(&parent).is_some_and(Node::is_dir) {
            return Err(NsError::NotADirectory);
        }
        let key = fold(&node.name);
        let siblings = self.children.entry(parent).or_default();
        if let Some(&other) = siblings.get(&key) {
            return Err(NsError::Collision(other));
        }
        siblings.insert(key, node.key);
        if node.is_dir() {
            self.children.entry(node.key).or_default();
        }
        if let Some(id) = node.id {
            self.by_id.insert(id, node.key);
        }
        let k = node.key;
        self.nodes.insert(k, node);
        Ok(k)
    }

    /// Move/rename `key` to `(new_parent, new_name)`. Case-only renames of the same node are fine.
    pub fn rename(
        &mut self,
        key: NodeKey,
        new_parent: NodeKey,
        new_name: &str,
    ) -> Result<(), NsError> {
        let node = self.nodes.get(&key).ok_or(NsError::NoSuchNode)?;
        let old_parent = node.parent.ok_or(NsError::NoSuchNode)?;
        let old_key = fold(&node.name);
        if !self.nodes.get(&new_parent).is_some_and(Node::is_dir) {
            return Err(NsError::NotADirectory);
        }
        if self.is_ancestor_or_self(key, new_parent) {
            return Err(NsError::Cycle);
        }
        let new_key = fold(new_name);
        if let Some(&other) = self.children.get(&new_parent).and_then(|m| m.get(&new_key)) {
            if other != key {
                return Err(NsError::Collision(other));
            }
        }
        if let Some(m) = self.children.get_mut(&old_parent) {
            m.remove(&old_key);
        }
        self.children
            .entry(new_parent)
            .or_default()
            .insert(new_key, key);
        if let Some(n) = self.nodes.get_mut(&key) {
            n.parent = Some(new_parent);
            n.name = new_name.to_string();
        }
        Ok(())
    }

    /// `true` when `anc` is `key` or one of its ancestors.
    pub fn is_ancestor_or_self(&self, anc: NodeKey, mut key: NodeKey) -> bool {
        loop {
            if key == anc {
                return true;
            }
            match self.nodes.get(&key).and_then(|n| n.parent) {
                Some(p) => key = p,
                None => return false,
            }
        }
    }

    /// Detach and return `key` and everything below it (post-order: children first).
    pub fn remove_subtree(&mut self, key: NodeKey) -> Vec<Node> {
        let mut order = Vec::new();
        self.post_order(key, &mut order);
        let mut out = Vec::with_capacity(order.len());
        for k in order {
            let Some(node) = self.nodes.remove(&k) else {
                continue;
            };
            if let Some(p) = node.parent {
                if let Some(m) = self.children.get_mut(&p) {
                    m.remove(&fold(&node.name));
                }
            }
            self.children.remove(&k);
            if let Some(id) = node.id {
                if self.by_id.get(&id) == Some(&k) {
                    self.by_id.remove(&id);
                }
            }
            out.push(node);
        }
        out
    }

    fn post_order(&self, key: NodeKey, out: &mut Vec<NodeKey>) {
        for c in self.children(key) {
            self.post_order(c, out);
        }
        out.push(key);
    }

    /// All nodes below `key` (excluding it), pre-order.
    pub fn descendants(&self, key: NodeKey) -> Vec<NodeKey> {
        let mut out = Vec::new();
        let mut stack = self.children(key);
        stack.reverse();
        while let Some(k) = stack.pop() {
            out.push(k);
            let mut c = self.children(k);
            c.reverse();
            stack.extend(c);
        }
        out
    }

    /// `/`-joined local path relative to the domain root ("" for the root).
    pub fn path_of(&self, mut key: NodeKey) -> String {
        let mut parts = Vec::new();
        while let Some(n) = self.nodes.get(&key) {
            match n.parent {
                Some(p) => {
                    parts.push(n.name.clone());
                    key = p;
                }
                None => break,
            }
        }
        parts.reverse();
        parts.join("/")
    }

    /// Resolve a `/`-separated local path (case-insensitively, like APFS). "" = root.
    pub fn resolve(&self, path: &str) -> Option<NodeKey> {
        let mut cur = self.root;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = self.lookup(cur, comp)?;
        }
        Some(cur)
    }

    /// Every node key, root included, in pre-order.
    pub fn all(&self) -> Vec<NodeKey> {
        let mut v = vec![self.root];
        v.extend(self.descendants(self.root));
        v
    }

    /// Nodes carrying the same provider identifier more than once (must never happen).
    pub fn duplicate_ids(&self) -> Vec<ItemId> {
        let mut seen: HashMap<ItemId, u32> = HashMap::new();
        for n in self.nodes.values() {
            if let Some(id) = n.id {
                *seen.entry(id).or_default() += 1;
            }
        }
        let mut d: Vec<ItemId> = seen
            .into_iter()
            .filter(|(_, c)| *c > 1)
            .map(|(id, _)| id)
            .collect();
        d.sort();
        d
    }
}

impl Default for LocalDisk {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(d: &mut LocalDisk, parent: NodeKey, name: &str) -> NodeKey {
        let n = d.blank(Some(parent), name.to_string(), Kind::File);
        d.insert(n).expect("insert")
    }

    #[test]
    fn namespace_is_case_and_nfd_insensitive() {
        let mut d = LocalDisk::new();
        let r = d.root();
        let a = file(&mut d, r, "README");
        let n = d.blank(Some(r), "readme".into(), Kind::File);
        assert_eq!(d.insert(n), Err(NsError::Collision(a)));
        let c = file(&mut d, r, "caf\u{e9}");
        let n = d.blank(Some(r), "cafe\u{301}".into(), Kind::File);
        assert_eq!(d.insert(n), Err(NsError::Collision(c)));
        assert_eq!(d.resolve("ReadMe"), Some(a));
        // case-only rename of the same node is allowed
        d.rename(a, r, "readme").expect("case rename");
        assert_eq!(d.get(a).map(|n| n.name.as_str()), Some("readme"));
    }

    #[test]
    fn subtree_removal_is_children_first_and_clears_ids() {
        let mut d = LocalDisk::new();
        let r = d.root();
        let mut dir = d.blank(Some(r), "d".into(), Kind::Dir);
        dir.id = Some(ItemId(5));
        let dk = d.insert(dir).expect("dir");
        let f = file(&mut d, dk, "f");
        d.set_id(f, Some(ItemId(6)));
        assert_eq!(d.path_of(f), "d/f");
        let gone = d.remove_subtree(dk);
        assert_eq!(gone.iter().map(|n| n.key).collect::<Vec<_>>(), vec![f, dk]);
        assert!(d.by_id(ItemId(5)).is_none() && d.by_id(ItemId(6)).is_none());
        assert!(d.resolve("d").is_none());
    }

    #[test]
    fn move_into_own_subtree_is_refused() {
        let mut d = LocalDisk::new();
        let r = d.root();
        let a = d.blank(Some(r), "a".into(), Kind::Dir);
        let a = d.insert(a).expect("a");
        let b = d.blank(Some(a), "b".into(), Kind::Dir);
        let b = d.insert(b).expect("b");
        assert_eq!(d.rename(a, b, "a"), Err(NsError::Cycle));
    }
}
