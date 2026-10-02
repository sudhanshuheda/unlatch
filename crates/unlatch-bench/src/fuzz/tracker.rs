//! "No VM-side write is ever lost": every agent write's final bytes must still exist somewhere on
//! the VM (in place, moved, or in a conflict copy) — unless the agent itself overwrote/removed
//! them, or the Mac deliberately destroyed them *after* it had been shown them (a quiesce
//! happened between the agent write and the Mac's destructive op).
//!
//! Also records which names both sides touched within one sync epoch: only there may the engine
//! legitimately produce conflict copies or `name 2.ext` duplicates.

use crate::fpsim::names::{fold, is_conflict_copy, numbered_base, strip_unlatch_marker};
use crate::fpsim::vmfs::VmTree;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};
use unlatch_proto::Version;

/// D3 (racy-git rule) at a daemon restart: files the agent changed shortly before the daemon
/// died come back with a new content version — a same-tick rewrite after its last observation
/// would be invisible — so the Mac's next save of such a file is a conflict copy, by design.
/// The daemon journals a quiet observation 25 ms after its last commit; this window also
/// covers a starved daemon thread on a loaded box.
pub const RACY_KILL_WINDOW: Duration = Duration::from_secs(1);

pub fn hash(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

#[derive(Clone, Debug)]
struct Blob {
    hash: u64,
    len: usize,
    path: String,
    epoch: u64,
    released: bool,
    loss_allowed: bool,
    op: usize,
    /// The version the Mac held for this path when the agent wrote (None = unknown item).
    mac_version_at_write: Option<Version>,
    /// Every path these bytes lived at (the Mac may act on an item under a path the agent has
    /// since moved it away from).
    history: Vec<String>,
    /// fpsim's applied change-set count when the agent wrote.
    sets_at_write: u64,
}

/// What the Mac knows about one item it is about to destroy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacKnows {
    pub path: String,
    pub version: Option<Version>,
    /// Hash of the materialized bytes, if the item is materialized.
    pub content: Option<u64>,
}

/// The VM path a Mac or VM path denotes: display markers (`stem (Unlatch N).ext`, rule 11) map
/// back to the real name; everything else is compared exactly — the VM is case- and
/// normalization-sensitive, so `café` (NFC) and `café` (NFD) are different files there.
pub fn real_path(p: &str) -> String {
    p.split('/')
        .filter(|c| !c.is_empty())
        .map(|c| strip_unlatch_marker(c).unwrap_or_else(|| c.to_string()))
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Clone, Debug, Default)]
pub struct Tracker {
    blobs: Vec<Blob>,
    epoch: u64,
    /// Per epoch: folded names the agent / the Mac touched.
    agent_names: HashMap<u64, HashSet<String>>,
    mac_names: HashMap<u64, HashSet<String>>,
    /// Mac write nonce → (epoch, folded target name, name already taken on the VM, is a create).
    mac_writes: HashMap<u64, (u64, String, bool, bool)>,
    /// Agent write nonce → (epoch, folded name).
    agent_writes: HashMap<u64, (u64, String)>,
    /// Names the fuzzer (either side) asked for; anything else on the VM was made by Unlatch.
    known_names: HashSet<String>,
    /// The link to the VM is currently cut (`GoOffline`).
    pub offline: bool,
    /// Wall clock of recent agent touches, by folded name (D3 window at a daemon kill).
    recent_touches: Vec<(Instant, String)>,
}

fn base_fold(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    fold(&strip_unlatch_marker(name).unwrap_or_else(|| name.to_string()))
}

/// `tag#nonce` from the first line of fuzz-written content.
pub fn content_tag(c: &[u8]) -> Option<(&str, u64)> {
    let line = c.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let (tag, n) = line.split_once('#')?;
    Some((tag, n.parse().ok()?))
}

impl Tracker {
    pub fn new() -> Tracker {
        Tracker::default()
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// A sync point passed: everything written so far has been shown to the Mac. Every blob is
    /// re-located by content (bytes are unique per write): renames that landed since — the
    /// Mac's queued ones included — are then reflected exactly.
    pub fn quiesced(&mut self, vm: &VmTree) {
        let mut at: HashMap<u64, &String> = HashMap::new();
        for (p, c) in vm.contents() {
            at.entry(hash(c)).or_insert(p);
        }
        for b in &mut self.blobs {
            if let Some(p) = at.get(&b.hash) {
                if **p != b.path {
                    b.path = (*p).clone();
                    b.history.push(b.path.clone());
                }
            }
        }
        self.epoch += 1;
    }

    pub fn note_name(&mut self, name: &str) {
        self.known_names.insert(name.to_string());
    }

    pub fn vm_touch(&mut self, path: &str) {
        self.agent_names
            .entry(self.epoch)
            .or_default()
            .insert(base_fold(path));
        let now = Instant::now();
        self.recent_touches
            .retain(|(t, _)| now.duration_since(*t) <= RACY_KILL_WINDOW);
        self.recent_touches.push((now, base_fold(path)));
        if let Some(n) = path.rsplit('/').next() {
            self.note_name(n);
        }
    }

    /// The daemon was killed: names the agent touched within [`RACY_KILL_WINDOW`] may come back
    /// with a new content version (D3), as if the agent had touched them in this epoch.
    pub fn daemon_killed(&mut self) {
        let now = Instant::now();
        let names = self.agent_names.entry(self.epoch).or_default();
        for (t, n) in &self.recent_touches {
            if now.duration_since(*t) <= RACY_KILL_WINDOW {
                names.insert(n.clone());
            }
        }
    }

    pub fn mac_touch(&mut self, path: &str) {
        self.mac_names
            .entry(self.epoch)
            .or_default()
            .insert(base_fold(path));
        if let Some(n) = path.rsplit('/').next() {
            self.note_name(n);
        }
    }

    /// The Mac wrote bytes tagged with `nonce` to `path` (`create`: a new item); `taken` = the
    /// VM already had that name.
    pub fn mac_wrote(&mut self, path: &str, nonce: u64, taken: bool, create: bool) {
        self.mac_writes
            .insert(nonce, (self.epoch, base_fold(path), taken, create));
        self.mac_touch(path);
    }

    /// The agent wrote bytes tagged with `nonce` to `path`.
    pub fn agent_nonce(&mut self, path: &str, nonce: u64) {
        self.agent_writes
            .insert(nonce, (self.epoch, base_fold(path)));
    }

    /// The agent's bytes now at `path` (after an agent write). `op` = index for reports;
    /// `mac_version` = the version the Mac held for `path` at that moment.
    pub fn agent_wrote(
        &mut self,
        path: &str,
        bytes: &[u8],
        op: usize,
        mac_version: Option<Version>,
        sets_at_write: u64,
    ) {
        self.blobs.push(Blob {
            hash: hash(bytes),
            len: bytes.len(),
            path: path.to_string(),
            epoch: self.epoch,
            released: false,
            loss_allowed: false,
            op,
            mac_version_at_write: mac_version,
            history: vec![path.to_string()],
            sets_at_write,
        });
        if let Some((_, n)) = content_tag(bytes) {
            self.agent_nonce(path, n);
        }
        self.vm_touch(path);
    }

    /// The agent itself is about to destroy these bytes (overwrite / rm / rename-over).
    pub fn agent_destroys(&mut self, bytes: &[u8]) {
        let h = hash(bytes);
        if let Some(b) = self.blobs.iter_mut().find(|b| !b.released && b.hash == h) {
            b.released = true;
        }
    }

    /// The Mac deletes or overwrites the item at `mac_path` (and everything below it); `knows`
    /// is what the Mac holds for each item there. Agent bytes the Mac had been shown may
    /// legitimately go: written before the last quiesce, or materialized on the Mac, or the Mac
    /// received a newer version of that item after the write.
    ///
    /// Files strictly *inside* a folder deleted recursively follow the engine's own rule
    /// (`seen_seq`): fair game once the Mac consumed a change set after the write (`sets_now`).
    pub fn mac_destroys(&mut self, mac_path: &str, knows: &[MacKnows], sets_now: u64) {
        let target = real_path(mac_path);
        let prefix = format!("{target}/");
        let epoch = self.epoch;
        let known: Vec<(String, &MacKnows)> =
            knows.iter().map(|k| (real_path(&k.path), k)).collect();
        for b in &mut self.blobs {
            // The Mac holds exactly these bytes in what it destroys (wherever the agent moved
            // the file since): it saw them.
            if known.iter().any(|(_, k)| k.content == Some(b.hash)) {
                b.loss_allowed = true;
                continue;
            }
            let paths: Vec<String> = b.history.iter().map(|h| real_path(h)).collect();
            if !paths.iter().any(|p| *p == target || p.starts_with(&prefix)) {
                continue;
            }
            let newer = known.iter().any(|(kp, k)| {
                paths.contains(kp) && k.version.is_some() && k.version != b.mac_version_at_write
            });
            let inside = paths.iter().any(|p| p.starts_with(&prefix));
            if b.epoch < epoch || newer || (inside && b.sets_at_write < sets_now) {
                b.loss_allowed = true;
            }
        }
        self.mac_touch(mac_path);
    }

    /// Bytes the agent moved: keep the recorded path current for later Mac-side permissions.
    pub fn agent_moved(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        for b in &mut self.blobs {
            if b.path == from {
                b.path = to.to_string();
                b.history.push(b.path.clone());
            } else if let Some(rest) = b.path.strip_prefix(&prefix) {
                b.path = format!("{to}/{rest}");
                b.history.push(b.path.clone());
            }
        }
        self.vm_touch(from);
        self.vm_touch(to);
    }

    /// The Mac moved/renamed `from` to `to` (both Mac paths).
    pub fn mac_moved(&mut self, from: &str, to: &str) {
        let (f, t) = (real_path(from), real_path(to));
        let prefix = format!("{f}/");
        for b in &mut self.blobs {
            let p = real_path(&b.path);
            if p == f {
                b.path = t.clone();
                b.history.push(b.path.clone());
            } else if let Some(rest) = p.strip_prefix(&prefix) {
                b.path = format!("{t}/{rest}");
                b.history.push(b.path.clone());
            }
        }
        self.mac_touch(from);
        self.mac_touch(to);
    }

    /// Violations of "no agent write lost" and of "conflict copies / duplicates only where both
    /// sides raced on a name".
    pub fn check(&self, vm: &VmTree) -> Vec<String> {
        let present: HashSet<u64> = vm.contents().map(|(_, c)| hash(c)).collect();
        let mut problems = Vec::new();
        for b in &self.blobs {
            if !b.released && !b.loss_allowed && !present.contains(&b.hash) {
                problems.push(format!(
                    "agent write lost: {} bytes written to {} by op #{} (epoch {})",
                    b.len, b.path, b.op, b.epoch
                ));
            }
        }
        // Conflict copies and `name N` duplicates are fine only where both sides raced on a
        // name within one sync epoch. Their bytes carry the tag of the write that made them.
        for (path, node) in &vm.nodes {
            let name = path.rsplit('/').next().unwrap_or(path);
            let conflict = is_conflict_copy(name);
            let numbered = !self.known_names.contains(name) && numbered_base(name).is_some();
            if !conflict && !numbered {
                continue;
            }
            let Some((tag, n)) = node.content.as_deref().and_then(content_tag) else {
                continue;
            };
            let here = fold(name);
            // `name N` from a Mac create *or rename* onto a name the agent touched in the same
            // epoch (the bytes' own write may have been under another name).
            let base_raced = numbered
                && numbered_base(name).map(|b| fold(&b)).is_some_and(|b| {
                    self.mac_names.iter().any(|(e, m)| {
                        m.contains(&b) && self.agent_names.get(e).is_some_and(|a| a.contains(&b))
                    })
                });
            // A side that wrote to this very name authored the file itself (the fuzzer edits
            // conflict copies too); only copies the engine made need a race.
            let raced = match tag {
                // An edit's bytes in a numbered file do not make the number: only creates can.
                "mac" | "drag" => self.mac_writes.get(&n).map(|(e, nm, taken, create)| {
                    (numbered && !conflict && !create)
                        || base_raced
                        || *nm == here
                        || *taken
                        || self.agent_names.get(e).is_some_and(|s| s.contains(nm))
                }),
                // Agent bytes in a numbered file come from the Mac re-offering an item it held (MQ-080),
                // never from a replayed create; only agent-byte conflict copies need a race.
                "agent" | "churn" | "mass" | "touch" | "append" if conflict => {
                    self.agent_writes.get(&n).map(|(e, nm)| {
                        *nm == here || self.mac_names.get(e).is_some_and(|s| s.contains(nm))
                    })
                }
                _ => None,
            };
            if raced == Some(false) {
                let what = if conflict {
                    "conflict copy"
                } else {
                    "numbered duplicate"
                };
                problems.push(format!(
                    "{what} without a racing edit of the same name: {path} (bytes {tag}#{n})"
                ));
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fpsim::vmfs::{VmKind, VmNode};

    fn tree(files: &[(&str, &[u8])]) -> VmTree {
        let mut t = VmTree::default();
        for (p, c) in files {
            t.nodes.insert(
                p.to_string(),
                VmNode {
                    kind: VmKind::File,
                    size: c.len() as u64,
                    content: Some(c.to_vec()),
                    target: None,
                    mode: 0o644,
                },
            );
        }
        t
    }

    #[test]
    fn lost_write_is_reported_unless_released_or_seen() {
        let mut t = Tracker::new();
        t.agent_wrote("d/a", b"one", 1, None, 0);
        assert!(
            t.check(&tree(&[("d/moved", b"one")])).is_empty(),
            "moved bytes still exist"
        );
        assert_eq!(t.check(&tree(&[])).len(), 1);
        // The agent overwrote its own bytes: nothing to keep.
        t.agent_destroys(b"one");
        t.agent_wrote("d/a", b"two", 2, None, 0);
        assert!(t.check(&tree(&[("d/a", b"two")])).is_empty());
        // The Mac deletes d before seeing "two": still must survive.
        let stale = MacKnows {
            path: "d/a".into(),
            version: None,
            content: Some(hash(b"one")),
        };
        t.mac_destroys("d", &[stale], 0);
        assert_eq!(t.check(&tree(&[])).len(), 1);
        // The Mac materialized exactly these bytes before overwriting them: its call.
        let seen = MacKnows {
            path: "d/a".into(),
            version: None,
            content: Some(hash(b"two")),
        };
        let mut t2 = t.clone();
        t2.mac_destroys("d/a", &[seen], 0);
        assert!(t2.check(&tree(&[])).is_empty());
        // It received a newer version of the item after the write: also its call.
        let newer = MacKnows {
            path: "d/a".into(),
            version: Some(Version {
                content: 9,
                meta: 9,
            }),
            content: None,
        };
        let mut t3 = t.clone();
        t3.mac_destroys("d", &[newer], 0);
        assert!(t3.check(&tree(&[])).is_empty());
        // After a quiesce the Mac has seen it; deleting is the user's call.
        t.quiesced(&tree(&[]));
        t.mac_destroys("d", &[], 0);
        assert!(t.check(&tree(&[])).is_empty());
    }

    /// Seed 210: `x 2` holds the bytes of a Mac create of `.env` that the Mac renamed onto `x`
    /// while the agent wrote `x`: a race on the duplicate's base name, whatever the bytes'
    /// own name was.
    #[test]
    fn numbered_duplicate_races_on_its_base_name() {
        let mut t = Tracker::new();
        t.note_name(".env");
        t.note_name("x");
        t.mac_wrote(".env", 70, false, true);
        let vm = tree(&[("x", b"agent#71\n"), ("x 2", b"mac#70\nbody")]);
        assert_eq!(t.check(&vm).len(), 1, "the Mac never targeted x");
        t.mac_moved(".env", "x");
        assert_eq!(
            t.check(&vm).len(),
            1,
            "the agent did not touch x in that epoch"
        );
        t.vm_touch("x");
        assert!(t.check(&vm).is_empty(), "{:?}", t.check(&vm));
    }

    /// D3: the daemon dies right after the agent touched `f`; the restart bumps `f`, and the
    /// Mac's save of it in that epoch becomes a conflict copy — by design. Names the agent
    /// touched before the window are not excused.
    #[test]
    fn daemon_kill_excuses_only_recent_agent_touches() {
        let mut t = Tracker::new();
        t.note_name("f");
        t.vm_touch("f");
        t.quiesced(&tree(&[]));
        t.mac_wrote("f", 7, false, false);
        let vm = tree(&[
            ("f", b"agent#1\n"),
            ("f (conflict from fpsim 2026-09-30 12.00)", b"mac#7\n"),
        ]);
        assert_eq!(t.check(&vm).len(), 1);
        let mut old = t.clone();
        old.recent_touches.clear(); // touched longer than RACY_KILL_WINDOW ago
        old.daemon_killed();
        assert_eq!(old.check(&vm).len(), 1);
        t.daemon_killed();
        assert!(t.check(&vm).is_empty(), "{:?}", t.check(&vm));
    }

    #[test]
    fn conflict_copies_need_a_race() {
        let mut t = Tracker::new();
        t.note_name("a.txt");
        t.mac_wrote("a.txt", 7, false, false);
        let mut c = b"mac#7\n".to_vec();
        c.extend_from_slice(b"body");
        let vm = tree(&[
            ("a.txt", b"agent#1\n"),
            ("a (conflict from mac 2026-09-30 12.00).txt", &c),
        ]);
        assert_eq!(t.check(&vm).len(), 1, "{:?}", t.check(&vm));
        t.vm_touch("A.txt");
        assert!(t.check(&vm).is_empty(), "{:?}", t.check(&vm));
        assert_eq!(content_tag(b"mac#7\nxyz"), Some(("mac", 7)));
    }
}
