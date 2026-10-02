//! File names as the Mac sees them: APFS-style folding, Finder/fileproviderd renames, and the
//! engine's documented display-name mapping for case/normalization collisions (D16, engine
//! rule 11).

use std::collections::{BTreeMap, HashMap};
use unicode_normalization::UnicodeNormalization;

/// Comparison key of a name on a case- and normalization-insensitive volume (APFS default):
/// `casefold(NFD(name))`. Lowercasing can recompose characters, hence the second NFD pass.
pub fn fold(name: &str) -> String {
    let nfd: String = name.nfd().collect();
    nfd.to_lowercase().nfd().collect()
}

/// `true` when two names denote the same directory entry on APFS.
pub fn same_name(a: &str, b: &str) -> bool {
    a == b || fold(a) == fold(b)
}

/// Split `name` into stem and extension (including the dot). Dotfiles have no extension.
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(i) => (&name[..i], &name[i..]),
    }
}

/// The name fileproviderd gives the *older* item when a server item collides with it only by
/// case/normalization (MQ-016): `stem N.ext`, N ≥ 2.
pub fn bounce_name(name: &str, n: u32) -> String {
    let (stem, ext) = split_ext(name);
    format!("{stem} {n}{ext}")
}

/// Finder's own collision resolution for drags/duplicates inside the mount (MQ-015):
/// `stem copy.ext`, then `stem copy 2.ext`, …
pub fn finder_copy_name(name: &str, n: u32) -> String {
    let (stem, ext) = split_ext(name);
    if n <= 1 {
        format!("{stem} copy{ext}")
    } else {
        format!("{stem} copy {n}{ext}")
    }
}

/// Display name the engine gives the N-th (N ≥ 2) member of a collision group (rule 11).
pub fn unlatch_display_name(real: &str, n: u32) -> String {
    let (stem, ext) = split_ext(real);
    format!("{stem} (Unlatch {n}){ext}")
}

/// Inverse of [`unlatch_display_name`]: `Some(real)` when `display` has the ` (Unlatch N)` marker.
pub fn strip_unlatch_marker(display: &str) -> Option<String> {
    let (stem, ext) = split_ext(display);
    let open = stem.rfind(" (Unlatch ")?;
    let tail = &stem[open + " (Unlatch ".len()..];
    let digits = tail.strip_suffix(')')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}{ext}", &stem[..open]))
}

/// Rule 11 applied to one directory listing: real name → display name. Within each group of
/// names that fold equal, the first in byte order keeps its name; the others are numbered from 2
/// in byte order.
pub fn display_names<'a>(names: impl IntoIterator<Item = &'a str>) -> HashMap<String, String> {
    let mut groups: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for n in names {
        groups.entry(fold(n)).or_default().push(n);
    }
    let mut out = HashMap::new();
    for (_, mut members) in groups {
        members.sort_unstable();
        for (i, m) in members.iter().enumerate() {
            let shown = if i == 0 {
                m.to_string()
            } else {
                unlatch_display_name(m, i as u32 + 1)
            };
            out.insert(m.to_string(), shown);
        }
    }
    out
}

/// Conflict copies are named `stem (conflict from <client> <date>).ext` by the daemon. The
/// date contains a dot (`hh.mm`), so the marker is located directly, never via the extension.
pub fn is_conflict_copy(name: &str) -> bool {
    name.contains(" (conflict from ")
}

/// Base name of a conflict copy (`a (conflict from mac 2026-09-30 12.00).txt` → `a.txt`,
/// `x (conflict from mac 2026-09-30 12.00)` → `x`).
pub fn conflict_base(name: &str) -> Option<String> {
    // The last marker: a conflict copy of a conflict copy is based on the inner copy.
    let i = name.rfind(" (conflict from ")?;
    let close = name[i..].find(')')? + i;
    Some(format!("{}{}", &name[..i], &name[close + 1..]))
}

/// Engine collision renames for creates (rule 2): `stem N.ext`. Returns the base name.
pub fn numbered_base(name: &str) -> Option<String> {
    let (stem, ext) = split_ext(name);
    let (base, n) = stem.rsplit_once(' ')?;
    if base.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{base}{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_is_case_and_normalization_insensitive() {
        assert!(same_name("README", "readme"));
        assert!(same_name("caf\u{e9}", "cafe\u{301}"));
        assert!(same_name("CAF\u{c9}", "cafe\u{301}"));
        assert!(!same_name("a", "b"));
    }

    #[test]
    fn ext_split() {
        assert_eq!(split_ext("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(split_ext(".bashrc"), (".bashrc", ""));
        assert_eq!(split_ext("Makefile"), ("Makefile", ""));
        assert_eq!(bounce_name("README-renamed.txt", 2), "README-renamed 2.txt");
        assert_eq!(finder_copy_name("run.sh", 1), "run copy.sh");
        assert_eq!(finder_copy_name("run.sh", 3), "run copy 3.sh");
    }

    #[test]
    fn rule11_mapping_roundtrips() {
        let m = display_names(["readme.md", "README.md", "other", "Readme.md"]);
        assert_eq!(m["README.md"], "README.md");
        assert_eq!(m["Readme.md"], "Readme (Unlatch 2).md");
        assert_eq!(m["readme.md"], "readme (Unlatch 3).md");
        assert_eq!(m["other"], "other");
        for (real, shown) in &m {
            if real != shown {
                assert_eq!(strip_unlatch_marker(shown).as_deref(), Some(real.as_str()));
            }
        }
        assert_eq!(strip_unlatch_marker("x (Unlatch).md"), None);
        assert_eq!(strip_unlatch_marker("plain.md"), None);
    }

    #[test]
    fn engine_generated_names() {
        assert!(is_conflict_copy(
            "a (conflict from mac 2026-09-30 12.00).txt"
        ));
        assert_eq!(
            conflict_base("a (conflict from mac 2026-09-30 12.00).txt").as_deref(),
            Some("a.txt")
        );
        assert_eq!(
            conflict_base("x (conflict from mac 2026-09-30 12.00)").as_deref(),
            Some("x")
        );
        assert_eq!(numbered_base("a 2.txt").as_deref(), Some("a.txt"));
        assert_eq!(numbered_base("a.txt"), None);
    }
}
