//! Real ssh inside the shaped link (profile suffix `-ssh`).
//!
//! The raw bridge (`unlatch-bench netlab connect`) carries plain bytes, so neither Unlatch nor sshfs
//! pays for ssh: crypto, per-packet MACs, and OpenSSH's 2 MiB channel window. `-ssh` profiles
//! put a real `ssh` ⇄ `sshd` pair around the shaped TCP connection, which is where the network
//! sits in a deployment:
//!
//! ```text
//!  engine / sshfs ── ssh -F <cfg> bench <cmd> ── ProxyCommand: netlab connect <sock> <svc>
//!        ── shaped TCP (netns) ──►  service <svc>: /usr/sbin/sshd -i -e -f <sshd_config>
//!        ──►  <cmd> (unlatchd stdio …, sftp-server, cat …) as the same user
//! ```
//!
//! `sshd` runs in inetd mode (`-i`), one per connection, spawned by the netlab
//! [`ServiceHost`](crate::netlab::ServiceHost) *outside* the namespace — inside it our uid maps
//! to 0 and sshd would try root-only privilege separation. Unprivileged sshd can only log in as
//! its own user, which is all the bench needs. Keys are generated per run (ed25519); the client
//! uses OpenSSH's defaults otherwise (cipher chacha20-poly1305, `Compression no` as Unlatch
//! configures it).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Per-profile ssh material (serialized into the scenario context).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshLab {
    pub dir: PathBuf,
    pub sshd: PathBuf,
    pub ssh: PathBuf,
    pub sshd_config: PathBuf,
    pub client_key: PathBuf,
    pub user: String,
}

fn find(cands: &[&str]) -> Option<PathBuf> {
    cands.iter().map(PathBuf::from).find(|p| p.is_file())
}

fn keygen(path: &Path) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("pub"));
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "unlatch-bench", "-f"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .context("spawn ssh-keygen")?;
    if !out.status.success() {
        bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn user_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| {
            Command::new("id")
                .arg("-un")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "root".into())
}

impl SshLab {
    /// Generate host and client keys and an sshd config under `dir`.
    pub fn setup(dir: &Path, sftp_server: Option<&Path>) -> Result<SshLab> {
        let sshd = find(&["/usr/sbin/sshd", "/usr/local/sbin/sshd", "/sbin/sshd"])
            .context("sshd not found (openssh-server)")?;
        let ssh = find(&["/usr/bin/ssh", "/usr/local/bin/ssh", "/bin/ssh"])
            .context("ssh not found (openssh-client)")?;
        std::fs::create_dir_all(dir)?;
        let host_key = dir.join("host_ed25519");
        let client_key = dir.join("client_ed25519");
        keygen(&host_key)?;
        keygen(&client_key)?;
        let auth = dir.join("authorized_keys");
        std::fs::copy(client_key.with_extension("pub"), &auth)?;
        let sftp = sftp_server
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "internal-sftp".into());
        let cfg = dir.join("sshd_config");
        std::fs::write(
            &cfg,
            format!(
                "HostKey {}\nAuthorizedKeysFile {}\nPubkeyAuthentication yes\n\
                 PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\n\
                 StrictModes no\nPidFile none\nLogLevel ERROR\nPrintMotd no\n\
                 Subsystem sftp {sftp}\n",
                host_key.display(),
                auth.display(),
            ),
        )?;
        Ok(SshLab {
            dir: dir.to_path_buf(),
            sshd,
            ssh,
            sshd_config: cfg,
            client_key,
            user: user_name(),
        })
    }

    /// argv of the per-connection sshd service.
    pub fn sshd_argv(&self) -> Vec<String> {
        vec![
            self.sshd.display().to_string(),
            "-i".into(),
            "-e".into(),
            "-f".into(),
            self.sshd_config.display().to_string(),
        ]
    }

    /// Write an ssh client config for host `bench` reaching `proxy_argv` (the netlab bridge to
    /// the sshd service); returns its path.
    pub fn client_config(&self, name: &str, proxy_argv: &[String]) -> Result<PathBuf> {
        let p = self.dir.join(format!("ssh_config-{name}"));
        std::fs::write(
            &p,
            format!(
                "Host bench\n  HostName bench\n  User {}\n  ProxyCommand {}\n  IdentityFile {}\n\
                 \x20 IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n\
                 \x20 GlobalKnownHostsFile /dev/null\n  BatchMode yes\n  Compression no\n\
                 \x20 ControlMaster no\n  ControlPath none\n  LogLevel ERROR\n",
                self.user,
                proxy_argv.join(" "),
                self.client_key.display()
            ),
        )?;
        Ok(p)
    }
}

/// Quote `argv` for a POSIX shell (the remote command line of `ssh host <cmd>`).
pub fn shell_quote(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty()
                && a.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@+%".contains(&b))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        let q = shell_quote(&[
            "/a b/unlatchd".into(),
            "stdio".into(),
            "--root".into(),
            "it's".into(),
            "".into(),
        ]);
        assert_eq!(q, r"'/a b/unlatchd' stdio --root 'it'\''s' ''");
    }
}
