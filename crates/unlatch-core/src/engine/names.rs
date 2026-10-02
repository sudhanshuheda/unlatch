//! Display names: case/normalization collision mapping (D16, rule 11), "name 2.ext" candidates
//! for creates, and recognition of the system's own "bounce" renames (MQ-016).

use unicode_normalization::UnicodeNormalization;

/// Collision key: `casefold(NFD(name))`, renormalized (folding can denormalize).
pub(crate) fn fold_key(name: &str) -> String {
    if name.is_ascii() {
        return name.to_ascii_lowercase();
    }
    let nfd: String = name.nfd().collect();
    caseless::default_case_fold_str(&nfd).nfd().collect()
}

/// Split `name` into `(stem, ext)` where `ext` includes the dot. Dotfiles and names without a
/// dot (or ending in one) have no extension.
pub(crate) fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Truncate `s` to at most `max` bytes on a char boundary.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Join a stem, a suffix and an extension, keeping the result ≤ 255 bytes by shortening the stem.
fn join_fit(stem: &str, suffix: &str, ext: &str) -> String {
    let budget = 255usize.saturating_sub(suffix.len() + ext.len());
    format!("{}{}{}", truncate(stem, budget), suffix, ext)
}

/// `name 2.ext`, `name 3.ext`, … (the Finder convention; create/rename collision fallback).
pub(crate) fn numbered(name: &str, n: u32) -> String {
    if n <= 1 {
        return name.to_string();
    }
    let (stem, ext) = split_ext(name);
    join_fit(stem, &format!(" {n}"), ext)
}

/// `stem (Unlatch N).ext` — display name of the N-th member of a collision group.
pub(crate) fn unlatch_numbered(name: &str, n: u32) -> String {
    let (stem, ext) = split_ext(name);
    join_fit(stem, &format!(" (Unlatch {n})"), ext)
}

/// Does `candidate` look like the system's local "bounce" of `orig` (`stem N.ext`, N ≥ 2)?
/// fileproviderd renames the losing item of a local collision this way (MQ-016).
pub(crate) fn is_bounce_of(orig: &str, candidate: &str) -> bool {
    let (stem, ext) = split_ext(orig);
    let Some(rest) = candidate.strip_prefix(stem) else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(ext) else {
        return false;
    };
    let Some(digits) = rest.strip_prefix(' ') else {
        return false;
    };
    !digits.is_empty()
        && digits.len() <= 4
        && digits.bytes().all(|b| b.is_ascii_digit())
        && digits.parse::<u32>().is_ok_and(|n| n >= 2)
}

/// Assign display names within one collision group.
///
/// `members` are the real names (all with the same [`fold_key`]); the first in byte order keeps
/// its name, the others become `stem (Unlatch N).ext` with the smallest N ≥ 2 whose key is not
/// `taken` (by another real name in the directory) and not already assigned. Returns displays in
/// the order of `members`.
pub(crate) fn assign_group(members: &[&str], taken: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut order: Vec<usize> = (0..members.len()).collect();
    order.sort_by(|&a, &b| members[a].as_bytes().cmp(members[b].as_bytes()));
    let mut out = vec![String::new(); members.len()];
    let mut used: Vec<String> = Vec::new();
    let mut n = 2u32;
    for (rank, &i) in order.iter().enumerate() {
        if rank == 0 {
            out[i] = members[i].to_string();
            used.push(fold_key(members[i]));
            continue;
        }
        loop {
            let cand = unlatch_numbered(members[i], n);
            n += 1;
            let key = fold_key(&cand);
            if !taken(&key) && !used.contains(&key) {
                used.push(key);
                out[i] = cand;
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_keys() {
        assert_eq!(fold_key("Foo.TXT"), fold_key("foo.txt"));
        // NFC "é" vs NFD "e\u{301}"
        assert_eq!(fold_key("caf\u{e9}"), fold_key("cafe\u{301}"));
        assert_eq!(fold_key("CAF\u{c9}"), fold_key("cafe\u{301}"));
        assert_eq!(fold_key("Stra\u{df}e"), fold_key("STRASSE"));
        assert_ne!(fold_key("a"), fold_key("b"));
    }

    #[test]
    fn ext_split_and_numbering() {
        assert_eq!(split_ext("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(split_ext(".bashrc"), (".bashrc", ""));
        assert_eq!(split_ext("Makefile"), ("Makefile", ""));
        assert_eq!(split_ext("x."), ("x.", ""));
        assert_eq!(numbered("notes.txt", 2), "notes 2.txt");
        assert_eq!(numbered("notes.txt", 1), "notes.txt");
        assert_eq!(numbered("dir", 3), "dir 3");
        assert_eq!(unlatch_numbered("Foo.txt", 2), "Foo (Unlatch 2).txt");
        let long = "é".repeat(200); // 400 bytes
        let n = numbered(&format!("{long}.txt"), 12);
        assert!(n.len() <= 255 && n.ends_with(" 12.txt"));
    }

    #[test]
    fn bounce_detection() {
        assert!(is_bounce_of("Foo.txt", "Foo 2.txt"));
        assert!(is_bounce_of("Foo (Unlatch 2).txt", "Foo (Unlatch 2) 3.txt"));
        assert!(is_bounce_of("dir", "dir 2"));
        assert!(!is_bounce_of("Foo.txt", "Foo 1.txt"));
        assert!(!is_bounce_of("Foo.txt", "Foo copy.txt"));
        assert!(!is_bounce_of("Foo.txt", "Bar 2.txt"));
        assert!(!is_bounce_of("Foo.txt", "Foo 2.md"));
    }

    #[test]
    fn group_assignment_first_in_byte_order_keeps_name() {
        let none = |_: &str| false;
        // "FOO" < "Foo" < "foo" in byte order.
        let d = assign_group(&["foo", "FOO", "Foo"], &none);
        assert_eq!(d, vec!["foo (Unlatch 3)", "FOO", "Foo (Unlatch 2)"]);
        // NFC/NFD pair: NFD ("e" + U+0301) sorts before NFC U+00E9.
        let d = assign_group(&["caf\u{e9}.txt", "cafe\u{301}.txt"], &none);
        assert_eq!(d[1], "cafe\u{301}.txt");
        assert_eq!(d[0], "caf\u{e9} (Unlatch 2).txt");
        // A generated name that would collide with a real name is skipped.
        let taken = |k: &str| k == fold_key("a (Unlatch 2).txt");
        let d = assign_group(&["A.txt", "a.txt"], &taken);
        assert_eq!(d, vec!["A.txt", "a (Unlatch 3).txt"]);
    }
}
