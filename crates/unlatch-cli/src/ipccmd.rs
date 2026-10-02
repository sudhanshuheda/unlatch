//! `unlatch ls|stat|cat|status`: debugging through an engine's IPC socket (the same protocol the
//! File Provider extension speaks).

use crate::config::{default_config_path, AgentConfig};
use crate::pathres::{normalize, resolve, Lister};
use anyhow::{anyhow, bail, Context};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use unlatch_core::ipc::IpcClient;
use unlatch_proto::ipc::{ConnState, EngineStatus, IpcItem, IpcRequest, IpcResponse};
use unlatch_proto::{ErrorCode, ItemId, Kind, ProtoError};

/// Which engine to talk to.
#[derive(Clone, Debug)]
pub struct IpcTarget {
    pub socket: PathBuf,
    pub domain: String,
}

/// `--socket` + `--domain`, else the agent config (`--config`, `$UNLATCH_CONFIG`, default path).
pub fn targets(
    socket: Option<&Path>,
    domain: Option<&str>,
    config: Option<&Path>,
    all: bool,
) -> anyhow::Result<Vec<IpcTarget>> {
    if let Some(s) = socket {
        let domain = domain.ok_or_else(|| {
            anyhow!("--socket needs --domain (the domain name the engine serves)")
        })?;
        return Ok(vec![IpcTarget {
            socket: s.to_path_buf(),
            domain: domain.to_string(),
        }]);
    }
    let path = config
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path);
    let cfg = AgentConfig::load(&path)
        .with_context(|| "no --socket given, so the agent config is needed".to_string())?;
    if all && domain.is_none() {
        return Ok(cfg
            .domains
            .iter()
            .map(|d| IpcTarget {
                socket: d.socket_path(),
                domain: d.name.clone(),
            })
            .collect());
    }
    let d = cfg.domain(domain)?;
    Ok(vec![IpcTarget {
        socket: d.socket_path(),
        domain: d.name.clone(),
    }])
}

fn connect(t: &IpcTarget) -> anyhow::Result<IpcClient> {
    IpcClient::connect(&t.socket, &t.domain, Duration::from_secs(5)).map_err(|e| {
        anyhow!(
            "cannot reach the engine for {:?} at {}: {e} (is `unlatch agent` running?)",
            t.domain,
            t.socket.display()
        )
    })
}

fn call(c: &IpcClient, req: IpcRequest) -> Result<IpcResponse, ProtoError> {
    match c.call(req, None, None)? {
        IpcResponse::Error { code, msg, .. } => Err(ProtoError::new(code, msg)),
        other => Ok(other),
    }
}

fn unexpected(r: IpcResponse) -> ProtoError {
    ProtoError::new(ErrorCode::Protocol, format!("unexpected reply {r:?}"))
}

struct IpcLister<'a>(&'a IpcClient);

impl Lister for IpcLister<'_> {
    fn item(&mut self, id: ItemId) -> Result<IpcItem, ProtoError> {
        match call(self.0, IpcRequest::Item { id })? {
            IpcResponse::Item(it) => Ok(it),
            other => Err(unexpected(other)),
        }
    }

    fn children(&mut self, dir: ItemId) -> Result<Vec<IpcItem>, ProtoError> {
        let mut out = Vec::new();
        let mut cursor = None;
        loop {
            match call(
                self.0,
                IpcRequest::Enumerate {
                    container: dir,
                    cursor,
                    limit: 1000,
                    viewer: false,
                },
            )? {
                IpcResponse::Page { items, next } => {
                    out.extend(items);
                    match next {
                        Some(n) => cursor = Some(n),
                        None => return Ok(out),
                    }
                }
                other => return Err(unexpected(other)),
            }
        }
    }
}

fn resolve_path(c: &IpcClient, path: &str) -> anyhow::Result<IpcItem> {
    let comps = normalize(path)?;
    resolve(&mut IpcLister(c), &comps).map_err(|e| anyhow!("{path}: {}", term_safe(&e.to_string())))
}

/// Escape text that came from the VM (file names, symlink targets, server-reported strings)
/// before it reaches the user's terminal, as GNU `ls -b` does: the VM is untrusted input, and a
/// raw ESC/BEL/CR or C1 control in a name could otherwise drive the terminal (OSC 52 clipboard
/// writes, retitling, erasing lines). Control characters become `\xNN` (C0, DEL), `\t`/`\n`/`\r`
/// or `\u{NNNN}` (C1), bidi overrides/isolates become `\u{NNNN}` (they reorder the rest of the
/// line), and a literal backslash is doubled so the escapes stay unambiguous. Everything else,
/// including non-ASCII text, passes through unchanged.
pub fn term_safe(s: &str) -> std::borrow::Cow<'_, str> {
    fn needs(c: char) -> bool {
        c == '\\'
            || c.is_control()
            || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }
    if !s.chars().any(needs) {
        return s.into();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x80 && needs(c) => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if needs(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.into()
}

fn kind_char(it: &IpcItem) -> char {
    match (it.entry.kind, it.symlink_blocked) {
        (Kind::Dir, _) => 'd',
        (Kind::Symlink, false) => 'l',
        _ => '-',
    }
}

fn mode_string(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// One `ls -l`-style line.
pub fn format_ls_line(it: &IpcItem, show_ids: bool) -> String {
    let e = &it.entry;
    let mut line = format!(
        "{}{} {:>12} {} ",
        kind_char(it),
        mode_string(e.mode),
        e.size,
        crate::timefmt::format_ns(e.mtime_ns)
    );
    if show_ids {
        line.push_str(&format!("{:>8} ", e.id.0));
    }
    line.push_str(&term_safe(&it.display_name));
    if e.kind == Kind::Dir && e.lazy {
        line.push_str("/ (lazy)");
    }
    if let (Kind::Symlink, Some(t)) = (e.kind, &e.symlink_target) {
        line.push_str(" -> ");
        line.push_str(&term_safe(t));
        if it.symlink_blocked {
            line.push_str(" (blocked: points outside the root)");
        }
    }
    line
}

pub fn ls(t: &IpcTarget, path: &str, show_ids: bool) -> anyhow::Result<()> {
    let c = connect(t)?;
    let it = resolve_path(&c, path)?;
    let mut out = std::io::stdout().lock();
    if it.entry.kind == Kind::Dir {
        let kids = IpcLister(&c)
            .children(it.entry.id)
            .map_err(|e| anyhow!("{path}: {}", term_safe(&e.to_string())))?;
        for k in &kids {
            writeln!(out, "{}", format_ls_line(k, show_ids))?;
        }
    } else {
        writeln!(out, "{}", format_ls_line(&it, show_ids))?;
    }
    Ok(())
}

pub fn format_stat(it: &IpcItem) -> String {
    let e = &it.entry;
    let mut s = String::new();
    s.push_str(&format!("name:         {}\n", term_safe(&it.display_name)));
    if it.display_name != e.name {
        s.push_str(&format!("name on VM:   {}\n", term_safe(&e.name)));
    }
    s.push_str(&format!("id:           {}\n", e.id));
    s.push_str(&format!("parent:       {}\n", e.parent));
    s.push_str(&format!(
        "kind:         {:?}{}\n",
        e.kind,
        if it.symlink_blocked {
            " (blocked symlink)"
        } else {
            ""
        }
    ));
    s.push_str(&format!("size:         {}\n", e.size));
    s.push_str(&format!("mode:         {:o}\n", e.mode));
    s.push_str(&format!(
        "mtime:        {} UTC\n",
        crate::timefmt::format_ns(e.mtime_ns)
    ));
    s.push_str(&format!(
        "version:      content={} meta={}\n",
        e.version.content, e.version.meta
    ));
    s.push_str(&format!("seq:          {}\n", e.seq));
    s.push_str(&format!(
        "access:       {:03b} (rwx for the VM user)\n",
        e.access
    ));
    s.push_str(&format!("caps:         {:#x}\n", it.caps));
    s.push_str(&format!("user_exec:    {}\n", it.user_exec));
    if let Some(t) = &e.symlink_target {
        s.push_str(&format!("target:       {}\n", term_safe(t)));
    }
    if e.kind == Kind::Dir {
        s.push_str(&format!("lazy:         {}\n", e.lazy));
    }
    s
}

pub fn stat(t: &IpcTarget, path: &str) -> anyhow::Result<()> {
    let c = connect(t)?;
    let it = resolve_path(&c, path)?;
    print!("{}", format_stat(&it));
    Ok(())
}

pub fn cat(t: &IpcTarget, path: &str) -> anyhow::Result<()> {
    let c = connect(t)?;
    let it = resolve_path(&c, path)?;
    if it.entry.kind == Kind::Dir {
        bail!("{path}: is a directory");
    }
    let dest = std::env::temp_dir().join(format!(
        "unlatch-cat-{}-{:08x}",
        std::process::id(),
        rand::random::<u32>()
    ));
    std::fs::create_dir_all(&dest)?;
    let result = (|| -> anyhow::Result<()> {
        let req = IpcRequest::Fetch {
            id: it.entry.id,
            version: None,
            dest_dir: dest.to_string_lossy().into_owned(),
        };
        match call(&c, req).map_err(|e| anyhow!("{path}: {e}"))? {
            IpcResponse::Fetched { path: fetched, .. } => {
                let mut f =
                    std::fs::File::open(&fetched).with_context(|| format!("opening {fetched}"))?;
                let mut out = std::io::stdout().lock();
                std::io::copy(&mut f, &mut out)?;
                out.flush()?;
                Ok(())
            }
            other => Err(unexpected(other).into()),
        }
    })();
    let _ = std::fs::remove_dir_all(&dest);
    result
}

pub fn format_status(domain: &str, st: &EngineStatus) -> String {
    let state = match &st.state {
        ConnState::Connecting => "connecting".to_string(),
        ConnState::Syncing { received } => format!("syncing ({received} entries received)"),
        ConnState::Live => "live".to_string(),
        ConnState::Offline { error, retry_in_ms } => {
            format!("offline: {} (retry in {retry_in_ms} ms)", term_safe(error))
        }
        ConnState::NeedsUser { reason, url } => {
            format!(
                "needs you: {}{}",
                term_safe(reason),
                url.as_ref()
                    .map(|u| format!(" ({})", term_safe(u)))
                    .unwrap_or_default()
            )
        }
        ConnState::Paused { reason } => format!("paused: {}", term_safe(reason)),
    };
    let mut s = format!("{domain}: {state}\n");
    s.push_str(&format!("  entries:          {}\n", st.entries));
    if let Some(rtt) = st.rtt_us {
        s.push_str(&format!(
            "  rtt:              {:.1} ms\n",
            rtt as f64 / 1000.0
        ));
    }
    s.push_str(&format!(
        "  cache:            {:.1} MiB\n",
        st.cache_bytes as f64 / (1024.0 * 1024.0)
    ));
    s.push_str(&format!("  pending uploads:  {}\n", st.pending_uploads));
    if let Some(info) = &st.server {
        s.push_str(&format!(
            "  server:           unlatchd {} on {} ({})\n",
            term_safe(&info.version),
            term_safe(&info.hostname),
            term_safe(&info.root_path)
        ));
        s.push_str(&format!(
            "  watches:          {}{}\n",
            info.watches,
            if info.polled { " (root is polled)" } else { "" }
        ));
        for w in &info.warnings {
            s.push_str(&format!("  warning:          {}\n", term_safe(w)));
        }
    }
    s
}

pub fn status(targets: &[IpcTarget]) -> anyhow::Result<()> {
    let mut failed = 0;
    for t in targets {
        match connect(t).and_then(|c| call(&c, IpcRequest::Status).map_err(anyhow::Error::from)) {
            Ok(IpcResponse::Status(st)) => print!("{}", format_status(&t.domain, &st)),
            Ok(other) => {
                failed += 1;
                eprintln!("{}: {}", t.domain, unexpected(other));
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}: {e:#}", t.domain);
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} engines unreachable", targets.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathres::tests::mk;

    #[test]
    fn ls_lines() {
        let mut f = mk(7, 1, "main.rs", Kind::File);
        f.entry.size = 42;
        f.entry.mode = 0o644;
        f.entry.mtime_ns = 0;
        assert_eq!(
            format_ls_line(&f, false),
            "-rw-r--r--           42 1970-01-01 00:00:00 main.rs"
        );
        assert!(format_ls_line(&f, true).contains("       7 main.rs"));
        let mut l = mk(8, 1, "link", Kind::Symlink);
        l.entry.mode = 0o777;
        l.entry.symlink_target = Some("/etc/passwd".into());
        l.symlink_blocked = true;
        let line = format_ls_line(&l, false);
        assert!(line.starts_with("-rwxrwxrwx"));
        assert!(line.ends_with("link -> /etc/passwd (blocked: points outside the root)"));
        let mut d = mk(9, 1, "node_modules", Kind::Dir);
        d.entry.lazy = true;
        assert!(format_ls_line(&d, false).starts_with('d'));
        assert!(format_ls_line(&d, false).ends_with("node_modules/ (lazy)"));
    }

    #[test]
    fn stat_text() {
        let mut f = mk(7, 1, "a", Kind::File);
        f.display_name = "a (Unlatch 1)".into();
        let s = format_stat(&f);
        assert!(s.contains("name:         a (Unlatch 1)"));
        assert!(s.contains("name on VM:   a"));
        assert!(s.contains("version:      content=1 meta=1"));
    }

    #[test]
    fn vm_controlled_text_cannot_inject_terminal_escapes() {
        // A VM file name / symlink target is untrusted: ESC, BEL, CR, C1 controls and bidi
        // overrides must reach the terminal escaped, never raw (OSC 52 clipboard write, erase
        // line + carriage return hiding the "blocked" warning).
        let osc52 = "x\u{1b}]52;c;Y3VybCBldmlsLnNofHNo\u{7}";
        let mut l = mk(8, 1, osc52, Kind::Symlink);
        l.entry.symlink_target = Some("/etc\u{1b}[2K\rpasswd\u{9b}31m\u{202e}".into());
        l.symlink_blocked = true;
        let line = format_ls_line(&l, true);
        let st = format_stat(&l);
        l.display_name = "y\u{7f}\n".into();
        let st2 = format_stat(&l);
        for out in [&line, &st, &st2] {
            assert!(
                !out.chars()
                    .any(|c| c != '\n' && (c.is_control() || c == '\u{202e}')),
                "raw control char in {out:?}"
            );
        }
        assert!(
            line.contains("x\\x1b]52;c;Y3VybCBldmlsLnNofHNo\\x07"),
            "{line}"
        );
        assert!(line.ends_with(
            " -> /etc\\x1b[2K\\rpasswd\\u{9b}31m\\u{202e} (blocked: points outside the root)"
        ));
        assert!(st2.contains("name:         y\\x7f\\n\n"), "{st2}");
        assert!(st2.contains("name on VM:   x\\x1b]52"), "{st2}");
        // Ordinary names (Unicode included) are untouched; a literal backslash stays readable.
        assert_eq!(
            term_safe("naïve 日本 (Unlatch 2).txt"),
            "naïve 日本 (Unlatch 2).txt"
        );
        assert_eq!(term_safe("a\\b"), "a\\\\b");
        // `unlatch status` prints VM-reported text too.
        let st = EngineStatus {
            state: ConnState::Offline {
                error: "boom\u{1b}[2J".into(),
                retry_in_ms: 1,
            },
            entries: 0,
            anchor: vec![],
            rtt_us: None,
            cache_bytes: 0,
            pending_uploads: 0,
            server: None,
        };
        let s = format_status("dev", &st);
        assert!(!s.contains('\u{1b}'), "{s:?}");
    }

    #[test]
    fn status_text() {
        let st = EngineStatus {
            state: ConnState::Offline {
                error: "no route".into(),
                retry_in_ms: 500,
            },
            entries: 10,
            anchor: vec![],
            rtt_us: Some(40_000),
            cache_bytes: 1 << 20,
            pending_uploads: 2,
            server: None,
        };
        let s = format_status("dev", &st);
        assert!(s.starts_with("dev: offline: no route (retry in 500 ms)"));
        assert!(s.contains("rtt:              40.0 ms"));
        assert!(s.contains("pending uploads:  2"));
    }

    #[test]
    fn target_selection() {
        let t = targets(Some(Path::new("/tmp/s.sock")), Some("dev"), None, false).unwrap();
        assert_eq!(t[0].domain, "dev");
        assert!(targets(Some(Path::new("/tmp/s.sock")), None, None, false).is_err());
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("agent.json");
        std::fs::write(
            &cfg,
            r#"{"domains":[{"name":"a","transport":{"ssh":{"destination":"h"}},"remote_root":"/r","socket":"/tmp/a.sock"},
                           {"name":"b","transport":{"ssh":{"destination":"h"}},"remote_root":"/r","socket":"/tmp/b.sock"}]}"#,
        )
        .unwrap();
        assert_eq!(targets(None, None, Some(&cfg), true).unwrap().len(), 2);
        assert!(
            targets(None, None, Some(&cfg), false).is_err(),
            "ambiguous for single-domain commands"
        );
        assert_eq!(
            targets(None, Some("b"), Some(&cfg), false).unwrap()[0].socket,
            PathBuf::from("/tmp/b.sock")
        );
    }
}
