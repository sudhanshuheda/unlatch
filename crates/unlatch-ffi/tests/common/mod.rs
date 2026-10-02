//! Sample values for every IPC variant, shared by the round-trip and fixture tests.
//! [`variants`] asks serde itself for each enum's variant list, so a variant added to
//! `unlatch_proto::ipc` without a sample here fails the tests instead of silently going untested.
#![allow(dead_code)]

use serde::de::{self, DeserializeOwned, Deserializer, Visitor};
use unlatch_proto::ipc::{
    caps, fields, ConnState, CreateKind, EngineStatus, IpcFrame, IpcItem, IpcRequest, IpcResponse,
    LocalMeta,
};
use unlatch_proto::wire::ServerInfo;
use unlatch_proto::{
    BaseVersion, Entry, ErrorCode, ItemId, Kind, Version, ACCESS_R, ACCESS_W, ACCESS_X,
};

/// Variant names of a serde enum, captured from the derive's own `deserialize_enum` call.
pub fn variants<T: DeserializeOwned>() -> &'static [&'static str] {
    struct Probe<'a>(&'a mut Option<&'static [&'static str]>);
    impl<'de> Deserializer<'de> for Probe<'_> {
        type Error = de::value::Error;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("not an enum"))
        }
        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = Some(variants);
            Err(de::Error::custom("probe"))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
            option unit unit_struct newtype_struct seq tuple tuple_struct map struct identifier
            ignored_any
        }
    }
    let mut out = None;
    let _ = T::deserialize(Probe(&mut out));
    out.expect("type is not a serde enum")
}

pub fn entry(id: u64, parent: u64, name: &str, kind: Kind) -> Entry {
    Entry {
        id: ItemId(id),
        parent: ItemId(parent),
        name: name.to_string(),
        kind,
        size: if kind == Kind::File { 1234 } else { 0 },
        mtime_ns: 1_727_654_400_123_456_789,
        mode: if kind == Kind::Dir { 0o755 } else { 0o644 },
        version: Version {
            content: 41,
            meta: 42,
        },
        symlink_target: (kind == Kind::Symlink).then(|| "../lib/x.so".to_string()),
        lazy: kind == Kind::Dir && name == "node_modules",
        seq: 42,
        access: ACCESS_R | ACCESS_W | if kind == Kind::Dir { ACCESS_X } else { 0 },
    }
}

pub fn full_local() -> LocalMeta {
    LocalMeta {
        tag_data: Some(vec![0x62, 0x70, 0x6c, 0x69, 0x73, 0x74]),
        last_used_ns: Some(1_727_654_400_000_000_000),
        favorite_rank: Some(3),
        creation_ns: Some(-5_000_000_000),
        xattrs: vec![
            ("user.note".into(), b"hi".to_vec()),
            ("com.example.empty".into(), vec![]),
        ],
        hidden: true,
        type_creator: Some((u32::from_be_bytes(*b"TEXT"), u32::from_be_bytes(*b"ttxt"))),
    }
}

pub fn item(id: u64, name: &str, kind: Kind, full: bool) -> IpcItem {
    let e = entry(id, 1, name, kind);
    IpcItem {
        display_name: e.name.clone(),
        caps: caps::READING | caps::WRITING | caps::RENAMING | caps::REPARENTING | caps::DELETING,
        local: if full {
            full_local()
        } else {
            LocalMeta::default()
        },
        user_exec: full,
        symlink_blocked: false,
        entry: e,
    }
}

pub fn blocked_symlink() -> IpcItem {
    let mut e = entry(9, 1, "etc-passwd", Kind::Symlink);
    e.symlink_target = Some("/etc/passwd".into());
    IpcItem {
        display_name: "etc-passwd".into(),
        caps: caps::READING,
        local: LocalMeta::default(),
        user_exec: false,
        symlink_blocked: true,
        entry: e,
    }
}

pub fn root_item() -> IpcItem {
    let mut e = entry(1, 1, "code", Kind::Dir);
    e.version = Version {
        content: 1,
        meta: 1,
    };
    IpcItem {
        display_name: "code".into(),
        caps: caps::READING | caps::WRITING,
        local: LocalMeta::default(),
        user_exec: false,
        symlink_blocked: false,
        entry: e,
    }
}

pub fn server_info() -> ServerInfo {
    ServerInfo {
        version: "0.1.0".into(),
        hostname: "devbox".into(),
        root_path: "/home/me/code".into(),
        entries: 130_000,
        watches: 4_096,
        polled: false,
        warnings: vec!["watch budget reached: 12 dirs polled".into()],
    }
}

pub fn status(state: ConnState) -> EngineStatus {
    let live = state == ConnState::Live;
    EngineStatus {
        state,
        entries: 130_000,
        anchor: vec![0xde, 0xad, 0xbe, 0xef, 0, 0, 0, 0, 0, 0, 0, 7],
        rtt_us: live.then_some(40_125),
        cache_bytes: 5 << 20,
        pending_uploads: 2,
        server: live.then(server_info),
    }
}

pub fn conn_states() -> Vec<(&'static str, ConnState)> {
    vec![
        ("Connecting", ConnState::Connecting),
        ("Syncing", ConnState::Syncing { received: 4_096 }),
        ("Live", ConnState::Live),
        (
            "Offline",
            ConnState::Offline {
                error: "ssh: connect to host devbox port 22: timed out".into(),
                retry_in_ms: 2_000,
            },
        ),
        (
            "NeedsUser",
            ConnState::NeedsUser {
                reason: "Tailscale SSH needs a login".into(),
                url: Some("https://login.tailscale.com/a/abc".into()),
            },
        ),
        (
            "Paused",
            ConnState::Paused {
                reason: "a remote change would delete 4 210 materialized items".into(),
            },
        ),
    ]
}

/// (variant name, file-name suffix, frame). Several samples per variant cover null/non-null.
pub fn requests() -> Vec<(&'static str, &'static str, IpcFrame<IpcRequest>)> {
    let f = |call: u64, msg: IpcRequest| IpcFrame { call, msg };
    vec![
        (
            "Hello",
            "",
            f(
                1,
                IpcRequest::Hello {
                    proto: unlatch_proto::PROTO_VERSION,
                    domain: "devbox".into(),
                },
            ),
        ),
        ("Item", "", f(2, IpcRequest::Item { id: ItemId(12) })),
        (
            "Enumerate",
            "",
            f(
                3,
                IpcRequest::Enumerate {
                    container: ItemId(1),
                    cursor: Some(vec![0, 0, 0, 200]),
                    limit: 500,
                    viewer: true,
                },
            ),
        ),
        (
            "Enumerate",
            "~nulls",
            f(
                4,
                IpcRequest::Enumerate {
                    container: ItemId(7),
                    cursor: None,
                    limit: 0,
                    viewer: false,
                },
            ),
        ),
        ("CurrentAnchor", "", f(5, IpcRequest::CurrentAnchor)),
        (
            "ChangesSince",
            "",
            f(
                6,
                IpcRequest::ChangesSince {
                    anchor: vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
                    limit: 200,
                },
            ),
        ),
        (
            "MaterializedChanged",
            "",
            f(
                7,
                IpcRequest::MaterializedChanged {
                    added: vec![ItemId(1), ItemId(12)],
                    removed: vec![ItemId(99)],
                    full: true,
                },
            ),
        ),
        (
            "Fetch",
            "",
            f(
                8,
                IpcRequest::Fetch {
                    id: ItemId(12),
                    version: Some(41),
                    dest_dir: "/private/var/folders/xy/T/wharf".into(),
                },
            ),
        ),
        (
            "Fetch",
            "~nulls",
            f(
                9,
                IpcRequest::Fetch {
                    id: ItemId(12),
                    version: None,
                    dest_dir: "/tmp".into(),
                },
            ),
        ),
        (
            "Create",
            "",
            f(
                10,
                IpcRequest::Create {
                    template_id: "NSFileProviderItemTemplate-1F2E".into(),
                    parent: ItemId(1),
                    name: "notes.md".into(),
                    kind: CreateKind::File,
                    has_content: true,
                    symlink_target: None,
                    mtime_ns: Some(1_727_654_400_000_000_000),
                    user_exec: Some(false),
                    changed_fields: fields::CONTENTS
                        | fields::FILENAME
                        | fields::CONTENT_MODIFICATION_DATE
                        | fields::TAG_DATA,
                    local: full_local(),
                    may_already_exist: true,
                    deletion_conflicted: false,
                },
            ),
        ),
        (
            "Create",
            "~symlink",
            f(
                11,
                IpcRequest::Create {
                    template_id: "t-2".into(),
                    parent: ItemId(5),
                    name: "current".into(),
                    kind: CreateKind::Symlink,
                    has_content: false,
                    symlink_target: Some("../releases/v2".into()),
                    mtime_ns: None,
                    user_exec: None,
                    changed_fields: 0,
                    local: LocalMeta::default(),
                    may_already_exist: false,
                    deletion_conflicted: true,
                },
            ),
        ),
        (
            "Create",
            "~package",
            f(
                12,
                IpcRequest::Create {
                    template_id: "t-3".into(),
                    parent: ItemId(1),
                    name: "Thing.app".into(),
                    kind: CreateKind::Package,
                    has_content: false,
                    symlink_target: None,
                    mtime_ns: None,
                    user_exec: None,
                    changed_fields: 0,
                    local: LocalMeta::default(),
                    may_already_exist: false,
                    deletion_conflicted: false,
                },
            ),
        ),
        (
            "Modify",
            "",
            f(
                13,
                IpcRequest::Modify {
                    id: ItemId(12),
                    base: BaseVersion {
                        content: Some(41),
                        meta: Some(42),
                    },
                    changed_fields: fields::CONTENTS
                        | fields::FILENAME
                        | fields::PARENT
                        | fields::FILE_SYSTEM_FLAGS,
                    new_parent: Some(ItemId(5)),
                    new_name: Some("renamed.md".into()),
                    has_content: true,
                    mtime_ns: Some(1_727_654_401_000_000_000),
                    user_exec: Some(true),
                    local: full_local(),
                },
            ),
        ),
        (
            "Modify",
            "~nulls",
            f(
                14,
                IpcRequest::Modify {
                    id: ItemId(12),
                    base: BaseVersion {
                        content: None,
                        meta: None,
                    },
                    changed_fields: fields::LAST_USED_DATE,
                    new_parent: None,
                    new_name: None,
                    has_content: false,
                    mtime_ns: None,
                    user_exec: None,
                    local: LocalMeta {
                        last_used_ns: Some(1),
                        ..LocalMeta::default()
                    },
                },
            ),
        ),
        (
            "Delete",
            "",
            f(
                15,
                IpcRequest::Delete {
                    id: ItemId(12),
                    base: BaseVersion {
                        content: Some(41),
                        meta: Some(42),
                    },
                    recursive: true,
                },
            ),
        ),
        ("Cancel", "", f(16, IpcRequest::Cancel { call: 8 })),
        ("Status", "", f(17, IpcRequest::Status)),
        (
            "ConfirmPaused",
            "",
            f(18, IpcRequest::ConfirmPaused { apply: false }),
        ),
    ]
}

pub fn responses() -> Vec<(&'static str, &'static str, IpcFrame<IpcResponse>)> {
    let f = |call: u64, msg: IpcResponse| IpcFrame { call, msg };
    vec![
        (
            "Hello",
            "",
            f(
                1,
                IpcResponse::Hello {
                    proto: unlatch_proto::PROTO_VERSION,
                    domain: "devbox".into(),
                },
            ),
        ),
        (
            "Item",
            "",
            f(2, IpcResponse::Item(item(12, "notes.md", Kind::File, true))),
        ),
        ("Item", "~root", f(2, IpcResponse::Item(root_item()))),
        (
            "Item",
            "~blocked-symlink",
            f(2, IpcResponse::Item(blocked_symlink())),
        ),
        (
            "Page",
            "",
            f(
                3,
                IpcResponse::Page {
                    items: vec![
                        item(12, "notes.md", Kind::File, false),
                        item(13, "node_modules", Kind::Dir, false),
                        item(14, "current", Kind::Symlink, false),
                        IpcItem {
                            display_name: "readme (Unlatch 1).md".into(),
                            ..item(15, "README.md", Kind::File, false)
                        },
                    ],
                    next: Some(vec![0, 0, 0, 4]),
                },
            ),
        ),
        (
            "Page",
            "~nulls",
            f(
                4,
                IpcResponse::Page {
                    items: vec![],
                    next: None,
                },
            ),
        ),
        (
            "Anchor",
            "",
            f(5, IpcResponse::Anchor(vec![1, 2, 3, 4, 5, 6, 7, 8, 9])),
        ),
        (
            "Changes",
            "",
            f(
                6,
                IpcResponse::Changes {
                    updated: vec![item(12, "notes.md", Kind::File, true)],
                    removed: vec![ItemId(20), ItemId(21)],
                    anchor: vec![1, 2, 3, 4, 5, 6, 7, 8, 10],
                    more: true,
                },
            ),
        ),
        (
            "Progress",
            "",
            f(
                8,
                IpcResponse::Progress {
                    done: 65_536,
                    total: 268_435_456,
                },
            ),
        ),
        (
            "Fetched",
            "",
            f(
                8,
                IpcResponse::Fetched {
                    path: "/private/var/folders/xy/T/wharf/12-41".into(),
                    item: item(12, "notes.md", Kind::File, false),
                },
            ),
        ),
        (
            "Done",
            "",
            f(
                13,
                IpcResponse::Done {
                    item: item(12, "notes.md", Kind::File, true),
                    still_pending: fields::TYPE_AND_CREATOR,
                    should_fetch_content: true,
                    conflict_copy: Some(item(
                        30,
                        "notes (conflict from Sam's MacBook 2026-09-30 10.00).md",
                        Kind::File,
                        false,
                    )),
                },
            ),
        ),
        (
            "Done",
            "~nulls",
            f(
                14,
                IpcResponse::Done {
                    item: item(12, "notes.md", Kind::File, false),
                    still_pending: 0,
                    should_fetch_content: false,
                    conflict_copy: None,
                },
            ),
        ),
        ("Deleted", "", f(15, IpcResponse::Deleted)),
        (
            "Status",
            "",
            f(17, IpcResponse::Status(status(ConnState::Live))),
        ),
        (
            "Status",
            "~offline",
            f(
                17,
                IpcResponse::Status(status(ConnState::Offline {
                    error: "timeout".into(),
                    retry_in_ms: 250,
                })),
            ),
        ),
        ("Ok", "", f(18, IpcResponse::Ok)),
        (
            "Error",
            "",
            f(
                15,
                IpcResponse::Error {
                    code: ErrorCode::DeletionRejected,
                    msg: "changed on the VM".into(),
                    current: Some(item(12, "notes.md", Kind::File, false)),
                },
            ),
        ),
        (
            "Error",
            "~nulls",
            f(
                16,
                IpcResponse::Error {
                    code: ErrorCode::Offline,
                    msg: "not connected".into(),
                    current: None,
                },
            ),
        ),
    ]
}

/// One `Error` sample per ErrorCode (Swift maps each onto an NSFileProviderError).
pub fn error_codes() -> Vec<(&'static str, ErrorCode)> {
    use ErrorCode::*;
    vec![
        ("NotFound", NotFound),
        ("Exists", Exists),
        ("NotDir", NotDir),
        ("IsDir", IsDir),
        ("NotEmpty", NotEmpty),
        ("Permission", Permission),
        ("VersionMismatch", VersionMismatch),
        ("InvalidName", InvalidName),
        ("NoSpace", NoSpace),
        ("Offline", Offline),
        ("AnchorExpired", AnchorExpired),
        ("Unsupported", Unsupported),
        ("Timeout", Timeout),
        ("Io", Io),
        ("Protocol", Protocol),
        ("IndexChanged", IndexChanged),
        ("DeletionRejected", DeletionRejected),
        ("CannotSync", CannotSync),
        ("ExcludedFromSync", ExcludedFromSync),
        ("NeedsUser", NeedsUser),
        ("RootReplaced", RootReplaced),
        ("Cancelled", Cancelled),
    ]
}

pub fn request_name(r: &IpcRequest) -> &'static str {
    match r {
        IpcRequest::Hello { .. } => "Hello",
        IpcRequest::Item { .. } => "Item",
        IpcRequest::Enumerate { .. } => "Enumerate",
        IpcRequest::CurrentAnchor => "CurrentAnchor",
        IpcRequest::ChangesSince { .. } => "ChangesSince",
        IpcRequest::MaterializedChanged { .. } => "MaterializedChanged",
        IpcRequest::Fetch { .. } => "Fetch",
        IpcRequest::Create { .. } => "Create",
        IpcRequest::Modify { .. } => "Modify",
        IpcRequest::Delete { .. } => "Delete",
        IpcRequest::Cancel { .. } => "Cancel",
        IpcRequest::Status => "Status",
        IpcRequest::ConfirmPaused { .. } => "ConfirmPaused",
    }
}

pub fn response_name(r: &IpcResponse) -> &'static str {
    match r {
        IpcResponse::Hello { .. } => "Hello",
        IpcResponse::Item(_) => "Item",
        IpcResponse::Page { .. } => "Page",
        IpcResponse::Anchor(_) => "Anchor",
        IpcResponse::Changes { .. } => "Changes",
        IpcResponse::Progress { .. } => "Progress",
        IpcResponse::Fetched { .. } => "Fetched",
        IpcResponse::Done { .. } => "Done",
        IpcResponse::Deleted => "Deleted",
        IpcResponse::Status(_) => "Status",
        IpcResponse::Ok => "Ok",
        IpcResponse::Error { .. } => "Error",
    }
}
