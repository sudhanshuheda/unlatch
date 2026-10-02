//! Push invalidation: engine changes → kernel cache invalidations.
//!
//! Runs on its own thread so a notification that has to wait for a kernel inode lock (held by a
//! request our workers are still serving) never blocks request handling; nothing here waits
//! for the request path, so there is no cycle.

use super::{lock, Inval, Shared};
use crate::backend::Backend;
use crate::inode::ROOT_INO;
use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use tracing::debug;
use unlatch_proto::ipc::IpcItem;
use unlatch_proto::ItemId;

/// The kernel side of invalidation (fuser's `Notifier`; a recorder in tests).
pub trait KernelInval: Send + 'static {
    fn inval_entry(&self, parent: u64, name: &OsStr) -> std::io::Result<()>;
    /// `offset < 0`: attributes only; `offset >= 0, len <= 0`: attributes + all cached data.
    fn inval_inode(&self, ino: u64, offset: i64, len: i64) -> std::io::Result<()>;
}

impl KernelInval for fuser::Notifier {
    fn inval_entry(&self, parent: u64, name: &OsStr) -> std::io::Result<()> {
        fuser::Notifier::inval_entry(self, parent, name)
    }
    fn inval_inode(&self, ino: u64, offset: i64, len: i64) -> std::io::Result<()> {
        fuser::Notifier::inval_inode(self, ino, offset, len)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    Entry {
        parent: u64,
        name: String,
    },
    /// `data = false` → attributes only.
    Inode {
        ino: u64,
        data: bool,
    },
}

struct Known {
    id: ItemId,
    ino: Option<u64>,
    dentry: Option<(ItemId, String)>,
    cached_content: Option<u64>,
}

impl<B: Backend> Shared<B> {
    /// Turn a batch of invalidation work into kernel actions, updating the inode map to match
    /// what the kernel will have forgotten.
    pub fn plan(&self, batch: Vec<Inval>) -> Vec<Action> {
        let mut ids: BTreeSet<ItemId> = BTreeSet::new();
        let mut parents: BTreeSet<ItemId> = BTreeSet::new();
        let mut actions: Vec<Action> = Vec::new();
        for inval in batch {
            match inval {
                Inval::Replica { ids: i, parents: p } => {
                    ids.extend(i);
                    parents.extend(p);
                }
                Inval::Inode { ino } => actions.push(Action::Inode { ino, data: true }),
                Inval::Entry { parent, name } => {
                    if let Some(pino) = lock(&self.inodes).ino_of(parent) {
                        actions.push(Action::Entry { parent: pino, name });
                    }
                }
                Inval::Reimport => actions.extend(self.plan_reimport()),
                Inval::Shutdown => {}
            }
        }

        // Snapshot what the kernel was told, then read the current state without the lock.
        let known: Vec<Known> = {
            let inodes = lock(&self.inodes);
            ids.iter()
                .map(|&id| {
                    let ino = inodes.ino_of(id);
                    let node = ino.and_then(|i| inodes.node(i));
                    Known {
                        id,
                        ino,
                        dentry: node.and_then(|n| n.dentry.clone()),
                        cached_content: node.and_then(|n| n.cached_content),
                    }
                })
                .collect()
        };
        let current: Vec<Option<IpcItem>> =
            known.iter().map(|k| self.backend.item(k.id).ok()).collect();

        let mut inodes = lock(&self.inodes);
        for (k, cur) in known.iter().zip(current) {
            let new_dentry = cur
                .as_ref()
                .map(|it| (it.entry.parent, it.display_name.clone()));
            match k.ino {
                Some(ino) => {
                    if k.dentry.is_some() && k.dentry != new_dentry {
                        // Moved, renamed or gone: the old name must stop resolving.
                        if let Some((p, n)) = &k.dentry {
                            if let Some(pino) = inodes.ino_of(*p) {
                                actions.push(Action::Entry {
                                    parent: pino,
                                    name: n.clone(),
                                });
                            }
                        }
                        if let Some(node) = inodes.node_mut(ino) {
                            node.dentry = None;
                        }
                    }
                    if k.dentry != new_dentry {
                        // The new name may be cached as a negative entry.
                        if let Some((p, n)) = &new_dentry {
                            if let Some(pino) = inodes.ino_of(*p) {
                                actions.push(Action::Entry {
                                    parent: pino,
                                    name: n.clone(),
                                });
                            }
                        }
                    }
                    // Pages can only be cached for a version we recorded at open.
                    let content_changed = match (&cur, k.cached_content) {
                        (_, None) => false,
                        (Some(it), Some(c)) => c != it.entry.version.content,
                        (None, Some(_)) => true,
                    };
                    if content_changed {
                        if let Some(node) = inodes.node_mut(ino) {
                            node.cached_content = None;
                        }
                    }
                    // Our own uploads record the new content version first, so they only
                    // refresh attributes and keep the pages just written (no re-download).
                    actions.push(Action::Inode {
                        ino,
                        data: content_changed,
                    });
                }
                None => {
                    // Never shown to the kernel: at most a negative dentry under its name.
                    if let Some((p, n)) = &new_dentry {
                        if let Some(pino) = inodes.ino_of(*p) {
                            actions.push(Action::Entry {
                                parent: pino,
                                name: n.clone(),
                            });
                        }
                    }
                }
            }
        }
        for p in parents {
            if let Some(pino) = inodes.ino_of(p) {
                // Drops the cached listing (FOPEN_CACHE_DIR) and the dir's attributes.
                actions.push(Action::Inode {
                    ino: pino,
                    data: true,
                });
            }
        }
        drop(inodes);
        if !self.root_ready.load(Ordering::SeqCst) {
            // Until the kernel has used the root's real attributes once, an inval_inode racing
            // its first GETATTR makes it discard the reply and check access against the
            // placeholder root mode (0) → spurious EACCES. Nothing is cached for the root yet,
            // so there is nothing to invalidate anyway.
            actions.retain(|a| !matches!(a, Action::Inode { ino: ROOT_INO, .. }));
        }
        dedup(actions)
    }

    /// The daemon index changed: every id may now name a different file. Drop every dentry the
    /// kernel holds, then every mapping (stale inodes answer ESTALE; inode numbers are never
    /// reused, so they cannot alias new files).
    fn plan_reimport(&self) -> Vec<Action> {
        let mut inodes = lock(&self.inodes);
        let mut actions = Vec::new();
        let mut seen = HashSet::new();
        // Collect names first: resetting forgets the parent inode numbers.
        let mut entries: Vec<(u64, String)> = Vec::new();
        for (p, n) in inodes.dentries() {
            if let Some(pino) = inodes.ino_of(p) {
                if seen.insert((pino, n.clone())) {
                    entries.push((pino, n));
                }
            }
        }
        inodes.reset();
        for (parent, name) in entries {
            actions.push(Action::Entry { parent, name });
        }
        actions.push(Action::Inode {
            ino: ROOT_INO,
            data: true,
        });
        actions
    }
}

fn dedup(actions: Vec<Action>) -> Vec<Action> {
    let mut seen = HashSet::new();
    actions
        .into_iter()
        .filter(|a| seen.insert(a.clone()))
        .collect()
}

/// Apply actions; errors (the kernel no longer knows the inode/name) are expected and ignored.
pub fn apply<K: KernelInval>(k: &K, actions: &[Action]) {
    for a in actions {
        let r = match a {
            Action::Entry { parent, name } => k.inval_entry(*parent, OsStr::new(name)),
            Action::Inode { ino, data } => k.inval_inode(*ino, if *data { 0 } else { -1 }, 0),
        };
        if let Err(e) = r {
            if e.raw_os_error() != Some(libc::ENOENT) {
                debug!(?a, error = %e, "kernel invalidation failed");
            }
        }
    }
}

/// The invalidation thread body: coalesce whatever is queued, plan, apply. Ends when every
/// sender is gone (engine shut down).
pub fn run<B: Backend, K: KernelInval>(sh: Arc<Shared<B>>, k: K, rx: Receiver<Inval>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        let stop = batch.contains(&Inval::Shutdown);
        let actions = sh.plan(batch);
        if stop {
            return;
        }
        apply(&k, &actions);
    }
}
