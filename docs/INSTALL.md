# Installing Unlatch: `npx unlatch`

Unlatch names the product, the npm package, the binaries (`unlatchd`, `unlatch`),
`$UNLATCH_HOME` and `~/.unlatch`. Every user-facing name in the installer is defined in
`npm/unlatch/lib/names.js`.

> **Status: pre-release.** The installer, the VM daemon and the Linux FUSE client run and are
> tested end to end (§6). The Mac app compiles in CI but has not been released, and nothing in
> it has run on a real Mac from this repository yet (docs/MACOS.md §8). The first npm release,
> `0.1.0-alpha.1`, is Linux only (§7): it ships `unlatch`, `unlatch-linux-x64` and
> `unlatch-linux-arm64`, not `unlatch-darwin-universal`.

## 1. The user journey

**On the VM** (over ssh, or from Claude Code or Codex running there). This is real output from
the Linux test box; only the user, host and path names are changed:

```text
$ cd ~/code && npx unlatch
› installing the Unlatch daemon
✓ daemon unlatchd 0.1.0 installed in /home/sam/.unlatch
› checking this VM
› starting the index server for this folder
✓ index server started (pid 81234)

Unlatch is sharing /home/sam/code

On your Mac, run this in Terminal:

    npx unlatch connect sam@dev.tail1234.ts.net:~/code

(dev.tail1234.ts.net: Tailscale MagicDNS name (works from any device on your tailnet))

If your Mac cannot reach that name, use one of these instead:
    npx unlatch connect sam@100.101.1.2:~/code    # Tailscale IP
    npx unlatch connect sam@dev:~/code    # this machine's host name (…)
```

`npx unlatch share [dir] --json` is the agent form (§4).

**On the Mac.** The app has not shipped, so this output was rendered by the installer against
the mocked app used in `test/mac.test.js`:

```text
$ npx unlatch connect sam@dev.tail1234.ts.net:~/code
✓ using your ~/.ssh/config alias "dev" for dev.tail1234.ts.net
› checking ssh dev
✓ ssh works (Linux x86_64)
› installing Unlatch.app 0.2.0 in /Applications
✓ Unlatch.app installed (/Applications/Unlatch.app)
› adding "dev-code" (answer any ssh dialogs that appear)
› Connecting
› Syncing: 48213 entries received

dev:~/code is in Finder: /Users/sam/Library/CloudStorage/Unlatch-dev-code
Status: npx unlatch status   Remove: npx unlatch remove dev-code
```

Running `npx unlatch` with no arguments in a Mac terminal starts a wizard. It lists the `Host`
aliases from `~/.ssh/config` (following `Include` and skipping wildcards and `Match`). You pick
one or type `user@host`, then choose a folder (default `~`). The wizard tests ssh
non-interactively and explains how to fix what failed: host key, key or agent, DNS/VPN,
timeout, refused. After that it runs the same steps as `connect`.

**On a Linux desktop:** `npx unlatch connect sam@dev:~/code --mount ~/dev` mounts the folder with
FUSE (`unlatch mount`). `npx unlatch remove --mount ~/dev` unmounts it.

## 2. What each mode does

### VM mode (`share`, and plain `npx unlatch` on Linux)

1. Resolves `unlatch-linux-{x64,arm64}` and verifies the packaged `unlatchd` against the
   manifest's sha256.
2. **Install directory.** Takes the first usable directory from `$UNLATCH_HOME`,
   `$XDG_DATA_HOME/unlatch`, `~/.unlatch`, `/var/tmp/unlatch-$UID`, `/tmp/unlatch-$UID`. A directory is
   usable when it is owned by you, has mode 0700, is not a symlink, is on a local filesystem,
   and passes an exec test. The Mac-side bootstrap
   (`crates/unlatch-core/src/transport/bootstrap.rs`) uses the same probe with the same code.
   `npm/unlatch/test/names.test.js` compares the two and fails if they drift apart.
3. Installs `unlatchd-<crate version>-<sha256[..16]>` there: copies it to a `.tmp` file, fsyncs
   it, verifies the sha256, sets mode 0700, then renames it into place. The Mac app bundles the
   byte-identical file, so its bootstrap finds the file, verifies it and skips the upload.
4. Checks: the root exists and is writable. The filesystem type is local (NFS, CIFS, FUSE, 9p
   and virtiofs get a warning because they are polled). Folders to watch are counted against
   `fs.inotify.max_user_watches`; lazy folders such as `node_modules` and `.git` are left out,
   and the daemon uses at most 50% of the limit. Disk space is enough. An ssh server is present:
   the session's own `SSH_CONNECTION`, a listener on port 22, systemd units, or Tailscale SSH.
   On hosts where logind kills user processes at logout, linger must be on.
5. Pre-starts `unlatchd serve --root <canonical root>` with `UNLATCH_HOME=<install dir>`. This is
   exactly the server a Mac's `unlatchd connect` attaches to, so the first scan is already done
   when the Mac connects. The server exits after 24 h without clients. `--no-serve` skips this
   step.
6. Prints the Mac command (`npm/unlatch/lib/hosts.js`). The host is chosen in this order: the
   Tailscale MagicDNS name (when `tailscale status --json` reports a running backend), the
   Tailscale IP, the server address from `SSH_CONNECTION` (or, without it, the source address of
   `ip route get 1.1.1.1`), then `hostname -f` or the host name. A non-default sshd port becomes
   `--port`. The folder is written as `~/…` when it is under `$HOME`.

   **Cloud VMs behind 1:1 NAT** (AWS, Google Cloud, Azure without Tailscale): sshd sees the
   private VPC address, which the Mac cannot reach. When that address is private but the ssh
   client's address is public, `share` asks the cloud's instance metadata service for the
   public IPv4 address (169.254.169.254: AWS IMDSv2 token + `public-ipv4`, GCP
   `access-configs/0/external-ip`, Azure `publicIpAddress`; all in parallel, 800 ms each) and
   puts it first. If none answers, the Mac command carries the placeholder `<your-ssh-host>`,
   `host_guess` is `true` and `host_hint` says what to put there (the address or
   `~/.ssh/config` alias the user ssh's with, and `--port` if a different port is forwarded).
   Pasting the placeholder unedited makes `connect` stop with that same advice. A private
   address reached from a private client (same LAN/VPN, or a jump host) is still offered first,
   but flagged as a guess with the same hint.

   On the Mac, `connect` swaps the host for a `~/.ssh/config` alias whose `HostName` matches it
   exactly (case-insensitive), so the alias's `User`, `IdentityFile` and `ProxyJump` apply. A
   short `HostName` also matches a Tailscale MagicDNS name with the same first label
   (`dev` ↔ `dev.tail1234.ts.net`); no other short/long pair does, because `dev` and
   `dev.corp.example.com` may be different machines.
7. `--install-dir DIR` installs into DIR instead. The Mac's bootstrap never looks there (it
   probes the list in step 2 as a non-interactive ssh session sees it), so `share` warns that
   the Mac would start a second daemon unless `UNLATCH_HOME=DIR` is set where non-interactive ssh
   sessions see it. It warns the same way when `$UNLATCH_HOME` or `$XDG_DATA_HOME` picked the
   directory, since those are often exported only for interactive shells. `status`, `doctor`
   and `uninstall` take the same `--install-dir`; with it they look only in DIR.

### Mac mode (`connect`, wizard, and the other Mac commands)

1. Tests ssh with `BatchMode=yes`. Host-key and auth failures only produce a warning, because the
   app's askpass dialogs handle them. DNS, timeout, refused and no-route failures stop the run
   (exit 3) unless `--force` is given.
2. Installs the app from `unlatch-darwin-universal`. It goes into `/Applications`, or into
   `~/Applications` if `/Applications` is not writable. The steps are
   `ditto -x -k app.zip <tmp in the same dir>`, then a rename into place (an older copy is quit
   and moved aside first), then `xattr -dr com.apple.quarantine`. The installed
   `CFBundleShortVersionString` is compared with the packaged version as semver: equal skips
   the install; **newer is never replaced** (an older `unlatch` from a stale npx cache keeps the
   newer app and warns, with action `newer`); only an older or unparseable version is replaced.
   `update --reinstall` is the explicit way back to the packaged version.
3. Launches the app once with `open -g` (MQ-061: an app that was never launched has no
   extension registered). After an update it runs `--cli repair-agent` (MQ-062/063).
4. Waits for the background agent. If macOS asks for approval, it opens System Settings →
   Login Items and waits up to 3 minutes; without a TTY it exits 3 straight away.
5. `--cli add …`. This step is skipped when a VM with the same host, port and root already
   exists, which makes `connect` idempotent.
6. Polls `--cli status` until the domain is `Live`. `NeedsUser` or `Paused` exits 3 with the
   reason. Any other state times out after 3 minutes; the domain keeps syncing in the
   background.
7. Runs `--cli open <id>`, which opens the Finder location.

`status`, `open [name]`, `remove <name>`, `doctor`, `update`, and `uninstall --yes`. Uninstall
follows the MQ-063 order: every domain is removed first while the agent is still alive, then
`--cli unregister-agent` runs, the app is quit, and the bundle is deleted. Files on the VM are
never touched. Edits that had not reached the VM yet are not uploaded: macOS moves them to a
folder whose path `remove`/`uninstall` print (docs/MACOS.md §8).

### Linux client mode (`connect … --mount <dir>`)

1. Tests ssh with `BatchMode=yes`. This mode has no askpass, so the test must pass.
2. Probes the VM over ssh with the same install-directory probe. The daemon is reused if the
   exact file is already there. Otherwise it is uploaded when the VM's architecture matches this
   machine's package: `cat > <dir>/<name>.tmp`, then sha256 verification, chmod 0700 and mv.
   If the architecture does not match, an existing `unlatchd-<version>-*` that `share` put there
   is used.
3. Runs `unlatch mount <dir> --host <dest> --root <path> --unlatchd <exact path> [--port]
   [--identity] [--state] [--name]`. With `--unlatchd`, the daemon's install directory is the
   directory the binary sits in, so the mount attaches to the same `serve` that `share`
   started.

Test-only flags: `--remote-home <dir>` puts that directory first in the VM probe, and
`--state <dir>` sets the client state directory. `UNLATCH_PLATFORM_DIR=<dir>` uses an unpacked
platform package instead of the installed one.

## 3. The app's command-line entry

The npm installer drives the Mac app through a command-line entry that lives in the app binary.
The Swift code is `mac/Unlatch/CLI/CLIMain.swift`, hooked in from `UnlatchEntry.swift`. It uses the
same XPC management calls as the menu-bar UI (`AgentClient`), so it has no privileges the UI
lacks.

```text
<App>.app/Contents/MacOS/<Exe> --cli status            [--json]
<App>.app/Contents/MacOS/<Exe> --cli add --name <n> --host <user@host[:port]|alias> --root <dir>
                                    [--port <p>] [--identity <file>] [--use-shell-agent] [--json]
<App>.app/Contents/MacOS/<Exe> --cli remove <id|name>  [--json]
<App>.app/Contents/MacOS/<Exe> --cli open <id|name>    [--no-reveal] [--json]
<App>.app/Contents/MacOS/<Exe> --cli register-agent | repair-agent | unregister-agent [--json]
```

- With `--json`, stdout carries exactly one object:
  - success: `{"ok": true, …}`
  - failure: `{"ok": false, "error": "…", "code": "usage|not_configured|requires_approval|agent_unavailable|not_found|failed"}`
- Exit codes:
  - 0: ok
  - 1: failed
  - 2: usage error
  - 3: the user has to act (approval, agent unreachable, timeout)
- A watchdog bounds each call: 30 s, 130 s for `remove`, or 610 s for `add`, which may show ssh
  dialogs.
- `status` → `{"app_version", "agent": "enabled|requires_approval|not_registered|not_found", "domains": [Domain]}`.
- `add` → `{"domain": Domain}`. It registers the domain and runs the interactive first connect
  (askpass dialogs may appear). If the connect fails, the domain is removed again, exactly as
  in the Add VM window.
- `open` → `{"id", "path"}`. It opens the Finder location unless `--no-reveal` is given.
- `remove` → `{"id", "preserved"}`. `preserved` is the folder where macOS kept files whose edits
  had not reached the VM, or `null` when there were none.
- `Domain` = `{"id", "name", "host", "port", "root", "identity", "state", "detail", "entries", "path", "last_error"}`:
  - `state` is `Connecting`, `Syncing`, `Live`, `Offline`, `NeedsUser`, `Paused` or `Unknown`;
  - `path` is the `~/Library/CloudStorage/…` URL once the domain is registered.
- `repair-agent` runs `unregister`, waits 6 s, then runs `register` (MQ-063).

The installer reads the bundle and executable names from the platform package's
`manifest.json` (defaults: `Unlatch.app`, `Unlatch`).

## 4. Agents (Claude Code, Codex)

- `npx -y unlatch share --json` prints:
  - `mac_command`
  - `host_guess` (bool): the first host may not work from the Mac; relay `host_hint`
  - `host_hint`: what to do about it (or `null`)
  - `host_candidates[]` (`host`, `source`, `note`, `mac_command`); `source` is one of
    `tailscale`, `tailscale-ip`, `cloud-metadata`, `ssh-connection`, `route`, `hostname`,
    `placeholder`
  - `path`, `remote_path`, `user`, `port`, `daemon_version`
  - `daemon {path, install_dir, installed_now, server}`
  - `linux_command`, `warnings[]` (each with its fix), `checks[]`

  Progress messages go nowhere in `--json` mode.
- `npx unlatch skill` installs the skill file `npm/unlatch/skill/SKILL.md`. It is generated from
  `site/src/SKILL.md` by `site/build.sh` (the same text the site serves as `/SKILL.md`);
  `test/skill.test.js` fails if the copies drift:
  - Claude Code: `~/.claude/skills/unlatch/SKILL.md`
  - Codex CLI: `~/.agents/skills/unlatch/SKILL.md`, the user-level skills directory in the
    current Codex docs. It is also written to `~/.codex/skills/unlatch/` when that older
    directory already exists.
  - `--claude`, `--codex` and `--all` choose where to install; `--print` writes the file to
    stdout instead.

## 5. Packages

| Package | Contents | `os`/`cpu` |
|---|---|---|
| `unlatch` | the zero-dependency installer (`bin/unlatch.js`, `lib/`, `skill/`) | any |
| `unlatch-linux-x64` | `bin/unlatchd` (static musl, byte-identical to the app's), `bin/unlatch` (static musl), `manifest.json` | linux/x64 |
| `unlatch-linux-arm64` | the same for aarch64 | linux/arm64 |
| `unlatch-darwin-universal` | `app/<App>.app.zip` (notarized, stapled), `bin/unlatch` (universal), `manifest.json` | darwin/x64+arm64 |

`manifest.json` has the schema `unlatch-platform/1`: `{platform, version, daemon?: {file, arch,
sha256, size, crate_version, remote_name}, cli?: {file, sha256, size}, app?: {zip, bundle,
executable, version, bundle_id, sha256}}`. It is written by `npm/scripts/assemble.mjs`.

## 6. Testing (Linux)

```sh
cd npm/unlatch && npm test                       # node:test, no dependencies: Mac flow mocked,
                                                 # VM flow with a shell stand-in for unlatchd
cargo build --release -p unlatchd -p unlatch-cli
npm/scripts/e2e-local.sh                         # assemble + npm pack + install into a temp prefix,
                                                 # share → connect --mount over `ssh localhost`,
                                                 # VM/client writes, unmount, uninstall
```

The e2e script needs `ssh localhost` to work without prompts, and FUSE. It never touches
`~/.unlatch`, and it checks that at the end.

## 7. Releasing (maintainers)

1. **One-time setup.**
   - Publish rights for the four unscoped packages: `unlatch`, `unlatch-linux-x64`,
     `unlatch-linux-arm64` and `unlatch-darwin-universal` (no npm org or scope is needed).
   - Add an npm automation token as the repository secret `NPM_TOKEN`.
   - The Mac signing secrets are listed at the top of `.github/workflows/release.yml`.
2. **Bump the version.** Set the same version in `Cargo.toml` (`[workspace.package] version`)
   and `npm/unlatch/package.json`, commit, and tag `vX.Y.Z`. The npm workflow refuses to run if
   the tag and the Cargo version differ, because the Cargo version is part of the daemon's file
   name on the VM.
3. **`release.yml`** builds unlatchd (musl, x86_64 and aarch64) and the signed, notarized app,
   then uploads the DMG and `unlatchd-*` to the draft GitHub release.
4. **`npm-release.yml`** starts when `Release` succeeds, or by hand with a tag:
   - builds the `unlatch` CLI (musl via cargo-zigbuild; universal on macOS);
   - takes unlatchd from the release assets and checks it against the app's
     `Resources/unlatchd/*.sha256`;
   - zips the stapled app out of the DMG with `ditto -c -k --keepParent`;
   - assembles the four packages with one version;
   - publishes with `npm/scripts/publish.mjs` (`npm publish --provenance --access public`):
     platform packages first, `unlatch` last and only once every platform package it pins is
     on the registry. Versions already on the registry are skipped, so after a partial failure
     "Re-run failed jobs" (or running the workflow by hand for the same tag) publishes only
     what is missing. If the registry cannot say whether a version exists, it stops instead
     of guessing. Pre-release versions (`X.Y.Z-rc.1`) are published under the `next` dist-tag.
5. Publish the GitHub release (it is a draft until then).

By hand, without CI, for a Linux-only pre-release (how `0.1.0-alpha.1` is cut; the Mac app is
not part of it):

```sh
V=0.1.0-alpha.1
# Map build-machine paths out of the binaries (panic locations, debug info).
export RUSTFLAGS="--remap-path-prefix=$PWD=/unlatch --remap-path-prefix=$HOME/.cargo=/cargo --remap-path-prefix=$HOME/.rustup=/rustup"
for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  cargo zigbuild --release --locked -p unlatchd -p unlatch-cli --target "$t"
done
# llvm-strip (rustup component llvm-tools) handles both arches. The CLI is stripped fully;
# unlatchd keeps its symbols but drops DWARF, which still carries zig's C/libc source paths.
# (No Mac app ships in this release, so unlatchd need not match an app-bundled copy byte for byte.)
llvm-strip <unlatch-x86_64> <unlatch-aarch64>
llvm-strip --strip-debug <unlatchd-x86_64> <unlatchd-aarch64>
node npm/scripts/assemble.mjs platform --key linux-x64 --version $V --out dist/unlatch-linux-x64 \
  --unlatchd <musl unlatchd-x86_64> --unlatch <musl unlatch-x86_64>
node npm/scripts/assemble.mjs platform --key linux-arm64 --version $V --out dist/unlatch-linux-arm64 \
  --unlatchd <musl unlatchd-aarch64> --unlatch <musl unlatch-aarch64>
node npm/scripts/assemble.mjs main --version $V --no-darwin --out dist/unlatch
(cd dist && npm pack ./unlatch-linux-x64 ./unlatch-linux-arm64 ./unlatch)
E2E_TARBALLS=dist npm/scripts/e2e-local.sh       # the packed tarballs, end to end
node npm/scripts/publish.mjs --tag latest dist/unlatch-linux-x64 dist/unlatch-linux-arm64 dist/unlatch
```

`main --no-darwin` leaves `unlatch-darwin-universal` out of the installer's
`optionalDependencies`, so no install asks the registry for a package that does not exist, and
on a Mac every app command stops at once with "the Mac app is not published yet", how to build
it from source, and exit code 1 (`--json`: `"code": "not_published"`). `share` adds the same
note to its `warnings` and sets `mac_app_published: false`. Without the flag (the CI path) the
installer pins all three platform packages.

`publish.mjs` publishes `unlatch` itself only after every platform package it pins exists at
that version, and stops with an error otherwise (an `unlatch` whose pinned package is missing
would print the "platform package is not installed" error on that platform).
