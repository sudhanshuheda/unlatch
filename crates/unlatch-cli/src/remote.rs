//! A persistent non-interactive `ssh … sh` session for `doctor` and `probe`.
//!
//! One ssh connection serves every command (no per-command handshake skewing latencies). Each
//! command is followed by a unique end marker carrying its exit status; a reader thread turns
//! stdout into lines so waits can time out.

use anyhow::{anyhow, bail, Context};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Single-quote `s` for POSIX sh.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A remote path as a shell word. `~` / `~/rest` expand via `$HOME` on the remote side (the
/// VM's home, never a local one); everything else is single-quoted.
pub fn remote_path(path: &str) -> String {
    if path == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("\"$HOME\"/{}", sq(rest))
    } else {
        sq(path)
    }
}

/// ssh argv for a non-interactive session (BatchMode: never prompt, fail fast instead).
pub fn ssh_command(dest: &str, extra: &[String], remote_cmd: &str) -> anyhow::Result<Command> {
    if dest.is_empty() || dest.starts_with('-') {
        bail!("invalid ssh destination {dest:?}");
    }
    let mut c = Command::new("ssh");
    c.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
    ]);
    c.args(extra);
    c.arg("--").arg(dest).arg(remote_cmd);
    Ok(c)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOut {
    pub stdout: String,
    pub status: i32,
}

pub struct RemoteShell {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    seq: u64,
    nonce: u64,
}

impl RemoteShell {
    pub fn connect(dest: &str, extra: &[String]) -> anyhow::Result<RemoteShell> {
        let mut cmd = ssh_command(dest, extra, "exec sh")?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .context("spawning ssh (is it installed and on PATH?)")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("ssh stdin missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ssh stdout missing"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("ssh stderr missing"))?;
        let (tx, lines) = channel();
        std::thread::Builder::new()
            .name("ssh-stdout".into())
            .spawn(move || {
                let mut r = BufReader::new(stdout);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match r.read_until(b'\n', &mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {
                            if buf.last() == Some(&b'\n') {
                                buf.pop();
                            }
                            if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                                return;
                            }
                        }
                    }
                }
            })?;
        let err_buf = Arc::new(Mutex::new(String::new()));
        let err_sink = Arc::clone(&err_buf);
        std::thread::Builder::new()
            .name("ssh-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut g = err_sink.lock().unwrap_or_else(|p| p.into_inner());
                    if g.len() < 64 * 1024 {
                        g.push_str(&line);
                        g.push('\n');
                    }
                }
            })?;
        let mut sh = RemoteShell {
            child,
            stdin,
            lines,
            stderr: err_buf,
            seq: 0,
            nonce: rand::random(),
        };
        // The first round trip proves the session works (auth, host key, remote shell).
        if let Err(e) = sh.run(":", Duration::from_secs(30)) {
            // Give the stderr reader a moment to collect ssh's reason.
            std::thread::sleep(Duration::from_millis(100));
            let msg = format!("{e:#}");
            if msg.contains("ssh said:") {
                return Err(anyhow!(msg));
            }
            return Err(anyhow!("{msg}{}", sh.stderr_suffix()));
        }
        Ok(sh)
    }

    /// Everything ssh / the remote shell printed on stderr so far.
    pub fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn stderr_suffix(&self) -> String {
        let e = self.stderr_text();
        if e.trim().is_empty() {
            String::new()
        } else {
            format!("\nssh said: {}", e.trim())
        }
    }

    fn marker(&self, seq: u64) -> String {
        format!("__UNLATCH_END_{:016x}_{seq}", self.nonce)
    }

    /// Send a command without waiting; returns its sequence number for [`RemoteShell::finish`].
    pub fn send(&mut self, cmd: &str) -> anyhow::Result<u64> {
        self.seq += 1;
        let seq = self.seq;
        // The newline before the marker ends a last output line that lacks one; `finish`
        // strips exactly that newline again.
        let wrapped = format!(
            "{{\n{cmd}\n}}\nprintf '\\n%s %d\\n' {} \"$?\"\n",
            self.marker(seq)
        );
        self.stdin
            .write_all(wrapped.as_bytes())
            .context("writing to ssh")?;
        self.stdin.flush().context("writing to ssh")?;
        Ok(seq)
    }

    /// Next output line of the command in flight.
    pub fn next_line(&mut self, timeout: Duration) -> anyhow::Result<String> {
        match self.lines.recv_timeout(timeout) {
            Ok(l) => Ok(l),
            Err(RecvTimeoutError::Timeout) => bail!("remote command timed out after {timeout:?}"),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("ssh session ended{}", self.stderr_suffix())
            }
        }
    }

    /// Collect output until command `seq`'s end marker.
    pub fn finish(&mut self, seq: u64, timeout: Duration) -> anyhow::Result<RemoteOut> {
        let deadline = Instant::now() + timeout;
        let marker = self.marker(seq);
        let mut out = String::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.next_line(left)?;
            if let Some(rest) = line.strip_prefix(&marker) {
                let status = rest.trim().parse::<i32>().unwrap_or(-1);
                if out.ends_with('\n') {
                    out.pop();
                }
                return Ok(RemoteOut {
                    stdout: out,
                    status,
                });
            }
            out.push_str(&line);
            out.push('\n');
        }
    }

    pub fn run(&mut self, cmd: &str, timeout: Duration) -> anyhow::Result<RemoteOut> {
        let seq = self.send(cmd)?;
        self.finish(seq, timeout)
    }

    /// Run and require exit status 0; returns stdout.
    pub fn check(&mut self, cmd: &str, timeout: Duration) -> anyhow::Result<String> {
        let out = self.run(cmd, timeout)?;
        if out.status != 0 {
            bail!(
                "remote command failed ({}): {cmd}\n{}",
                out.status,
                out.stdout.trim()
            );
        }
        Ok(out.stdout)
    }

    /// Round-trip times of `n` no-op commands, in milliseconds.
    pub fn rtt_samples(&mut self, n: usize) -> anyhow::Result<Vec<f64>> {
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let t0 = Instant::now();
            self.run(":", Duration::from_secs(10))?;
            v.push(t0.elapsed().as_secs_f64() * 1e3);
        }
        Ok(v)
    }
}

impl Drop for RemoteShell {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"exit 0\n");
        let _ = self.stdin.flush();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Actionable hint for an ssh failure, from its stderr.
pub fn ssh_hint(stderr: &str, dest: &str) -> Option<String> {
    let s = stderr.to_ascii_lowercase();
    let hint = if s.contains("host key verification failed")
        || s.contains("remote host identification has changed")
    {
        format!("the host key is unknown or changed: run `ssh {dest}` once in a terminal and check/accept the key")
    } else if s.contains("permission denied") {
        format!(
            "key authentication failed: load your key (`ssh-add`), or install it on the VM with `ssh-copy-id {dest}`; \
             Unlatch never uses passwords in the background"
        )
    } else if s.contains("could not resolve hostname") || s.contains("name or service not known") {
        format!(
            "{dest:?} does not resolve: check the host name or add a `Host` entry to ~/.ssh/config"
        )
    } else if s.contains("connection timed out") || s.contains("operation timed out") {
        "the VM did not answer: check it is running and reachable (VPN / Tailscale / security group)".to_string()
    } else if s.contains("connection refused") {
        "nothing listens on the ssh port: is sshd running, and is the port right?".to_string()
    } else if s.contains("no route to host") {
        "no route to the VM: on macOS 15+, a LAN address also needs Local Network permission"
            .to_string()
    } else if s.contains("https://") || s.contains("http://") {
        format!("ssh wants you to visit a login URL (2FA / Tailscale check mode): run `ssh {dest}` once interactively")
    } else {
        return None;
    };
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sq("a b"), "'a b'");
        assert_eq!(sq("it's"), r"'it'\''s'");
        assert_eq!(remote_path("~"), "\"$HOME\"");
        assert_eq!(remote_path("~/code/x y"), "\"$HOME\"/'code/x y'");
        assert_eq!(remote_path("/srv/$x"), "'/srv/$x'");
    }

    #[test]
    fn quoted_strings_survive_a_real_shell() {
        let nasty = "a'b\"c $HOME `x` \\ \n end";
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", sq(nasty)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), nasty);
    }

    #[test]
    fn rejects_option_like_destinations() {
        assert!(ssh_command("-oProxyCommand=x", &[], "true").is_err());
        assert!(ssh_command("", &[], "true").is_err());
        assert!(ssh_command("host", &[], "true").is_ok());
    }

    #[test]
    fn hints() {
        assert!(ssh_hint("Permission denied (publickey).", "vm")
            .unwrap()
            .contains("ssh-copy-id vm"));
        assert!(ssh_hint("Host key verification failed.", "vm")
            .unwrap()
            .contains("host key"));
        assert!(ssh_hint(
            "ssh: Could not resolve hostname vm: Name or service not known",
            "vm"
        )
        .is_some());
        assert!(ssh_hint("all good", "vm").is_none());
    }

    /// Needs `ssh localhost` to work non-interactively (it does on the dev box).
    #[test]
    #[ignore = "needs ssh localhost"]
    fn session_markers_and_status() {
        let mut sh = RemoteShell::connect("localhost", &[]).unwrap();
        assert_eq!(
            sh.run("printf 'a\\nb'", Duration::from_secs(10)).unwrap(),
            RemoteOut {
                stdout: "a\nb".into(),
                status: 0
            }
        );
        assert_eq!(
            sh.run("echo x; false", Duration::from_secs(10)).unwrap(),
            RemoteOut {
                stdout: "x\n".into(),
                status: 1
            }
        );
        assert_eq!(sh.run("true", Duration::from_secs(10)).unwrap().stdout, "");
        assert_eq!(sh.rtt_samples(3).unwrap().len(), 3);
    }
}
