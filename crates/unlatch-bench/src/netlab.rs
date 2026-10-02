//! Kernel-shaped network lab (review D24, §2(f)6).
//!
//! A userspace delay proxy cannot show bandwidth, queueing, congestion windows or slow start after
//! idle (design review D24), so the bench shapes traffic with `tc netem` inside an unprivileged user+net
//! namespace (`unshare -rn`). Both systems under test (sshfs and Unlatch) cross the *same* shaped
//! TCP connection:
//!
//! ```text
//!  client process (sshfs / unlatch engine / unlatch mount)          host netns, unshaped
//!     │ stdio
//!  `unlatch-bench netlab connect <client.sock> <service>`           host netns
//!     │ unix socket (filesystem path, visible from both netns)
//!  ┌─ holder (`unshare -rn unlatch-bench netlab holder`) ───────────────────────────────┐
//!  │  client relay ──TCP 127.0.0.1:P over `lo` (mtu 1500, netem delay+rate)── server  │
//!  │                                                                         relay    │
//!  └──────────────────────────────────────────────────────────────────────────┬───────┘
//!     │ unix socket  <dir>/svc/<service>.sock
//!  ServiceHost (host netns, normal credentials): spawns the server command per connection
//!  (`unlatchd stdio …`, `sftp-server`, `cat file`, …) with the socket as its stdin/stdout.
//! ```
//!
//! Why not `nsenter` + run the servers inside the namespace: inside the user namespace our uid
//! is mapped to 0, so `faccessat` answers as if root (CAP_DAC_OVERRIDE over our own files) and
//! abstract unix sockets (unlatchd serve) would be namespaced. Keeping the servers outside and
//! only relaying inside removes those artefacts; the shaped path is identical. The review asks
//! for "nsenter … or equivalent": the unix-socket hop is the equivalent (filesystem-path unix
//! sockets are not network-namespaced).
//!
//! RTT: netem on `lo` delays every packet on egress, and both directions egress `lo`, so the
//! one-way delay is `rtt / 2`. Measured by [`calibrate`]. Note that on `lo` one qdisc carries
//! both directions, so a simultaneous upload and download share the rate.
//!
//! MTU is forced to 1500: loopback's default 64 KiB MTU makes the initial congestion window
//! ~640 KB and hides slow start entirely (design review D24).
//!
//! The first line a client writes is `HNL1 <service>\n`; the server relay strips it and connects
//! to `<dir>/svc/<service>.sock`. `@stats` is answered by the holder with per-service byte
//! counters (JSON), so scenarios can measure bytes moved (T14, T16).

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Header magic of the service-selection line.
const HEADER_MAGIC: &str = "HNL1";
/// Pseudo-service answered by the holder itself.
pub const STATS_SERVICE: &str = "@stats";
const MAX_HEADER: usize = 256;
const PUMP_BUF: usize = 128 * 1024;
/// Extra queueing allowed at the bottleneck beyond the delay line, as time at line rate.
/// A modest buffer (not a multi-second bufferbloat queue) so latency-under-load numbers (T13)
/// reflect the system under test rather than a pathological link.
const BOTTLENECK_BUFFER: Duration = Duration::from_millis(50);
const WIRE_PACKET: u64 = 1514;

// ------------------------------------------------------------------------------------------
// Profiles
// ------------------------------------------------------------------------------------------

/// Bottleneck queue discipline of a profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Queue {
    /// netem's own FIFO does delay *and* rate: the delay line plus [`BOTTLENECK_BUFFER`] at line
    /// rate (×1.5 for the reverse direction's ACKs). A drop-tail buffer of ≈ 1.25 BDP at RTT
    /// 40 ms — what a plain TCP bulk transfer fills (bufferbloat).
    #[default]
    Netem,
    /// A modern AQM bottleneck: netem delays (no rate), then `tbf` shapes to the rate and its
    /// child `fq_codel` (Linux's default qdisc; flow isolation + CoDel, target 5 ms) manages the
    /// queue. Name suffix `-fqcodel`.
    FqCodel,
}

/// One network condition: round-trip time and (optional) rate limit, applied symmetrically,
/// plus the bottleneck queue discipline and whether the session runs over real ssh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub rtt_ms: u32,
    /// `None` = no rate limit (only the delay is applied).
    pub rate_mbit: Option<u32>,
    #[serde(default)]
    pub queue: Queue,
    /// Sessions cross a real `ssh` ⇄ `sshd` pair inside the shaped link (name suffix `-ssh`):
    /// both sshfs and Unlatch then pay ssh crypto and the ssh channel window.
    #[serde(default)]
    pub ssh: bool,
}

impl Profile {
    pub fn new(rtt_ms: u32, rate_mbit: Option<u32>) -> Profile {
        let mut p = Profile {
            name: String::new(),
            rtt_ms,
            rate_mbit,
            queue: Queue::Netem,
            ssh: false,
        };
        p.rename();
        p
    }

    pub fn with_queue(mut self, queue: Queue) -> Profile {
        self.queue = queue;
        self.rename();
        self
    }

    pub fn with_ssh(mut self, ssh: bool) -> Profile {
        self.ssh = ssh;
        self.rename();
        self
    }

    fn rename(&mut self) {
        let mut name = match self.rate_mbit {
            Some(r) => format!("rtt{}-bw{r}", self.rtt_ms),
            None => format!("rtt{}", self.rtt_ms),
        };
        if self.queue == Queue::FqCodel {
            name.push_str("-fqcodel");
        }
        if self.ssh {
            name.push_str("-ssh");
        }
        self.name = name;
    }

    /// The same link without the ssh flag (what the namespace itself is configured from).
    pub fn link_name(&self) -> String {
        self.clone().with_ssh(false).name
    }

    /// Parse `rtt<ms>[-bw<mbit>][-fqcodel][-ssh]`.
    pub fn parse(s: &str) -> Result<Profile> {
        let s = s.trim();
        let mut parts = s.split('-');
        let rtt = parts
            .next()
            .and_then(|p| p.strip_prefix("rtt"))
            .ok_or_else(|| anyhow!("profile {s:?}: expected rtt<ms>[-bw<mbit>][-fqcodel][-ssh]"))?;
        let rtt_ms: u32 = rtt
            .parse()
            .with_context(|| format!("profile {s:?}: bad rtt"))?;
        if rtt_ms > 10_000 {
            bail!("profile {s:?}: rtt too large");
        }
        let mut rate_mbit = None;
        let mut queue = Queue::Netem;
        let mut ssh = false;
        for (i, part) in parts.enumerate() {
            if let Some(b) = part.strip_prefix("bw") {
                if i != 0 {
                    bail!("profile {s:?}: bw must follow rtt");
                }
                let r: u32 = b
                    .parse()
                    .with_context(|| format!("profile {s:?}: bad bandwidth"))?;
                if r == 0 {
                    bail!("profile {s:?}: bandwidth must be > 0");
                }
                rate_mbit = Some(r);
            } else if part == "fqcodel" {
                queue = Queue::FqCodel;
            } else if part == "ssh" {
                ssh = true;
            } else {
                bail!("profile {s:?}: unknown part {part:?}");
            }
        }
        if queue == Queue::FqCodel && rate_mbit.is_none() {
            bail!("profile {s:?}: -fqcodel needs a rate (-bw<mbit>)");
        }
        let p = Profile::new(rtt_ms, rate_mbit)
            .with_queue(queue)
            .with_ssh(ssh);
        if p.name != s {
            bail!("profile {s:?}: write it as {:?}", p.name);
        }
        Ok(p)
    }

    /// Comma-separated list.
    pub fn parse_list(s: &str) -> Result<Vec<Profile>> {
        s.split(',')
            .filter(|p| !p.trim().is_empty())
            .map(Profile::parse)
            .collect()
    }

    /// The review's full matrix: {RTT 0, 40, 100 ms} × {20, 50, 200 Mbit/s}.
    pub fn standard() -> Vec<Profile> {
        let mut v = Vec::new();
        for rtt in [0, 40, 100] {
            for bw in [20, 50, 200] {
                v.push(Profile::new(rtt, Some(bw)));
            }
        }
        v
    }

    /// Default for `--quick`: a fast LAN and the design's reference WAN.
    pub fn quick() -> Vec<Profile> {
        vec![Profile::new(0, Some(200)), Profile::new(40, Some(50))]
    }

    pub fn one_way_delay_us(&self) -> u64 {
        u64::from(self.rtt_ms) * 1000 / 2
    }

    pub fn rate_bytes_per_sec(&self) -> Option<u64> {
        self.rate_mbit.map(|r| u64::from(r) * 1_000_000 / 8)
    }

    /// netem `limit` (packets) of one direction: the delay line plus [`BOTTLENECK_BUFFER`] at
    /// line rate.
    pub fn netem_limit(&self) -> u64 {
        match self.rate_bytes_per_sec() {
            Some(bps) => {
                let secs = self.one_way_delay_us() as f64 / 1e6 + BOTTLENECK_BUFFER.as_secs_f64();
                // ×1.5: the ACKs of the reverse direction share this queue.
                (bps as f64 * secs / WIRE_PACKET as f64 * 1.5).ceil() as u64 + 64
            }
            None => 100_000,
        }
    }

    /// Does this profile shape anything at all?
    pub fn shaped(&self) -> bool {
        self.rtt_ms > 0 || self.rate_mbit.is_some()
    }

    /// Every `tc` invocation that shapes `lo` for this profile, given the server relay's TCP
    /// port. Each direction gets its **own** bottleneck, like a real access link (separate
    /// uplink and downlink queues): a `prio` root classifies packets from the port (server →
    /// client, "down") into band 1 and packets to it ("up") into band 2; everything else takes
    /// the unshaped band 3. Each shaped band carries one direction's data plus the other
    /// direction's ACKs, and holds either one `netem` (delay + rate + drop-tail limit) or, for
    /// [`Queue::FqCodel`], `netem` (delay only) → `tbf` (rate) → `fq_codel`.
    pub fn tc_commands(&self, port: u16) -> Vec<Vec<String>> {
        if !self.shaped() {
            return Vec::new();
        }
        let s = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let port = port.to_string();
        let mut v = vec![
            s(&[
                "qdisc", "add", "dev", "lo", "root", "handle", "1:", "prio", "bands", "3",
                "priomap", "2", "2", "2", "2", "2", "2", "2", "2", "2", "2", "2", "2", "2", "2",
                "2", "2",
            ]),
            s(&[
                "filter", "add", "dev", "lo", "parent", "1:", "protocol", "ip", "prio", "1", "u32",
                "match", "ip", "sport", &port, "0xffff", "flowid", "1:1",
            ]),
            s(&[
                "filter", "add", "dev", "lo", "parent", "1:", "protocol", "ip", "prio", "2", "u32",
                "match", "ip", "dport", &port, "0xffff", "flowid", "1:2",
            ]),
        ];
        for (band, h) in [("1:1", 10u32), ("1:2", 20u32)] {
            let (h0, h1, h2) = (
                format!("{h}:"),
                format!("{}:", h + 1),
                format!("{}:", h + 2),
            );
            let mut netem = s(&[
                "qdisc", "add", "dev", "lo", "parent", band, "handle", &h0, "netem",
            ]);
            if self.rtt_ms > 0 {
                netem.push("delay".into());
                netem.push(format!("{}us", self.one_way_delay_us()));
            }
            match (self.queue, self.rate_mbit, self.rate_bytes_per_sec()) {
                (Queue::FqCodel, Some(r), Some(bps)) => {
                    // Delay line only (never drops); rate and queue management in the children.
                    netem.extend(s(&["limit", "100000"]));
                    v.push(netem);
                    // Bucket of ~1 ms at line rate (≥ 2 packets).
                    let burst = (bps / 1000).max(2 * WIRE_PACKET).to_string();
                    v.push(s(&[
                        "qdisc",
                        "add",
                        "dev",
                        "lo",
                        "parent",
                        &format!("{h}:1"),
                        "handle",
                        &h1,
                        "tbf",
                        "rate",
                        &format!("{r}mbit"),
                        "burst",
                        &burst,
                        "latency",
                        "200ms",
                    ]));
                    v.push(s(&[
                        "qdisc",
                        "add",
                        "dev",
                        "lo",
                        "parent",
                        &format!("{}:1", h + 1),
                        "handle",
                        &h2,
                        "fq_codel",
                    ]));
                }
                _ => {
                    if let Some(r) = self.rate_mbit {
                        netem.push("rate".into());
                        netem.push(format!("{r}mbit"));
                    }
                    netem.push("limit".into());
                    netem.push(self.netem_limit().to_string());
                    v.push(netem);
                }
            }
        }
        v
    }
}

// ------------------------------------------------------------------------------------------
// Header line
// ------------------------------------------------------------------------------------------

pub fn valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'))
        && !name.starts_with('.')
}

fn header_line(service: &str) -> String {
    format!("{HEADER_MAGIC} {service}\n")
}

/// Read the header line byte by byte (so no protocol bytes after it are consumed).
fn read_header<R: Read>(r: &mut R) -> Result<String> {
    let mut line = Vec::with_capacity(32);
    let mut b = [0u8; 1];
    loop {
        let n = r.read(&mut b)?;
        if n == 0 {
            bail!("eof before netlab header");
        }
        if b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
        if line.len() > MAX_HEADER {
            bail!("netlab header too long");
        }
    }
    let line = String::from_utf8(line).context("netlab header not utf-8")?;
    let svc = line
        .strip_prefix(HEADER_MAGIC)
        .and_then(|s| s.strip_prefix(' '))
        .ok_or_else(|| anyhow!("bad netlab header {line:?}"))?;
    if !valid_service_name(svc) {
        bail!("bad service name {svc:?}");
    }
    Ok(svc.to_string())
}

// ------------------------------------------------------------------------------------------
// Byte pumps
// ------------------------------------------------------------------------------------------

trait HalfClose {
    fn close_write(&self);
    fn close_both(&self);
}

impl HalfClose for UnixStream {
    fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
    fn close_both(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

impl HalfClose for TcpStream {
    fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
    fn close_both(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

/// Copy `r` → `w` until EOF, then half-close `w`. On an error both sides are torn down.
fn pump<R, W>(mut r: R, mut w: W, counter: Option<Arc<AtomicU64>>)
where
    R: Read + HalfClose,
    W: Write + HalfClose,
{
    let mut buf = vec![0u8; PUMP_BUF];
    loop {
        match r.read(&mut buf) {
            Ok(0) => {
                w.close_write();
                return;
            }
            Ok(n) => {
                if w.write_all(&buf[..n]).is_err() {
                    r.close_both();
                    w.close_both();
                    return;
                }
                if let Some(c) = &counter {
                    c.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                r.close_both();
                w.close_both();
                return;
            }
        }
    }
}

/// Bidirectional relay; returns when both directions are done.
fn relay<A, B>(a: A, b: B, up: Option<Arc<AtomicU64>>, down: Option<Arc<AtomicU64>>) -> Result<()>
where
    A: Read + Write + HalfClose + Send + 'static + TryCloneStream,
    B: Read + Write + HalfClose + Send + 'static + TryCloneStream,
{
    let a2 = a.try_clone_stream()?;
    let b2 = b.try_clone_stream()?;
    let t = std::thread::Builder::new()
        .name("netlab-pump".into())
        .spawn(move || pump(a2, b2, up))?;
    pump(b, a, down);
    let _ = t.join();
    Ok(())
}

trait TryCloneStream: Sized {
    fn try_clone_stream(&self) -> std::io::Result<Self>;
}
impl TryCloneStream for UnixStream {
    fn try_clone_stream(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
}
impl TryCloneStream for TcpStream {
    fn try_clone_stream(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
}

// ------------------------------------------------------------------------------------------
// Holder (runs inside `unshare -rn`)
// ------------------------------------------------------------------------------------------

#[derive(Default)]
struct SvcCounters {
    up: Arc<AtomicU64>,
    down: Arc<AtomicU64>,
    conns: AtomicU64,
}

/// Per-service byte counters as seen on the shaped link (`@stats`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SvcStats {
    /// Client → server payload bytes (excluding the header line).
    pub up: u64,
    /// Server → client payload bytes.
    pub down: u64,
    pub conns: u64,
}

pub struct HolderArgs {
    pub dir: PathBuf,
    pub profile: Profile,
    /// Value for `net.ipv4.tcp_slow_start_after_idle` in the namespace (`None` = leave, i.e. 1).
    pub slow_start_after_idle: Option<u8>,
}

fn find_tool(name: &str) -> Result<PathBuf> {
    for d in ["/usr/sbin", "/sbin", "/usr/bin", "/bin"] {
        let p = Path::new(d).join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!("{name} not found in /usr/sbin, /sbin, /usr/bin, /bin")
}

fn run_tool(tool: &Path, args: &[String]) -> Result<()> {
    let out = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn {}", tool.display()))?;
    if !out.status.success() {
        bail!(
            "{} {} failed: {}",
            tool.display(),
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Configure the namespace's loopback (called inside the namespace).
fn configure_namespace(args: &HolderArgs, port: u16) -> Result<()> {
    let ip = find_tool("ip")?;
    run_tool(
        &ip,
        &["link", "set", "lo", "up", "mtu", "1500"].map(String::from),
    )?;
    // Real links carry MTU-sized packets through the bottleneck; with TSO/GSO on, netem would
    // queue and pace 64 KiB super-packets instead. Best effort: ethtool may be missing.
    if let Ok(ethtool) = find_tool("ethtool") {
        let off = ["-K", "lo", "tso", "off", "gso", "off", "gro", "off"].map(String::from);
        if let Err(e) = run_tool(&ethtool, &off) {
            eprintln!("netlab: {e:#} (continuing with offloads on)");
        }
    }
    let cmds = args.profile.tc_commands(port);
    if !cmds.is_empty() {
        let tc = find_tool("tc")?;
        for c in &cmds {
            run_tool(&tc, c)?;
        }
    }
    if let Some(v) = args.slow_start_after_idle {
        std::fs::write(
            "/proc/sys/net/ipv4/tcp_slow_start_after_idle",
            format!("{v}\n"),
        )
        .context("set tcp_slow_start_after_idle in netns")?;
    }
    Ok(())
}

fn client_sock_path(dir: &Path) -> PathBuf {
    dir.join("client.sock")
}

pub fn svc_dir(dir: &Path) -> PathBuf {
    dir.join("svc")
}

pub fn svc_sock_path(dir: &Path, service: &str) -> PathBuf {
    svc_dir(dir).join(format!("{service}.sock"))
}

/// Entry point of `unlatch-bench netlab holder`. Prints `ready <port>` once serving and exits
/// when its stdin reaches EOF (the owning [`Netlab`] went away).
pub fn holder_main(args: HolderArgs) -> Result<()> {
    // `lo` must be up before binding; the shaping is keyed on the server relay's port.
    let ip = find_tool("ip")?;
    run_tool(&ip, &["link", "set", "lo", "up"].map(String::from))?;
    let tcp = TcpListener::bind("127.0.0.1:0").context("bind tcp in netns")?;
    let port = tcp.local_addr()?.port();
    configure_namespace(&args, port)?;
    std::fs::create_dir_all(svc_dir(&args.dir))?;
    let csock = client_sock_path(&args.dir);
    let _ = std::fs::remove_file(&csock);
    let unix = UnixListener::bind(&csock).with_context(|| format!("bind {}", csock.display()))?;

    let counters: Arc<Mutex<BTreeMap<String, Arc<SvcCounters>>>> = Arc::default();

    // Server side of the shaped link.
    {
        let dir = args.dir.clone();
        let counters = counters.clone();
        std::thread::Builder::new()
            .name("netlab-srv-accept".into())
            .spawn(move || {
                for conn in tcp.incoming() {
                    let Ok(conn) = conn else { continue };
                    let dir = dir.clone();
                    let counters = counters.clone();
                    let _ = std::thread::Builder::new()
                        .name("netlab-srv".into())
                        .spawn(move || serve_shaped_conn(conn, &dir, &counters));
                }
            })?;
    }
    // Client side of the shaped link.
    std::thread::Builder::new()
        .name("netlab-cli-accept".into())
        .spawn(move || {
            for conn in unix.incoming() {
                let Ok(conn) = conn else { continue };
                let _ = std::thread::Builder::new()
                    .name("netlab-cli".into())
                    .spawn(move || {
                        if let Ok(tcp) = TcpStream::connect(("127.0.0.1", port)) {
                            let _ = tcp.set_nodelay(true);
                            let _ = relay(conn, tcp, None, None);
                        }
                    });
            }
        })?;

    {
        let mut out = std::io::stdout().lock();
        writeln!(out, "ready {port}")?;
        out.flush()?;
    }
    // Lifetime = the owner's stdin pipe.
    let mut sink = [0u8; 64];
    let mut stdin = std::io::stdin().lock();
    loop {
        match stdin.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    let _ = std::fs::remove_file(&csock);
    Ok(())
}

fn serve_shaped_conn(
    mut conn: TcpStream,
    dir: &Path,
    counters: &Mutex<BTreeMap<String, Arc<SvcCounters>>>,
) {
    let _ = conn.set_nodelay(true);
    let Ok(service) = read_header(&mut conn) else {
        let _ = conn.shutdown(Shutdown::Both);
        return;
    };
    if service == STATS_SERVICE {
        let snapshot: BTreeMap<String, SvcStats> = match counters.lock() {
            Ok(m) => m
                .iter()
                .map(|(k, c)| {
                    (
                        k.clone(),
                        SvcStats {
                            up: c.up.load(Ordering::Relaxed),
                            down: c.down.load(Ordering::Relaxed),
                            conns: c.conns.load(Ordering::Relaxed),
                        },
                    )
                })
                .collect(),
            Err(_) => BTreeMap::new(),
        };
        if let Ok(js) = serde_json::to_vec(&snapshot) {
            let _ = conn.write_all(&js);
        }
        let _ = conn.shutdown(Shutdown::Both);
        return;
    }
    let c = match counters.lock() {
        Ok(mut m) => m.entry(service.clone()).or_default().clone(),
        Err(_) => return,
    };
    c.conns.fetch_add(1, Ordering::Relaxed);
    match UnixStream::connect(svc_sock_path(dir, &service)) {
        Ok(backend) => {
            let _ = relay(conn, backend, Some(c.up.clone()), Some(c.down.clone()));
        }
        Err(_) => {
            let _ = conn.shutdown(Shutdown::Both);
        }
    }
}

// ------------------------------------------------------------------------------------------
// Client side
// ------------------------------------------------------------------------------------------

/// Connect to `service` through the shaped link (in-process).
pub fn connect_via(client_sock: &Path, service: &str) -> Result<UnixStream> {
    if !valid_service_name(service) {
        bail!("bad service name {service:?}");
    }
    let mut s = UnixStream::connect(client_sock)
        .with_context(|| format!("connect {}", client_sock.display()))?;
    s.write_all(header_line(service).as_bytes())?;
    Ok(s)
}

/// Entry point of `unlatch-bench netlab connect <client.sock> <service> [ignored…]`: bridge
/// stdin/stdout to `service`. Extra arguments are ignored so that the command can be used as
/// sshfs's `ssh_command` (sshfs appends `-x -a … host -s sftp`).
pub fn connect_main(client_sock: &Path, service: &str) -> Result<i32> {
    let sock = connect_via(client_sock, service)?;
    let stdin = File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let stdout = File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    let sock_w = sock.try_clone()?;
    std::thread::Builder::new()
        .name("netlab-stdin".into())
        .spawn(move || {
            let mut stdin = stdin;
            let mut sock_w = sock_w;
            let mut buf = vec![0u8; PUMP_BUF];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = sock_w.shutdown(Shutdown::Write);
                        return;
                    }
                    Ok(n) => {
                        if sock_w.write_all(&buf[..n]).is_err() {
                            return;
                        }
                    }
                }
            }
        })?;
    let mut sock = sock;
    let mut stdout = stdout;
    let mut buf = vec![0u8; PUMP_BUF];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // The server closed: the session is over even if our stdin is still open.
    Ok(0)
}

/// A running shaped namespace. Dropping it tears the namespace down.
pub struct Netlab {
    pub profile: Profile,
    dir: PathBuf,
    bench_exe: PathBuf,
    holder: Child,
    holder_stdin: Option<ChildStdin>,
    port: u16,
}

impl Netlab {
    /// Create the namespace for `profile`, using `dir` for its sockets (created if missing;
    /// keep it short: unix socket paths are limited to 108 bytes).
    pub fn start(profile: &Profile, dir: &Path, bench_exe: &Path) -> Result<Netlab> {
        Self::start_with(profile, dir, bench_exe, None)
    }

    pub fn start_with(
        profile: &Profile,
        dir: &Path,
        bench_exe: &Path,
        slow_start_after_idle: Option<u8>,
    ) -> Result<Netlab> {
        std::fs::create_dir_all(svc_dir(dir))?;
        let sock = client_sock_path(dir);
        if sock.as_os_str().len() >= 100 {
            bail!(
                "netlab dir path too long for unix sockets: {}",
                dir.display()
            );
        }
        let unshare = find_tool("unshare")?;
        let mut cmd = Command::new(unshare);
        cmd.arg("-rn")
            .arg(bench_exe)
            .arg("netlab")
            .arg("holder")
            .arg("--dir")
            .arg(dir)
            .arg("--profile")
            .arg(&profile.name);
        if let Some(v) = slow_start_after_idle {
            cmd.arg("--slow-start-after-idle").arg(v.to_string());
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut holder = cmd.spawn().context("spawn `unshare -rn` netlab holder")?;
        let stdout = holder
            .stdout
            .take()
            .ok_or_else(|| anyhow!("holder stdout"))?;
        let stderr = holder.stderr.take();
        let holder_stdin = holder.stdin.take();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let r = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = tx.send(r);
        });
        let line = match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(l)) => l,
            other => {
                let _ = holder.kill();
                let mut err = String::new();
                if let Some(mut e) = stderr {
                    let _ = e.read_to_string(&mut err);
                }
                let _ = holder.wait();
                bail!("netlab holder did not start ({other:?}): {}", err.trim());
            }
        };
        let port: u16 = match line
            .trim()
            .strip_prefix("ready ")
            .and_then(|p| p.parse().ok())
        {
            Some(p) => p,
            None => {
                let _ = holder.kill();
                let mut err = String::new();
                if let Some(mut e) = stderr {
                    let _ = e.read_to_string(&mut err);
                }
                let _ = holder.wait();
                bail!("netlab holder failed: {:?} {}", line.trim(), err.trim());
            }
        };
        if let Some(e) = stderr {
            // Drain so the holder never blocks on a full stderr pipe.
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut { e }, &mut std::io::sink());
            });
        }
        Ok(Netlab {
            profile: profile.clone(),
            dir: dir.to_path_buf(),
            bench_exe: bench_exe.to_path_buf(),
            holder,
            holder_stdin,
            port,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn client_sock(&self) -> PathBuf {
        client_sock_path(&self.dir)
    }

    pub fn svc_dir(&self) -> PathBuf {
        svc_dir(&self.dir)
    }

    /// PID of the namespace holder (for `nsenter --user --net -t <pid> --preserve-credentials`).
    pub fn holder_pid(&self) -> u32 {
        self.holder.id()
    }

    /// TCP port of the server relay inside the namespace.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn connect(&self, service: &str) -> Result<UnixStream> {
        connect_via(&self.client_sock(), service)
    }

    /// argv of a stdio bridge to `service` (for `Transport::Command`, sshfs `ssh_command`, …).
    pub fn connect_argv(&self, service: &str) -> Vec<String> {
        connect_argv(&self.bench_exe, &self.client_sock(), service)
    }

    pub fn stats(&self) -> Result<BTreeMap<String, SvcStats>> {
        stats_via(&self.client_sock())
    }
}

impl Drop for Netlab {
    fn drop(&mut self) {
        drop(self.holder_stdin.take());
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.holder.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.holder.kill();
        let _ = self.holder.wait();
    }
}

pub fn connect_argv(bench_exe: &Path, client_sock: &Path, service: &str) -> Vec<String> {
    vec![
        bench_exe.display().to_string(),
        "netlab".into(),
        "connect".into(),
        client_sock.display().to_string(),
        service.into(),
    ]
}

/// Byte counters of every service seen on the shaped link.
pub fn stats_via(client_sock: &Path) -> Result<BTreeMap<String, SvcStats>> {
    let mut s = connect_via(client_sock, STATS_SERVICE)?;
    s.shutdown(Shutdown::Write)?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    serde_json::from_slice(&buf).context("parse @stats")
}

// ------------------------------------------------------------------------------------------
// Services (server side, host netns)
// ------------------------------------------------------------------------------------------

/// What a service does with each connection.
#[derive(Clone, Debug)]
pub enum Service {
    /// Spawn `argv` with the connection as stdin and stdout.
    Command {
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<PathBuf>,
        /// Append the children's stderr here (`None` = discard).
        stderr: Option<PathBuf>,
    },
    /// Echo every byte back.
    Echo,
    /// Repeatedly: read `u64 LE n`, write `n` bytes.
    Source,
    /// Repeatedly: read `u64 LE n`, read `n` bytes, write `u64 LE n`.
    Sink,
}

impl Service {
    pub fn command(argv: Vec<String>) -> Service {
        Service::Command {
            argv,
            env: Vec::new(),
            cwd: None,
            stderr: None,
        }
    }
}

/// Serves one [`Service`] on `<dir>/svc/<name>.sock`. Dropping it stops the listener and kills
/// every child it spawned.
pub struct ServiceHost {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    children: Arc<Mutex<Vec<Child>>>,
}

impl ServiceHost {
    pub fn start(netlab_dir: &Path, name: &str, svc: Service) -> Result<ServiceHost> {
        if !valid_service_name(name) || name.starts_with('@') {
            bail!("bad service name {name:?}");
        }
        std::fs::create_dir_all(svc_dir(netlab_dir))?;
        let path = svc_sock_path(netlab_dir, name);
        let _ = std::fs::remove_file(&path);
        let listener =
            UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let children: Arc<Mutex<Vec<Child>>> = Arc::default();
        let accept = {
            let stop = stop.clone();
            let children = children.clone();
            std::thread::Builder::new()
                .name(format!("svc-{name}"))
                .spawn(move || {
                    for conn in listener.incoming() {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        let Ok(conn) = conn else { continue };
                        handle_service_conn(conn, &svc, &children);
                    }
                })?
        };
        Ok(ServiceHost {
            path,
            stop,
            accept: Some(accept),
            children,
        })
    }

    /// PIDs of children still running (e.g. the `unlatchd stdio` serving the current session).
    pub fn child_pids(&self) -> Vec<u32> {
        match self.children.lock() {
            Ok(mut v) => {
                v.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
                v.iter().map(|c| c.id()).collect()
            }
            Err(_) => Vec::new(),
        }
    }

    /// Kill every running child (simulates a server crash / connection kill at the VM end).
    pub fn kill_children(&self) {
        if let Ok(mut v) = self.children.lock() {
            for c in v.iter_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
            v.clear();
        }
    }
}

impl Drop for ServiceHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = UnixStream::connect(&self.path);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
        self.kill_children();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn handle_service_conn(conn: UnixStream, svc: &Service, children: &Mutex<Vec<Child>>) {
    match svc {
        Service::Command {
            argv,
            env,
            cwd,
            stderr,
        } => {
            let Some((prog, args)) = argv.split_first() else {
                return;
            };
            let (Ok(a), Ok(b)) = (conn.try_clone(), conn.try_clone()) else {
                return;
            };
            let stdin: OwnedFd = a.into();
            let stdout: OwnedFd = b.into();
            let mut cmd = Command::new(prog);
            cmd.args(args)
                .stdin(Stdio::from(stdin))
                .stdout(Stdio::from(stdout));
            for (k, v) in env {
                cmd.env(k, v);
            }
            if let Some(d) = cwd {
                cmd.current_dir(d);
            }
            match stderr.as_ref().and_then(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .ok()
            }) {
                Some(f) => cmd.stderr(Stdio::from(f)),
                None => cmd.stderr(Stdio::null()),
            };
            drop(conn);
            if let Ok(child) = cmd.spawn() {
                if let Ok(mut v) = children.lock() {
                    v.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
                    v.push(child);
                }
            }
        }
        Service::Echo => {
            let _ = std::thread::Builder::new()
                .name("svc-echo".into())
                .spawn(move || {
                    if let Ok(w) = conn.try_clone() {
                        pump(conn, w, None);
                    }
                });
        }
        Service::Source => {
            let _ = std::thread::Builder::new()
                .name("svc-source".into())
                .spawn(move || {
                    let _ = source_loop(conn);
                });
        }
        Service::Sink => {
            let _ = std::thread::Builder::new()
                .name("svc-sink".into())
                .spawn(move || {
                    let _ = sink_loop(conn);
                });
        }
    }
}

fn read_u64<R: Read>(r: &mut R) -> std::io::Result<Option<u64>> {
    let mut b = [0u8; 8];
    let mut got = 0;
    while got < 8 {
        let n = r.read(&mut b[got..])?;
        if n == 0 {
            return if got == 0 {
                Ok(None)
            } else {
                Err(std::io::ErrorKind::UnexpectedEof.into())
            };
        }
        got += n;
    }
    Ok(Some(u64::from_le_bytes(b)))
}

fn source_loop(mut s: UnixStream) -> std::io::Result<()> {
    // Incompressible-looking but cheap payload.
    let block: Vec<u8> = (0..PUMP_BUF as u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    while let Some(mut n) = read_u64(&mut s)? {
        while n > 0 {
            let k = n.min(block.len() as u64) as usize;
            s.write_all(&block[..k])?;
            n -= k as u64;
        }
    }
    Ok(())
}

fn sink_loop(mut s: UnixStream) -> std::io::Result<()> {
    let mut buf = vec![0u8; PUMP_BUF];
    while let Some(n) = read_u64(&mut s)? {
        let mut left = n;
        while left > 0 {
            let k = left.min(buf.len() as u64) as usize;
            let got = s.read(&mut buf[..k])?;
            if got == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            left -= got as u64;
        }
        s.write_all(&n.to_le_bytes())?;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// Calibration
// ------------------------------------------------------------------------------------------

/// Achieved link characteristics of one profile, measured through the shaped path.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Calibration {
    pub profile: String,
    /// Median application-level round trip (1-byte echo), ms.
    pub rtt_ms_p50: f64,
    pub rtt_ms_min: f64,
    /// Download / upload goodput, Mbit/s (bulk transfer on a warm connection).
    pub down_mbit: f64,
    pub up_mbit: f64,
    /// Time to fetch 256 KiB on a warm connection vs. after 2 s idle
    /// (`tcp_slow_start_after_idle = 1` collapses cwnd to IW10).
    pub fetch_256k_warm_ms: f64,
    pub fetch_256k_idle_ms: f64,
}

pub fn echo_rtt(s: &mut UnixStream, n: usize) -> Result<Vec<Duration>> {
    let mut out = Vec::with_capacity(n);
    let mut b = [0u8; 1];
    for i in 0..n {
        let t = Instant::now();
        s.write_all(&[i as u8])?;
        s.read_exact(&mut b)?;
        out.push(t.elapsed());
    }
    Ok(out)
}

/// Request `n` bytes from a [`Service::Source`] connection; returns the elapsed time.
pub fn source_fetch(s: &mut UnixStream, n: u64) -> Result<Duration> {
    let t = Instant::now();
    s.write_all(&n.to_le_bytes())?;
    let mut buf = vec![0u8; PUMP_BUF];
    let mut left = n;
    while left > 0 {
        let k = left.min(buf.len() as u64) as usize;
        let got = s.read(&mut buf[..k])?;
        if got == 0 {
            bail!("source closed early");
        }
        left -= got as u64;
    }
    Ok(t.elapsed())
}

/// Push `n` bytes into a [`Service::Sink`] connection; returns the time until acknowledged.
pub fn sink_push(s: &mut UnixStream, n: u64) -> Result<Duration> {
    let block = vec![0x5au8; PUMP_BUF];
    let t = Instant::now();
    s.write_all(&n.to_le_bytes())?;
    let mut left = n;
    while left > 0 {
        let k = left.min(block.len() as u64) as usize;
        s.write_all(&block[..k])?;
        left -= k as u64;
    }
    let mut ack = [0u8; 8];
    s.read_exact(&mut ack)?;
    if u64::from_le_bytes(ack) != n {
        bail!("sink ack mismatch");
    }
    Ok(t.elapsed())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Measure the achieved RTT and bandwidth of `lab`. Starts temporary echo/source/sink services.
/// `bulk_secs` bounds each bandwidth transfer (at the configured rate).
pub fn calibrate(lab: &Netlab, bulk_secs: f64, idle: Duration) -> Result<Calibration> {
    let _echo = ServiceHost::start(lab.dir(), "cal-echo", Service::Echo)?;
    let _src = ServiceHost::start(lab.dir(), "cal-source", Service::Source)?;
    let _sink = ServiceHost::start(lab.dir(), "cal-sink", Service::Sink)?;

    let mut e = lab.connect("cal-echo")?;
    // Connection setup + warm-up.
    echo_rtt(&mut e, 3)?;
    let mut rtts: Vec<f64> = echo_rtt(&mut e, 30)?.into_iter().map(ms).collect();
    rtts.sort_by(f64::total_cmp);

    // Bandwidth: aim for `bulk_secs` of transfer at the nominal rate (cap 256 MiB unshaped).
    let bulk = match lab.profile.rate_bytes_per_sec() {
        Some(bps) => ((bps as f64 * bulk_secs) as u64).max(1 << 20),
        None => 256 << 20,
    };
    let mut src = lab.connect("cal-source")?;
    source_fetch(&mut src, 1 << 20)?; // grow cwnd first
    let d = source_fetch(&mut src, bulk)?;
    let down_mbit = bulk as f64 * 8.0 / d.as_secs_f64() / 1e6;

    let mut sink = lab.connect("cal-sink")?;
    sink_push(&mut sink, 1 << 20)?;
    let d = sink_push(&mut sink, bulk)?;
    let up_mbit = bulk as f64 * 8.0 / d.as_secs_f64() / 1e6;

    // Warm vs after-idle 256 KiB fetch on the (warm) source connection.
    let mut warm = Vec::new();
    for _ in 0..3 {
        warm.push(ms(source_fetch(&mut src, 256 << 10)?));
    }
    warm.sort_by(f64::total_cmp);
    std::thread::sleep(idle);
    let idle_ms = ms(source_fetch(&mut src, 256 << 10)?);

    Ok(Calibration {
        profile: lab.profile.name.clone(),
        rtt_ms_p50: rtts[rtts.len() / 2],
        rtt_ms_min: rtts[0],
        down_mbit,
        up_mbit,
        fetch_256k_warm_ms: warm[warm.len() / 2],
        fetch_256k_idle_ms: idle_ms,
    })
}

/// What the shaped link itself allows for an interactive round trip while a plain TCP bulk
/// download *and* upload saturate it (the T13 floor, reported as the `raw` row): each bulk
/// direction and the probe run on their own TCP connections, so nothing but the link's
/// bottleneck queue sits in front of the probe. A drop-tail queue ([`Queue::Netem`]) is filled
/// by TCP (bufferbloat) and every probe byte waits behind it in both directions; with
/// [`Queue::FqCodel`] the probe flow is isolated from the bulk flows.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LoadFloor {
    pub profile: String,
    /// 1-byte echo RTT on the idle link.
    pub idle_rtt_ms_p50: f64,
    /// 1-byte echo RTT during the bulk transfers (one probe every `every`).
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub samples: usize,
    /// Goodput of the two bulk transfers during the probe window.
    pub down_mbit: f64,
    pub up_mbit: f64,
}

fn pctl(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

pub fn load_floor(lab: &Netlab, window: Duration, every: Duration) -> Result<LoadFloor> {
    load_floor_at(lab.dir(), &lab.profile.name, window, every)
}

/// [`load_floor`] for a netlab known only by its socket directory (scenario subprocesses).
pub fn load_floor_at(
    dir: &Path,
    profile: &str,
    window: Duration,
    every: Duration,
) -> Result<LoadFloor> {
    let tag = std::process::id();
    let (se, ss, sk) = (
        format!("floor-echo-{tag}"),
        format!("floor-source-{tag}"),
        format!("floor-sink-{tag}"),
    );
    let _echo = ServiceHost::start(dir, &se, Service::Echo)?;
    let _src = ServiceHost::start(dir, &ss, Service::Source)?;
    let _sink = ServiceHost::start(dir, &sk, Service::Sink)?;
    let sock = client_sock_path(dir);
    let mut e = connect_via(&sock, &se)?;
    echo_rtt(&mut e, 3)?;
    let mut idle: Vec<f64> = echo_rtt(&mut e, 20)?.into_iter().map(ms).collect();
    idle.sort_by(f64::total_cmp);

    let stop = Arc::new(AtomicBool::new(false));
    let down = Arc::new(AtomicU64::new(0));
    let up = Arc::new(AtomicU64::new(0));
    // Continuous streams (one request for "everything"), so neither direction ever idles.
    const ENDLESS: u64 = 1 << 40;
    let dl = {
        let mut c = connect_via(&sock, &ss)?;
        let (stop, down) = (stop.clone(), down.clone());
        std::thread::spawn(move || -> std::io::Result<()> {
            c.write_all(&ENDLESS.to_le_bytes())?;
            let mut buf = vec![0u8; PUMP_BUF];
            while !stop.load(Ordering::Relaxed) {
                let n = c.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                down.fetch_add(n as u64, Ordering::Relaxed);
            }
            let _ = c.shutdown(Shutdown::Both);
            Ok(())
        })
    };
    let ul = {
        let mut c = connect_via(&sock, &sk)?;
        let (stop, up) = (stop.clone(), up.clone());
        std::thread::spawn(move || -> std::io::Result<()> {
            c.write_all(&ENDLESS.to_le_bytes())?;
            let block = vec![0x5au8; 16 << 10];
            while !stop.load(Ordering::Relaxed) {
                c.write_all(&block)?;
                up.fetch_add(block.len() as u64, Ordering::Relaxed);
            }
            let _ = c.shutdown(Shutdown::Both);
            Ok(())
        })
    };
    // Let both transfers fill the bottleneck (slow start, queue build-up).
    std::thread::sleep(Duration::from_secs(1));
    let (d0, u0) = (down.load(Ordering::Relaxed), up.load(Ordering::Relaxed));
    let t = Instant::now();
    let mut probes = Vec::new();
    let mut probe_err = None;
    while t.elapsed() < window {
        match echo_rtt(&mut e, 1) {
            Ok(v) => probes.extend(v.into_iter().map(ms)),
            Err(err) => {
                probe_err = Some(err);
                break;
            }
        }
        std::thread::sleep(every);
    }
    let el = t.elapsed().as_secs_f64();
    let (d1, u1) = (down.load(Ordering::Relaxed), up.load(Ordering::Relaxed));
    stop.store(true, Ordering::Relaxed);
    let _ = dl.join();
    let _ = ul.join();
    if let Some(err) = probe_err {
        return Err(err.context("load floor probe"));
    }
    probes.sort_by(f64::total_cmp);
    Ok(LoadFloor {
        profile: profile.to_string(),
        idle_rtt_ms_p50: pctl(&idle, 0.5),
        p50_ms: pctl(&probes, 0.5),
        p99_ms: pctl(&probes, 0.99),
        max_ms: probes.last().copied().unwrap_or(0.0),
        samples: probes.len(),
        down_mbit: (d1 - d0) as f64 * 8.0 / el / 1e6,
        up_mbit: (u1 - u0) as f64 * 8.0 / el / 1e6,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_profiles() {
        let p = Profile::parse("rtt40-bw50").unwrap();
        assert_eq!(p.rtt_ms, 40);
        assert_eq!(p.rate_mbit, Some(50));
        assert_eq!(p.name, "rtt40-bw50");
        assert_eq!(p.one_way_delay_us(), 20_000);
        let p = Profile::parse("rtt0").unwrap();
        assert_eq!(p.rate_mbit, None);
        assert!(p.tc_commands(4000).is_empty());
        assert!(Profile::parse("40ms").is_err());
        assert!(Profile::parse("rtt40-bw0").is_err());
        assert!(Profile::parse("rttx-bw5").is_err());
        assert_eq!(
            Profile::parse_list("rtt0-bw200, rtt100-bw20")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(Profile::standard().len(), 9);
        let p = Profile::parse("rtt40-bw50-fqcodel-ssh").unwrap();
        assert_eq!(p.queue, Queue::FqCodel);
        assert!(p.ssh);
        assert_eq!(p.link_name(), "rtt40-bw50-fqcodel");
        assert_eq!(
            Profile::parse("rtt100-bw20-ssh").unwrap().queue,
            Queue::Netem
        );
        assert!(
            Profile::parse("rtt40-fqcodel").is_err(),
            "fq_codel needs a rate"
        );
        assert!(
            Profile::parse("rtt40-bw50-ssh-fqcodel").is_err(),
            "canonical order"
        );
        assert!(Profile::parse("rtt40-bw50-foo").is_err());
    }

    #[test]
    fn fqcodel_tc_commands() {
        let p = Profile::parse("rtt40-bw50-fqcodel").unwrap();
        let c: Vec<String> = p.tc_commands(4000).iter().map(|a| a.join(" ")).collect();
        assert_eq!(c.len(), 3 + 2 * 3, "{c:?}");
        assert!(
            c[1].contains("match ip sport 4000 0xffff flowid 1:1"),
            "{}",
            c[1]
        );
        assert!(
            c[2].contains("match ip dport 4000 0xffff flowid 1:2"),
            "{}",
            c[2]
        );
        for band in [&c[3..6], &c[6..9]] {
            assert!(
                band[0].contains("netem delay 20000us limit 100000"),
                "{}",
                band[0]
            );
            assert!(!band[0].contains("rate"), "{}", band[0]);
            assert!(band[1].contains("tbf rate 50mbit"), "{}", band[1]);
            assert!(band[2].ends_with("fq_codel"), "{}", band[2]);
        }
        assert_eq!(
            Profile::parse("rtt40-bw50").unwrap().tc_commands(1).len(),
            5
        );
    }

    #[test]
    fn netem_args_and_limit() {
        let p = Profile::parse("rtt100-bw20").unwrap();
        let c: Vec<String> = p.tc_commands(4000).iter().map(|a| a.join(" ")).collect();
        assert!(c[0].contains("root handle 1: prio bands 3"), "{}", c[0]);
        for (band, h) in [("1:1", "10:"), ("1:2", "20:")] {
            let want = format!("parent {band} handle {h} netem delay 50000us rate 20mbit limit ");
            assert!(c.iter().any(|x| x.contains(&want)), "{want} in {c:?}");
        }
        // 2.5 MB/s × (50 + 50) ms ≈ 250 KB ≈ 166 packets, ×1.5 for ACKs, +64.
        let l = p.netem_limit();
        assert!((300..350).contains(&l), "{l}");
        let p = Profile::parse("rtt0-bw200").unwrap();
        let c = p.tc_commands(4000).concat().join(" ");
        assert!(!c.contains("delay"), "{c}");
    }

    #[test]
    fn header_roundtrip() {
        let h = header_line("unlatchd-t4");
        let mut r = h.as_bytes();
        assert_eq!(read_header(&mut r).unwrap(), "unlatchd-t4");
        let mut bad = &b"HNL1 ../x\n"[..];
        assert!(read_header(&mut bad).is_err());
        let mut junk = &b"SSH-2.0\n"[..];
        assert!(read_header(&mut junk).is_err());
        let long = format!("HNL1 {}\n", "a".repeat(400));
        assert!(read_header(&mut long.as_bytes()).is_err());
        assert!(valid_service_name("@stats"));
        assert!(!valid_service_name("a/b"));
        assert!(!valid_service_name(".hidden"));
    }

    #[test]
    fn header_does_not_consume_payload() {
        let data = b"HNL1 svc\n\0UNLATCHrest";
        let mut r = &data[..];
        read_header(&mut r).unwrap();
        assert_eq!(r, b"\0UNLATCHrest");
    }

    #[test]
    fn source_sink_echo_services_direct() {
        let dir = tempfile::tempdir().unwrap();
        let _e = ServiceHost::start(dir.path(), "e", Service::Echo).unwrap();
        let _s = ServiceHost::start(dir.path(), "s", Service::Source).unwrap();
        let _k = ServiceHost::start(dir.path(), "k", Service::Sink).unwrap();
        let mut e = UnixStream::connect(svc_sock_path(dir.path(), "e")).unwrap();
        assert_eq!(echo_rtt(&mut e, 5).unwrap().len(), 5);
        let mut s = UnixStream::connect(svc_sock_path(dir.path(), "s")).unwrap();
        source_fetch(&mut s, 3 << 20).unwrap();
        source_fetch(&mut s, 7).unwrap();
        let mut k = UnixStream::connect(svc_sock_path(dir.path(), "k")).unwrap();
        sink_push(&mut k, 3 << 20).unwrap();
        sink_push(&mut k, 0).unwrap();
    }

    #[test]
    fn command_service_spawns_per_connection() {
        let dir = tempfile::tempdir().unwrap();
        let host =
            ServiceHost::start(dir.path(), "cat", Service::command(vec!["cat".into()])).unwrap();
        let mut c = UnixStream::connect(svc_sock_path(dir.path(), "cat")).unwrap();
        c.write_all(b"hello").unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello");
        drop(host);
        assert!(!svc_sock_path(dir.path(), "cat").exists());
    }
}
