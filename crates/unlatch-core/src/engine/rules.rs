//! Item presentation rules: symlink in-root rule (D12, rule 9), exec-bit rule (rule 10) and
//! capabilities (never `allowsTrashing`, D7).

use unlatch_proto::ipc::caps;
use unlatch_proto::{Kind, ACCESS_R, ACCESS_W, ACCESS_X};

/// Decide how a VM symlink is shown. `parent_depth` = depth of the link's parent below the root
/// (root = 0). `root_path` = canonical absolute root on the VM (from `ServerInfo`).
///
/// Returns `Some(target)` (possibly rewritten from absolute to relative) when the link passes the
/// in-root rule: `"../"×k` then ≥ 1 normal component (no `.`, `..`, empty), `k ≤ parent_depth`.
/// By induction such a chain can never resolve outside the root. `None` → `symlink_blocked`.
pub(crate) fn symlink_view(
    target: &str,
    parent_depth: usize,
    root_path: Option<&str>,
) -> Option<String> {
    let rel: String = if let Some(abs) = target.strip_prefix('/') {
        let root = root_path?.trim_end_matches('/');
        let root = root.strip_prefix('/')?;
        let inner = if root.is_empty() {
            abs
        } else {
            abs.strip_prefix(root)?.strip_prefix('/')?
        };
        format!("{}{}", "../".repeat(parent_depth), inner)
    } else {
        target.to_string()
    };
    let mut ups = 0usize;
    let mut rest = rel.as_str();
    while let Some(r) = rest.strip_prefix("../") {
        ups += 1;
        rest = r;
    }
    if ups > parent_depth || rest.is_empty() {
        return None;
    }
    if rest
        .split('/')
        .all(|c| !c.is_empty() && c != "." && c != "..")
    {
        Some(rel)
    } else {
        None
    }
}

/// Exec bit shown to the Mac: never for `.command`/`.tool`/`.terminal` files (Terminal runs them
/// on double-click) nor for anything below `*.app/Contents/MacOS/`. `ancestors` = names of the
/// containing directories, nearest first. Every component compares ASCII-case-insensitively: the
/// Mac volume is case-insensitive APFS, so `X.app/contents/macos/X` launches just the same.
pub(crate) fn exec_allowed<'a>(name: &str, ancestors: impl IntoIterator<Item = &'a str>) -> bool {
    let lower = name.to_ascii_lowercase();
    if [".command", ".tool", ".terminal"]
        .iter()
        .any(|e| lower.ends_with(e))
    {
        return false;
    }
    // Look for consecutive ancestors  X.app / Contents / MacOS  (nearest-first: MacOS, Contents, X.app).
    let mut prev2: Option<&str> = None;
    let mut prev1: Option<&str> = None;
    for a in ancestors {
        if prev2.is_some_and(|p| p.eq_ignore_ascii_case("MacOS"))
            && prev1.is_some_and(|p| p.eq_ignore_ascii_case("Contents"))
            && is_app_name(a)
        {
            return false;
        }
        prev2 = prev1;
        prev1 = Some(a);
    }
    true
}

fn is_app_name(name: &str) -> bool {
    // Byte-wise: a multi-byte char straddling the cut must not panic.
    let b = name.as_bytes();
    b.len() >= 4 && b[b.len() - 4..].eq_ignore_ascii_case(b".app")
}

/// Can renaming a directory from/to `name` change any descendant's [`exec_allowed`] verdict?
/// Only names that can take part in the `X.app/Contents/MacOS` pattern can.
pub(crate) fn exec_relevant_dir_name(name: &str) -> bool {
    is_app_name(name) || name.eq_ignore_ascii_case("Contents") || name.eq_ignore_ascii_case("MacOS")
}

/// Capabilities from the daemon's access bits. `parent_access` = access of the containing
/// directory (`None` = unknown → assume writable). Unlatch never sets allowsTrashing.
pub(crate) fn capabilities(
    kind: Kind,
    access: u8,
    blocked: bool,
    is_root: bool,
    parent_access: Option<u8>,
) -> u32 {
    let mut c = 0;
    match kind {
        Kind::Dir => {
            if access & (ACCESS_R | ACCESS_X) != 0 {
                c |= caps::CONTENT_ENUMERATING;
            }
            if access & ACCESS_W != 0 && access & ACCESS_X != 0 {
                c |= caps::ADDING_SUB_ITEMS;
            }
        }
        Kind::File | Kind::Symlink => {
            if access & ACCESS_R != 0 || blocked || kind == Kind::Symlink {
                c |= caps::READING;
            }
            if access & ACCESS_W != 0 && !blocked && kind == Kind::File {
                c |= caps::WRITING;
            }
        }
    }
    if !is_root && parent_access.is_none_or(|p| p & ACCESS_W != 0) {
        c |= caps::RENAMING | caps::REPARENTING | caps::DELETING;
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_rule() {
        // link at depth-2 parent (root/a/b/link)
        assert_eq!(symlink_view("c.txt", 2, None).as_deref(), Some("c.txt"));
        assert_eq!(symlink_view("../x/y", 2, None).as_deref(), Some("../x/y"));
        assert_eq!(symlink_view("../../x", 2, None).as_deref(), Some("../../x"));
        assert_eq!(symlink_view("../../../x", 2, None), None); // escapes
        assert_eq!(symlink_view("x/../../..", 2, None), None); // '..' after descent
        assert_eq!(symlink_view("./x", 2, None), None);
        assert_eq!(symlink_view("x//y", 2, None), None);
        assert_eq!(symlink_view("x/", 2, None), None);
        assert_eq!(symlink_view("..", 2, None), None);
        assert_eq!(symlink_view("../..", 2, None), None);
        assert_eq!(symlink_view("", 0, None), None);
        // root-level link: no ups allowed
        assert_eq!(symlink_view("../x", 0, None), None);
        // absolute targets
        assert_eq!(symlink_view("/etc/passwd", 1, Some("/home/u/p")), None);
        assert_eq!(
            symlink_view("/home/u/p/src/a.rs", 1, Some("/home/u/p")).as_deref(),
            Some("../src/a.rs")
        );
        assert_eq!(
            symlink_view("/home/u/p/src/a.rs", 0, Some("/home/u/p/")).as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(symlink_view("/home/u/px/a", 0, Some("/home/u/p")), None);
        assert_eq!(symlink_view("/home/u/p", 0, Some("/home/u/p")), None);
        assert_eq!(
            symlink_view("/home/u/p/../../etc", 0, Some("/home/u/p")),
            None
        );
        assert_eq!(symlink_view("/x", 0, None), None);
    }

    #[test]
    fn exec_rule() {
        assert!(exec_allowed("run.sh", ["bin", "proj"]));
        assert!(!exec_allowed("evil.command", ["bin"]));
        assert!(!exec_allowed("x.TOOL", []));
        assert!(!exec_allowed("x.terminal", []));
        assert!(!exec_allowed(
            "Foo",
            ["MacOS", "Contents", "Foo.app", "dist"]
        ));
        assert!(!exec_allowed(
            "helper",
            ["sub", "MacOS", "Contents", "Foo.app"]
        ));
        assert!(exec_allowed("Foo", ["MacOS", "Contents", "Foo"]));
        assert!(exec_allowed("Foo", ["MacOS", "Stuff", "Foo.app"]));
    }

    #[test]
    fn exec_rule_bundle_path_is_case_insensitive() {
        // The Mac volume is case-insensitive APFS: Evil.app/contents/macos/Evil still launches.
        assert!(!exec_allowed("Evil", ["macos", "contents", "Evil.app"]));
        assert!(!exec_allowed("Evil", ["MACOS", "Contents", "Evil.APP"]));
        assert!(!exec_allowed(
            "Evil",
            ["MacOS", "CONTENTS", "evil.app", "dist"]
        ));
        assert!(exec_allowed("Evil", ["macos", "stuff", "Evil.app"]));
        assert!(exec_allowed("Evil", ["macos", "contents", "a€"]));
    }

    #[test]
    fn caps_never_trash() {
        let rw = ACCESS_R | ACCESS_W | ACCESS_X;
        let c = capabilities(Kind::File, rw, false, false, Some(rw));
        assert_eq!(c & (1 << 4), 0);
        assert_ne!(c & caps::WRITING, 0);
        assert_ne!(c & caps::DELETING, 0);
        let c = capabilities(Kind::File, rw, true, false, Some(rw));
        assert_eq!(c & caps::WRITING, 0);
        assert_ne!(c & caps::READING, 0);
        let c = capabilities(
            Kind::File,
            ACCESS_R,
            false,
            false,
            Some(ACCESS_R | ACCESS_X),
        );
        assert_eq!(c & (caps::WRITING | caps::DELETING | caps::RENAMING), 0);
        let c = capabilities(Kind::Dir, rw, false, true, None);
        assert_eq!(c & (caps::DELETING | caps::RENAMING | caps::REPARENTING), 0);
        assert_ne!(c & caps::ADDING_SUB_ITEMS, 0);
    }
}
