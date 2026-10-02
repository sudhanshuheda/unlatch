//! Resolving the user's login-shell environment (review (e)10, D21): a launchd-started app gets
//! launchd's PATH and agent socket, not the shell's, so ProxyCommands calling Homebrew tools and
//! agents exported in `.zshrc` would fail. Same approach as VS Code.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Shell-specific or per-process variables that must not leak into ssh's environment.
const SKIP: &[&str] = &["_", "SHLVL", "PWD", "OLDPWD"];

pub(crate) fn resolve(shell: &str, timeout: Duration) -> Vec<(String, String)> {
    let mark = format!("__UNLATCH_ENV_{:016x}__", rand::random::<u64>());
    let script = format!("printf '%s' {mark}; env -0; printf '%s' {mark}");
    let mut cmd = Command::new(shell);
    cmd.args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe. A new session has no controlling terminal, so an
    // interactive (-i) shell cannot stop on SIGTTIN/SIGTTOU or grab the user's tty, and the
    // whole tree it spawns can be killed as one process group.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("login env: cannot run {shell}: {e}");
            return Vec::new();
        }
    };
    let Some(mut out) = child.stdout.take() else {
        return Vec::new();
    };
    // Stream stdout through a channel: rc files may start daemons (agents) that inherit stdout
    // and keep it open long after the shell exits, so neither EOF nor exit is a usable signal.
    // The closing mark is.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match out.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let result = loop {
        if let Some(env) = parse(&buf, mark.as_bytes()) {
            break Some(env);
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(chunk) => buf.extend_from_slice(&chunk),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break parse(&buf, mark.as_bytes())
            }
        }
    };
    match result {
        Some(env) => {
            // Reap without blocking the caller.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            env
        }
        None => {
            tracing::warn!("login env: {shell} gave no environment within {timeout:?}; using the inherited one");
            // Negative pid: the process group setsid created (never 0, which would be ours).
            let pgid = child.id() as libc::pid_t;
            // SAFETY: plain kill(2) on a process group we created.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
            let _ = child.wait();
            Vec::new()
        }
    }
}

/// Entries between the two marks, or `None` until the closing mark has arrived.
fn parse(buf: &[u8], mark: &[u8]) -> Option<Vec<(String, String)>> {
    let find = |from: usize| {
        buf[from..]
            .windows(mark.len())
            .position(|w| w == mark)
            .map(|p| p + from)
    };
    let body_start = find(0)? + mark.len();
    let end = find(body_start)?;
    let env = buf[body_start..end]
        .split(|&b| b == 0)
        .filter_map(|kv| {
            let kv = std::str::from_utf8(kv).ok()?;
            let (k, v) = kv.split_once('=')?;
            (!k.is_empty() && !SKIP.contains(&k)).then(|| (k.to_string(), v.to_string()))
        })
        .collect();
    Some(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_between_marks_skipping_noise() {
        let buf = b"motd noise MARKA=1\0B=x=y\0_=/bin/env\0\xff=bad\0noeq\0MARKtrailing";
        assert_eq!(
            parse(buf, b"MARK"),
            Some(vec![("A".into(), "1".into()), ("B".into(), "x=y".into())])
        );
        assert_eq!(parse(b"no marks", b"MARK"), None);
        assert_eq!(parse(b"MARKA=1\0", b"MARK"), None);
    }

    #[test]
    fn real_login_shell() {
        let env = resolve("/bin/bash", Duration::from_secs(10));
        assert!(
            env.iter().any(|(k, v)| k == "PATH" && !v.is_empty()),
            "{env:?}"
        );
        assert!(env.iter().any(|(k, _)| k == "HOME"));
        assert!(!env.iter().any(|(k, _)| k == "SHLVL"));
    }

    #[test]
    fn hung_shell_times_out_and_is_killed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = dir.path().join("slowsh");
        std::fs::write(&shell, "#!/bin/sh\nsleep 30 &\nsleep 30\n").expect("write");
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let t0 = Instant::now();
        let env = resolve(shell.to_str().expect("utf8"), Duration::from_millis(300));
        assert!(env.is_empty());
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "took {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn lingering_daemon_holding_stdout_does_not_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = dir.path().join("rcsh");
        // Mimics an rc file that starts an agent inheriting stdout, then runs the command.
        std::fs::write(
            &shell,
            "#!/bin/sh\nsleep 5 &\nshift 3\nexec /bin/sh -c \"$1\"\n",
        )
        .expect("write");
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let t0 = Instant::now();
        let env = resolve(shell.to_str().expect("utf8"), Duration::from_secs(4));
        assert!(env.iter().any(|(k, _)| k == "PATH"), "{env:?}");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "took {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn missing_shell_is_empty() {
        assert!(resolve("/nonexistent/shell", Duration::from_secs(1)).is_empty());
    }
}
