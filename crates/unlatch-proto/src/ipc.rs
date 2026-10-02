//! File Provider extension / host UI ⇄ engine protocol.
//!
//! Transport: on macOS, XPC to the engine agent (MachService, code-signing requirement enforced),
//! each XPC message carrying one encoded [`IpcFrame`] plus an optional `FileHandle`. On Linux and
//! in tests: a unix socket with the same frames and file descriptors passed via `SCM_RIGHTS`
//! (at most one fd per frame, attached to the frame that declares `has_content: true`).
//!
//! **Multiplexed:** every request frame has a caller-chosen `call` id; the engine replies with the
//! same `call` id, possibly out of order. Long calls (`Fetch`, `Create`/`Modify` with content)
//! may send any number of `IpcResponse::Progress` frames before the final reply.
//! `IpcRequest::Cancel { call }` aborts a call (final reply: `Error { code: Cancelled }`).
//!
//! Every request maps 1:1 onto an `NSFileProviderReplicatedExtension` / enumerator / host call, so
//! the Swift shim contains no logic.

use crate::{BaseVersion, Entry, ErrorCode, ItemId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcFrame<T> {
    pub call: u64,
    pub msg: T,
}

/// Mac-only metadata. Stored by the engine (keyed by ItemId), never sent to the VM.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalMeta {
    pub tag_data: Option<Vec<u8>>,
    pub last_used_ns: Option<i64>,
    pub favorite_rank: Option<u64>,
    pub creation_ns: Option<i64>,
    pub xattrs: Vec<(String, Vec<u8>)>,
    pub hidden: bool,
    pub type_creator: Option<(u32, u32)>,
}

/// `NSFileProviderItemCapabilities` bits the engine computes (subset used by Unlatch).
pub mod caps {
    pub const READING: u32 = 1 << 0;
    pub const WRITING: u32 = 1 << 1;
    pub const REPARENTING: u32 = 1 << 2;
    pub const RENAMING: u32 = 1 << 3;
    pub const DELETING: u32 = 1 << 5;
    pub const ADDING_SUB_ITEMS: u32 = 1 << 1; // same bit as WRITING for folders (Apple's alias)
    pub const CONTENT_ENUMERATING: u32 = 1 << 0; // same bit as READING for folders
    /// Unlatch never sets allowsTrashing (1 << 4).
    pub const EXCLUDING_FROM_SYNC: u32 = 1 << 7;
}

/// `changed_fields` bits (subset of `NSFileProviderItemFields`).
pub mod fields {
    pub const CONTENTS: u32 = 1 << 0;
    pub const FILENAME: u32 = 1 << 1;
    pub const PARENT: u32 = 1 << 2;
    pub const LAST_USED_DATE: u32 = 1 << 3;
    pub const TAG_DATA: u32 = 1 << 4;
    pub const FAVORITE_RANK: u32 = 1 << 5;
    pub const CREATION_DATE: u32 = 1 << 6;
    pub const CONTENT_MODIFICATION_DATE: u32 = 1 << 7;
    pub const FILE_SYSTEM_FLAGS: u32 = 1 << 8;
    pub const EXTENDED_ATTRIBUTES: u32 = 1 << 9;
    pub const TYPE_AND_CREATOR: u32 = 1 << 10;
}

/// What the system sees for one item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcItem {
    pub entry: Entry,
    /// Name shown to the system (differs from `entry.name` for case/normalization collisions:
    /// `stem (Unlatch N).ext`).
    pub display_name: String,
    /// Display name of `entry.parent` is irrelevant; parent is by id. Capabilities bits (`caps`).
    pub caps: u32,
    pub local: LocalMeta,
    /// Exec bit shown to the Mac (false by default; see DESIGN exec rule).
    pub user_exec: bool,
    /// A VM symlink that fails the in-root rule: exposed as a read-only file whose content is
    /// the target text.
    pub symlink_blocked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateKind {
    File,
    Dir,
    Symlink,
    /// `.package` template (bundles like `.app`) — v1: `ExcludedFromSync`.
    Package,
    /// `.aliasFile` — v1: `ExcludedFromSync`.
    Alias,
}

#[allow(clippy::large_enum_variant)] // decoded once per call; boxing would churn every caller
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcRequest {
    /// First message on every connection.
    Hello {
        proto: u32,
        domain: String,
    },
    /// `item(for:)`
    Item {
        id: ItemId,
    },
    /// `enumerateItems(for:startingAt:)` for a container (`ItemId::ROOT` = rootContainer).
    /// `viewer` = `request.isFileViewerRequest` (a person is looking → prefetch small files).
    Enumerate {
        container: ItemId,
        cursor: Option<Vec<u8>>,
        limit: u32,
        viewer: bool,
    },
    /// `currentSyncAnchor` of the working-set enumerator.
    CurrentAnchor,
    /// `enumerateChanges(for: .workingSet, from:)`.
    ChangesSince {
        anchor: Vec<u8>,
        limit: u32,
    },
    /// From `materializedItemsDidChange` → `enumeratorForMaterializedItems()`.
    /// `full = true`: `added` is the complete materialized set (replace).
    MaterializedChanged {
        added: Vec<ItemId>,
        removed: Vec<ItemId>,
        full: bool,
    },
    /// `fetchContents(for:version:)`. The engine clones the content into `dest_dir`
    /// (the extension's `temporaryDirectoryURL`) and returns that path.
    Fetch {
        id: ItemId,
        version: Option<u64>,
        dest_dir: String,
    },
    /// `createItem(basedOn:fields:contents:options:)`. Content (files) arrives as the frame's fd.
    Create {
        template_id: String,
        parent: ItemId,
        name: String,
        kind: CreateKind,
        has_content: bool,
        symlink_target: Option<String>,
        mtime_ns: Option<i64>,
        user_exec: Option<bool>,
        changed_fields: u32,
        local: LocalMeta,
        may_already_exist: bool,
        deletion_conflicted: bool,
    },
    /// `modifyItem(_:baseVersion:changedFields:contents:options:)`.
    Modify {
        id: ItemId,
        base: BaseVersion,
        changed_fields: u32,
        new_parent: Option<ItemId>,
        new_name: Option<String>,
        has_content: bool,
        mtime_ns: Option<i64>,
        user_exec: Option<bool>,
        local: LocalMeta,
    },
    /// `deleteItem(identifier:baseVersion:options:)`.
    Delete {
        id: ItemId,
        base: BaseVersion,
        recursive: bool,
    },
    Cancel {
        call: u64,
    },
    /// Engine status for UI / diagnostics.
    Status,
    /// Host UI: user confirmed a paused mass deletion (`ConnState::Paused`).
    ConfirmPaused {
        apply: bool,
    },
}

#[allow(clippy::large_enum_variant)] // decoded once per call; boxing would churn every caller
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcResponse {
    Hello {
        proto: u32,
        domain: String,
    },
    Item(IpcItem),
    Page {
        items: Vec<IpcItem>,
        next: Option<Vec<u8>>,
    },
    Anchor(Vec<u8>),
    /// `more = true` → call again from `anchor` immediately. Never empty while the engine's
    /// seq is beyond `anchor` (an empty set at a held anchor drops pending changes, MQ-004).
    Changes {
        updated: Vec<IpcItem>,
        removed: Vec<ItemId>,
        anchor: Vec<u8>,
        more: bool,
    },
    /// Streamed before the final reply of long calls.
    Progress {
        done: u64,
        total: u64,
    },
    Fetched {
        path: String,
        item: IpcItem,
    },
    /// Create/Modify result. `still_pending` = `changed_fields` bits not handled;
    /// `should_fetch_content` → the system must re-download (conflict: server kept its content).
    Done {
        item: IpcItem,
        still_pending: u32,
        should_fetch_content: bool,
        conflict_copy: Option<IpcItem>,
    },
    Deleted,
    Status(EngineStatus),
    Ok,
    /// `current` is set for `DeletionRejected` (the item as it exists now).
    Error {
        code: ErrorCode,
        msg: String,
        current: Option<IpcItem>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnState {
    Connecting,
    /// Initial snapshot in progress.
    Syncing {
        received: u64,
    },
    Live,
    Offline {
        error: String,
        retry_in_ms: u64,
    },
    /// A human must act (host key, password/2FA, login URL). No automatic retry.
    NeedsUser {
        reason: String,
        url: Option<String>,
    },
    /// Mass-deletion guard tripped; waiting for `ConfirmPaused`.
    Paused {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStatus {
    pub state: ConnState,
    pub entries: u64,
    pub anchor: Vec<u8>,
    /// Last measured round-trip time in microseconds.
    pub rtt_us: Option<u64>,
    pub cache_bytes: u64,
    pub pending_uploads: u32,
    pub server: Option<crate::wire::ServerInfo>,
}
