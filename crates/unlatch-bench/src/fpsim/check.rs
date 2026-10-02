//! Invariants after quiesce: what the simulated Mac shows equals the VM tree (modulo the
//! documented display-name mapping and symlink rule), the materialized tree stays inside the
//! domain, nothing outside the root changed.

use super::names::{fold, strip_unlatch_marker};
use super::sim::VisibleItem;
use super::vmfs::{VmKind, VmNode, VmTree};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use unlatch_proto::Kind;

/// How a VM symlink must appear on the Mac (engine rule 9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymlinkView {
    /// Exposed as a symlink resolving to this root-relative path.
    Exposed(String),
    /// Exposed as a read-only file whose content is the target text.
    Blocked,
    /// Edge of the rule the design does not pin down (e.g. a bare `..`): accept either.
    Either,
}

/// Lexically resolve `target` relative to directory `dir` (both root-relative). `None` when it
/// climbs above the root.
pub fn resolve_lexical(dir: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|c| !c.is_empty()).collect();
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Rule 9: a VM symlink at `link_path` with `target` is shown as a symlink iff its target is
/// `"../"×k` + normal components with k ≤ depth of the link's parent; absolute targets inside
/// the root are first rewritten relative.
pub fn symlink_view(link_path: &str, target: &str, root_abs: Option<&str>) -> SymlinkView {
    let parent = link_path.rsplit_once('/').map_or("", |(p, _)| p);
    let depth = parent.split('/').filter(|c| !c.is_empty()).count();
    let rel = if let Some(abs) = target.strip_prefix('/') {
        let Some(root) = root_abs else {
            return SymlinkView::Blocked;
        };
        let root = root.trim_end_matches('/');
        let Some(inside) = format!("/{abs}").strip_prefix(root).map(str::to_string) else {
            return SymlinkView::Blocked;
        };
        let Some(inside) = inside.strip_prefix('/') else {
            // The root itself (or a sibling sharing its prefix) — not a normal in-root target.
            return if inside.is_empty() {
                SymlinkView::Either
            } else {
                SymlinkView::Blocked
            };
        };
        if inside
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return SymlinkView::Blocked;
        }
        return SymlinkView::Exposed(inside.to_string());
    } else {
        target
    };
    let comps: Vec<&str> = rel.split('/').collect();
    let k = comps.iter().take_while(|c| **c == "..").count();
    let rest = &comps[k..];
    if k > depth {
        return SymlinkView::Blocked;
    }
    if rest.is_empty() {
        return SymlinkView::Either;
    }
    if rest.iter().any(|c| c.is_empty() || *c == "." || *c == "..") {
        // A trailing slash is an empty component too; the rule rejects it but an implementation
        // could normalise it away.
        if rest.last() == Some(&"")
            && rest[..rest.len() - 1]
                .iter()
                .all(|c| !c.is_empty() && *c != "." && *c != "..")
        {
            return SymlinkView::Either;
        }
        return SymlinkView::Blocked;
    }
    match resolve_lexical(parent, rel) {
        Some(p) => SymlinkView::Exposed(p),
        None => SymlinkView::Blocked,
    }
}

/// Compare the Mac's visible tree against the VM. Only containers the Mac has enumerated are
/// compared entry-by-entry (MQ-001: a never-opened folder has no listing on the Mac).
pub fn compare_trees(vm: &VmTree, visible: &[VisibleItem]) -> Vec<String> {
    let mut problems = Vec::new();
    let by_path: HashMap<&str, &VisibleItem> =
        visible.iter().map(|v| (v.path.as_str(), v)).collect();
    let mut sim_children: BTreeMap<&str, Vec<&VisibleItem>> = BTreeMap::new();
    for v in visible {
        // Mac-only items have no VM counterpart. An item whose upload failed for good still
        // shows under its name (with an error badge): only its content may diverge.
        if v.local_only {
            continue;
        }
        let parent = v.path.rsplit_once('/').map_or("", |(p, _)| p);
        sim_children.entry(parent).or_default().push(v);
    }
    let mut stack: Vec<(String, String)> = vec![(String::new(), String::new())];
    while let Some((sim_dir, vm_dir)) = stack.pop() {
        let vm_names: Vec<String> = vm
            .children(&vm_dir)
            .into_iter()
            .filter(|n| {
                vm.nodes
                    .get(&join(&vm_dir, n))
                    .is_some_and(|x| x.kind != VmKind::Other)
            })
            .collect();
        let vm_set: HashSet<&str> = vm_names.iter().map(String::as_str).collect();
        let mut folds: HashMap<String, u32> = HashMap::new();
        for n in &vm_names {
            *folds.entry(fold(n)).or_default() += 1;
        }
        let kids = sim_children
            .get(sim_dir.as_str())
            .cloned()
            .unwrap_or_default();
        let mut seen_real: HashSet<String> = HashSet::new();
        for v in kids {
            let shown = v.path.rsplit_once('/').map_or(v.path.as_str(), |(_, n)| n);
            // D16 / rule 11: `stem (Unlatch N).ext` stands for the real name `stem.ext` when that
            // name collides with a sibling on the Mac's case-insensitive volume.
            let real = match strip_unlatch_marker(shown) {
                Some(r)
                    if vm_set.contains(r.as_str())
                        && folds.get(&fold(&r)).copied().unwrap_or(0) > 1 =>
                {
                    r
                }
                _ => shown.to_string(),
            };
            if !seen_real.insert(real.clone()) {
                problems.push(format!("{}: two Mac items map to VM name {real:?}", v.path));
                continue;
            }
            let vm_path = join(&vm_dir, &real);
            if v.sync_error {
                // Its VM name is accounted for, if it has one (a failed create has none).
                continue;
            }
            let Some(node) = vm.nodes.get(&vm_path).filter(|n| n.kind != VmKind::Other) else {
                problems.push(format!(
                    "{}: shown on the Mac, missing on the VM{}",
                    v.path,
                    pending_note(v)
                ));
                continue;
            };
            compare_item(&vm_path, node, v, vm.root_abs.as_deref(), &mut problems);
            if v.kind == Kind::Dir && node.kind == VmKind::Dir && v.enumerated {
                stack.push((v.path.clone(), vm_path));
            }
        }
        let root_or_enumerated =
            sim_dir.is_empty() || by_path.get(sim_dir.as_str()).is_some_and(|d| d.enumerated);
        if root_or_enumerated {
            for n in &vm_names {
                if !seen_real.contains(n) {
                    problems.push(format!(
                        "{}: on the VM, missing on the Mac",
                        join(&vm_dir, n)
                    ));
                }
            }
        }
    }
    problems
}

fn pending_note(v: &VisibleItem) -> &'static str {
    if v.pending {
        " (upload still pending)"
    } else {
        ""
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn compare_item(
    vm_path: &str,
    node: &VmNode,
    v: &VisibleItem,
    root_abs: Option<&str>,
    out: &mut Vec<String>,
) {
    match node.kind {
        VmKind::Dir => {
            if v.kind != Kind::Dir {
                out.push(format!("{}: VM dir shown as {:?}", v.path, v.kind));
            }
        }
        VmKind::File => {
            if v.kind != Kind::File || v.symlink_blocked {
                out.push(format!("{}: VM file shown as {:?}", v.path, v.kind));
                return;
            }
            if v.size != node.size {
                out.push(format!(
                    "{}: size {} on the Mac, {} on the VM{}",
                    v.path,
                    v.size,
                    node.size,
                    pending_note(v)
                ));
            }
            if let (Some(mac), Some(vmc)) = (&v.content, &node.content) {
                if mac != vmc {
                    out.push(format!(
                        "{}: materialized content differs from the VM ({} vs {} bytes){}",
                        v.path,
                        mac.len(),
                        vmc.len(),
                        pending_note(v)
                    ));
                }
            }
        }
        VmKind::Symlink => {
            let target = node.target.clone().unwrap_or_default();
            let blocked_ok = |out: &mut Vec<String>| {
                if !(v.symlink_blocked || v.kind == Kind::File) {
                    out.push(format!(
                        "{}: out-of-root symlink {target:?} exposed as a symlink",
                        v.path
                    ));
                    return;
                }
                if v.size != target.len() as u64 {
                    out.push(format!(
                        "{}: blocked symlink size {} != target length {}",
                        v.path,
                        v.size,
                        target.len()
                    ));
                }
                if let Some(c) = &v.content {
                    if c.as_slice() != target.as_bytes() {
                        out.push(format!(
                            "{}: blocked symlink content is not the target text",
                            v.path
                        ));
                    }
                }
            };
            match symlink_view(vm_path, &target, root_abs) {
                SymlinkView::Blocked => blocked_ok(out),
                SymlinkView::Exposed(want) => {
                    if v.kind != Kind::Symlink || v.symlink_blocked {
                        out.push(format!(
                            "{}: in-root symlink {target:?} not exposed as a symlink",
                            v.path
                        ));
                        return;
                    }
                    let parent = v.path.rsplit_once('/').map_or("", |(p, _)| p);
                    let got = v.symlink_target.as_deref().and_then(|t| {
                        if t.starts_with('/') {
                            None
                        } else {
                            resolve_lexical(parent, t)
                        }
                    });
                    // The Mac path of the parent may carry display names; compare by VM path.
                    let vm_parent = vm_path.rsplit_once('/').map_or("", |(p, _)| p);
                    let got_vm = v
                        .symlink_target
                        .as_deref()
                        .and_then(|t| resolve_lexical(vm_parent, t));
                    if got_vm.as_deref() != Some(want.as_str())
                        && got.as_deref() != Some(want.as_str())
                    {
                        out.push(format!(
                            "{}: symlink target {:?} does not resolve to {want:?}",
                            v.path, v.symlink_target
                        ));
                    }
                }
                SymlinkView::Either => {
                    if v.kind == Kind::Symlink && !v.symlink_blocked {
                        return;
                    }
                    blocked_ok(out)
                }
            }
        }
        VmKind::Other => {}
    }
}

/// Write the Mac's visible tree into `dir` as real files/dirs/symlinks, then check that every
/// entry's realpath stays inside `dir` (D12: the domain can never point outside itself).
pub fn materialize_and_check_realpaths(
    visible: &[VisibleItem],
    dir: &Path,
) -> io::Result<Vec<String>> {
    let mut problems = Vec::new();
    std::fs::create_dir_all(dir)?;
    let root = std::fs::canonicalize(dir)?;
    let mut items: Vec<&VisibleItem> = visible.iter().collect();
    items.sort_by_key(|v| v.path.matches('/').count());
    for v in &items {
        let p = root.join(&v.path);
        if let Some(parent) = p.parent() {
            if !parent.is_dir() {
                continue;
            }
        }
        match v.kind {
            Kind::Dir => std::fs::create_dir(&p)?,
            Kind::Symlink if !v.symlink_blocked => {
                let t = v.symlink_target.clone().unwrap_or_default();
                std::os::unix::fs::symlink(&t, &p)?;
            }
            _ => std::fs::write(&p, v.content.as_deref().unwrap_or_default())?,
        }
    }
    for v in &items {
        let p = root.join(&v.path);
        if v.kind == Kind::Symlink && !v.symlink_blocked {
            let t = v.symlink_target.clone().unwrap_or_default();
            let parent = v.path.rsplit_once('/').map_or("", |(p, _)| p);
            if t.starts_with('/') || resolve_lexical(parent, &t).is_none() {
                problems.push(format!(
                    "{}: symlink target {t:?} leaves the domain",
                    v.path
                ));
                continue;
            }
        }
        // Dangling in-root links cannot be canonicalized; their lexical form was checked above.
        if let Ok(real) = std::fs::canonicalize(&p) {
            if !real.starts_with(&root) {
                problems.push(format!(
                    "{}: realpath {} is outside the domain",
                    v.path,
                    real.display()
                ));
            }
        }
    }
    Ok(problems)
}

/// Fingerprint of a directory tree outside the root (sentinel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint(pub BTreeMap<String, (u32, u64, Vec<u8>, i64)>);

/// `lstat` + content of everything below `dir` (never follows symlinks).
pub fn fingerprint(dir: &Path) -> io::Result<Fingerprint> {
    let mut out = BTreeMap::new();
    fp_walk(dir, "", &mut out)?;
    Ok(Fingerprint(out))
}

fn fp_walk(
    dir: &Path,
    rel: &str,
    out: &mut BTreeMap<String, (u32, u64, Vec<u8>, i64)>,
) -> io::Result<()> {
    for ent in std::fs::read_dir(dir)? {
        let ent = ent?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let m = std::fs::symlink_metadata(ent.path())?;
        let body = if m.file_type().is_file() {
            std::fs::read(ent.path())?
        } else if m.file_type().is_symlink() {
            std::fs::read_link(ent.path())?
                .to_string_lossy()
                .into_owned()
                .into_bytes()
        } else {
            Vec::new()
        };
        out.insert(
            child.clone(),
            (
                m.mode(),
                m.len(),
                body,
                m.mtime_nsec() + m.mtime() * 1_000_000_000,
            ),
        );
        if m.file_type().is_dir() {
            fp_walk(&ent.path(), &child, out)?;
        }
    }
    Ok(())
}

/// Differences between two fingerprints, as readable lines.
pub fn fingerprint_diff(before: &Fingerprint, after: &Fingerprint) -> Vec<String> {
    let mut v = Vec::new();
    for (k, a) in &before.0 {
        match after.0.get(k) {
            None => v.push(format!("outside-root {k}: deleted")),
            Some(b) if b != a => v.push(format!("outside-root {k}: modified")),
            _ => {}
        }
    }
    for k in after.0.keys() {
        if !before.0.contains_key(k) {
            v.push(format!("outside-root {k}: created"));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_rule_nine() {
        assert_eq!(
            symlink_view("a/b/l", "../c", None),
            SymlinkView::Exposed("a/c".into())
        );
        assert_eq!(symlink_view("a/l", "../../x", None), SymlinkView::Blocked);
        assert_eq!(
            symlink_view("l", "x/y", None),
            SymlinkView::Exposed("x/y".into())
        );
        assert_eq!(symlink_view("l", "x/../y", None), SymlinkView::Blocked);
        assert_eq!(
            symlink_view("l", "/etc/passwd", Some("/r")),
            SymlinkView::Blocked
        );
        assert_eq!(
            symlink_view("d/l", "/r/x", Some("/r")),
            SymlinkView::Exposed("x".into())
        );
        assert_eq!(
            symlink_view("d/l", "/rr/x", Some("/r")),
            SymlinkView::Blocked
        );
        assert_eq!(symlink_view("d/l", "..", None), SymlinkView::Either);
        assert_eq!(resolve_lexical("a", "../../x"), None);
    }

    fn vis(path: &str, kind: Kind, content: Option<&[u8]>) -> VisibleItem {
        VisibleItem {
            path: path.into(),
            kind,
            size: content.map_or(0, |c| c.len() as u64),
            content: content.map(<[u8]>::to_vec),
            symlink_target: None,
            symlink_blocked: false,
            enumerated: kind == Kind::Dir,
            local_only: false,
            pending: false,
            sync_error: false,
            parent_enumerated: true,
            id: None,
        }
    }

    fn file(c: &[u8]) -> VmNode {
        VmNode {
            kind: VmKind::File,
            size: c.len() as u64,
            content: Some(c.to_vec()),
            target: None,
            mode: 0o644,
        }
    }

    #[test]
    fn compare_detects_differences_and_honours_display_mapping() {
        let mut vm = VmTree::default();
        vm.nodes.insert("README".into(), file(b"a"));
        vm.nodes.insert("readme".into(), file(b"b"));
        vm.nodes.insert("x".into(), file(b"x"));
        let mut dataless = vis("x", Kind::File, None);
        dataless.size = 1;
        let ok = vec![
            vis("README", Kind::File, Some(b"a")),
            vis("readme (Unlatch 2)", Kind::File, Some(b"b")),
            dataless,
        ];
        assert!(
            compare_trees(&vm, &ok).is_empty(),
            "{:?}",
            compare_trees(&vm, &ok)
        );
        let bounced = vec![
            vis("README 2", Kind::File, Some(b"a")),
            vis("readme", Kind::File, Some(b"b")),
            vis("x", Kind::File, Some(b"stale")),
        ];
        let p = compare_trees(&vm, &bounced);
        assert!(p.iter().any(|l| l.contains("README 2")), "{p:?}");
        assert!(p.iter().any(|l| l.contains("content differs")), "{p:?}");
    }

    /// Fuzz seed 267: an edit the VM refused for good (a read-only hard link, `.cannotSynchronize`)
    /// stays on the Mac under its name with an error badge. Its name is accounted for — the VM
    /// file is not "missing on the Mac" — only its content may differ; a failed create has no
    /// VM counterpart at all.
    #[test]
    fn errored_items_account_for_their_name_only() {
        let mut vm = VmTree::default();
        vm.nodes.insert("README".into(), file(b"agent"));
        let mut errored = vis("README", Kind::File, Some(b"mac edit"));
        errored.sync_error = true;
        let mut failed_create = vis("new.txt", Kind::File, Some(b"x"));
        failed_create.sync_error = true;
        let p = compare_trees(&vm, &[errored.clone(), failed_create]);
        assert!(p.is_empty(), "{p:?}");
        // Without the error the content difference is a problem; without the item, the name is.
        errored.sync_error = false;
        let p = compare_trees(&vm, &[errored]);
        assert!(p.iter().any(|l| l.contains("content differs")), "{p:?}");
        let p = compare_trees(&vm, &[]);
        assert!(p.iter().any(|l| l.contains("missing on the Mac")), "{p:?}");
    }

    #[test]
    fn realpath_check_flags_escapes() -> io::Result<()> {
        let t = tempfile::tempdir()?;
        let mut good = vis("l", Kind::Symlink, None);
        good.symlink_target = Some("d/f".into());
        let mut bad = vis("m", Kind::Symlink, None);
        bad.symlink_target = Some("../outside".into());
        let d = vis("d", Kind::Dir, None);
        let p = materialize_and_check_realpaths(&[d, good, bad], &t.path().join("m"))?;
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].starts_with("m:"));
        Ok(())
    }
}
