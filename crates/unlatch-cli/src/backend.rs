//! The engine operations the FUSE frontend needs, as a trait.
//!
//! [`unlatch_core::Engine`] implements it; the FUSE tests implement it with an in-memory model so
//! the kernel-facing layer (inode mapping, caching flags, invalidation, handle lifecycle) is
//! exercised against a real kernel mount without a VM.

use std::path::Path;
use unlatch_core::{
    CancelToken, CreateRequest, Engine, Fetched, Modified, ModifyRequest, Page, Result,
};
use unlatch_proto::ipc::IpcItem;
use unlatch_proto::{BaseVersion, ItemId};

pub trait Backend: Send + Sync + 'static {
    /// Replica metadata of one item (never touches the network).
    fn item(&self, id: ItemId) -> Result<IpcItem>;
    /// Child of `parent` by display name (may wait for a not-yet-listed container).
    fn lookup(&self, parent: ItemId, name: &str) -> Result<IpcItem>;
    /// One page of `container`'s children.
    fn list(
        &self,
        container: ItemId,
        cursor: Option<&[u8]>,
        limit: u32,
        viewer: bool,
    ) -> Result<Page>;
    /// A byte range of a file's content.
    fn read(&self, id: ItemId, offset: u64, len: u32) -> Result<Vec<u8>>;
    /// A private copy of the current content inside `dest_dir`.
    fn fetch(&self, id: ItemId, dest_dir: &Path) -> Result<Fetched>;
    fn create(&self, req: CreateRequest) -> Result<Modified>;
    fn modify(&self, id: ItemId, base: BaseVersion, req: ModifyRequest) -> Result<Modified>;
    fn delete(&self, id: ItemId, base: BaseVersion, recursive: bool) -> Result<()>;
}

impl Backend for Engine {
    fn item(&self, id: ItemId) -> Result<IpcItem> {
        Engine::item(self, id)
    }
    fn lookup(&self, parent: ItemId, name: &str) -> Result<IpcItem> {
        Engine::lookup(self, parent, name)
    }
    fn list(
        &self,
        container: ItemId,
        cursor: Option<&[u8]>,
        limit: u32,
        viewer: bool,
    ) -> Result<Page> {
        Engine::list(self, container, cursor, limit, viewer)
    }
    fn read(&self, id: ItemId, offset: u64, len: u32) -> Result<Vec<u8>> {
        Engine::read(self, id, offset, len)
    }
    fn fetch(&self, id: ItemId, dest_dir: &Path) -> Result<Fetched> {
        Engine::fetch(self, id, None, dest_dir, &|_, _| {}, &CancelToken::new())
    }
    fn create(&self, req: CreateRequest) -> Result<Modified> {
        Engine::create(self, req)
    }
    fn modify(&self, id: ItemId, base: BaseVersion, req: ModifyRequest) -> Result<Modified> {
        Engine::modify(self, id, base, req)
    }
    fn delete(&self, id: ItemId, base: BaseVersion, recursive: bool) -> Result<()> {
        Engine::delete(self, id, base, recursive)
    }
}

/// Collect every page of a container listing.
pub fn list_all<B: Backend + ?Sized>(
    b: &B,
    container: ItemId,
    viewer: bool,
) -> Result<Vec<IpcItem>> {
    const PAGE: u32 = 2048;
    let mut out = Vec::new();
    let mut cursor: Option<Vec<u8>> = None;
    loop {
        let page = b.list(container, cursor.as_deref(), PAGE, viewer)?;
        out.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(out),
        }
    }
}
