# Unlatch bench and verification loop

`unlatch-bench` measures Unlatch against **sshfs** and the **local disk** over a kernel-shaped network,
checks targets T1–T17 (DESIGN §6 as amended by the design review, §2(a)6 and §2(f)6–7), and drives
`scripts/verify.sh`.

```sh
scripts/verify.sh               # fmt → clippy → tests → build → fpsim → fuzz → bench (quick) → compare
scripts/verify.sh --full        # all 9 network profiles, 200 fuzz/fpsim seeds
scripts/verify.sh --only bench  # one stage (stages: fmt clippy test build fpsim fuzz bench compare)
scripts/bench.sh --quick        # build + bench only; extra args go to `unlatch-bench run`
```

Outputs:

Runs write to `target/bench/` (ignored by git):

- `target/bench/latest.json` holds every measurement. See `unlatch_bench::report::RunReport`.
- `target/bench/SCORECARD.md` has one table per profile: unlatch, sshfs, local and raw side by side, with the target, pass/fail, and speedup vs sshfs.
- `target/bench/logs/<stage>.log` holds one log per verify stage.
- `target/bench/logs/bench-<profile>.log` holds the scenario progress notes plus the `unlatchd` and sshfs stderr.
- `unlatch-bench compare bench/baseline.json target/bench/latest.json` exits 1 when an Unlatch row regresses.

The published results are committed separately: `bench/baseline.json` (the report regressions are
measured against) and `bench/results/SCORECARD.md` (the numbers the README and the site quote). To
publish a run, copy its `latest.json` to `bench/baseline.json` and its `SCORECARD.md` to
`bench/results/`, then update `site/benchmarks.json` and run `site/build.sh`.

## How the network is shaped (design review D24)

A userspace delay proxy can't show bandwidth, queueing or slow start. The bench therefore uses
kernel shaping in an **unprivileged user+net namespace**, with no root needed:

```
client (sshfs / unlatch engine / unlatch mount)                         host netns, unshaped
  │ stdio
unlatch-bench netlab connect <dir>/client.sock <service>            stdio ⇄ unix socket
  │                                   (-ssh profiles: `ssh -F <cfg> bench …`, ProxyCommand = this)
holder: `unshare -rn unlatch-bench netlab holder`
  lo: mtu 1500, TSO/GSO/GRO off; root `prio`, u32 filters on the server port:
      band 1 = server→client ("down"), band 2 = client→server ("up"), band 3 = unshaped
      each shaped band: netem delay RTT/2 rate R limit (delay line + 50 ms)×1.5   (default)
                     or netem delay RTT/2 → tbf rate R → fq_codel                (-fqcodel)
  client relay ──── TCP 127.0.0.1:P (shaped, TCP_NODELAY) ────► server relay
  │
<dir>/svc/<service>.sock  →  ServiceHost (host netns) spawns the server per connection:
  unlatchd stdio --root … --state …  |  sftp-server  |  cat <file>
  (-ssh profiles: /usr/sbin/sshd -i -e -f <cfg>, which runs those as the remote command)
```

- **Both systems cross the same shaped TCP connection.**
  - sshfs runs as `sshfs -o ssh_command="<client>" bench:<root> <mnt>`: the raw bridge (which
    ignores the ssh arguments sshfs appends), or `ssh -F <cfg>` in `-ssh` profiles.
  - The Unlatch engine uses `Transport::Command { argv: <client> + unlatchd stdio … }`; `unlatch mount`
    gets the same argv after `--command`.
  - Raw profiles: neither side pays ssh crypto. `-ssh` profiles: both do (see below).
- **One bottleneck per direction.** A real access link has separate uplink and downlink queues.
  Until 30 Sep the bench ran both directions through one FIFO on `lo`, so under a simultaneous
  download and upload every packet queued behind both, doubling T13's queueing and making
  upload and download share one rate. Now a `prio` root classifies by the relay's port into two
  independently shaped bands. Each band still carries the other direction's ACKs (hence the
  ×1.5 in the drop-tail limit).
- **Queue discipline.** The default is netem's own drop-tail FIFO (≈ 1.25 BDP at RTT 40 ms, 0.75
  BDP at 100 ms): what a plain TCP bulk transfer fills (bufferbloat). `-fqcodel` puts `tbf`
  (rate) and `fq_codel` (Linux's default qdisc; flow isolation + CoDel) behind a delay-only
  netem — a modern AQM router.
- **Real ssh (`-ssh`).** `sshd` runs in inetd mode (`-i`), one per connection, as a netlab
  service *outside* the namespace (inside it our uid maps to 0 and sshd would attempt
  root-only privilege separation); `UsePAM no`, `StrictModes no`, ed25519 keys generated per
  run, `Subsystem sftp` for sshfs. The client is OpenSSH with its defaults (chacha20-poly1305)
  and `Compression no`, as Unlatch uses it. Both sshfs and Unlatch then pay ssh crypto and the
  2 MiB channel window, and T8's `raw` row is `ssh bench cat file` as DESIGN §6 defines it.
- **Servers run outside the namespace.** Inside it our uid maps to 0, so `faccessat` answers as root, and abstract sockets are namespaced. Only the relay runs in the namespace; filesystem unix sockets cross the namespace boundary. `unlatch-bench netlab up --profile rtt40-bw50 --serve name=cmd` prints the holder pid if you want `nsenter --user --net -t <pid> --preserve-credentials` (e.g. `ss -tin` to inspect the relay's TCP state).
- **RTT is twice the netem delay.** Both directions are delayed by RTT/2 in their own band. Measured, not assumed; see `calibrate` below.
- **MTU 1500 and offloads off.** Loopback's 64 KiB MTU hides slow start (IW10 ≈ 640 KB), and with TSO on, netem paces 64 KiB super-packets.
- **Idle behaviour is the kernel default.** `tcp_slow_start_after_idle` stays at 1. After-idle variants sleep 2 s before the operation. Use `--slow-start-after-idle` on the holder to experiment.
- **`@stats` reports bytes moved.** It returns per-service byte counters for the shaped link; T11, T13 and T14 use them.
- **Client disk vs VM disk (`--mac-dir`).** By default everything lives under `--work`
  (`target/unlatch-bench-work`, on this VM the shared xfs `/home`). `--mac-dir DIR` moves the
  "Mac side" — engine replica (SQLite, `synchronous=FULL`), content cache, upload staging, FUSE
  client state, downloads — elsewhere, e.g. tmpfs, which is closer to a quiet laptop SSD than a
  disk shared by 70 users. The VM side (tree, `unlatchd` state) stays in `--work`.

Profiles are named `rtt<ms>[-bw<mbit>][-fqcodel][-ssh]`:

- `--full` runs {0, 40, 100 ms} × {20, 50, 200 Mbit/s}.
- The quick default is `rtt40-bw50`, the design's reference WAN.

### The link's own floor under load (`unlatch-bench netlab floor`, T13 `raw` row)

p99 of a 1-byte echo on its own TCP connection while continuous plain-TCP streams saturate both
directions, sampled every 100 ms (8 s windows, 30 Sep 2026, load average ≈ 40 on 124 CPUs):

| profile | idle RTT | probe p50 | probe p99 | T13 target |
|---|---:|---:|---:|---:|
| rtt40-bw50 (drop-tail) | 40.5 ms | 168 ms | **461 ms** | 70 ms |
| rtt100-bw20 (drop-tail) | 100.4 ms | 242 ms | **294 ms** | 130 ms |
| rtt40-bw50-fqcodel | 40.4 ms | 41.1 ms | 42.1 ms | 70 ms |
| rtt100-bw20-fqcodel | 100.4 ms | 100.7 ms | 103.4 ms | 130 ms |

On a drop-tail link TCP fills the bottleneck buffer (bufferbloat) and the probe's own packets
are tail-dropped (RTO), so **no protocol that opens separate TCP connections can meet T13
there**. Unlatch multiplexes everything over one connection and bounds its own bulk bytes in
flight with credit (below), so it keeps the queue short itself and beats this floor. With
fq_codel the probe flow is isolated and the floor is ≈ the idle RTT.

### Measured link calibration (`unlatch-bench netlab calibrate`, this VM, kernel 6.8, 30 Sep 2026, per-direction bottlenecks)

| profile | RTT p50 | down | up | 256 KiB fetch warm | after 2 s idle |
|---|---:|---:|---:|---:|---:|
| rtt0-bw200 | 0.14 ms | 191.2 Mbit/s | 191.2 Mbit/s | 11.4 ms | 11.9 ms |
| rtt40-bw50 | 40.4 ms | 46.9 Mbit/s | 46.9 Mbit/s | 84.2 ms | 211 ms |
| rtt100-bw20 | 100.4 ms | 18.3 Mbit/s | 18.2 Mbit/s | 210 ms | 516 ms |
| rtt40-bw50-fqcodel | 40.4 ms | 44.7 Mbit/s | 34.1 Mbit/s | 86.7 ms | 207 ms |
| rtt100-bw20-fqcodel | 100.6 ms | 16.0 Mbit/s | 16.1 Mbit/s | 225 ms | 519 ms |

(`-ssh` profiles calibrate the same link; ssh only changes the endpoints.)

- **Rate:** goodput is 95% of the line rate at RTT 0, 94% at 40 ms and 91% at 100 ms on a 2–4 s transfer; the rest is header overhead and TCP ramp-up. The tbf shaper of `-fqcodel` gives a few percent less, with more variance.
- **Idle:** the idle column reproduces the D24 finding. After 2 s idle, a 256 KiB read costs about 5 RTT, because cwnd falls back to IW10.

## Synthetic tree (`unlatch_bench::tree`)

The tree is deterministic (ChaCha8, seed `0x4a7c4b3e20260930`), generated in parallel (~4 s) and
cached in `target/unlatch-bench-work/cache/tree-<spec>/`:

- `vm/repo/` has 100 000 entries: nested source-like dirs; lognormal file sizes (median 2 KiB, σ 1.2, cap 4 MiB) plus 16 blobs of 1–8 MiB; mixed text-like and random content.
- `vm/repo/node_modules/` has 30 007 entries, packages of 16 entries each. It is lazy by name.
- `vm/flat/` has exactly 1000 files: 990 files of ≤ 12 KiB and 10 files of 256 KiB.
- `vm/big/big.bin` is 256 MiB of incompressible data.
- `vm/scratch/` holds the files scenarios create; it is emptied after every scenario.
- A separate wide lazy fixture serves T15: `node_modules` with 500k files (50k in quick mode), 250 per package.

`unlatch-bench gen-tree --out DIR [--tiny]` builds the tree on its own.

## Scenarios

Each (target, system) pair runs in its own `unlatch-bench scenario` subprocess, with a deadline: 180 s
quick, 900 s full. A panicking (`todo!()`) or hanging system under test becomes an `n/a`, `error`
or `timeout` row that names the last step. The run itself never crashes. Unlatch rows are judged
against targets. Baselines are shown for comparison only.

| # | Unlatch (how) | baselines |
|---|---|---|
| T1 | `Engine::list` of `flat/` (1000), all pages, p50 | local: readdir + lstat each |
| T2 | `Engine::item`, p50 µs | local: lstat |
| T3 | FUSE `ls -la flat/` (`unlatch mount`), warm and first-after-mount | local `ls -la`; sshfs first `ls` after a fresh mount (cold) and repeated (warm, 20 s dir cache) |
| T4 | VM write → `Engine::lookup` succeeds; → `ReplicaChanged` with the id; FUSE readdir poll | sshfs: stat poll of the exact path (1 LSTAT); readdir poll (`--full`; bounded by the 20 s dcache) |
| T5 | 1000 files over 1 s → `Engine::list` shows all | sshfs readdir count (`--full`) |
| T6 | `Engine::fetch` after a viewer enumeration plus prefetch time | sshfs second open; local page cache |
| T7 | `Engine::fetch` of never-fetched ≈4 KiB and 256 KiB files, warm and after 2 s idle, prefetch off | sshfs first open of never-opened files |
| T8 | `Engine::fetch` of a file sized to ~3 s (quick) or ~20 s (full) at line rate, capped at 256 MiB | **raw**: `cat` over the same bridge; sshfs read; local |
| T9 | `Engine::create` of 4 KiB until it returns; the file is checked on the VM | sshfs create+write+fsync+close; local write+fsync(file, dir) |
| T10 | `drop_connection` + 100 VM creates during the outage → all visible | — |
| T11 | `Engine::start → wait_live` with a fresh replica on a warm daemon index (plus a cold-daemon variant); bytes on the link | recursive readdir+lstat via sshfs (8 s budget in quick mode, then extrapolated `~`), local walk |
| T12 | `VmRSS` of the serving `unlatchd` ÷ `ServerInfo.entries`, measured after T11 | — |
| T13 | p99 `server_barrier` (Pong) and lazy-dir `list` (ListDir) during a download of the random 256 MiB `big.bin` + a random upload (≈ 2× the window at line rate); sampling starts once both move bytes on the link; 20 and 50 Mbit/s only; 5 s window (20 s full) | **raw**: the link floor (above); sshfs p99 uncached lstat during download + upload |
| T14 | bytes moved on the link ÷ bytes appended to a materialized 10 MiB log (1 KB / 10 ms) | — |
| T15 | `unlatchd stdio` directly: `ListDir(node_modules)` entries and inotify watches added | — |
| T16 | restart `unlatchd stdio` on the same state: spawn → `Welcome(Resume)` time, snapshot bytes | — |
| T17 | 1000 same-size in-place rewrites (200 quick): new content version within 1 s | — |

`unlatch mount` is invoked as `unlatch mount <MNT> --root <R> --state <S> --command <client argv…>`,
which is exactly the real CLI (`unlatch mount --help`: `--command <ARGV>...` takes the rest of the
line, hyphen values included; a unit test pins the expansion). `UNLATCH_BENCH_MOUNT_ARGV` (a JSON
array; `{cmd...}` splices the client argv and `{cmd}` joins it) still overrides it.

Binaries are found in this order:

1. `--unlatchd` / `--unlatch` / `--sshfs` flags;
2. `UNLATCHD_BIN` / `UNLATCH_BIN` / `SSHFS_BIN`;
3. next to `unlatch-bench`;
4. `./target/release`;
5. `PATH`, then `~/.local/bin`.

The sftp server is found at `/usr/lib/openssh/sftp-server` (or libexec).

## Results

The current scorecard is `bench/results/SCORECARD.md` (its full report is `bench/baseline.json`):
three required profiles over the raw bridge and over ssh, plus fq_codel variants of the two WAN
profiles, with host load average per profile.

Headline rows (quick mode, 1 Oct 2026, host load average 25–87 on 124 CPUs, per profile in
the scorecard):

| row | rtt0-bw200 | rtt40-bw50 | rtt100-bw20 | …-ssh (0 / 40 / 100) | …-fqcodel (40 / 100) |
|---|---|---|---|---|---|
| T4 ms engine / FUSE readdir, target RTT/2+15 | 2.6 / 1.6 ✅ | **22.8 / 23.2 ✅** | 52.7 / 54.8 ✅ | 2.8/1.8 · 22.9/24.1 · 53.3/62.3 ✅ | 23.5/26.3 · 52.6/54.3 ✅ |
| T7 256 KiB cold (warm link) ms | 13.7 | 86.1 | 218 | 12.8 · 86.1 · 213 | 84.7 · 316 |
| T8 Mbit/s unlatch / raw / sshfs | 187 / 191 / 190 | 44.4 / 44.8 / 30.9 | 14.1 / 16.4 / 11.3 | 180/184/184 · 44.1/39.8/30.6 · 14.4/13.4/11.1 | 40.7/41.8/31.2 · 13.5/14.7/11.3 |
| T13 p99 ms unlatch (pong / listdir), target RTT+30 | — | 81 / 89 ❌ | **126 ✅** / 141 ❌ | — · **66 ✅** / 72 ❌ · **120 ✅** / 146 ❌ | 99 / 138 ❌ · 133 / 209 ❌ |
| T13 p99 ms, link floor (raw) / sshfs | — | 272 / 1597 | 256 / 3856 | — · 327 / 1141 · 295 / 2642 | 45 / 1191 · 105 / 2775 |
| T9 ms unlatch / sshfs / local, target RTT+5 | **4.7 ✅** / 2.9 / 1.1 | 46.3 / 213 / 1.3 ❌ | 106.1 / 510 / 1.1 ❌ | **4.9 ✅** · 50.7 · 106.6 ❌ | 47.8 · 107.4 ❌ |

Reading T13: Unlatch is 14–30× better than sshfs and far below the drop-tail floor. Quick-mode
p99s are the worst of 9–18 samples and move ±30 ms with this host's load (the rtt40-bw50 row
above was 54 / 67 on 30 Sep and 81 / 89 here at load 39–47; see "1 Oct" below for repeated
full-mode runs, which show no regression). The listdir rows are dominated by the VM-side first
scan of a cold lazy directory on the shared xfs `/home`.

Reading T9: every remaining miss is 1–6 ms over the target: three ordered barriers on the VM's
shared disk (see "Small upload until durable"). The rtt0 rows now pass.

## 1 Oct 2026: T4, T7, T9 and T13 near-misses (perf2)

All numbers are medians of the stated runs at rtt40-bw50 unless noted, with the host load
average (1 min) at the time.

- **T4 (VM write → visible): 36.8 / 38.9 → 22.7 / 23–25 ms** (engine / FUSE readdir; 3 runs,
  load 25–33). Timestamped breakdown before: inotify 0.1 ms, unlatchd debounce **16.3 ms**,
  reconcile + publish 0.2 ms, one-way network 20.2 ms, engine apply 0.1 ms; on the FUSE path
  the SQLite commit (2.2 ms, more on a busy disk) came before `ReplicaChanged`, then
  invalidation 0.1 ms. The debounce's first growth check compared against an empty queue, so
  every batch slept two full 8 ms steps. Now a batch closes after 2 ms of quiet
  (`UNLATCHD_SETTLE_US`) and bursts still coalesce in 8 ms steps up to 50 ms (T5 29 → 31–37 ms,
  target 340). `ReplicaChanged` now goes out before the commit (FUSE reads the in-memory
  replica); anchors, waiters and `WorkingSetChanged` stay after it (D5). The bench's
  `visible_event` row used to read the event log once and could match a late event for the
  previous file's parent (values of 2–10 ms below the one-way delay); it now waits for the
  event naming the new id.
- **T9 (4 KiB upload): 47.0–47.7 → 45.0–46.4 ms** (5 runs including the scorecard run, load
  28–47). The no-op second replica commit after the reply is gone, and SQLite uses
  `fdatasync` on Linux (event commit 2.2 → 1.3 ms). See "Small upload until durable".
- **T7 (256 KiB cold open): the 125 ms was real but order-dependent.** Run alone, T7 got 86 ms
  (its engine starts with a snapshot, which grows the credit window); after T6 (the verify
  order) its engine resumes and the window stays at the 256 KiB initial credit. Credit is
  charged per frame body as sent, so a 256 KiB file costs ~80 B more than that and its last
  bytes waited a credit round trip: 97–136 ms per fetch (first fetch of a never-loaded
  connection ≈ 212 ms is TCP slow start, as in the idle case). The window now starts one unit
  (16 KiB) above the initial credit, granted at session start, and the end of every read
  returns the sub-unit credit still owed: 86–87 ms in every position (3 runs after T6, load
  42–45). T8 and T13 unchanged within noise.
- **T13 at rtt100-bw20, full mode (≈ 35 samples per row).** Before: pong p99 143–156 ms,
  listdir 175–262 ms (3 runs, load 38–48). After: pong p99 125–143 (median 134), listdir
  158–220 (5 runs, load 38–49); over ssh pong 145/147 → 136/141 (load 24–34). Per-ping timestamps showed unlatchd's Ping
  barrier walking the whole 100k-node index to find polled directories (2.5 ms per Pong, and
  every second under the state lock); it now skips the walk when no directory was ever
  polled (0.05 ms). What is left: grants and credit returns cross the other direction's queue
  and arrive bunched, so each side sends bursts (down-direction delay correlates with bytes
  unlatchd wrote in the previous 60 ms: 160–200 KB vs 137 KB at line rate → +20–37 ms). A
  unlatchd pacer (1.25 × the returned-credit rate, 16 KiB bucket) cut rtt100 pong p50/p90 by
  5–8 ms but made rtt40-bw50 worse in back-to-back runs, so it is not in. Listdir p99 is the
  VM's cold-directory scan: 15–180 ms on this disk in the same seconds.
- **No T13 regression at rtt40-bw50.** The scorecard run's 81 / 89 ms is noise of a quick-mode
  maximum: in full mode, alternating old and new builds (load 55–126), pong p99 was 77 / 86 /
  76 ms before and 74 / 76 / 54 ms after; listdir p99 (126–389 ms both) tracked the VM disk.

## 1 Oct 2026: post-merge verify misses and the Write stall (perf3)

The verify run after the rename (quick mode, load average 92–153) reported T9 64.9 ms, T11
4.33 s and T13 925 ms against the perf2 baseline (46 ms, 1.18 s, 89 ms at load 25–87). All
three are the host, not the code. Interleaved runs of the perf2 build ("old") and
the renamed build ("new"), back to back, rtt40-bw50, full-mode T13, medians
[min..max]:

| run (load avg) | build | T9 ms | T11 warm s | T11 cold s | T13 pong p99 ms | T13 listdir p99 ms |
|---|---|---:|---:|---:|---:|---:|
| work on tmpfs, 5 rounds (51–84) | old | 41.5 [41.1..42.0] | 1.04 [1.02..1.21] | 1.03 [0.99..1.10] | 63 [60..78] | 75 [63..93] |
| | new | 41.7 [41.1..42.3] | 1.03 [1.00..1.07] | 1.04 [1.00..1.06] | 63 [57..106] | 71 [66..77] |
| | new + fix | 41.6 [41.2..42.6] | 1.08 [1.00..1.10] | 1.01 [0.96..1.12] | 60 [58..340] | 74 [69..82] |
| work on `/home`, 4 rounds (45–84) | old | 87 [65..107] | 2.24 [1.36..2.43] | 1.99 [1.97..2.32] | 61 [58..82] | 655 [563..939] |
| | new | 88 [63..107] | 1.63 [1.37..1.95] | 1.68 [1.20..2.06] | 60 [57..96] | 395 [175..3093] |
| | new + fix | 196 [61..354] | 1.50 [1.27..1.83] | 1.70 [1.36..2.75] | 75 [70..78] | 769 [248..2333] |

- **T9** on `/home` is the shared disk's fsync latency (`/dev/sdc` at 99% utilisation,
  ~8 k writes/s from other users): in the verify run the *local* write+fsync row was 4.3 ms
  against 1.3 ms in the baseline. T9 alone, 8 interleaved rounds on `/home` (load 31–82): old
  48.5 [45.6..118], new 48.8 [45.9..362], new + fix 58.7 [45.6..362] ms; quiet rounds give
  45.6–46.5 ms for all three, every build has 360 ms rounds.
- **T11** includes the engine's fresh SQLite replica (`Engine::start` alone took 0.05–1.4 s
  on `/home`, 2 ms on tmpfs) and its commits. An 8-way interleaved bisect over every merge
  since perf2 on tmpfs (5 rounds, load 60–95) gave 1.00–1.08 s for every build (old 1.03, new
  1.03).
- **T13 listdir** is the VM's first scan of a cold lazy directory and the fsyncs under the
  daemon lock on the same disk: 175–3093 ms on `/home` for every build, 63–93 ms on tmpfs.
  Pong p99 stayed 57–96 ms throughout (the verify run's was 57.8).

**The real regression is one T13 does not see.** A data-safety fix (TESTING row 42) re-hashed our
inode after every Write's publish (≤ 64 MiB always) to prove no agent wrote into the window, and
did it while holding the core lock: every Stat / ListDir / Ping of every session waited for
the whole file to be re-read. T13's upload never completes inside its sampling window, so it
never measured the commit. `crates/unlatchd/tests/write_stall.rs` does: two sessions on one
`unlatchd serve` (tmpfs), one uploads 64 MiB (create, then replace) three times, the other
samples `Stat(root)` back to back. Longest Stat during a Write, three runs each:

| build | per-write max (ms) |
|---|---|
| perf2 build | 1–16 |
| two builds before the row-42 fix | 1.7–13 (one 52 outlier) |
| the row-42 fix, the renamed build | 29–63, every write |
| dev + fix | 3.4–15 |

The fix: the staged O_TMPFILE takes an exclusive write lease (`F_SETLEASE F_WRLCK`) when it
is created, while it is unnamed and ours alone, and keeps it until the reply is decided. Any
open of the inode by anyone waits for us, so no other write can land on our bytes; `F_GETLEASE`
still `F_WRLCK` at the check proves no open was even attempted, and a lease being broken is
reported as a race without reading anything. The re-hash remains the fallback (no leases,
network filesystems, a named staging file, a write in place). TESTING row 49.

Follow-up (TESTING row 50): a write lease breaks on any open, read-only included, so an editor,
LSP or indexer reading the file as it changed made the Write report a race and the Mac's next
save a conflict copy (135 of 720 probe rounds). The staged file now holds a **read lease**
instead, taken once its bytes are written and synced, after closing its writer (unnamed, so
nobody else can open it): readers neither break nor wait for it; an open for writing or a
truncate breaks it (race, as before), and `open(O_RDONLY|O_TRUNC)`, which gets past it, always
changes the size the stat checks compare. 0 of 720 rounds; write_stall median 10.0 ms (write
lease 10.3, dev 39.7), six interleaved rounds each at load 38–60.

## Interactive latency under bulk load (T13): what was wrong and what Unlatch does now

Baseline on 30 Sep: p99 433 ms at rtt40-bw50. Measured again with a load that
actually loads the link (see the first bullet), Unlatch was at 379 ms (rtt40-bw50) and 785 ms
(rtt100-bw20).

- **The T13 load was compressible.** The "2 GiB" download and the upload were sparse files
  (all zeros); Unlatch LZ4-compresses frames about 250×, so the link was nearly idle for Unlatch
  while sshfs (no compression) was saturated. T13 now downloads the random 256 MiB `big.bin`,
  uploads random bytes, and starts sampling only once both move bytes on the link.
- **Uploads had no sender window.** Only the server's fixed 1 MiB grant limited them: ≈ 490 ms
  of queue at 17 Mbit/s, in front of every request and download grant. The engine now keeps
  upload bytes in flight (sent and not yet returned by the server's grants) within its own
  window, and unlatchd returns upload credit per chunk (≥ 4 KiB) instead of per 64 KiB.
- **min_rtt was measured under load.** It came only from 5 s pings; a session that starts with
  a snapshot sees 193 ms on a 40 ms path, and every window was sized for that queue. The engine
  now sends RTT probes (`Stat` of the root, which has no server-side effects, unlike Ping's forced flush) every
  200 ms while bulk data moves. It marks samples clean or dirty, and runs a short
  BBR-style drain (both windows 64 KiB until a probe sent after the queues emptied returns)
  when the minimum is dirty or older than 10 s.
- **The window rule was neutrally stable.** `W = rate × RTT`, with an averaged rate, sticks
  below the link rate: the credit loop's latency includes the other direction's queue, grant
  granularity and thread hops on the host. The window is now
  `bw_max × (min_rtt + Q) + unit + feedback`:
  - `bw_max` is the windowed max over 1 s;
  - `Q = min(min_rtt/16, 2.5 ms)`;
  - `unit` is the credit granularity, ≈ 2 ms at line rate (4–16 KiB);
  - a signed delay-feedback term moves by one unit per 100 ms toward a 4–10 ms band of
    probe queueing delay.

  The controller also has:
  - a startup phase (gain 1.5, HyStart-like exit on delay);
  - the review's back-off: ×0.75 when two probes in a row exceed min_rtt + 25 ms.

  Per direction, at most ≈ `bw × Q` plus one unit of our own bulk waits ahead of an
  interactive frame. That is under CoDel's 5 ms target, so an fq_codel router does not drop
  our single flow; a drop would head-of-line block the interactive frames for an RTT.
- **Credit was granted in decompressed bytes.** `wire.rs` says frame bodies as sent. A
  compressible stream therefore inflated the server's credit without bound, and the next
  incompressible stream could flood the pipes. The engine (and its test fake server) now use
  wire bytes.
- **unlatchd read inside the credit loop.** A streamed `Read` now reads 256 KiB ahead of the
  credit, so a slow `pread` on a busy disk no longer adds to the loop's latency.

What is left above RTT in T13, and why:

- **Pong** (network + daemon): the probe's own jitter on this host (load average 30–70).
- **ListDir** (a never-listed lazy dir):
  - the VM-side first scan of a cold directory on the shared xfs `/home`, under the daemon lock;
  - on the Mac side, the engine's replica commit (SQLite, `synchronous=FULL`) before `list`
    returns.

  Both are disk, not queueing: in the same seconds Pong stays within target. The `--mac-dir`
  run separates the client half.
- **Drop-tail links are bounded by TCP, not by Unlatch.** The link floor table above shows
  separate-connection traffic cannot meet T13 on them. Unlatch can, because it keeps its own
  queue short, but only while it is the link's only heavy user.

## Small upload until durable (T9)

T9 is 3 ordered barriers on the VM plus the engine's own commit:

1. `fsync` of the O_TMPFILE staging file, covering content + mode + mtime;
2. `linkat` + `fsync` of the parent directory;
3. op record appended to the journal + `fdatasync`;
4. reply, then the engine's SQLite commit of the new item.

The order is what makes replays safe: an op record may only become durable after the file it
describes, or a replay after a crash would answer "written" for a file that is gone. It stays.

With everything on tmpfs the whole upload costs **0.67 ms at RTT 0 and 41.4 ms at RTT 40**
(code ≈ 1 ms). On `/home` at RTT 0 it was 7.7 ms before and 5.8–6.1 ms after this work, against
1.2–1.8 ms for the local write+fsync(file)+fsync(dir) baseline. The difference is fsync latency
on a disk shared by 70 users. The changes that got there:

- mode/mtime are now set before the data fsync (they used to be set after it);
- the append-only journal uses `fdatasync`.

Since 1 Oct the engine no longer commits its replica a second time after the reply: the
reply's upsert repeats what the pushed `Events` already committed, so the applier releases the
caller without a transaction (it was one more fsync on the Mac side). The bundled SQLite is
also built with `HAVE_FDATASYNC` on Linux (`.cargo/config.toml`): with `fsync()` every replica
commit paid an xfs log force for the WAL's mtime (≈ 2.2 → 1.3 ms per commit here).

Measured breakdown at rtt40-bw50 (median of 10, load ≈ 30): upload 20.7 ms one-way; on the VM
fsync(staging file) 1.9 ms, `linkat` + fsync(dir) 0.8 ms, journal fdatasync 1.3 ms; the
reply 20.2 ms one-way. The pushed `Events` leave right after the dir fsync, so the engine's
commit of them (1.3 ms) runs in parallel with the journal sync and ends about when the reply
arrives. What remains above `RTT + 5` is these three ordered barriers on a disk shared by 70
users; none is redundant (the journal's append could avoid one xfs log force with a
preallocated, zero-filled journal, but the event commit then becomes the critical path).

## Gaps and known limitations

- **No asymmetric profile yet.** Each direction has its own bottleneck now, but both get the same rate. The review's 100 down / 10 up variant needs only a second rate on band 2 (`Profile::tc_commands`); it is not profiled.
- **sshd runs outside the namespace, in inetd mode.** The review suggested `sshd -D` on a port inside the netns; inside the user namespace sshd would run as uid 0 and try privilege separation. The shaped path (ssh ⇄ network ⇄ sshd) is the same.
- **T13 is duration-bounded.** It runs for a 5 s (quick) or 20 s (full) window instead of a full 2 GiB download plus a 500 MiB upload. At ≤ 50 Mbit/s the bulk transfer never finishes inside the window anyway. The upload is sized to about 2× the window at line rate.
- **T8 uses a smaller file where the link is slow.** The file is sized to about 3 s (quick) or 20 s (full) at line rate, not always 256 MiB.
- **T12 includes the startup verification walk.** RSS is measured right after the initial sync, so it includes any memory the background verification walk still holds.
- **Some numbers are noisy.** This VM is shared (load average 30–70 on 124 CPUs during the 30 Sep runs, recorded per profile in the report). The local T9 row (fsync) and every row that includes an fsync on `/home` move with other users' disk load; quick-mode p99s over ~10–17 samples are effectively maxima.
