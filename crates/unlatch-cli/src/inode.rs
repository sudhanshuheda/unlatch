//! ItemId ↔ FUSE inode mapping.
//!
//! Inode numbers are allocated from a counter and **never reused** within a mount, so an inode
//! the kernel still holds after a reimport (new daemon index → ids mean different files, see
//! `IndexId`) can never alias a different file: [`InodeMap::reset`] drops every mapping and the
//! stale inodes answer `ESTALE`. The root is always inode 1 ↔ `ItemId::ROOT`.
//!
//! Each node tracks the kernel's lookup count (`nlookup`) so mappings are dropped on `forget`,
//! the name the kernel last learned for it (to invalidate the right dentry when the item moves
//! or disappears on the VM), and the content version whose pages the kernel may have cached
//! (so `open` can say `FOPEN_KEEP_CACHE` exactly when that is still valid).

use std::collections::HashMap;
use unlatch_proto::ItemId;

pub const ROOT_INO: u64 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// `None` = provisional: created locally, not yet uploaded (deferred create).
    pub id: Option<ItemId>,
    /// Kernel lookup count (entries replied via lookup/create/mkdir/readdirplus minus forgets).
    pub nlookup: u64,
    /// `(parent, display name)` as last reported to the kernel.
    pub dentry: Option<(ItemId, String)>,
    /// Content version the kernel's page cache may hold for this inode.
    pub cached_content: Option<u64>,
    /// Open file handles; a node with open handles survives `forget` until released.
    pub open: u32,
}

impl Node {
    fn new(id: Option<ItemId>) -> Self {
        Node {
            id,
            nlookup: 0,
            dentry: None,
            cached_content: None,
            open: 0,
        }
    }
}

#[derive(Debug)]
pub struct InodeMap {
    by_ino: HashMap<u64, Node>,
    by_id: HashMap<ItemId, u64>,
    next: u64,
    /// Bumped by [`InodeMap::reset`]; reported as the FUSE generation.
    generation: u64,
}

impl Default for InodeMap {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeMap {
    pub fn new() -> Self {
        let mut m = InodeMap {
            by_ino: HashMap::new(),
            by_id: HashMap::new(),
            next: ROOT_INO + 1,
            generation: 0,
        };
        m.insert_root();
        m
    }

    fn insert_root(&mut self) {
        let mut root = Node::new(Some(ItemId::ROOT));
        // The kernel never forgets the root; pin it.
        root.nlookup = 1;
        self.by_ino.insert(ROOT_INO, root);
        self.by_id.insert(ItemId::ROOT, ROOT_INO);
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.by_ino.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_ino.is_empty()
    }

    /// Inode for `id`, allocating one if needed (lookup count unchanged).
    pub fn ino_for(&mut self, id: ItemId) -> u64 {
        if let Some(&ino) = self.by_id.get(&id) {
            return ino;
        }
        let ino = self.alloc(Node::new(Some(id)));
        self.by_id.insert(id, ino);
        ino
    }

    fn alloc(&mut self, node: Node) -> u64 {
        let ino = self.next;
        self.next += 1;
        self.by_ino.insert(ino, node);
        ino
    }

    /// The kernel was given an entry for `id` named `name` in `parent`: map it, count the
    /// lookup, remember the dentry.
    pub fn remember(&mut self, id: ItemId, parent: ItemId, name: &str) -> u64 {
        let ino = self.ino_for(id);
        if let Some(node) = self.by_ino.get_mut(&ino) {
            node.nlookup += 1;
            if ino != ROOT_INO {
                node.dentry = Some((parent, name.to_string()));
            }
        }
        ino
    }

    /// A locally created file that has no ItemId yet (its create is deferred to close).
    pub fn alloc_provisional(&mut self, parent: ItemId, name: &str) -> u64 {
        let mut node = Node::new(None);
        node.nlookup = 1;
        node.dentry = Some((parent, name.to_string()));
        self.alloc(node)
    }

    /// Bind a provisional inode to the id its create produced.
    pub fn bind(&mut self, ino: u64, id: ItemId, parent: ItemId, name: &str) {
        if let Some(node) = self.by_ino.get_mut(&ino) {
            node.id = Some(id);
            node.dentry = Some((parent, name.to_string()));
            self.by_id.insert(id, ino);
        }
    }

    pub fn id_of(&self, ino: u64) -> Option<ItemId> {
        self.by_ino.get(&ino).and_then(|n| n.id)
    }

    pub fn ino_of(&self, id: ItemId) -> Option<u64> {
        self.by_id.get(&id).copied()
    }

    pub fn node(&self, ino: u64) -> Option<&Node> {
        self.by_ino.get(&ino)
    }

    pub fn node_mut(&mut self, ino: u64) -> Option<&mut Node> {
        self.by_ino.get_mut(&ino)
    }

    pub fn contains(&self, ino: u64) -> bool {
        self.by_ino.contains_key(&ino)
    }

    /// The kernel dropped `n` lookups of `ino`.
    pub fn forget(&mut self, ino: u64, n: u64) {
        if ino == ROOT_INO {
            return;
        }
        let drop_it = match self.by_ino.get_mut(&ino) {
            Some(node) => {
                node.nlookup = node.nlookup.saturating_sub(n);
                node.nlookup == 0 && node.open == 0
            }
            None => false,
        };
        if drop_it {
            self.remove(ino);
        }
    }

    pub fn open_inc(&mut self, ino: u64) {
        if let Some(node) = self.by_ino.get_mut(&ino) {
            node.open += 1;
        }
    }

    pub fn open_dec(&mut self, ino: u64) {
        let drop_it = match self.by_ino.get_mut(&ino) {
            Some(node) => {
                node.open = node.open.saturating_sub(1);
                ino != ROOT_INO && node.open == 0 && node.nlookup == 0
            }
            None => false,
        };
        if drop_it {
            self.remove(ino);
        }
    }

    fn remove(&mut self, ino: u64) {
        if let Some(node) = self.by_ino.remove(&ino) {
            if let Some(id) = node.id {
                // Several inodes may have pointed at one id (a create that returned an existing
                // item); only drop the reverse mapping if it is ours.
                if self.by_id.get(&id) == Some(&ino) {
                    self.by_id.remove(&id);
                }
            }
        }
    }

    /// Every `(parent, name)` the kernel may hold a dentry for.
    pub fn dentries(&self) -> Vec<(ItemId, String)> {
        self.by_ino
            .values()
            .filter_map(|n| n.dentry.clone())
            .collect()
    }

    /// Forget every mapping except the root (daemon index changed → ids were reassigned).
    pub fn reset(&mut self) {
        self.by_ino.clear();
        self.by_id.clear();
        self.generation += 1;
        self.insert_root();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_one_and_pinned() {
        let mut m = InodeMap::new();
        assert_eq!(m.ino_for(ItemId::ROOT), ROOT_INO);
        assert_eq!(m.id_of(ROOT_INO), Some(ItemId::ROOT));
        m.forget(ROOT_INO, 1_000);
        assert_eq!(m.id_of(ROOT_INO), Some(ItemId::ROOT));
    }

    #[test]
    fn allocation_is_stable_and_bidirectional() {
        let mut m = InodeMap::new();
        let a = m.remember(ItemId(42), ItemId::ROOT, "a");
        let b = m.remember(ItemId(43), ItemId::ROOT, "b");
        assert_ne!(a, b);
        assert_ne!(a, ROOT_INO);
        assert_eq!(m.remember(ItemId(42), ItemId::ROOT, "a"), a);
        assert_eq!(m.id_of(a), Some(ItemId(42)));
        assert_eq!(m.ino_of(ItemId(43)), Some(b));
        assert_eq!(m.node(a).map(|n| n.nlookup), Some(2));
        assert_eq!(
            m.node(a).and_then(|n| n.dentry.clone()),
            Some((ItemId::ROOT, "a".to_string()))
        );
    }

    #[test]
    fn forget_drops_mapping_at_zero_and_inos_are_never_reused() {
        let mut m = InodeMap::new();
        let a = m.remember(ItemId(7), ItemId::ROOT, "x");
        m.remember(ItemId(7), ItemId::ROOT, "x");
        m.forget(a, 1);
        assert_eq!(m.id_of(a), Some(ItemId(7)));
        m.forget(a, 1);
        assert_eq!(m.id_of(a), None);
        assert_eq!(m.ino_of(ItemId(7)), None);
        let again = m.remember(ItemId(7), ItemId::ROOT, "x");
        assert_ne!(again, a, "inode numbers must not be recycled");
    }

    #[test]
    fn open_handles_keep_node_alive_past_forget() {
        let mut m = InodeMap::new();
        let a = m.remember(ItemId(9), ItemId::ROOT, "f");
        m.open_inc(a);
        m.forget(a, 1);
        assert_eq!(m.id_of(a), Some(ItemId(9)));
        m.open_dec(a);
        assert_eq!(m.id_of(a), None);
    }

    #[test]
    fn provisional_then_bind() {
        let mut m = InodeMap::new();
        let p = m.alloc_provisional(ItemId::ROOT, "new.txt");
        assert_eq!(m.id_of(p), None);
        assert!(m.contains(p));
        m.bind(p, ItemId(100), ItemId::ROOT, "new.txt");
        assert_eq!(m.id_of(p), Some(ItemId(100)));
        assert_eq!(m.ino_of(ItemId(100)), Some(p));
    }

    #[test]
    fn two_inodes_one_id_only_owner_clears_reverse_map() {
        let mut m = InodeMap::new();
        let a = m.remember(ItemId(5), ItemId::ROOT, "a");
        let p = m.alloc_provisional(ItemId::ROOT, "a");
        m.bind(p, ItemId(5), ItemId::ROOT, "a");
        assert_eq!(m.ino_of(ItemId(5)), Some(p));
        m.forget(a, 1);
        assert_eq!(
            m.ino_of(ItemId(5)),
            Some(p),
            "old inode must not drop the new owner's mapping"
        );
    }

    #[test]
    fn reset_clears_all_but_root_and_bumps_generation() {
        let mut m = InodeMap::new();
        let a = m.remember(ItemId(11), ItemId::ROOT, "a");
        let g = m.generation();
        m.reset();
        assert_eq!(m.id_of(a), None);
        assert_eq!(m.generation(), g + 1);
        assert_eq!(m.id_of(ROOT_INO), Some(ItemId::ROOT));
        let b = m.ino_for(ItemId(11));
        assert!(
            b > a,
            "inodes allocated after a reset must not collide with stale ones"
        );
        assert_eq!(m.len(), 2);
    }
}
