//! The VM side as an agent sees it: plain filesystem operations under the root, plus a snapshot
//! of the tree for invariant checks.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmKind {
    File,
    Dir,
    Symlink,
    /// Socket, FIFO, device: never exposed by Unlatch.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmNode {
    pub kind: VmKind,
    pub size: u64,
    pub content: Option<Vec<u8>>,
    pub target: Option<String>,
    pub mode: u32,
}

/// The VM tree below the root, keyed by `/`-joined relative path (root itself excluded).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VmTree {
    /// Canonical absolute root path (absolute symlink targets inside it are exposed relative).
    pub root_abs: Option<String>,
    pub nodes: BTreeMap<String, VmNode>,
}

impl VmTree {
    /// Names directly below `dir` ("" = root).
    pub fn children(&self, dir: &str) -> Vec<String> {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        self.nodes
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .filter_map(|(k, _)| {
                let rest = &k[prefix.len()..];
                (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_string())
            })
            .collect()
    }

    /// Every regular file's content (for "was this write lost?" lookups).
    pub fn contents(&self) -> impl Iterator<Item = (&String, &Vec<u8>)> {
        self.nodes
            .iter()
            .filter_map(|(p, n)| n.content.as_ref().map(|c| (p, c)))
    }
}

/// Names the daemon stages with and never exposes (review (d)2 fallback).
pub fn is_daemon_internal(name: &str) -> bool {
    name.starts_with(".unlatch-")
}

/// Agent-style operations on the VM root. Paths are `/`-separated, relative to the root.
pub trait VmFs {
    fn write(&mut self, path: &str, data: &[u8]) -> io::Result<()>;
    fn append(&mut self, path: &str, data: &[u8]) -> io::Result<()>;
    fn read(&mut self, path: &str) -> io::Result<Vec<u8>>;
    fn mkdir(&mut self, path: &str) -> io::Result<()>;
    fn remove_file(&mut self, path: &str) -> io::Result<()>;
    fn remove_dir_all(&mut self, path: &str) -> io::Result<()>;
    /// `rename(2)`: replaces an existing file (or empty dir) at `to`.
    fn rename(&mut self, from: &str, to: &str) -> io::Result<()>;
    fn symlink(&mut self, target: &str, path: &str) -> io::Result<()>;
    fn hard_link(&mut self, existing: &str, new: &str) -> io::Result<()>;
    fn set_mode(&mut self, path: &str, mode: u32) -> io::Result<()>;
    /// `lstat` kind, `None` if missing.
    fn kind(&mut self, path: &str) -> Option<VmKind>;
    fn list(&mut self, dir: &str) -> io::Result<Vec<String>>;
    fn snapshot(&mut self) -> io::Result<VmTree>;
    /// Absolute path of the root when it is a real directory (for out-of-root symlinks).
    fn root_abs(&self) -> Option<PathBuf>;
}

/// The real VM root on this machine (e2e runs).
pub struct RealFs {
    root: PathBuf,
}

impl RealFs {
    pub fn new(root: &Path) -> RealFs {
        RealFs {
            root: root.to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Absolute path of `rel`, refusing to traverse a symlinked intermediate component: the agent
    /// never writes *through* a link, so "nothing outside the root changed" measures Unlatch alone.
    fn abs(&self, rel: &str) -> io::Result<PathBuf> {
        let mut p = self.root.clone();
        let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        for (i, c) in comps.iter().enumerate() {
            if *c == "." || *c == ".." {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "dot component"));
            }
            p.push(c);
            if i + 1 < comps.len() {
                match std::fs::symlink_metadata(&p) {
                    Ok(m) if m.file_type().is_symlink() => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "symlinked ancestor",
                        ));
                    }
                    _ => {}
                }
            }
        }
        Ok(p)
    }
}

impl VmFs for RealFs {
    fn write(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        let p = self.abs(path)?;
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write through symlink",
            ));
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(p)?;
        f.write_all(data)
    }

    fn append(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        let p = self.abs(path)?;
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "append through symlink",
            ));
        }
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(p)?;
        f.write_all(data)
    }

    fn read(&mut self, path: &str) -> io::Result<Vec<u8>> {
        let p = self.abs(path)?;
        if std::fs::symlink_metadata(&p)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read through symlink",
            ));
        }
        std::fs::read(p)
    }

    fn mkdir(&mut self, path: &str) -> io::Result<()> {
        std::fs::create_dir(self.abs(path)?)
    }

    fn remove_file(&mut self, path: &str) -> io::Result<()> {
        std::fs::remove_file(self.abs(path)?)
    }

    fn remove_dir_all(&mut self, path: &str) -> io::Result<()> {
        let p = self.abs(path)?;
        // std's remove_dir_all does not follow symlinks; a symlink itself is just unlinked.
        if std::fs::symlink_metadata(&p)?.file_type().is_symlink() {
            return std::fs::remove_file(p);
        }
        std::fs::remove_dir_all(p)
    }

    fn rename(&mut self, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(self.abs(from)?, self.abs(to)?)
    }

    fn symlink(&mut self, target: &str, path: &str) -> io::Result<()> {
        std::os::unix::fs::symlink(target, self.abs(path)?)
    }

    fn hard_link(&mut self, existing: &str, new: &str) -> io::Result<()> {
        std::fs::hard_link(self.abs(existing)?, self.abs(new)?)
    }

    fn set_mode(&mut self, path: &str, mode: u32) -> io::Result<()> {
        let p = self.abs(path)?;
        if std::fs::symlink_metadata(&p)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "chmod through symlink",
            ));
        }
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    }

    fn kind(&mut self, path: &str) -> Option<VmKind> {
        let m = std::fs::symlink_metadata(self.abs(path).ok()?).ok()?;
        Some(kind_of(&m))
    }

    fn list(&mut self, dir: &str) -> io::Result<Vec<String>> {
        let p = self.abs(dir)?;
        if std::fs::symlink_metadata(&p)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "list through symlink",
            ));
        }
        let mut v: Vec<String> = std::fs::read_dir(p)?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        v.sort();
        Ok(v)
    }

    fn snapshot(&mut self) -> io::Result<VmTree> {
        let root_abs = std::fs::canonicalize(&self.root)?
            .to_string_lossy()
            .into_owned();
        let mut tree = VmTree {
            root_abs: Some(root_abs),
            nodes: BTreeMap::new(),
        };
        walk_real(&self.root, "", &mut tree)?;
        Ok(tree)
    }

    fn root_abs(&self) -> Option<PathBuf> {
        std::fs::canonicalize(&self.root).ok()
    }
}

fn kind_of(m: &std::fs::Metadata) -> VmKind {
    let t = m.file_type();
    if t.is_symlink() {
        VmKind::Symlink
    } else if t.is_dir() {
        VmKind::Dir
    } else if t.is_file() {
        VmKind::File
    } else {
        VmKind::Other
    }
}

fn walk_real(dir: &Path, rel: &str, tree: &mut VmTree) -> io::Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        // Unreadable dirs (agent chmod 000) are simply not listed.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e),
    };
    for ent in rd {
        let ent = ent?;
        // Non-UTF-8 names are not exposed by Unlatch (DESIGN §8).
        let Ok(name) = ent.file_name().into_string() else {
            continue;
        };
        if is_daemon_internal(&name) {
            continue;
        }
        let path = ent.path();
        let m = std::fs::symlink_metadata(&path)?;
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let kind = kind_of(&m);
        let mut node = VmNode {
            kind,
            size: m.len(),
            content: None,
            target: None,
            mode: m.mode() & 0o7777,
        };
        match kind {
            VmKind::File => match std::fs::read(&path) {
                Ok(c) => {
                    node.size = c.len() as u64;
                    node.content = Some(c);
                }
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
                Err(e) => return Err(e),
            },
            VmKind::Symlink => {
                let t = std::fs::read_link(&path)?.to_string_lossy().into_owned();
                node.size = t.len() as u64;
                node.target = Some(t);
            }
            VmKind::Dir => node.size = 0,
            VmKind::Other => {}
        }
        tree.nodes.insert(child.clone(), node);
        if kind == VmKind::Dir {
            walk_real(&path, &child, tree)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_fs_roundtrip_and_refuses_symlinked_ancestors() -> io::Result<()> {
        let t = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let mut fs = RealFs::new(t.path());
        fs.mkdir("d")?;
        fs.write("d/a.txt", b"hello")?;
        fs.append("d/a.txt", b" world")?;
        assert_eq!(fs.read("d/a.txt")?, b"hello world");
        fs.symlink(&outside.path().to_string_lossy(), "out")?;
        assert!(fs.write("out/evil", b"x").is_err());
        assert!(std::fs::read_dir(outside.path())?.next().is_none());
        let snap = fs.snapshot()?;
        assert_eq!(snap.children(""), vec!["d".to_string(), "out".to_string()]);
        assert_eq!(
            snap.nodes["d/a.txt"].content.as_deref(),
            Some(&b"hello world"[..])
        );
        assert_eq!(snap.nodes["out"].kind, VmKind::Symlink);
        Ok(())
    }
}
