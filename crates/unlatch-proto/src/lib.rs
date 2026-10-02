//! Unlatch protocol types and framing.
//!
//! Two protocols share one framing format:
//! * [`wire`] — client engine ⇄ `unlatchd` over the ssh stdio pipe.
//! * [`ipc`]  — File Provider extension / menu-bar UI ⇄ engine (XPC on macOS carrying these
//!   exact bytes; a unix socket in tests and on Linux).
//!
//! A wire session starts with a raw [`frame::Preamble`] in each direction (never postcard), then
//! frames: `u32 LE length` (of the rest) + `u8 flags` + payload. `flags & FLAG_LZ4` → payload is
//! LZ4 block data prefixed by its `u32 LE` decompressed length (bounded by `MAX_FRAME`).
//! Messages are `postcard`-encoded serde enums.
//!
//! **Encoding freeze (v1):** postcard is positional. Never reorder/insert fields or variants.
//! Add new *variants at the end* of an enum, or new data only through the trailing `ext` fields.
//! Golden-bytes tests in `tests/golden.rs` pin the v1 encoding.
//!
//! Authoritative design: `docs/DESIGN.md` as amended by `docs/review/2026-09-30-design-review.md`.

pub mod frame;
pub mod ipc;
pub mod wire;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to [`wire`] or [`ipc`].
pub const PROTO_VERSION: u32 = 1;

/// Stable identifier of a file-system item, assigned by `unlatchd`, scoped to an [`IndexId`].
///
/// Stable across renames and across atomic replace (write-temp + rename-over). Never reused for a
/// different file within one index. `ItemId::ROOT` is always the configured root directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemId(pub u64);

impl ItemId {
    pub const ROOT: ItemId = ItemId(1);
    /// The system's trash container (`.trashContainer`). Never assigned by `unlatchd`; a reparent to
    /// it is treated as a delete (Unlatch does not sync a trash).
    pub const TRASH: ItemId = ItemId(u64::MAX);
}

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identity of one daemon index (random u128 created with `index.bin`). Every [`ItemId`] and
/// [`Version`] is only meaningful within the index that issued it. A new index id (VM re-imaged,
/// root replaced, `index.bin` lost, format change) means: never execute id-addressed ops from the
/// old index; the engine reimports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IndexId(pub u128);

/// Idempotency key of a mutation. The daemon persists `op → result` (≥ 7 days) and answers a
/// replayed op with the stored result instead of executing it again.
pub type OpId = [u8; 16];

/// Maps 1:1 onto `NSFileProviderItemVersion { contentVersion, metadataVersion }`.
///
/// Both components are **daemon-assigned index sequence numbers**, never stat hashes:
/// * `content` = index seq at the last observed content change of the item;
/// * `meta`    = index seq at the last change of (parent, name, mode & 0o7777, symlink target).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Version {
    pub content: u64,
    pub meta: u64,
}

/// A base version supplied by the system. `None` component = unknown
/// (`NSFileProviderItemVersion.beforeFirstSyncComponent`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaseVersion {
    pub content: Option<u64>,
    pub meta: Option<u64>,
}

impl From<Version> for BaseVersion {
    fn from(v: Version) -> Self {
        BaseVersion {
            content: Some(v.content),
            meta: Some(v.meta),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

pub const ACCESS_R: u8 = 1;
pub const ACCESS_W: u8 = 2;
pub const ACCESS_X: u8 = 4;

/// Metadata of one item. The unit of the replica.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: ItemId,
    /// Parent id. The root's parent is itself (`ItemId::ROOT`).
    pub parent: ItemId,
    /// Single path component, valid UTF-8, never empty / "." / ".." / containing '/' or NUL.
    pub name: String,
    pub kind: Kind,
    /// Bytes for files, target length for symlinks, 0 for dirs.
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Unix permission bits (low 12 bits of st_mode).
    pub mode: u32,
    pub version: Version,
    pub symlink_target: Option<String>,
    /// Dir only: `true` when this directory's children have not been scanned/watched yet
    /// (lazy dir, mount point, or watch budget exhausted). `ListDir` scans exactly one level.
    pub lazy: bool,
    /// Index seq of the last change to this entry. Clients apply last-writer-wins by `seq`.
    pub seq: u64,
    /// `ACCESS_R | ACCESS_W | ACCESS_X` for the daemon's user (via `faccessat(AT_EACCESS)`).
    pub access: u8,
}

/// Error codes shared by both protocols. The macOS shim maps these onto `NSFileProviderError`.
/// Append-only (encoding freeze).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorCode {
    NotFound,
    /// Name already taken in the destination directory.
    Exists,
    NotDir,
    IsDir,
    NotEmpty,
    Permission,
    /// `base` version did not match the current item (someone else changed it).
    VersionMismatch,
    InvalidName,
    NoSpace,
    /// Engine not connected and the answer needs the VM.
    Offline,
    /// Anchor older than the tombstone GC horizon (or from another replica); re-enumerate.
    AnchorExpired,
    Unsupported,
    Timeout,
    Io,
    Protocol,
    /// The daemon's index id differs from the one the op was addressed to.
    IndexChanged,
    /// Delete refused: the item (or something below it) changed or appeared since the client
    /// last saw it. The engine returns the current item with it.
    DeletionRejected,
    /// Permanent failure for this item (maps to `.cannotSynchronize`).
    CannotSync,
    /// The item must stay local (`.DS_Store`, `._*`, packages in v1…).
    ExcludedFromSync,
    /// A human must act (host key, password/2FA, login URL). Never retried automatically.
    NeedsUser,
    /// The configured root was deleted, moved or replaced.
    RootReplaced,
    /// Cancelled by the caller.
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {msg}")]
pub struct ProtoError {
    pub code: ErrorCode,
    pub msg: String,
}

impl ProtoError {
    pub fn new(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
        }
    }
}

/// Validate a single path component as accepted on the wire.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255
        && !name.bytes().any(|b| b == b'/' || b == 0)
}

/// Default lazy directory names (children not scanned or watched until first listed).
/// The daemon persists the effective list per root; clients only suggest it on first creation.
pub const DEFAULT_LAZY_NAMES: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    "dist",
    "build",
    ".cache",
    ".gradle",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
];

/// Names Finder/macOS create that must never be uploaded to the VM (unless the VM already has
/// an item with that name). `*.nosync` is matched by suffix.
pub fn is_mac_local_name(name: &str) -> bool {
    name == ".DS_Store"
        || name.starts_with("._")
        || name == "Icon\r"
        || name == ".localized"
        || name.ends_with(".nosync")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("a.txt"));
        assert!(valid_name("..hidden"));
        assert!(!valid_name(""));
        assert!(!valid_name("."));
        assert!(!valid_name(".."));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("a\0b"));
        assert!(is_mac_local_name(".DS_Store"));
        assert!(is_mac_local_name("._foo"));
        assert!(!is_mac_local_name("foo.txt"));
    }
}
