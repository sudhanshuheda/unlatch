//! Unlatch client engine.
//!
//! One [`Engine`] per VM root ("domain"). It owns the connection to `unlatchd`, the metadata replica
//! (SQLite), the content cache, prefetch, uploads, Mac-only metadata and the File Provider change
//! journal. Its public API is **blocking** and thread-safe (internally a tokio runtime), because
//! every consumer — the IPC dispatcher, the FFI layer, the FUSE frontend, fpsim and the
//! benchmarks — calls it from plain threads.
//!
//! The API is shaped like `NSFileProviderReplicatedExtension`: [`Engine::item`],
//! [`Engine::list`], [`Engine::anchor`], [`Engine::changes_since`], [`Engine::fetch`],
//! [`Engine::create`], [`Engine::modify`] (and their progress/cancel forms
//! [`Engine::create_with`], [`Engine::modify_with`]), [`Engine::delete`], [`Engine::materialized_changed`].
//!
//! Authoritative design: `docs/DESIGN.md` as amended by `docs/review/2026-09-30-design-review.md`
//! (the review wins where they disagree). Measured macOS quirks: `docs/review/sshdrive-macos-quirks.md`.
//!
//! Module ownership:
//! * `transport` — spawning ssh / a command, bootstrap/upload of `unlatchd`, preamble exchange,
//!   ssh failure classification.
//! * `ipc` — IPC dispatcher (shared by the unix-socket server and the macOS XPC bridge), server,
//!   client.
//! * everything else (`engine` internals: connection supervisor, session, replica, journal,
//!   working set, cache, prefetch, ops) is private to the crate.

pub mod ipc;
pub mod transport;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use unlatch_proto::ipc::{EngineStatus, IpcItem, LocalMeta};
use unlatch_proto::{BaseVersion, ErrorCode, ItemId, ProtoError};

pub type Result<T> = std::result::Result<T, ProtoError>;

pub(crate) fn err(code: ErrorCode, msg: impl Into<String>) -> ProtoError {
    ProtoError::new(code, msg)
}

/// How to reach `unlatchd`.
#[derive(Clone, Debug)]
pub enum Transport {
    /// `ssh <args> <destination> sh -s` running the bootstrap script, then `unlatchd connect`.
    /// `destination` is an ssh-config alias or `user@host`. Background (non-interactive)
    /// connects use `BatchMode=yes` + `ControlMaster=auto`, `ControlPath=~/.ssh/unlatch-%C`,
    /// `ControlPersist=10m`; interactive connects use `SSH_ASKPASS` (see `EngineConfig`).
    /// Always `-T -o Compression=no -o ServerAliveInterval=15 -o ServerAliveCountMax=3`.
    Ssh {
        destination: String,
        port: Option<u16>,
        identity: Option<PathBuf>,
        /// Extra ssh args, appended before the destination.
        extra_args: Vec<String>,
    },
    /// Spawn this argv directly; its stdin/stdout speak the wire protocol (preamble first).
    /// Tests: `["/path/to/unlatchd", "connect", "--root", "/tmp/x", "--state", "/tmp/s"]`.
    Command {
        argv: Vec<String>,
        env: Vec<(String, String)>,
    },
}

#[derive(Clone, Debug)]
pub struct PrefetchConfig {
    /// Files up to this size in a *viewer* enumeration are prefetched. 0 disables prefetch.
    pub max_file: u64,
    /// Byte budget per enumerated container.
    pub per_container: u64,
    /// Global token bucket: sustained bytes per minute and burst.
    pub bytes_per_min: u64,
    pub burst: u64,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            max_file: 256 * 1024,
            per_container: 8 << 20,
            bytes_per_min: 64 << 20,
            burst: 16 << 20,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Domain name (display + IPC hello), e.g. "devbox".
    pub name: String,
    pub transport: Transport,
    /// Root directory on the VM (absolute or `~/…`).
    pub remote_root: String,
    /// Remote `unlatchd` command override (skips bootstrap/upload), e.g. a path already on the VM.
    pub unlatchd_command: Option<String>,
    /// Remote install directory the bootstrap probe tries **first** (before `$UNLATCH_HOME`,
    /// `$XDG_DATA_HOME/unlatch`, `~/.unlatch`, …), subject to the same checks (0700, owned, not a
    /// symlink, local fs, exec test). `None` = the normal probe order. For tests and unusual VM
    /// layouts; ssh does not forward the Mac's environment, so this is the only way to steer it.
    pub remote_install_dir: Option<String>,
    /// Local `unlatchd` binaries to upload when missing on the VM, keyed by `uname -m`
    /// (`x86_64`, `aarch64`), with their sha256 (hex). Empty = never upload.
    pub unlatchd_upload: Vec<UnlatchdBinary>,
    /// Replica database lives here (created if missing).
    pub state_dir: PathBuf,
    /// Content cache directory (created if missing). Must be on the same volume as the
    /// `dest_dir`s passed to `fetch` for clonefile/reflink to apply (falls back to copy).
    pub cache_dir: PathBuf,
    /// Scratch space for staging uploads.
    pub temp_dir: PathBuf,
    pub cache_budget: u64,
    pub prefetch: PrefetchConfig,
    /// Suggested lazy names for a *new* server index (the server's persisted list wins).
    pub default_lazy_names: Vec<String>,
    /// Sanitized machine name for conflict files (host supplies `SCDynamicStoreCopyComputerName`).
    pub client_name: String,
    /// Environment for spawning ssh (the host resolves the login-shell env: PATH, SSH_AUTH_SOCK…).
    /// Empty = inherit.
    pub ssh_env: Vec<(String, String)>,
    /// Path of an askpass helper for interactive connects (`SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE=force`).
    pub askpass: Option<PathBuf>,
    /// Maximum time `list`/`lookup` wait for a not-yet-known container before `Timeout`/`Offline`.
    pub list_timeout: Duration,
    /// Show the exec bit to the Mac (default false).
    pub expose_exec: bool,
    /// Mass-deletion guard thresholds: pause when one batch would remove more than this fraction
    /// of materialized items (only once it removes more than `mass_delete_min` of them), or more
    /// than `mass_delete_abs` of them regardless of the fraction.
    pub mass_delete_frac: f64,
    pub mass_delete_abs: u64,
    /// Floor of the fraction rule (default 32): a batch removing at most this many materialized
    /// items never trips it (deleting 1 of 3 downloaded files is not a mass deletion).
    pub mass_delete_min: u64,
}

#[derive(Clone, Debug)]
pub struct UnlatchdBinary {
    pub arch: String,
    pub path: PathBuf,
    pub sha256_hex: String,
}

impl EngineConfig {
    /// Sensible defaults for everything but the transport/root/dirs/client name.
    pub fn new(
        name: &str,
        transport: Transport,
        remote_root: &str,
        state_dir: PathBuf,
        client_name: &str,
    ) -> Self {
        let cache_dir = state_dir.join("cache");
        let temp_dir = state_dir.join("tmp");
        Self {
            name: name.to_string(),
            transport,
            remote_root: remote_root.to_string(),
            unlatchd_command: None,
            remote_install_dir: None,
            unlatchd_upload: Vec::new(),
            state_dir,
            cache_dir,
            temp_dir,
            cache_budget: 5 << 30,
            prefetch: PrefetchConfig::default(),
            default_lazy_names: unlatch_proto::DEFAULT_LAZY_NAMES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            client_name: client_name.to_string(),
            ssh_env: Vec::new(),
            askpass: None,
            list_timeout: Duration::from_secs(20),
            expose_exec: false,
            mass_delete_frac: 0.20,
            mass_delete_abs: 1000,
            mass_delete_min: 32,
        }
    }
}

/// Notifications for the embedding app. Delivered on an engine thread; keep handlers cheap.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    /// The working set changed (emitted only **after** the SQLite commit containing the change).
    /// The macOS host calls `signalEnumerator(for: .workingSet)`. Coalesced (≤ 1 per 5 ms).
    WorkingSetChanged {
        anchor: Vec<u8>,
    },
    /// Any replica change (including non-materialized containers). FUSE uses it to invalidate
    /// kernel caches. `ids` = changed/removed items, `parents` = containers whose listing changed.
    ReplicaChanged {
        ids: Vec<ItemId>,
        parents: Vec<ItemId>,
    },
    StatusChanged(EngineStatus),
    /// Connection is live again: host calls `signalErrorResolved(.serverUnreachable)` (MQ-005).
    ErrorResolved,
    /// Host calls `reimportItems(below:)` (index id changed, replica uuid mismatch…).
    Reimport {
        below: ItemId,
    },
    NeedsUser {
        reason: String,
        url: Option<String>,
    },
}

pub type EventHandler = Arc<dyn Fn(EngineEvent) + Send + Sync>;

/// Cooperative cancellation for long calls.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst)
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// One page of a container listing.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub items: Vec<IpcItem>,
    /// Opaque cursor for the next page; `None` = last page.
    pub next: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Changes {
    pub updated: Vec<IpcItem>,
    pub removed: Vec<ItemId>,
    /// Opaque `(replica_uuid, seq)`.
    pub anchor: Vec<u8>,
    pub more: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fetched {
    /// Private copy inside `dest_dir` the caller may move/delete.
    pub path: PathBuf,
    pub item: IpcItem,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Modified {
    pub item: IpcItem,
    /// `changed_fields` bits not handled (system will re-offer them later).
    pub still_pending: u32,
    /// Conflict / content differs: the system must re-download the item.
    pub should_fetch_content: bool,
    pub conflict_copy: Option<IpcItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateKind {
    File,
    Dir,
    Symlink,
    Package,
    Alias,
}

impl From<unlatch_proto::ipc::CreateKind> for CreateKind {
    fn from(k: unlatch_proto::ipc::CreateKind) -> Self {
        use unlatch_proto::ipc::CreateKind as K;
        match k {
            K::File => CreateKind::File,
            K::Dir => CreateKind::Dir,
            K::Symlink => CreateKind::Symlink,
            K::Package => CreateKind::Package,
            K::Alias => CreateKind::Alias,
        }
    }
}

#[derive(Debug)]
pub struct CreateRequest {
    /// `itemTemplate.itemIdentifier` — stable across replays (op id = H(domain, template_id)).
    pub template_id: String,
    pub parent: ItemId,
    pub name: String,
    pub kind: CreateKind,
    /// File content (the engine reads it fully before returning).
    pub content: Option<std::fs::File>,
    pub symlink_target: Option<String>,
    pub mtime_ns: Option<i64>,
    pub user_exec: Option<bool>,
    pub changed_fields: u32,
    pub local: LocalMeta,
    pub may_already_exist: bool,
    pub deletion_conflicted: bool,
}

#[derive(Debug, Default)]
pub struct ModifyRequest {
    pub changed_fields: u32,
    pub new_parent: Option<ItemId>,
    pub new_name: Option<String>,
    pub content: Option<std::fs::File>,
    pub mtime_ns: Option<i64>,
    pub user_exec: Option<bool>,
    pub local: LocalMeta,
}

mod engine;

/// The engine. Cheap to clone (shared handle).
#[derive(Clone)]
pub struct Engine {
    inner: Arc<engine::Inner>,
}

impl Engine {
    /// Load the replica from `state_dir` (so reads work immediately, offline) and start
    /// connecting in the background (non-interactive). Never blocks on the network.
    pub fn start(cfg: EngineConfig, events: Option<EventHandler>) -> Result<Engine> {
        engine::Inner::start(cfg, events).map(|inner| Engine { inner })
    }

    pub fn status(&self) -> EngineStatus {
        self.inner.status()
    }

    /// The domain name this engine serves (`EngineConfig::name`); the IPC `Hello` must match it.
    pub fn name(&self) -> &str {
        self.inner.name()
    }

    /// Connect now, allowing interactive auth (askpass). Used by "Add VM" and "Retry" in the UI.
    pub fn connect_interactive(&self) -> Result<()> {
        self.inner.connect_interactive()
    }

    /// Block until the initial sync (or resume) is complete and the engine is `Live`.
    pub fn wait_live(&self, timeout: Duration) -> Result<()> {
        self.inner.wait_live(timeout)
    }

    /// Round-trip a `Ping`: after it returns, every event the server emitted before receiving
    /// the ping has been applied (committed) locally and signalled.
    pub fn server_barrier(&self, timeout: Duration) -> Result<()> {
        self.inner.server_barrier(timeout)
    }

    /// Block until no uploads/mutations are pending and the apply queue is empty.
    pub fn wait_idle(&self, timeout: Duration) -> Result<()> {
        self.inner.wait_idle(timeout)
    }

    // ---- File Provider-shaped API -------------------------------------------------------

    /// Metadata of one item from the replica. Never touches the network.
    pub fn item(&self, id: ItemId) -> Result<IpcItem> {
        self.inner.item(id)
    }

    /// Children of `container` (sorted by display name, byte order). If the container's listing
    /// isn't known yet (initial sync in progress, lazy dir), issues a priority `ListDir` and waits
    /// up to `list_timeout`. `viewer` → prefetch small files (budgeted).
    pub fn list(
        &self,
        container: ItemId,
        cursor: Option<&[u8]>,
        limit: u32,
        viewer: bool,
    ) -> Result<Page> {
        self.inner.list(container, cursor, limit, viewer)
    }

    /// Lookup a child by (display) name (FUSE `lookup`). Same sync-on-demand semantics as `list`.
    pub fn lookup(&self, parent: ItemId, name: &str) -> Result<IpcItem> {
        self.inner.lookup(parent, name)
    }

    /// Update the persisted materialized set M (from `materializedItemsDidChange`).
    pub fn materialized_changed(
        &self,
        added: &[ItemId],
        removed: &[ItemId],
        full: bool,
    ) -> Result<()> {
        self.inner.materialized_changed(added, removed, full)
    }

    /// Current working-set anchor (opaque `(replica_uuid, seq)`).
    pub fn anchor(&self) -> Vec<u8> {
        self.inner.anchor()
    }

    /// Working-set changes after `anchor`: every item with `changed_seq > anchor.seq` where
    /// `id ∈ M || old_parent ∈ M || new_parent ∈ M`, and tombstones likewise (children before
    /// parents). `Err(AnchorExpired)` only below the tombstone GC horizon; an anchor from another
    /// replica uuid → `Err(AnchorExpired)` + `EngineEvent::Reimport`.
    pub fn changes_since(&self, anchor: &[u8], limit: u32) -> Result<Changes> {
        self.inner.changes_since(anchor, limit)
    }

    /// Materialize content into `dest_dir` (clonefile/reflink from the cache when current,
    /// else a streamed download under credit). `progress(done, total)`.
    pub fn fetch(
        &self,
        id: ItemId,
        version: Option<u64>,
        dest_dir: &Path,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Fetched> {
        self.inner.fetch(id, version, dest_dir, progress, cancel)
    }

    /// Read a byte range (FUSE `read`), served from the cache (downloads the file on first read).
    pub fn read(&self, id: ItemId, offset: u64, len: u32) -> Result<Vec<u8>> {
        self.inner.read(id, offset, len)
    }

    /// Never returns `Exists`: an existing same-kind item that matches (hash, or
    /// `may_already_exist`) is returned; otherwise the item is created as `name 2.ext`, `3`, …
    ///
    /// Same as [`Engine::create_with`] without progress or cancellation.
    pub fn create(&self, req: CreateRequest) -> Result<Modified> {
        self.create_with(req, &|_, _| {}, &CancelToken::new())
    }

    /// [`Engine::create`] with upload progress and cancellation. `progress(done, total)` reports
    /// content bytes handed to the connection (`total` = the file's size; called with `(0, total)`
    /// once the content is staged, then after every chunk, ending with `(total, total)`; never
    /// for items without content). `cancel` is checked while staging, between chunks and while
    /// waiting for unlatchd's reply: a cancelled call aborts the upload (unlatchd discards the staged
    /// bytes) and returns `Err(Cancelled)`; a create unlatchd already committed still shows up
    /// through the change stream.
    pub fn create_with(
        &self,
        req: CreateRequest,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Modified> {
        self.inner.create(req, progress, cancel)
    }

    /// Content conflicts never error: the reply carries `should_fetch_content` and the conflict
    /// copy. Metadata-only changes to Mac-only fields generate no network traffic.
    ///
    /// Same as [`Engine::modify_with`] without progress or cancellation.
    pub fn modify(&self, id: ItemId, base: BaseVersion, req: ModifyRequest) -> Result<Modified> {
        self.modify_with(id, base, req, &|_, _| {}, &CancelToken::new())
    }

    /// [`Engine::modify`] with upload progress and cancellation (same semantics as
    /// [`Engine::create_with`]; progress is reported only when the call uploads new content).
    pub fn modify_with(
        &self,
        id: ItemId,
        base: BaseVersion,
        req: ModifyRequest,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Modified> {
        self.inner.modify(id, base, req, progress, cancel)
    }

    /// Unknown id → Ok. Changed since seen / non-empty non-recursive dir / partial recursive
    /// delete → `Err(DeletionRejected)` (use [`Engine::item`] for the current state).
    pub fn delete(&self, id: ItemId, base: BaseVersion, recursive: bool) -> Result<()> {
        self.inner.delete(id, base, recursive)
    }

    /// Resolve a paused mass deletion (`ConnState::Paused`).
    pub fn confirm_paused(&self, apply: bool) -> Result<()> {
        self.inner.confirm_paused(apply)
    }

    // ---- lifecycle ------------------------------------------------------------------------

    /// Hint from the OS (network path changed / wake from sleep): reconnect now if not live.
    pub fn network_changed(&self) {
        self.inner.network_changed()
    }

    /// Kill the current connection (tests: simulate a network drop). Reconnect follows.
    pub fn drop_connection(&self) {
        self.inner.drop_connection()
    }

    /// Serve the IPC protocol on a unix socket (mode 0600). Returns once the listener is bound.
    pub fn serve_ipc(&self, socket_path: &Path) -> Result<ipc::IpcServerHandle> {
        ipc::serve(self.clone(), socket_path)
    }

    /// Flush the replica to disk and stop.
    pub fn shutdown(&self) {
        self.inner.shutdown()
    }
}
