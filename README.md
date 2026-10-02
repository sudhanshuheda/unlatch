# Unlatch

**Unlatch your VM.** Your cloud VM's files in macOS Finder, as snappy as a local folder.

[![CI](https://github.com/sudhanshuheda/unlatch/actions/workflows/ci.yml/badge.svg?branch=dev)](https://github.com/sudhanshuheda/unlatch/actions/workflows/ci.yml)

Unlatch puts a folder from a remote Linux machine in the Finder sidebar. It is built for people
who run Claude Code or Codex on a cloud VM and want to browse, preview and drag what the agent
writes without copying files back and forth.

> **Status: pre-release.** The VM daemon, the installer and the Linux FUSE client run and are
> tested end to end on Linux. The Mac app compiles and passes its bundle checks in CI, but it
> has not shipped, and nothing on the Mac side has been run on a real Mac from this repository
> yet. No npm package is published, and unlatch.dev is not live. To try Unlatch today, build
> it from source ([below](#building-from-source)).

## Why

Network filesystems feel slow in Finder because every listing, `stat` and preview waits for a
round trip, and SFTP has no change notifications, so a client either polls or shows stale
folders. Unlatch works differently:

- **Finder never waits on the network.** The Mac keeps a replica of the whole folder tree, so
  listing a folder or reading its attributes is a local lookup. File contents download the first
  time you open them, and the small files in a folder are fetched ahead of time when you look at
  it.
- **The VM pushes changes.** A small daemon on the VM watches the folder with inotify and sends
  each change over your ssh connection as it happens. Files your agent writes show up in Finder
  without a refresh or a polling interval.
- **It works in every Mac app.** Unlatch is a File Provider extension, so the VM is a real Finder
  location: Quick Look, Preview, copy and paste, and dragging a file into Slack or a browser all
  work. Editors with a remote mode give you this speed inside the editor only.

When you and the agent edit the same file, Unlatch keeps both versions: your save lands next to
the agent's as `name (conflict from <your Mac> <date>).ext`. Nothing is overwritten silently.

## Install

These are the commands as they will work once the packages are published.

**On the VM** (over ssh, or ask the agent running there):

```sh
cd ~/code
npx unlatch share
```

This installs the daemon, checks the VM (inotify limits, filesystem type, disk space, sshd),
starts the index server for the folder, and prints the one command to run on your Mac. It picks
the host name your Mac is most likely to reach: a Tailscale name if there is one, the cloud
provider's public address for a VM behind NAT, or a placeholder with a hint when it cannot tell.

**On your Mac**, paste the line it printed into Terminal:

```sh
npx unlatch connect you@dev-box:~/code
```

The first run installs Unlatch.app, adds the VM, and opens the folder in Finder. Running
`npx unlatch` on the Mac with no arguments starts a wizard that lists the hosts in your
`~/.ssh/config`.

**From Claude Code or Codex** on the VM, paste:

```text
Set up Unlatch so I can browse this machine in Finder: https://unlatch.dev/SKILL.md
```

The agent runs `npx -y unlatch share --json` and gives you the line for your Mac. The skill
file the agent reads is [`site/dist/SKILL.md`](https://github.com/sudhanshuheda/unlatch/blob/dev/site/dist/SKILL.md)
in this repository; unlatch.dev will serve the same file. `npx unlatch skill` installs it for
Claude Code (`~/.claude/skills/unlatch/`) and Codex (`~/.agents/skills/unlatch/`), so that
"show me this folder in Finder" is enough.

**On a Linux desktop**, mount the folder with FUSE instead of Finder:

```sh
npx unlatch connect you@dev-box:~/code --mount ~/dev
npx unlatch remove --mount ~/dev
```

Other commands: `status`, `open`, `remove <name>`, `doctor`, `update`, `uninstall --yes`. Every
command takes `--json`. Uninstalling removes the VMs from Finder first and never touches your
files on the VM. The full installer reference is [docs/INSTALL.md](docs/INSTALL.md).

## Requirements

- **Mac:** macOS 13 or later.
- **VM:** Linux (x86_64 or aarch64) that you can reach with ssh, with inotify (any recent
  kernel). Folders on NFS, CIFS, FUSE, 9p or virtiofs work but are polled every 1 to 30 seconds
  instead of watched, and `share` and `doctor` warn about it. On hosts where logind kills user
  processes at logout, linger must be on.
- **Node 18 or later** on both sides, only to run `npx`.
- **Linux desktop client:** FUSE 3 (`fusermount3`).

Windows is not supported.

The macOS behaviour Unlatch relies on was measured on macOS 26. Nothing has been measured on
macOS 14 or 15 yet. The supported-version list will be the versions on which `unlatch probe`
(below) has passed, and so far that list is empty.

## How it works

```
 Finder / any Mac app
      │  fileproviderd
      ▼
 UnlatchFileProvider.appex      sandboxed Swift shim, no logic of its own
      │  XPC (code-signing checked both ways)
      ▼
 Unlatch.app --agent            launchd agent; libunlatch engine (Rust):
      │                         replica, content cache, sync, conflict policy
      │  ssh -T host unlatchd connect     (your system ssh, stdio)
 ──── network ───────────────────────────────────────────────────────────
      ▼
 unlatchd connect ⇄ unix socket ⇄ unlatchd serve      one per shared folder,
                                  index + inotify + event log   outlives ssh
                                                                  Linux VM
```

- **All the logic is Rust and runs on Linux in tests.** The Swift layer marshals File Provider
  calls to `libunlatch` and tells the system when to re-enumerate. The extension is a stateless
  client; the engine lives in a launchd agent, because macOS kills idle extensions while the
  connection to the VM and its push stream have to stay up.
- **Your system `ssh` is the transport.** `~/.ssh/config`, ssh-agent or 1Password's agent,
  ProxyJump, Tailscale, `known_hosts` and certificates all apply as they do for `ssh`.
- **The server outlives the ssh session.** `unlatchd serve` keeps watching while your laptop
  sleeps and keeps an event log, so a reconnect replays a few events instead of rescanning.
  Its index survives daemon restarts and VM reboots.
- **Stable identity.** Each item has an id that survives renames and the "write a temp file,
  rename over" save that editors and agents use, so Finder keeps track of the file.
- **Lazy heavy folders.** `node_modules`, `.git`, `target`, `.venv` and similar are listed but
  not scanned or watched until you open them.
- **Interactive traffic first.** One ssh connection carries everything. Listings, small reads
  and change events go ahead of bulk transfers, bulk data moves in frames of at most 64 KiB, and a credit window
  keeps large downloads from filling the link's queue. Frames are LZ4-compressed when it helps;
  ssh compression is off.
- **Writes are never blind.** An upload carries the version it was based on. If the file
  changed on the VM in the meantime, the VM's version keeps the name and yours is saved as a
  conflict copy.

The design is in [docs/DESIGN.md](docs/DESIGN.md), as amended by the
[design review](docs/review/2026-09-30-design-review.md). The Mac side, including signing and
troubleshooting, is in [docs/MACOS.md](docs/MACOS.md).

## Numbers (preliminary)

These come from `bench/results/SCORECARD.md`, a quick-mode run on 1 October 2026. Conditions
matter, so read them before the table:

- Both sides ran on one shared Linux host (124 CPUs, load average 25 to 80 during the runs).
  The "Mac" side is the Unlatch engine API or the Linux FUSE frontend, not Finder. Nothing here
  was measured on a Mac.
- The network is shaped in an unprivileged network namespace with `tc netem`: one bottleneck per
  direction, MTU 1500, offloads off. Unlatch and sshfs cross the same shaped TCP path. The rows
  below use real `ssh` to `sshd` (the `-ssh` profiles), so both pay ssh encryption.
- The tree is synthetic: 100,000 entries plus 30,000 in a lazy `node_modules`, 793 MiB.
- Values are medians unless the row says p99. Quick-mode p99s come from 9 to 18 samples.

| What | RTT 40 ms, 50 Mbit/s: Unlatch | sshfs | RTT 100 ms, 20 Mbit/s: Unlatch | sshfs |
|---|---:|---:|---:|---:|
| First `ls -la` of a 1,000-file folder after mounting (FUSE) | 17.0 ms | 345 ms | 30.5 ms | 759 ms |
| Change on the VM visible on the client | 22.9 ms | 42.0 ms | 53.3 ms | 101 ms |
| Open a small file again (Unlatch: prefetched when its folder was listed) | 0.29 ms | 82.8 ms | 0.06 ms | 205 ms |
| Open a 4 KiB file never opened before | 43.4 ms | 124 ms | 104 ms | 309 ms |
| Open a 256 KiB file never opened before | 86.1 ms | 212 ms | 213 ms | 521 ms |
| Large file throughput (`ssh host cat`: 39.8 / 13.4 Mbit/s) | 44.1 Mbit/s | 30.6 Mbit/s | 14.4 Mbit/s | 11.1 Mbit/s |
| Upload a 4 KiB file until durable on the VM | 50.7 ms | 218 ms | 107 ms | 515 ms |
| Initial sync of a 100k-entry tree (plus 30k in a lazy folder) | 1.62 s | ~336 s | 3.04 s | ~377 s |
| p99 listing latency during a large download and upload | 72.4 ms | 1,141 ms | 146 ms | 2,642 ms |

Notes on reading it:

- The sshfs change-visibility row polls `stat` on the exact path. A folder listing in sshfs is
  bounded by its 20 s directory cache, which quick mode does not measure.
- The sshfs whole-tree row is a recursive walk stopped after 8 s and extrapolated.
- On an unshaped local link (RTT about 0.1 ms) sshfs is as fast as or faster than Unlatch on
  several rows: change visibility (0.52 ms against 2.81 ms) and the 4 KiB upload (3.6 ms
  against 4.9 ms). Unlatch's advantage grows with latency.
- Unlatch misses some of its own targets. Across all profiles, 161 rows were judged against
  targets and 22 failed. The 4 KiB upload is 1 to 6 ms over its target of RTT + 5 ms (three
  ordered disk syncs on the VM's shared disk; [bench/README.md](bench/README.md) has the
  breakdown). p99 latency under load is over its target of RTT + 30 ms in every profile where
  it was measured; the ping half of that measurement alone met the target in three of six.
- The daemon used 183 to 189 bytes of memory per indexed entry.

How the network is shaped, every scenario, and the full results are in
[bench/README.md](bench/README.md) and [bench/results/SCORECARD.md](bench/results/SCORECARD.md).

## How correctness is checked

The hardest behaviour lives inside `fileproviderd`, a macOS daemon that CI cannot run. The Linux
verification loop stands in for it. [docs/TESTING.md](docs/TESTING.md) describes each layer and,
as plainly, what it does not cover.

- **fpsim, a model of fileproviderd.** It drives the engine only through the same IPC the Swift
  extension uses and reproduces measured macOS File Provider behaviour: folders enumerated only
  once, enumeration backoff after errors, trash loops, replayed creates and writes, collisions
  between case and Unicode-normalization twins, and more. Each rule has a failing-first
  scenario: it runs against a scripted engine that breaks exactly that rule (28 switchable
  flaws) and must catch it.
- **Prior art: [sshdrive](https://github.com/alecdwm/sshdrive).** sshdrive is a File Provider drive
  over SFTP; its source is public but has no licence, so nothing from it is copied here. Its catalog of measured macOS File Provider quirks informed this
  design, and fpsim models those measurements, referenced by their `MQ-xxx` ids. No sshdrive code
  is used in Unlatch.
- **Fuzzing.** Seeded random interleavings of agent operations on the VM (atomic saves, renames,
  `rm -rf`, hard links, symlinks, `git checkout`-style churn), Mac operations through fpsim, and
  faults (killed daemon, cut link, engine restart, crashes before and after commit). The trees
  must converge, and no write on the VM may be lost. A failure prints its seed and a shrunk op
  list that `unlatch-bench fuzz --replay` runs again. A crash/replay matrix checks that every
  operation happens exactly once.
- **Stress.** `crates/unlatchd/tests/stress.rs` churns a real directory with hard links and
  daemon restarts and checks the index against the disk in both directions. A nightly workflow
  runs 6,000 seeds in each of three timing shapes, plus 300 fuzz seeds.
- **Benchmarks.** The netns bench above, with regression checks against `bench/baseline.json`.
- **Adversarial review.** Before implementation, four critics (File Provider, performance,
  consistency, security and UX) reviewed the design and raised 41 findings; the decisions on them are recorded in
  [docs/review/](docs/review/). Each claim that decided an amendment was checked, and the
  decisions became 25 design changes. [docs/TESTING.md](docs/TESTING.md) §5 lists 48 further
  requirements found by the fuzzer, the stress tests and a later review of the implementation, each with
  its regression test (one, a narrow timing case, is recorded there as open).
- **On a real Mac**, `unlatch probe` measures what a user sees: how fast a VM write appears in
  `~/Library/CloudStorage/`, first listings, cold and prefetched reads, saves, renames, a
  conflict and deletes. It has not been run on a Mac yet.

## Security model

- **It rides your ssh.** Unlatch opens no listening TCP port on either machine and has no
  account or server of its own. Everything goes through one ssh session started by your system
  `ssh`. On the VM, `unlatchd serve` listens on an abstract unix socket whose name includes a
  random nonce from a 0700 state directory, and both ends check the peer's user id.
- **The VM is untrusted input to the Mac.** Everything on the VM, and everything running as the
  VM user, is treated as untrusted. Symlinks are shown only when they stay inside the shared
  folder; others become read-only placeholders. The executable bit is hidden by default. Sockets,
  FIFOs, device files and names that are not UTF-8 are not exposed. Frame sizes are bounded
  before decompression.
- **The daemon stays inside the folder.** Every request names an item id, which the daemon
  resolves through its index. Names are validated, files are opened beneath the root without
  following symlinks, and recursive deletes never cross into another mount.
- **The daemon binary is checked.** It is a static binary, and its SHA-256 is verified before
  it runs on the VM.
- **What is stored.** On the Mac: the list of VMs (name, host, port, folder, and the path of an
  identity file if you chose one), a replica of the folder's metadata, and a content cache of
  files you opened or that were prefetched (5 GiB by default, least recently used first). On the
  VM: the daemon in `~/.unlatch` and its index and event journal under `~/.unlatch/state/`,
  which the daemon never shows in the shared folder. Passwords, passphrases and private keys
  are never stored.
- **Prompts only when you connect.** With key-based ssh nothing is asked. If ssh needs a
  password, passphrase, 2FA code or a new host-key confirmation, the Mac app shows ssh's own
  prompt in a dialog (`unlatch-askpass`) during an interactive connect and passes the answer to
  ssh. Background reconnects run with `BatchMode=yes` and never prompt: if one needs you, the
  menu says why and waits. Your shell's `SSH_AUTH_SOCK` is used only if you turn it on for that
  VM.
- **Only Unlatch can drive the engine.** The extension and the menu-bar app reach the engine
  over XPC, and both ends check the other's code signature (same team, allow-listed bundle ids).
  The extension is sandboxed. The host app is not, because it runs `/usr/bin/ssh` and reads
  `~/.ssh`.

## Repository layout

```
crates/
  unlatch-proto/     wire protocol (client ⇄ unlatchd) and IPC protocol (extension ⇄ engine)
  unlatchd/          VM daemon: index, inotify watcher, event log, file operations
  unlatch-core/      client engine: replica, content cache, sync, File Provider-shaped API
  unlatch-ffi/       C ABI for the Mac app (libunlatch.a, include/unlatch.h)
  unlatch-cli/       `unlatch`: FUSE mount, headless agent, doctor, probe, IPC debugging
  unlatch-bench/     netns network lab, synthetic trees, fpsim, fuzzer, benchmarks
mac/                 Swift: Unlatch (app + agent), UnlatchFileProvider, UnlatchShared,
                     UnlatchAskpass; XcodeGen project.yml, Signing.xcconfig, build scripts
npm/                 the `unlatch` installer, platform packages, release scripts
site/                the unlatch.dev page and the agent skill (SKILL.md)
scripts/             verify.sh (the full verification loop), bench.sh
bench/               bench docs, baseline, results and scorecards
docs/                DESIGN, TESTING, MACOS, INSTALL, PROTOCOL-NOTES, review/
```

## Building from source

**Rust (Linux or macOS).** Rust 1.85 or later. The FUSE frontend is pure Rust and needs only
`fusermount3` at run time.

```sh
cargo build --release -p unlatchd -p unlatch-cli
cargo test --workspace
scripts/verify.sh              # fmt, clippy, tests, fpsim, fuzz, bench (quick), compare
scripts/verify.sh --full       # all 9 network profiles, more seeds
```

The bench needs unprivileged user and network namespaces, `tc`, and `sshfs` for the
comparison rows; set `UNLATCH_NO_NETNS=1` to skip it where namespaces are unavailable. To try
the Linux client against a VM, put `unlatchd` on the VM and mount:

```sh
unlatch mount ~/vm --host dev-box --root ~/code --unlatchd /path/on/vm/unlatchd
fusermount3 -u ~/vm
```

`npm/scripts/e2e-local.sh` runs the whole installer flow (`share`, then `connect --mount`) over
`ssh localhost`.

**Mac app.** Building needs macOS 14 or later with full Xcode 16 or later (the Command Line
Tools alone cannot build it; the app itself targets macOS 13), rustup, and
[XcodeGen](https://github.com/yonaskolb/XcodeGen). To bundle the Linux daemon, also install `zig`
and `cargo-zigbuild`, or pass binaries built on Linux with `--prebuilt`.

The quickest way to try it on your Mac: one script checks every prerequisite (and prints the fix
for anything missing), finds your Apple Development signing team, builds, installs
`/Applications/Unlatch.app` and opens it.

```sh
mac/scripts/dev-install.sh                        # or: --prebuilt DIR with unlatchd-x86_64, unlatchd-aarch64
```

By hand:

```sh
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer   # if Xcode is not selected yet
mac/scripts/build-rust.sh                         # universal libunlatch.a and static unlatchd
swift test --package-path mac/UnlatchShared       # protocol fixtures, mappings, ssh-config parsing
cd mac && xcodegen generate && open Unlatch.xcodeproj
```

The Finder integration only runs when the app is signed with a real Team ID. Put your team and
a reverse-DNS prefix you own in `mac/Signing.local.xcconfig` (git-ignored):

```
UNLATCH_TEAM_ID = ABCDE12345
UNLATCH_BUNDLE_PREFIX = com.example
```

Forks must change the bundle prefix. An ad-hoc ("Sign to Run Locally") or unsigned build
compiles but cannot run the File Provider extension. Whether a free Personal Team is enough has
not been verified. [docs/MACOS.md](docs/MACOS.md) explains why, what CI checks in the bundle,
and how to debug a build that Finder does not show.

## Contributing

Issues and pull requests are welcome at
[github.com/sudhanshuheda/unlatch](https://github.com/sudhanshuheda/unlatch). Work happens on
the `dev` branch.

- Run `scripts/verify.sh` before opening a pull request. It exits non-zero on any failed gate or
  on a benchmark regression of more than 10%.
- A behaviour change in the engine or daemon should come with a test: an fpsim scenario that
  fails without the change, a fuzz seed, or a unit test. A fuzz failure's replay file is the
  best bug report.
- CI's macOS minutes are the scarce ones. To iterate on the Mac app, run only the macOS jobs:
  `gh workflow run ci.yml --ref <branch> -f only=macos`.
- If you can run the app on a Mac, a JSON report from `unlatch probe` for your macOS version
  is among the most useful contributions right now.
- Cite measured macOS behaviour by its `MQ-xxx` id. Do not copy code or text from sshdrive.

## License

Unlatch is dual-licensed under the [MIT license](LICENSE-MIT) or the
[Apache License 2.0](LICENSE-APACHE), at your option.
