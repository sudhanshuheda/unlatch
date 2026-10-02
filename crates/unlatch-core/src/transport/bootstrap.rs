//! The POSIX-sh bootstrap script run as `ssh … sh -s` (review (d)11, D21, D22).
//!
//! The remote command is exactly `sh -s`, which every login shell (bash, zsh, fish, csh) parses
//! the same way. The script arrives on stdin; the binary, when needed, follows on the *same*
//! stdin. POSIX requires `sh` not to read ahead of a command that consumes stdin, and the engine
//! only sends the binary after the script asks for it and sends nothing more until the next
//! marker, so neither `sh` nor `head -c` can swallow bytes meant for someone else.
//!
//! Markers are `UNLATCH-<nonce>-<WORD> …\n` lines on stdout; the per-connection nonce keeps a motd
//! or `.bashrc` echo from ever being mistaken for one.

use crate::{err, EngineConfig, Result};
use std::path::PathBuf;
use unlatch_proto::ErrorCode;

/// One binary the script may request.
#[derive(Clone, Debug)]
pub(crate) struct Upload {
    pub arch: String,
    pub sha256: String,
    pub size: u64,
    pub path: PathBuf,
}

impl Upload {
    /// File name on the VM: version *and* hash, so two Macs with different builds of the same
    /// version never ping-pong one path, and a corrupt file simply fails verification.
    pub(crate) fn remote_name(&self) -> String {
        format!(
            "unlatchd-{}-{}",
            env!("CARGO_PKG_VERSION"),
            &self.sha256[..16]
        )
    }
}

/// Validate the configured uploads and stat their sizes.
pub(crate) fn uploads(cfg: &EngineConfig) -> Result<Vec<Upload>> {
    cfg.unlatchd_upload
        .iter()
        .map(|b| {
            let sha = b.sha256_hex.to_ascii_lowercase();
            if sha.len() != 64 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err(err(
                    ErrorCode::Protocol,
                    format!("invalid sha256 for unlatchd {}: {:?}", b.arch, b.sha256_hex),
                ));
            }
            if b.arch.is_empty()
                || !b
                    .arch
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                return Err(err(
                    ErrorCode::Protocol,
                    format!("invalid unlatchd architecture {:?}", b.arch),
                ));
            }
            let size = std::fs::metadata(&b.path)
                .map_err(|e| {
                    err(
                        ErrorCode::Io,
                        format!("unlatchd binary {}: {e}", b.path.display()),
                    )
                })?
                .len();
            Ok(Upload {
                arch: b.arch.clone(),
                sha256: sha,
                size,
                path: b.path.clone(),
            })
        })
        .collect()
}

/// Single-quote `s` for POSIX sh.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Shell expression for the remote root: `~` and `~/…` expand via `$HOME` (never `getpwuid`,
/// which a static musl binary cannot do on SSSD/LDAP hosts).
pub(crate) fn root_expr(root: &str) -> String {
    if root == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = root.strip_prefix("~/") {
        format!("\"$HOME\"/{}", sh_quote(rest))
    } else {
        sh_quote(root)
    }
}

/// Script for a user-supplied `unlatchd_command` (no probe, no upload). The command is inserted
/// verbatim as shell words after `exec` (so `~/bin/unlatchd` works; environment assignments need
/// `env VAR=value …`).
pub(crate) fn override_script(nonce: &str, command: &str, root: &str) -> String {
    format!(
        "{{\nprintf 'UNLATCH-%s-EXEC\\n' {n}\nexec {command} connect --root {root}\n}}\n",
        n = sh_quote(nonce),
        root = root_expr(root)
    )
}

/// Full probe/verify/upload/exec script.
pub(crate) fn script(
    nonce: &str,
    uploads: &[Upload],
    root: &str,
    remote_home: Option<&str>,
) -> String {
    let mut table = String::new();
    for u in uploads {
        table.push_str(&format!(
            "  {arch}) want_sha={sha}; want_size={size}; want_name={name} ;;\n",
            arch = u.arch,
            sha = u.sha256,
            size = u.size,
            name = sh_quote(&u.remote_name()),
        ));
    }
    // The whole script is one brace group: the shell must read all of it before running any of
    // it. dash and busybox sh *do* read ahead on pipes (measured), so without this the tail of
    // the script could still be in the pipe when `head -c` starts reading the binary.
    let mut s = String::with_capacity(4096);
    s.push_str("{\n");
    s.push_str(&format!("N={}\n", sh_quote(nonce)));
    s.push_str(&format!(
        "H_OVERRIDE={}\n",
        sh_quote(remote_home.unwrap_or(""))
    ));
    s.push_str(&format!("ROOT_ARG={}\n", root_expr(root)));
    s.push_str(
        r#"m() { printf 'UNLATCH-%s-%s\n' "$N" "$*"; }
die() { m "ERR $1 $2"; exit 1; }
umask 077
arch=$(uname -m 2>/dev/null) || arch=unknown
case "$arch" in amd64|x86_64) arch=x86_64 ;; arm64|aarch64) arch=aarch64 ;; esac
want_sha=; want_size=; want_name=
case "$arch" in
"#,
    );
    s.push_str(&table);
    s.push_str(
        r#"  *) ;;
esac
hash_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d ' ' -f 1
  elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | sed 's/^.*= *//'
  else echo none; fi
}
# Network/virtual filesystems: locks, exec and inotify are unreliable there (D22).
remote_fs() {
  t=$(stat -f -c %T "$1" 2>/dev/null) || return 1
  case "$t" in nfs*|cifs|smb*|fuse*|9p|v9fs|afs|virtiofs|ceph|glusterfs|lustre|gpfs) return 0 ;; esac
  return 1
}
usable() {
  d=$1
  [ -n "$d" ] || return 1
  case "$d" in /*) ;; *) return 1 ;; esac
  if [ ! -e "$d" ] && [ ! -L "$d" ]; then mkdir -p "$d" 2>/dev/null || return 1; fi
  [ -d "$d" ] && [ ! -L "$d" ] && [ -O "$d" ] || return 1
  chmod 700 "$d" 2>/dev/null || return 1
  case "$(ls -ld "$d" 2>/dev/null)" in 'drwx------ '*|'drwx------.'*) ;; *) return 1 ;; esac
  if remote_fs "$d"; then return 1; fi
  t="$d/.unlatch-exec-test.$$"
  printf '#!/bin/sh\nexit 0\n' > "$t" 2>/dev/null && chmod 700 "$t" 2>/dev/null && "$t" 2>/dev/null
  r=$?
  rm -f "$t"
  return $r
}
uid=$(id -u 2>/dev/null) || uid=unknown
dir=
for c in "$H_OVERRIDE" "${UNLATCH_HOME:-}" "${XDG_DATA_HOME:+$XDG_DATA_HOME/unlatch}" "${HOME:+$HOME/.unlatch}" "/var/tmp/unlatch-$uid" "/tmp/unlatch-$uid"; do
  if usable "$c"; then dir=$c; break; fi
done
[ -n "$dir" ] || die fatal "no usable install directory on the VM (needs a local, exec-capable directory owned by you, mode 0700)"
if [ -n "$want_sha" ]; then
  [ "$(hash_of /dev/null)" != none ] || die fatal "no sha256 tool on the VM (sha256sum, shasum or openssl)"
  bin="$dir/$want_name"
  have=
  if [ -f "$bin" ] && [ ! -L "$bin" ]; then have=$(hash_of "$bin"); fi
  if [ "$have" != "$want_sha" ]; then
    rm -f "$bin"
    tmp="$bin.$$.tmp"
    m "NEED $arch"
    head -c "$want_size" > "$tmp" || { rm -f "$tmp"; die retry "upload interrupted"; }
    got=$(hash_of "$tmp")
    if [ "$got" != "$want_sha" ]; then rm -f "$tmp"; die retry "uploaded unlatchd failed verification ($got)"; fi
    chmod 700 "$tmp" || { rm -f "$tmp"; die retry "cannot chmod $tmp"; }
    sync "$tmp" 2>/dev/null || sync
    mv -f "$tmp" "$bin" || { rm -f "$tmp"; die retry "cannot install $bin"; }
    sync "$dir" 2>/dev/null || true
    [ "$(hash_of "$bin")" = "$want_sha" ] || die retry "installed unlatchd failed verification"
  fi
elif command -v unlatchd >/dev/null 2>&1; then
  bin=$(command -v unlatchd)
else
  die fatal "no unlatchd for VM architecture $arch"
fi
UNLATCH_HOME=$dir
export UNLATCH_HOME
m EXEC
exec "$bin" connect --root "$ROOT_ARG"
}
"#,
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_install_dir_is_tried_first() {
        let s = script("n0", &[], "/r", Some("/opt/it's unlatch"));
        assert!(s.contains(&format!("H_OVERRIDE={}\n", sh_quote("/opt/it's unlatch"))));
        // Probe order: the override first, then the standard candidates.
        let probe = s.find("for c in \"$H_OVERRIDE\"").expect("probe loop");
        assert!(s[probe..].find("UNLATCH_HOME").is_some());
        let none = script("n0", &[], "/r", None);
        assert!(none.contains("H_OVERRIDE=''\n"), "{none}");
    }
}
