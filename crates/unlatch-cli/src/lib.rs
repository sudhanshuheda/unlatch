//! The `unlatch` command-line tool, as a library so its pieces are testable.
//!
//! * [`backend`] — the slice of the engine API the FUSE frontend uses (a trait, so the FUSE
//!   layer can be tested against an in-memory backend while the engine is being built).
//! * [`inode`] — ItemId ↔ FUSE inode mapping with kernel lookup counts.
//! * `fuse` (Linux) — the FUSE frontend behind `unlatch mount`.
//! * [`config`] — `unlatch agent` configuration file.
//! * [`pathres`] — path → item resolution over IPC for `unlatch ls|stat|cat`.
//! * [`doctor`], [`probe`] — diagnostics and the cross-platform verification probe.
//! * [`stats`] — percentile helpers shared by the probe.

pub mod agent;
pub mod backend;
pub mod config;
pub mod doctor;
pub mod errno;
#[cfg(target_os = "linux")]
pub mod fuse;
pub mod inode;
pub mod ipccmd;
pub mod mount;
pub mod pathres;
pub mod probe;
pub mod remote;
pub mod signals;
pub mod stats;
pub mod timefmt;

/// This machine's name, sanitized for conflict-copy file names (D19: supplied by the host).
///
/// Uses `gethostname(2)`, which works on both Linux and macOS (unlike reading `/etc/hostname`,
/// which does not exist on macOS — the bug D19 calls out).
pub fn machine_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes; gethostname NUL-terminates on success when the
    // name fits, and we bound the scan by the buffer length anyway.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    // Drop the domain part: conflict names should read "from dev-laptop", not the FQDN.
    name.split('.').next().unwrap_or_default().to_string()
}

/// `$HOME`, falling back to `/` (never `getpwuid`: D22 — static binaries cannot use NSS).
pub fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/".into())
}

/// Expand a leading `~/` (or a lone `~`) with `$HOME`.
pub fn expand_tilde(p: &str) -> std::path::PathBuf {
    if p == "~" {
        home_dir()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        std::path::PathBuf::from(p)
    }
}

/// Short stable hex digest (FNV-1a 64) used to derive default per-root state directories.
pub fn short_hash(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.bytes().chain(std::iter::once(0u8)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let home = home_dir();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/a/b"), home.join("a/b"));
        assert_eq!(
            expand_tilde("/abs/~/x"),
            std::path::PathBuf::from("/abs/~/x")
        );
        assert_eq!(expand_tilde("rel"), std::path::PathBuf::from("rel"));
    }

    #[test]
    fn short_hash_is_stable_and_separates_parts() {
        assert_eq!(short_hash(&["a", "b"]), short_hash(&["a", "b"]));
        assert_ne!(short_hash(&["ab", ""]), short_hash(&["a", "b"]));
        assert_eq!(short_hash(&["x"]).len(), 16);
    }

    #[test]
    fn machine_name_has_no_domain() {
        assert!(!machine_name().contains('.'));
    }
}
