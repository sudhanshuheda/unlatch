//! `unlatch doctor`: checks a VM for everything Unlatch needs, with a fix for each problem.

use crate::remote::{remote_path, ssh_hint, RemoteShell};
use crate::stats::{median, round3};
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn check(name: &str, level: Level, detail: impl Into<String>, fix: Option<String>) -> Check {
    Check {
        name: name.to_string(),
        level,
        detail: detail.into(),
        fix,
    }
}

/// Filesystems inotify cannot see remote writes on (D14/D22): the daemon polls them.
const POLLED_FS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb2",
    "smb",
    "smbfs",
    "fuseblk",
    "fuse",
    "9p",
    "v9fs",
    "virtiofs",
    "afs",
    "ceph",
    "glusterfs",
    "lustre",
    "gpfs",
];

/// The remote probe. Prints `key=value` lines; values never contain newlines.
fn script(root: &str) -> String {
    let root = remote_path(root);
    format!(
        r#"
say() {{ printf '%s=%s\n' "$1" "$(printf '%s' "$2" | tr '\n' ' ')"; }}
say arch "$(uname -m)"
say kernel "$(uname -r)"
say os "$( ( . /etc/os-release 2>/dev/null && printf '%s' "$PRETTY_NAME" ) || uname -s)"
say uid "$(id -u)"
say home "$HOME"
say shell "$SHELL"
R={root}
if [ -d "$R" ]; then say root.exists yes; else say root.exists no; fi
[ -w "$R" ] && say root.writable yes || say root.writable no
say root.fstype "$(stat -f -c %T "$R" 2>/dev/null || df -T "$R" 2>/dev/null | awk 'NR==2{{print $2}}')"
say root.top_entries "$(ls -A "$R" 2>/dev/null | wc -l | tr -d ' ')"
say inotify.max_user_watches "$(cat /proc/sys/fs/inotify/max_user_watches 2>/dev/null)"
say inotify.max_user_instances "$(cat /proc/sys/fs/inotify/max_user_instances 2>/dev/null)"
say inotify.used_watches "$(cat /proc/[0-9]*/fdinfo/* 2>/dev/null | grep -c '^inotify wd:')"
say unlatchd.path "$(command -v unlatchd 2>/dev/null)"
i=0
for d in "$UNLATCH_HOME" "${{XDG_DATA_HOME:+$XDG_DATA_HOME/unlatch}}" "$HOME/.unlatch" "/var/tmp/unlatch-$(id -u)" "/tmp/unlatch-$(id -u)"; do
  [ -n "$d" ] || continue
  i=$((i+1))
  st=ok; why=
  if [ -L "$d" ]; then st=bad; why="is a symlink"
  elif [ -d "$d" ]; then
    [ "$(stat -c %u "$d" 2>/dev/null)" = "$(id -u)" ] || {{ st=bad; why="not owned by you"; }}
    m=$(stat -c %a "$d" 2>/dev/null)
    [ "$st" = ok ] && [ "$m" != 700 ] && {{ st=warn; why="mode $m (want 700)"; }}
    fs=$(stat -f -c %T "$d" 2>/dev/null)
    case "$fs" in nfs*|cifs|smb*|fuse*|9p|v9fs|afs) st=bad; why="on $fs (network fs)";; esac
    if [ "$st" != bad ]; then
      t="$d/.unlatch-doctor-$$"
      if printf '#!/bin/sh\nexit 0\n' > "$t" 2>/dev/null && chmod 700 "$t" && "$t" 2>/dev/null; then :; else st=bad; why="cannot write+exec (noexec mount or read-only)"; fi
      rm -f "$t"
    fi
    say "install.$i.unlatchd" "$(ls "$d"/unlatchd-* 2>/dev/null | head -n 3 | tr '\n' ' ')"
  else
    p=$(dirname "$d")
    if [ -w "$p" ]; then st=create; why="does not exist (would be created)"; else st=bad; why="does not exist and $p is not writable"; fi
  fi
  say "install.$i.path" "$d"
  say "install.$i.status" "$st"
  say "install.$i.why" "$why"
done
if command -v loginctl >/dev/null 2>&1; then say linger "$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null)"; fi
say kill_user_processes "$(grep -E '^[[:space:]]*KillUserProcesses' /etc/systemd/logind.conf 2>/dev/null | tail -n1 | cut -d= -f2)"
"#
    )
}

pub fn parse_kv(out: &str) -> BTreeMap<String, String> {
    out.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

fn num(kv: &BTreeMap<String, String>, k: &str) -> Option<u64> {
    kv.get(k).and_then(|v| v.parse().ok())
}

/// Turn the remote facts into checks (pure, unit-tested).
pub fn evaluate(kv: &BTreeMap<String, String>, root: &str, rtt_ms: Option<f64>) -> Vec<Check> {
    let mut out = Vec::new();
    let get = |k: &str| kv.get(k).map(String::as_str).unwrap_or("");

    let arch = get("arch");
    match arch {
        "x86_64" | "aarch64" | "arm64" => out.push(check(
            "remote arch",
            Level::Ok,
            format!("{arch}, {}, {}", get("os"), get("kernel")),
            None,
        )),
        other => out.push(check(
            "remote arch",
            Level::Fail,
            format!("{other:?} is not supported"),
            Some("unlatchd ships for x86_64 and aarch64 Linux only".into()),
        )),
    }

    if get("root.exists") == "yes" {
        let fs = get("root.fstype");
        let lvl = if POLLED_FS.iter().any(|p| fs.starts_with(p)) {
            Level::Warn
        } else {
            Level::Ok
        };
        let fix = (lvl == Level::Warn).then(|| {
            "inotify cannot see changes made by other machines on this filesystem, so unlatchd polls it (changes show up \
             after seconds, not milliseconds). If the files live on a VM/container host, run unlatchd there with the \
             bind-mounted directory as root"
                .to_string()
        });
        out.push(check(
            "root",
            lvl,
            format!(
                "{root} on {fs}, {} top-level entries",
                get("root.top_entries")
            ),
            fix,
        ));
        if get("root.writable") != "yes" {
            out.push(check(
                "root writable",
                Level::Warn,
                "the root is read-only for your user",
                Some("Finder will show it read-only".into()),
            ));
        }
    } else {
        out.push(check(
            "root",
            Level::Fail,
            format!("{root} does not exist on the VM"),
            Some(format!("create it: ssh HOST mkdir -p {root}")),
        ));
    }

    match (
        num(kv, "inotify.max_user_watches"),
        num(kv, "inotify.used_watches"),
    ) {
        (Some(max), Some(used)) => {
            let free = max.saturating_sub(used);
            let detail = format!("{used} of {max} watches in use by your processes, {free} free (unlatchd uses at most half the free budget)");
            if free < 8192 {
                out.push(check(
                    "inotify",
                    Level::Warn,
                    detail,
                    Some(
                        "raise the limit: `echo fs.inotify.max_user_watches=524288 | sudo tee /etc/sysctl.d/60-unlatch.conf && \
                         sudo sysctl --system` (otherwise unlatchd polls some directories)"
                            .into(),
                    ),
                ));
            } else {
                out.push(check("inotify", Level::Ok, detail, None));
            }
        }
        _ => out.push(check(
            "inotify",
            Level::Warn,
            "could not read inotify limits",
            None,
        )),
    }

    // Install directory (first usable one in D22 order).
    let mut chosen: Option<String> = None;
    let mut rejected = Vec::new();
    for i in 1..=5 {
        let path = get(&format!("install.{i}.path"));
        if path.is_empty() {
            continue;
        }
        let st = get(&format!("install.{i}.status"));
        let why = get(&format!("install.{i}.why"));
        match st {
            "ok" | "create" | "warn" if chosen.is_none() => {
                let note = if st == "ok" {
                    String::new()
                } else {
                    format!(" ({why})")
                };
                let installed = get(&format!("install.{i}.unlatchd"));
                let inst = if installed.is_empty() {
                    "unlatchd not installed yet (uploaded on first connect)".to_string()
                } else {
                    format!("installed: {installed}")
                };
                chosen = Some(format!("{path}{note}; {inst}"));
            }
            "bad" => rejected.push(format!("{path}: {why}")),
            _ => {}
        }
    }
    match chosen {
        Some(c) => {
            let lvl = if rejected.is_empty() { Level::Ok } else { Level::Info };
            let detail = if rejected.is_empty() { c } else { format!("{c} (skipped {})", rejected.join("; ")) };
            out.push(check("install dir", lvl, detail, None));
        }
        None => out.push(check(
            "install dir",
            Level::Fail,
            format!("no usable install directory: {}", rejected.join("; ")),
            Some("set UNLATCH_HOME on the VM (in ~/.profile) to a local, exec-capable directory you own".into()),
        )),
    }

    if get("linger") == "no" && get("kill_user_processes").eq_ignore_ascii_case("yes") {
        out.push(check(
            "session lifetime",
            Level::Warn,
            "logind kills your processes at logout and linger is off: unlatchd serve would die with the ssh session",
            Some("`sudo loginctl enable-linger $USER`".into()),
        ));
    }

    if let Some(rtt) = rtt_ms {
        let lvl = if rtt > 150.0 { Level::Info } else { Level::Ok };
        let fix = (lvl == Level::Info).then(|| {
            "high latency: browsing stays instant (local replica), but first opens of files wait ~1 RTT; a region closer to you helps".to_string()
        });
        out.push(check(
            "rtt",
            lvl,
            format!("median {rtt:.1} ms over ssh"),
            fix,
        ));
    }
    out
}

#[derive(Serialize)]
pub struct Report {
    pub host: String,
    pub root: String,
    pub connect_ms: Option<f64>,
    pub rtt_ms: Option<f64>,
    pub checks: Vec<Check>,
    pub facts: BTreeMap<String, String>,
}

fn local_checks() -> Vec<Check> {
    let mut v = Vec::new();
    let on_path = |bin: &str| {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
            .unwrap_or(false)
    };
    if on_path("ssh") {
        v.push(check("local ssh", Level::Ok, "ssh found on PATH", None));
    } else {
        v.push(check(
            "local ssh",
            Level::Fail,
            "ssh not found on PATH",
            Some("install OpenSSH client".into()),
        ));
    }
    if cfg!(target_os = "linux") {
        let fuse = std::path::Path::new("/dev/fuse").exists();
        let fm = on_path("fusermount3") || on_path("fusermount");
        if fuse && fm {
            v.push(check(
                "local fuse",
                Level::Ok,
                "/dev/fuse and fusermount3 present (`unlatch mount` works)",
                None,
            ));
        } else {
            v.push(check(
                "local fuse",
                Level::Info,
                "FUSE unavailable: `unlatch mount` will not work here",
                Some("install fuse3 (provides fusermount3)".into()),
            ));
        }
    }
    if cfg!(target_os = "macos") {
        let fpc = std::path::Path::new("/usr/bin/fileproviderctl").exists();
        v.push(check(
            "fileproviderctl",
            if fpc { Level::Ok } else { Level::Info },
            if fpc {
                "present"
            } else {
                "missing (needed only for probe diagnostics)"
            },
            None,
        ));
    }
    v
}

pub fn run(host: &str, root: &str, ssh_args: &[String]) -> Report {
    let mut checks = local_checks();
    let t0 = Instant::now();
    let mut facts = BTreeMap::new();
    let mut connect_ms = None;
    let mut rtt_ms = None;
    match RemoteShell::connect(host, ssh_args) {
        Err(e) => {
            let msg = format!("{e:#}");
            let fix = ssh_hint(&msg, host)
                .or_else(|| Some(format!("try `ssh -v {host} true` to see why")));
            checks.push(check(
                "ssh",
                Level::Fail,
                format!("cannot connect non-interactively: {}", msg.trim()),
                fix,
            ));
        }
        Ok(mut sh) => {
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            connect_ms = Some(round3(ms));
            checks.push(check(
                "ssh",
                Level::Ok,
                format!("non-interactive login works ({ms:.0} ms to connect)"),
                None,
            ));
            let stderr_noise = sh.stderr_text();
            match sh.run(&script(root), Duration::from_secs(60)) {
                Ok(out) => facts = parse_kv(&out.stdout),
                Err(e) => checks.push(check("remote probe", Level::Fail, format!("{e:#}"), None)),
            }
            if !stderr_noise.trim().is_empty() {
                checks.push(check(
                    "shell noise",
                    Level::Info,
                    format!("the login printed on stderr: {}", stderr_noise.trim()),
                    Some("harmless for Unlatch (the protocol skips junk before its preamble), but check ~/.bashrc".into()),
                ));
            }
            match sh.rtt_samples(10) {
                Ok(s) => rtt_ms = median(&s).map(round3),
                Err(e) => checks.push(check("rtt", Level::Warn, format!("{e:#}"), None)),
            }
            if !facts.is_empty() {
                checks.extend(evaluate(&facts, root, rtt_ms));
            }
        }
    }
    Report {
        host: host.to_string(),
        root: root.to_string(),
        connect_ms,
        rtt_ms,
        checks,
        facts,
    }
}

pub fn print(report: &Report) {
    println!("unlatch doctor: {} : {}", report.host, report.root);
    for c in &report.checks {
        let tag = match c.level {
            Level::Ok => "[ok]  ",
            Level::Info => "[info]",
            Level::Warn => "[warn]",
            Level::Fail => "[FAIL]",
        };
        println!("{tag} {:<17} {}", c.name, c.detail);
        if let Some(f) = &c.fix {
            println!("       {:<17} fix: {f}", "");
        }
    }
}

pub fn failed(report: &Report) -> bool {
    report.checks.iter().any(|c| c.level == Level::Fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(extra: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut kv = parse_kv(
            "arch=x86_64\nkernel=6.8.0\nos=Ubuntu 24.04\nroot.exists=yes\nroot.writable=yes\nroot.fstype=ext2/ext3\n\
             root.top_entries=12\ninotify.max_user_watches=65536\ninotify.used_watches=100\n\
             install.1.path=/home/u/.unlatch\ninstall.1.status=create\ninstall.1.why=does not exist (would be created)\n",
        );
        for (k, v) in extra {
            kv.insert(k.to_string(), v.to_string());
        }
        kv
    }

    fn level_of(checks: &[Check], name: &str) -> Level {
        checks
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.level)
            .unwrap_or_else(|| panic!("no check {name}"))
    }

    #[test]
    fn healthy_vm() {
        let c = evaluate(&facts(&[]), "/srv", Some(12.0));
        assert!(c.iter().all(|c| c.level == Level::Ok), "{c:#?}");
    }

    #[test]
    fn nfs_root_warns_with_fix() {
        let c = evaluate(&facts(&[("root.fstype", "nfs")]), "/srv", None);
        assert_eq!(level_of(&c, "root"), Level::Warn);
        assert!(c
            .iter()
            .find(|c| c.name == "root")
            .unwrap()
            .fix
            .as_ref()
            .unwrap()
            .contains("polls"));
    }

    #[test]
    fn low_inotify_budget_warns() {
        let c = evaluate(
            &facts(&[
                ("inotify.max_user_watches", "8192"),
                ("inotify.used_watches", "4000"),
            ]),
            "/srv",
            None,
        );
        assert_eq!(level_of(&c, "inotify"), Level::Warn);
    }

    #[test]
    fn unsupported_arch_and_missing_root_fail() {
        let c = evaluate(
            &facts(&[("arch", "riscv64"), ("root.exists", "no")]),
            "/srv",
            None,
        );
        assert_eq!(level_of(&c, "remote arch"), Level::Fail);
        assert_eq!(level_of(&c, "root"), Level::Fail);
    }

    #[test]
    fn install_dir_selection() {
        let c = evaluate(
            &facts(&[
                ("install.1.status", "bad"),
                (
                    "install.1.why",
                    "cannot write+exec (noexec mount or read-only)",
                ),
                ("install.2.path", "/var/tmp/unlatch-1000"),
                ("install.2.status", "ok"),
                ("install.2.unlatchd", "/var/tmp/unlatch-1000/unlatchd-0.1.0"),
            ]),
            "/srv",
            None,
        );
        let d = c.iter().find(|c| c.name == "install dir").unwrap();
        assert_eq!(d.level, Level::Info);
        assert!(
            d.detail.starts_with("/var/tmp/unlatch-1000; installed"),
            "{}",
            d.detail
        );
        assert!(d.detail.contains("noexec"));
        let none = evaluate(
            &facts(&[
                ("install.1.status", "bad"),
                ("install.1.why", "is a symlink"),
            ]),
            "/srv",
            None,
        );
        assert_eq!(level_of(&none, "install dir"), Level::Fail);
    }

    #[test]
    fn linger_warning() {
        let c = evaluate(
            &facts(&[("linger", "no"), ("kill_user_processes", "yes")]),
            "/srv",
            None,
        );
        assert_eq!(level_of(&c, "session lifetime"), Level::Warn);
    }

    #[test]
    fn script_is_valid_sh_and_runs_locally() {
        let dir = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script(&dir.path().to_string_lossy()))
            .env("UNLATCH_HOME", dir.path().join("hh"))
            .output()
            .unwrap();
        let kv = parse_kv(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(kv.get("root.exists").map(String::as_str), Some("yes"));
        assert!(kv.contains_key("arch"));
        assert_eq!(
            kv.get("install.1.status").map(String::as_str),
            Some("create")
        );
        assert!(num(&kv, "inotify.max_user_watches").is_some());
    }
}
