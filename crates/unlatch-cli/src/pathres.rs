//! Domain-relative path → item resolution (used by `unlatch ls|stat|cat` over IPC).
//!
//! Paths are relative to the domain root; a leading `/` is allowed and means the same. `.` and
//! empty components are skipped; `..` pops one level but never above the root. Components are
//! matched against **display names** (what the system sees, D16), byte for byte.

use unlatch_proto::ipc::IpcItem;
use unlatch_proto::{ErrorCode, ItemId, Kind, ProtoError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    AboveRoot,
    InvalidComponent(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::AboveRoot => write!(f, "path escapes the domain root"),
            PathError::InvalidComponent(c) => write!(f, "invalid path component {c:?}"),
        }
    }
}

impl std::error::Error for PathError {}

/// Split and normalize a domain-relative path.
pub fn normalize(path: &str) -> Result<Vec<String>, PathError> {
    let mut out: Vec<String> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    return Err(PathError::AboveRoot);
                }
            }
            c if c.contains('\0') => return Err(PathError::InvalidComponent(c.to_string())),
            c => out.push(c.to_string()),
        }
    }
    Ok(out)
}

/// What resolution needs from the engine.
pub trait Lister {
    fn item(&mut self, id: ItemId) -> Result<IpcItem, ProtoError>;
    fn children(&mut self, dir: ItemId) -> Result<Vec<IpcItem>, ProtoError>;
}

/// Resolve normalized components to an item, starting at the root.
pub fn resolve<L: Lister>(lister: &mut L, components: &[String]) -> Result<IpcItem, ProtoError> {
    let mut cur = lister.item(ItemId::ROOT)?;
    for (depth, comp) in components.iter().enumerate() {
        if cur.entry.kind != Kind::Dir {
            let so_far = components[..depth].join("/");
            return Err(ProtoError::new(
                ErrorCode::NotDir,
                format!("{so_far} is not a directory"),
            ));
        }
        let children = lister.children(cur.entry.id)?;
        cur = children
            .into_iter()
            .find(|c| &c.display_name == comp)
            .ok_or_else(|| {
                ProtoError::new(
                    ErrorCode::NotFound,
                    format!("{} not found", components[..=depth].join("/")),
                )
            })?;
    }
    Ok(cur)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use unlatch_proto::ipc::LocalMeta;
    use unlatch_proto::{Entry, Version};

    pub(crate) fn mk(id: u64, parent: u64, name: &str, kind: Kind) -> IpcItem {
        IpcItem {
            entry: Entry {
                id: ItemId(id),
                parent: ItemId(parent),
                name: name.to_string(),
                kind,
                size: 0,
                mtime_ns: 0,
                mode: 0o644,
                version: Version {
                    content: 1,
                    meta: 1,
                },
                symlink_target: None,
                lazy: false,
                seq: 1,
                access: 7,
            },
            display_name: name.to_string(),
            caps: 0,
            local: LocalMeta::default(),
            user_exec: false,
            symlink_blocked: false,
        }
    }

    struct Fake {
        items: HashMap<ItemId, IpcItem>,
        list_calls: usize,
    }

    impl Lister for Fake {
        fn item(&mut self, id: ItemId) -> Result<IpcItem, ProtoError> {
            self.items
                .get(&id)
                .cloned()
                .ok_or_else(|| ProtoError::new(ErrorCode::NotFound, "no"))
        }
        fn children(&mut self, dir: ItemId) -> Result<Vec<IpcItem>, ProtoError> {
            self.list_calls += 1;
            Ok(self
                .items
                .values()
                .filter(|i| i.entry.parent == dir && i.entry.id != dir)
                .cloned()
                .collect())
        }
    }

    fn tree() -> Fake {
        let mut items = HashMap::new();
        for it in [
            mk(1, 1, "", Kind::Dir),
            mk(2, 1, "src", Kind::Dir),
            mk(3, 2, "main.rs", Kind::File),
            mk(4, 1, "README", Kind::File),
        ] {
            items.insert(it.entry.id, it);
        }
        Fake {
            items,
            list_calls: 0,
        }
    }

    #[test]
    fn normalize_rules() {
        assert_eq!(normalize("").unwrap(), Vec::<String>::new());
        assert_eq!(normalize("/").unwrap(), Vec::<String>::new());
        assert_eq!(normalize("a//b/./c/").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(normalize("/a/b/../c").unwrap(), vec!["a", "c"]);
        assert_eq!(normalize(".."), Err(PathError::AboveRoot));
        assert_eq!(normalize("a/../.."), Err(PathError::AboveRoot));
        assert!(matches!(
            normalize("a\0b"),
            Err(PathError::InvalidComponent(_))
        ));
    }

    #[test]
    fn resolves_nested_paths() {
        let mut f = tree();
        assert_eq!(
            resolve(&mut f, &normalize("src/main.rs").unwrap())
                .unwrap()
                .entry
                .id,
            ItemId(3)
        );
        assert_eq!(resolve(&mut f, &[]).unwrap().entry.id, ItemId::ROOT);
        assert_eq!(f.list_calls, 2);
    }

    #[test]
    fn missing_and_not_dir() {
        let mut f = tree();
        let e = resolve(&mut f, &normalize("src/nope").unwrap()).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        assert!(e.msg.contains("src/nope"));
        let e = resolve(&mut f, &normalize("README/x").unwrap()).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotDir);
    }

    #[test]
    fn matches_display_name_not_real_name() {
        let mut f = tree();
        let mut shadow = mk(5, 1, "readme", Kind::File);
        shadow.display_name = "readme (Unlatch 1)".to_string();
        f.items.insert(ItemId(5), shadow);
        assert_eq!(
            resolve(&mut f, &normalize("readme (Unlatch 1)").unwrap())
                .unwrap()
                .entry
                .id,
            ItemId(5)
        );
        assert!(resolve(&mut f, &normalize("readme").unwrap()).is_err());
    }
}
