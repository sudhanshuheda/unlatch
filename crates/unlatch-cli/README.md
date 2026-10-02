# unlatch (CLI)

The `unlatch` command: a Linux FUSE frontend for the Unlatch engine, a headless engine host for the
File Provider extension, IPC debugging commands, a VM checker, and the user-side verification
probe.

```
unlatch mount <MOUNTPOINT> (--host <ssh-dest> | --command <argv…>) --root <remote root> [options]
unlatch agent --config <file.json> [--domain NAME]…
unlatch ls [PATH] | stat PATH | cat PATH   [--socket S --domain D | --config C [--domain D]]
unlatch status                             [--socket S --domain D | --config C [--domain D]]
unlatch doctor --host <ssh-dest> --root <remote root> [--json]
unlatch probe  --local <PATH> --ssh <ssh-dest> --remote <remote root> [--json out.json] [-n 20]
```

Logging goes to stderr; set `UNLATCH_LOG_LEVEL` (or `RUST_LOG`), e.g. `UNLATCH_LOG_LEVEL=debug`.

## `unlatch mount` (Linux)

```
unlatch mount ~/vm --host dev-box --root ~/code            # detaches once mounted
unlatch mount ~/vm --host dev-box --root ~/code --foreground
fusermount3 -u ~/vm                                       # or Ctrl-C / SIGTERM in the foreground
```

For tests and benchmarks, talk to a local `unlatchd` instead of ssh (put `--command` last; it takes
the rest of the line):

```
unlatch mount /tmp/mnt --root /srv/x --foreground --command /path/to/unlatchd stdio --root /srv/x --state /tmp/hs
```

Options: `--state DIR` (client replica and cache; default `~/.unlatch/mount/<hash of source>`),
`--name`, `--ttl-secs` (default 300), `--no-prefetch`, `--no-exec`, `--workers` (default 16),
`--wait-secs` (wait for the first sync before mounting; a state dir with no replica always waits
up to 60 s), `--port`, `--identity`, `--ssh-arg` (repeatable), `--unlatchd <remote command>`.

Without `--foreground` the command re-executes itself in a new session with stderr going to
`<state>/mount.log`, and returns once the kernel has accepted the mount (or prints the log tail
if it failed).

### How it stays fast and correct

* **Long kernel caches, pushed invalidation.** Entries and attributes are handed to the kernel
  with a long TTL (`--ttl-secs`), missing names are cached as negative dentries, directory
  listings are kept in the kernel (`FOPEN_CACHE_DIR`), and file pages survive re-opens
  (`FOPEN_KEEP_CACHE`) while the content version is unchanged. Correctness comes from the engine's
  `EngineEvent::ReplicaChanged`: a dedicated thread turns each change into `inval_entry` (old
  and new names, which also kills negative dentries) and `inval_inode` (attributes, plus cached
  pages only when the content version the kernel may hold actually moved). A reply computed
  before a change but delivered after it goes out with TTL 0 (a change epoch is checked). Before
  the engine is `Live`, negative lookups are not cached.
* **Never block the request loop on the network.** Lookups, reads, listings and uploads run on a
  worker pool with their reply; `getattr`, `open`, `create`, `opendir`, `readlink`, `statfs`
  and the close of clean descriptors answer inline from the local replica. Read-only opens set
  `FOPEN_NOFLUSH`, so `close()` costs no round trip.
* **Writes.** Each inode being written has one scratch file shared by its descriptors. Content
  is loaded lazily at the first write (or starts empty with `O_TRUNC`, which is handled
  atomically); the base version is the one actually loaded. On `close`/`fsync` the file is
  uploaded with `Engine::modify(base)`. If the VM changed meanwhile, the engine keeps the VM's
  content and stores ours as a conflict copy on the VM (`should_fetch_content`); the mount then
  drops its cached pages so readers see the VM version. An upload that still fails when the last
  descriptor closes is moved to `<state>/unsynced/` instead of being dropped.
* **Creates are deferred.** A new file gets a provisional inode and is created on the VM by its
  first upload (one round trip per new file). A file deleted before it was closed never reaches
  the VM (editor swap files).
* **POSIX rename over an existing file** removes the target first (the daemon renames with
  `RENAME_NOREPLACE`), so editor "write temp + rename" saves work. `RENAME_EXCHANGE` is
  refused.
* **Permissions.** Files show as owned by the mounting user; the owner permission bits are what
  the *VM user* may do (`Entry.access`), and the kernel enforces them (`default_permissions`).
  `chmod` maps to the exec bit only (other bits are accepted but not synced); `chown` to another
  user is refused. The exec bit is shown unless `--no-exec` (the macOS exec rule of D12 does not
  apply to a Linux mount).
* **Inodes.** Allocated from a counter, never reused; mappings are dropped on `forget`. On a
  daemon index change (`EngineEvent::Reimport`) every mapping is dropped and stale inodes answer
  `ESTALE`, so an old inode can never alias a different file.
* **Errors.** `Offline`/`NeedsUser` → `EHOSTDOWN`, a delete or write refused because the VM
  changed (`DeletionRejected`, `VersionMismatch`) → `EBUSY`, `IndexChanged` → `ESTALE`, a
  non-empty `rmdir` → `ENOTEMPTY`.

Measured on the dev box (loaded, ~60 load average), release build, `--command unlatchd stdio`,
1000-entry directory, Python `scandir`+`lstat` loop:

| | local ext4 | unlatch | sshfs (`localhost`) |
|---|---|---|---|
| `ls -la` warm | 2.7 ms | 2.6 ms | 3.5 ms |
| `ls -la` first | 3.2 ms | 25 ms | 76 ms |
| `stat` warm | 2.3 µs | 2.4 µs | 2.1 µs |
| open+read+close warm | 13 µs | 62 µs | 597 µs |

## `unlatch agent`

Runs one engine per domain (loading its replica immediately, connecting in the background) and
serves the IPC protocol on each domain's unix socket (mode 0600), until SIGINT/SIGTERM/SIGHUP.
A stale socket file left by a dead agent is removed; a live one is an error.

### Config schema (`--config`, default `$UNLATCH_CONFIG` or `~/.config/unlatch/agent.json`)

```json
{
  "client_name": "laptop",
  "domains": [
    {
      "name": "dev",
      "transport": { "ssh": { "destination": "dev-box", "port": 22,
                              "identity": "~/.ssh/id_ed25519", "extra_args": ["-o", "ProxyJump=bastion"] } },
      "remote_root": "~/code",
      "state_dir": "~/.unlatch/client/dev",
      "socket": "~/.unlatch/run/dev.sock"
    },
    {
      "name": "local-test",
      "transport": { "command": { "argv": ["/path/unlatchd", "stdio", "--root", "/srv/x", "--state", "/tmp/hs"],
                                  "env": { "UNLATCHD_LOG": "/tmp/unlatchd.log" } } },
      "remote_root": "/srv/x"
    }
  ]
}
```

| Field | Required | Meaning |
|---|---|---|
| `client_name` | no | Machine name used in conflict-copy names (default: this host's name). Sanitized (≤ 32 bytes, no `/`, NUL or control characters). |
| `domains[].name` | yes | Domain name (1–64 chars, no `/`); the IPC `Hello` must use it. Unique. |
| `domains[].transport` | yes | Either `{"ssh": {destination, port?, identity?, extra_args?}}` or `{"command": {argv, env?}}`. A destination starting with `-` is rejected. |
| `domains[].remote_root` | yes | Root on the VM, absolute or `~/…`. |
| `domains[].state_dir` | no | Replica + default cache/temp location. Default `~/.unlatch/client/<name>`. Created 0700. |
| `domains[].socket` | no | IPC socket. Default `<state_dir>/engine.sock`. At most 103 bytes (macOS `sun_path`), unique. |
| `domains[].cache_dir`, `temp_dir` | no | Override `<state_dir>/cache`, `<state_dir>/tmp`. |
| `domains[].client_name` | no | Per-domain override of `client_name`. |
| `domains[].unlatchd_command` | no | Remote `unlatchd` command (skips bootstrap/upload). |
| `domains[].remote_install_dir` | no | VM directory the bootstrap tries first for `unlatchd` (same checks as the default probe). |
| `domains[].unlatchd_upload` | no | `[{arch, path, sha256}]` binaries to upload when missing on the VM. |
| `domains[].cache_budget_bytes` | no | Content cache budget (default 5 GiB). |
| `domains[].prefetch` | no | `{max_file, per_container, bytes_per_min, burst}` in bytes. |
| `domains[].default_lazy_names` | no | Lazy directory names suggested for a new server index. |
| `domains[].list_timeout_ms` | no | How long a listing of an unknown container may wait. |
| `domains[].expose_exec` | no | Show the exec bit (default false, D12). |
| `domains[].mass_delete_frac`, `mass_delete_abs`, `mass_delete_min` | no | Mass-deletion guard thresholds (default 0.20 / 1000; the fraction rule only applies above `mass_delete_min` = 32 removals). |
| `domains[].ssh_env` | no | Environment for spawning ssh (`{"SSH_AUTH_SOCK": "…"}`). |
| `domains[].askpass` | no | Askpass helper for interactive connects. |

Unknown fields are errors (typos do not silently fall back to defaults). `~/` is expanded in
local paths.

## `unlatch ls | stat | cat | status`

Debug an engine through its IPC socket, exactly as the File Provider extension would. Paths are
relative to the domain root and matched by display name.

```
unlatch ls src --socket ~/.unlatch/run/dev.sock --domain dev --ids
unlatch cat README.md --config agent.json --domain dev
unlatch status                      # every domain in the default config
```

## `unlatch doctor`

Checks, over one non-interactive ssh session (`BatchMode=yes`), and prints a fix for each
problem: local ssh / FUSE (or `fileproviderctl` on macOS), ssh reachability (with hints for
host-key, auth, DNS, timeouts, login-URL prompts), remote arch (x86_64/aarch64), the root
(exists, writable, filesystem type — NFS/CIFS/FUSE/9p/virtiofs roots are polled, D14/D22),
inotify limits and this user's current watch usage, the install-directory probe in D22 order
(`$UNLATCH_HOME`, `$XDG_DATA_HOME/unlatch`, `~/.unlatch`, `/var/tmp/unlatch-$UID`, `/tmp/unlatch-$UID`:
owned, 0700, not a symlink, local fs, passes an exec test), logind linger, and the ssh RTT
(median of 10 round trips). Exit status 1 if any check fails. `--json` prints the report.

## `unlatch probe`

The user-side verification probe (review §2(f)8). It runs unchanged on Linux against a
`unlatch mount` and on macOS against `~/Library/CloudStorage/<domain>`: it only uses portable file
APIs locally and polls `stat` every 1 ms to detect "visible". VM-side completion is detected by a
shell loop over the same ssh session that reports back; one-way latencies are the observed time
minus half the median ssh RTT (both are recorded). Everything is created under
`<remote>/unlatch-probe-<pid>-<rand>` and removed at the end.

Checks: `dir_visible`, `vm_write_visible` (VM write via ssh → visible locally; p50/p95 over
`-n`), `first_ls_1k` (first listing of a never-listed 1000-entry directory created via ssh: time,
entries seen by the first listing, time until complete, `ls -la` time), `cat_small` (cold and
second read of 4 KiB files, content verified), `rename_round_trip`, `save_round_trip` (plus the
local write+close time), `conflict` (Mac opens, agent edits on the VM, Mac saves: pass iff both
versions survive; reports which version holds the canonical name and the conflict-copy names, and
how long the local view took to converge), `delete_round_trip`. Exit status 1 unless every check
passed and cleanup succeeded.

### JSON report (`--json`), schema `unlatch-probe/1`

```json
{
  "schema": "unlatch-probe/1",
  "unlatch_version": "0.1.0",
  "started_at": "2026-09-30T14:28:54Z",
  "os": { "family": "linux|macos", "version": "Ubuntu 24.04.4 LTS | macOS 15.1", "kernel": "6.8.0-139-generic", "arch": "x86_64" },
  "fileproviderctl": null,
  "local": "/home/me/vm", "ssh": "dev-box", "remote": "~/code", "subdir": "unlatch-probe-123-0a1b2c3d",
  "rtt_ms": { "n": 10, "min": 0.1, "p50": 0.14, "p95": 0.2, "max": 0.3, "mean": 0.15 },
  "poll_interval_ms": 1.0,
  "checks": [
    { "id": "vm_write_visible", "title": "…", "ok": true,
      "samples_ms": [ … ], "summary_ms": { "n": 20, "min": …, "p50": …, "p95": …, "max": …, "mean": … },
      "extra": { "raw_ms": [ … ], "definition": "…" } },
    { "id": "conflict", "ok": true, "samples_ms": [], "summary_ms": null,
      "extra": { "both_preserved": true, "vm_version_preserved": true, "mac_version_preserved": true,
                 "canonical_name_holds": "vm|mac|neither", "conflict_copies": ["k (conflict from … ).txt"],
                 "local_converged_ms": 2.5, "local_close_error": null } }
  ],
  "cleanup_ok": true
}
```

* `ok`: `true` pass, `false` fail (with `error`), `null` not run.
* `fileproviderctl`: whether `/usr/bin/fileproviderctl` exists (macOS), `null` elsewhere.
* Extra fields per check: `first_ls_1k` → `first_ls_ms`, `first_ls_entries`, `complete_ms`,
  `ls_la_ms`, `ls_la_entries`; `cat_small` → `cold`, `warm` (summaries), `warm_samples_ms`,
  `content_mismatches`; `save_round_trip` → `local_write_close` (summary).

## Tests

```
export CARGO_TARGET_DIR=…            # any
cargo test -p unlatch-cli              # unit tests + FUSE mount tests against an in-memory backend
cargo build -p unlatchd                # for the end-to-end tests (or set UNLATCHD_BIN)
cargo test -p unlatch-cli -- --ignored # e2e: real engine + `unlatchd stdio` + FUSE; ssh localhost probe
```

* The FUSE layer is written against a small `Backend` trait (implemented by `Engine`), so
  `src/fuse/tests.rs` mounts it for real (skipped when `/dev/fuse` or `fusermount3` is missing)
  over an in-memory backend that models the engine contract (seq versions, conflict copies,
  never-failing creates, `DeletionRejected`, `ReplicaChanged` events), with hour-long TTLs to
  prove that VM changes arrive through push invalidation, that listings and pages are served
  from the kernel cache until invalidated, and that conflicting edits keep both versions.
* `tests/e2e.rs` (ignored): the real `unlatch` binary mounts `unlatchd stdio` and does
  ls/cat/write/overwrite/rename/mkdir/rm/rmdir, checks VM-side changes arrive, and that SIGTERM
  unmounts; a second test runs `unlatch agent` and drives `ls/stat/cat/status` over IPC.
* Ignored ssh tests (`ssh localhost` must work non-interactively): the remote-shell session and
  the probe end to end against an identity "mount".
