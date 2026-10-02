# Unlatch on macOS

How the Mac side is put together, how to build and sign it, how to install it, and what to do when
Finder does not show your VM. The design rationale is in `docs/DESIGN.md` as amended by
`docs/review/2026-09-30-design-review.md`. `MQ-xxx` ids refer to the measured File Provider quirks
listed in `docs/review/sshdrive-macos-quirks.md`.

## 1. What is in the bundle

```
Unlatch.app/Contents/
  MacOS/Unlatch                         menu-bar UI; with --agent: the engine agent (libunlatch inside)
  MacOS/unlatch-askpass                 SSH_ASKPASS helper (native password / host-key dialogs)
  PlugIns/UnlatchFileProvider.appex     NSFileProviderReplicatedExtension, sandboxed, thin shim
  Library/LaunchAgents/<prefix>.unlatch.agent.plist   SMAppService agent: MachService + KeepAlive
  Resources/unlatchd/unlatchd-{x86_64,aarch64}(.sha256) static Linux daemons uploaded to VMs
```

| Piece | Runs as | Talks to |
|---|---|---|
| `Unlatch` (UI) | the app you launch; menu-bar only (`LSUIElement`) | agent, over XPC (management calls) |
| `Unlatch --agent` | launchd agent registered with `SMAppService.agent`, started on demand through its MachService `<TeamID>.<prefix>.unlatch.engine`, kept alive | libunlatch engines (one per VM), `ssh` |
| `UnlatchFileProvider.appex` | started by fileproviderd, killed when idle (MQ-003, MQ-073) | agent, over XPC (IPC frames) |
| `unlatch-askpass` | spawned by `ssh` during an interactive connect | user |

The engine lives in the agent, never in the extension: the extension is started per request and
killed when idle, while the connection to the VM and its push stream must stay up. Every
extension method is one IPC request (the same postcard frames the Linux tests use over a unix
socket) carried in one XPC message, with the new content passed as a `FileHandle`. Both XPC ends
check each other's code signature (`setCodeSigningRequirement`: our team, allow-listed bundle
ids), so no other process can drive the VM through Unlatch's authenticated ssh session.

Swift never parses postcard. It works with `Codable` mirrors of the Rust protocol types
(`mac/UnlatchShared/Sources/UnlatchShared/IpcModels.swift`), and libunlatch converts between serde's
JSON and frames (`unlatch_ipc_encode_request_json` / `unlatch_ipc_decode_response_json`).
`cargo test -p unlatch-ffi` writes a JSON sample of every protocol variant to
`mac/UnlatchShared/Fixtures`, and the Swift tests decode, re-encode and re-frame every one, so the two
sides cannot drift apart unnoticed.

### Host-side File Provider calls (review §2(e)8)

* At agent start, whenever an engine becomes `Live`, and on every `ErrorResolved` event:
  `signalErrorResolved(.serverUnreachable)`, then `signalEnumerator(.workingSet)`. Only that call
  lifts fileproviderd's backoff after failed enumerations (MQ-005) and flushes writes queued while
  the VM was unreachable (MQ-037).
* `WorkingSetChanged` → `signalEnumerator(.workingSet)`; `Reimport` → `reimportItems(below:)`.
* The materialized set is sent to the engine as `MaterializedChanged { full: true }`, collected
  from `enumeratorForMaterializedItems()`: by the extension in `materializedItemsDidChange`, and
  by the agent at startup.
* Domains are added with `supportsSyncingTrash = false` (its default is YES, MQ-008). The trash
  enumerator answers `NSFeatureUnsupportedError`, never `.noSuchItem`, which would loop (MQ-009,
  MQ-010). A move to the trash is treated as a delete.
* The root item has `contentPolicy = .downloadLazilyAndEvictOnRemoteUpdate`; everything else
  inherits it. `item(for:)` never answers `.noSuchItem` unless the engine is live, because the
  system deletes the local item when it gets that answer (MQ-011).

## 2. Building

Requirements: macOS 14 or later with Xcode 16 or later (the app itself targets macOS 13+),
[rustup](https://rustup.rs), [XcodeGen](https://github.com/yonaskolb/XcodeGen)
(`brew install xcodegen`), and, to bundle `unlatchd`, `zig` and `cargo-zigbuild`
(`brew install zig && cargo install cargo-zigbuild`).

```sh
# 1. Rust: universal libunlatch.a -> mac/build/rust, static unlatchd -> mac/build/unlatchd
mac/scripts/build-rust.sh

# 2. Swift unit tests (protocol fixtures, item/error mapping, ssh-config parsing, ...)
swift test --package-path mac/UnlatchShared

# 3. Xcode project (generated; not checked in)
cd mac && xcodegen generate && open Unlatch.xcodeproj
#    or: xcodebuild -project mac/Unlatch.xcodeproj -scheme Unlatch -configuration Debug build
```

The Xcode build runs `build-rust.sh --xcode` itself (aggregate target `RustLib`), so after the
first setup, building in Xcode is enough. Without zig and cargo-zigbuild it only warns: the app
can then only use VMs that already have `unlatchd` installed.

`build-rust.sh` options: `--ffi-only`, `--unlatchd-only`, `--debug`, `--stage-bundle <app>`.
`UNLATCHD_PREBUILT_DIR=<dir>` uses existing `unlatchd-x86_64` / `unlatchd-aarch64` binaries (CI builds
them on Linux), and `UNLATCH_REQUIRE_UNLATCHD=1` makes a missing unlatchd an error.

`libunlatch.a` is always built with `cargo rustc -p unlatch-ffi --lib --crate-type staticlib`, not
`cargo build`: the crate is also an rlib, and when rustc emits both in one invocation the release
profile's thin LTO is not applied to the static library. The same invocation prints rustc's
`native-static-libs`, which becomes `mac/build/rust/native-libs.xcconfig` (the Swift targets'
link flags). Every symbol in `unlatch.h` is checked with `nm --no-llvm-bc` per architecture: Rust
objects embed LLVM bitcode that Xcode's older LLVM cannot parse, so a plain `nm` fails on them.

The `unlatch` CLI (for `unlatch probe` and `unlatch doctor`) is built separately:
`cargo build -p unlatch-cli --release`.

## 3. Signing: set your team first

The app group, the MachService, the extension's document group and every bundle id come from two
settings in `mac/Signing.xcconfig`:

| Setting | Placeholder | Meaning |
|---|---|---|
| `UNLATCH_TEAM_ID` | `XXXXXXXXXX` | your Apple Developer Team ID (Xcode → Settings → Accounts) |
| `UNLATCH_BUNDLE_PREFIX` | `dev.unlatch.example` | a reverse-DNS prefix you own. **Forks must change it**: two installs with the same bundle ids signed by different teams confuse pluginkit and fileproviderd |

Put yours in `mac/Signing.local.xcconfig` (git-ignored):

```
UNLATCH_TEAM_ID = ABCDE12345
UNLATCH_BUNDLE_PREFIX = com.example
```

or pass them to `xcodebuild` (`UNLATCH_TEAM_ID=… UNLATCH_BUNDLE_PREFIX=…`). Everything else is derived:

* app group `UNLATCH_APP_GROUP = <TeamID>.<prefix>.unlatch`, which is
  `$(TeamIdentifierPrefix)$(UNLATCH_BUNDLE_PREFIX).unlatch` when you sign with that team. It must be
  team-prefixed: a `group.` id needs a provisioning profile, and macOS 15 and later prompt for (and
  macOS 27 denies) access to another app's group container;
* bundle ids `<prefix>.unlatch`, `<prefix>.unlatch.fileprovider`, `<prefix>.unlatch.askpass`;
* MachService `<app group>.engine`. A sandboxed process may look up Mach services whose names
  start with one of its app groups, which is how the extension finds the agent;
* LaunchAgent `<prefix>.unlatch.agent.plist`, generated at build time by
  `mac/scripts/gen-launchagent.sh`;
* Info.plist keys `UnlatchAppGroup`, `UnlatchTeamID`, `UnlatchBundlePrefix`, which the Swift code reads
  at runtime (nothing is hard-coded).

Signing is manual and uses no provisioning profile. With a profile, Xcode would add
`com.apple.application-identifier` to the app's entitlements, and since the app binary is also
the launchd agent, AMFI would then refuse to launch the agent (MQ-066). Debug builds use your
"Apple Development" certificate and Release builds "Developer ID Application". Unlatch uses no
restricted entitlement, so no profile is needed. Never add `keychain-access-groups`: an ad-hoc
signature with it is killed at exec (MQ-065), and a Developer ID one needs a profile that names
the exact certificate (MQ-064).

### Why ad-hoc builds cannot run the Finder integration

"Sign to Run Locally" (ad-hoc) has no Team ID, so the team-prefixed app group cannot be valid and
the extension, the agent and the app no longer share a container, a MachService namespace or a
code-signing requirement. The typical symptom is `NSFileProviderErrorDomain -2001` ("The
application cannot be used right now") or an agent that never answers. The code-signing
requirement the agent enforces (`anchor apple generic and certificate leaf[subject.OU] = <team>`)
rejects ad-hoc peers by design. So there are three tiers:

1. **Release** (maintainer's Developer ID, notarized DMG): works for everyone.
2. **Build it yourself with a Team ID**: set `UNLATCH_TEAM_ID` and `UNLATCH_BUNDLE_PREFIX` and sign
   with your Apple Development or Developer ID certificate. Whether a free Personal Team
   is enough has not been verified yet.
3. **No signing at all**: CI builds this way (`CODE_SIGNING_ALLOWED=NO`) to check that everything
   compiles and that the entitlements agree. It cannot run the Finder integration. Without signing,
   use the CLI and the Linux FUSE frontend instead.

### What CI asserts

`.github/workflows/ci.yml` (macOS job) builds the unsigned app, then runs
`mac/scripts/check-bundle.py`, which checks that:

* the app/agent and appex entitlements name exactly the same app group, the appex is sandboxed,
  and the agent has no `com.apple.application-identifier`, `com.apple.developer.team-identifier`
  or `keychain-access-groups`;
* `NSExtensionFileProviderDocumentGroup` and `UnlatchAppGroup` equal that group; the pipeline
  depths are 16 (download) and 4 (upload); `NSLocalNetworkUsageDescription` is present;
* the LaunchAgent's `MachServices` is exactly `{<group>.engine}`, it has `KeepAlive` and `--agent`,
  and its `BundleProgram` exists;
* `unlatch-askpass` and both unlatchd binaries are in the bundle, with checksums that match.

Then it signs the bundle ad hoc with the expanded entitlements (`mac/scripts/adhoc-sign.sh`),
runs `codesign --verify --strict --deep`, and repeats the checks against the entitlements codesign
actually embedded. The script's own tests (`python3 mac/scripts/test_check_bundle.py`) run on
Linux. The build also fails if Xcode reports that a File Provider method "nearly matches" an
optional requirement: such a method compiles but the system never calls it.

Every macOS stage after the Rust build runs even when an earlier one failed, and the job uploads
`xcodebuild.log` (artifact `macos-build-logs`), so one run reports every Swift, Xcode and bundle
problem at once. macOS minutes are the scarce ones: to iterate on the app, run only the musl and
macOS jobs with `gh workflow run ci.yml --ref <branch> --raw-field only=macos` (`only=linux` runs
just the Linux jobs; different values do not cancel each other).

Tag builds (`.github/workflows/release.yml`) sign with Developer ID, notarize (`notarytool`),
staple, and assess with `spctl -a -vv`, for both the app and the DMG. The secrets and variables
it needs are listed at the top of that file. Pull requests need none of them.

## 4. Installing, and the quirks that break installs

* **Move Unlatch.app to /Applications and open it once from Finder.** LaunchServices does not
  register the extension of a quarantined bundle that nobody has launched (MQ-061). If the
  extension still does not show up: `xattr -dr com.apple.quarantine /Applications/Unlatch.app &&
  open -g /Applications/Unlatch.app`.
* **After upgrading from a DMG,** a stale LaunchServices record for the old copy on the mounted
  disk image can block the new extension from registering, and nothing is logged as an error
  (MQ-081). Look for extra records and remove them:
  ```sh
  LSREG=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
  $LSREG -dump | grep -B2 -A2 '<prefix>.unlatch$'
  $LSREG -u "/Volumes/Unlatch …/Unlatch.app"        # every path that is not /Applications/Unlatch.app
  $LSREG -f -R -trusted /Applications/Unlatch.app && open -g /Applications/Unlatch.app
  ```
* **Background agent approval.** On first launch the app registers its agent with
  `SMAppService`. If macOS asks for approval, the menu shows "Open Login Items Settings"; turn
  Unlatch on under System Settings → General → Login Items & Extensions. The menu re-reads the
  registration on every refresh (every 2 s), so the banner goes away once you approve.
* **Replaced bundle, agent dead.** `SMAppService.register()` does not repair a registration whose
  bundle was deleted and replaced (MQ-062), and `unregister()` returns before launchd drops the job,
  so registering again right away makes the agent die at launch with a code-signature violation
  (MQ-063). In both states `SMAppService` still reports the agent as enabled, so the menu goes by
  whether the agent answers: every call to it has a deadline (10 s for the 2 s status refresh),
  and after two unanswered refreshes in a row the menu shows "The background agent is not
  responding" with "Repair Background Agent". Repair unregisters the agent, waits 6 s, and
  registers it again (`--cli repair-agent` and `npx unlatch update` do the same). By hand:
  `launchctl print gui/$UID/<prefix>.unlatch.agent` shows the job state and last exit code.
* **Local Network permission.** For a VM on your LAN (RFC 1918, link-local, `.local`, UTM/Parallels
  networks), macOS asks once whether Unlatch may find devices on the local network (MQ-069). The
  agent opens a probe connection before running ssh so that the prompt names Unlatch; without
  permission, ssh fails with "No route to host".
* **First connect.** "Add VM" runs ssh interactively: host-key confirmations, key passphrases,
  passwords and 2FA prompts appear as Unlatch dialogs (`unlatch-askpass`). Background reconnects never
  prompt. If a reconnect needs you (changed host key, expired login, "visit this URL"), the menu
  shows the reason and a "Connect…" button, and Unlatch does not retry on its own.
* **Environment.** The agent is started by launchd and does not see your shell's `PATH` or
  `SSH_AUTH_SOCK`. It asks your login shell once per launch (`$SHELL -l -i -c 'env -0'`, 5 s
  timeout) and uses that `PATH`, so ProxyCommands that call Homebrew tools work. Your shell's
  `SSH_AUTH_SOCK` (1Password, Secretive, gpg-agent) is used only if you tick "Use the SSH agent
  from my login shell" for that VM. `launchctl setenv` does not reach the agent (MQ-067).

## 5. Checking it end to end: `unlatch probe`

`unlatch probe` runs on your Mac against a VM that is already added. It measures what a user sees:
how long a VM-side write takes to appear in `~/Library/CloudStorage/…`, the first listing of a
directory nobody has listed, cold and prefetched reads, rename and save round trips, a conflict,
and deletes. It writes the JSON scorecard described in the review (§2(f)8), including the
macOS version. The supported-OS list is the set of versions the probe has actually passed on.

```sh
cargo build -p unlatch-cli --release
target/release/unlatch probe \
  --local ~/Library/CloudStorage/Unlatch-<Name in Finder> \
  --ssh <same destination as in Unlatch> \
  --remote <the VM folder you added> \
  --json probe-$(sw_vers -productVersion).json
```

The mount directory is `<app name>-<display name>` with spaces removed (MQ-050).
Everything the probe creates goes into an `unlatch-probe-*` directory, which it removes at the end.
Do not run it on an idle headless Mac and read the latencies as real: fileproviderd throttles
background work there (MQ-071).

## 6. Troubleshooting

Logs: everything Unlatch logs uses the subsystem `unlatch` (categories `agent`, `extension`).

```sh
log stream --level debug --predicate 'subsystem == "unlatch"'
log stream --predicate 'process == "fileproviderd" OR subsystem == "com.apple.FileProvider"'
log show --last 1h --predicate 'subsystem == "unlatch" AND messageType >= error'
```

Is the extension registered, and which copy?

```sh
pluginkit -m -v -i <prefix>.unlatch.fileprovider      # empty output = not registered (see MQ-061/081)
codesign -d --entitlements - --xml /Applications/Unlatch.app | plutil -p -
codesign -d --entitlements - --xml /Applications/Unlatch.app/Contents/PlugIns/UnlatchFileProvider.appex | plutil -p -
```

Both entitlement dumps must list the same `<TeamID>.<prefix>.unlatch` group, and the app's must
not contain `com.apple.application-identifier`.

Is the agent running?

```sh
launchctl print gui/$UID/<prefix>.unlatch.agent        # state, last exit code, MachServices
```

What does fileproviderd think?

```sh
fileproviderctl dump                                  # domains, pending jobs, throttles, errors
fileproviderctl evaluate ~/Library/CloudStorage/Unlatch-<Name>/<file>   # per-item state and versions
fileproviderctl check                                 # consistency check of the local replica
```

Common symptoms:

| Symptom | Likely cause |
|---|---|
| "The application cannot be used right now" (-2001) | ad-hoc/unsigned build, app-group mismatch, or extension not registered (§3, §4) |
| VM changes stop appearing; menu is green | enumeration backoff (MQ-005); the agent should have signalled `ErrorResolved`, so check the `agent` log for `signalErrorResolved` |
| Saved file does not reach the VM after a reconnect | queued write waiting for `signalErrorResolved` (MQ-037); same check |
| `ls -la` of the mount hangs on `.Trash` | trash enumerator answered `noSuchItem` (MQ-009); must never happen, so file a bug |
| Menu shows "Paused: …" | mass-deletion guard: a remote change would delete many materialized items; choose "Keep My Files" or "Apply Deletions" |
| Agent restarts every 10 s after an upgrade | MQ-063; the menu says the agent is not responding (after about 20 s); use "Repair Background Agent" |
| Menu lists a VM with no host that says it has no saved VM settings | a Finder location with no record in `domains.json` (a damaged file, kept as `domains.json.unreadable-<date>` next to it, §7); Remove it, then add the VM again |

## 7. The C ABI (`crates/unlatch-ffi/include/unlatch.h`)

`libunlatch.a` (a universal static library, Rust crate `unlatch-ffi`) exposes:

* `unlatch_engine_start(config_json, event_cb, ctx, &err)` → handle; `unlatch_engine_stop`,
  `unlatch_engine_status_json`, `unlatch_engine_network_changed`, `unlatch_engine_connect_interactive`
  (blocking), `unlatch_engine_confirm_paused`, `unlatch_engine_call_json` (one in-process IPC call).
* The XPC bridge: `unlatch_dispatcher_new(engine, reply_cb, ctx)`, `unlatch_dispatcher_submit(d,
  frame, len, fd)` (fd ownership passes to libunlatch), `unlatch_dispatcher_free` (cancels the calls
  still in flight; no callback runs after it returns).
* JSON helpers: `unlatch_ipc_{encode,decode}_{request,response}_json`.
* Ownership: `unlatch_free_string`, `unlatch_free_bytes`. Errors come back as JSON
  `{"code","msg"}` through `char **out_error`. No panic crosses the boundary.

Engine config (JSON; unknown keys are rejected):

```json
{
  "name": "devbox-3fa2c1",
  "transport": {"ssh": {"destination": "devbox", "port": 22, "identity": "/Users/me/.ssh/id_ed25519", "extra_args": []}},
  "remote_root": "~/code",
  "state_dir": "/Users/me/Library/Group Containers/<group>/Library/Application Support/Unlatch/domains/<id>/state",
  "client_name": "Sam's MacBook",
  "cache_dir": "…", "temp_dir": "…",
  "unlatchd_upload": [{"arch": "x86_64", "path": "…/Resources/unlatchd/unlatchd-x86_64", "sha256_hex": "…"}],
  "ssh_env": {"PATH": "…"},
  "askpass": "/Applications/Unlatch.app/Contents/MacOS/unlatch-askpass",
  "expose_exec": false
}
```

Optional: `unlatchd_command`, `remote_install_dir`, `cache_budget`, `prefetch {max_file,
per_container, bytes_per_min, burst}`, `default_lazy_names`, `list_timeout_ms`,
`mass_delete_frac`, `mass_delete_abs`, `mass_delete_min`;
`transport` may also be `{"command": {"argv": [...], "env": {...}}}`. The same schema is in
`crates/unlatch-ffi/src/config.rs`, with samples in `mac/UnlatchShared/Fixtures/engine_config_*.json`.

Events (`event_cb`, JSON with `"type"` and `"domain"`): `WorkingSetChanged {anchor}`,
`ErrorResolved`, `Reimport {below}`, `NeedsUser {reason, url}`, `StatusChanged {status}`.

The agent keeps its state in the app-group container, under
`Library/Application Support/Unlatch/`: `domains.json` holds the configured VMs, and
`domains/<id>/{state,cache,tmp}` each engine's replica, content cache and staging area. The cache
is on the same volume as the extension's temporary directory, so a fetch is a `clonefile`, not a
copy. A record that cannot be read (an unknown schema, a hand edit) is skipped instead of
dropping every VM, missing flags default to off, and a damaged `domains.json` is first copied to
`domains.json.unreadable-<date>`. At start the agent compares its records with the Finder
domains the system has for Unlatch; a domain without a record is listed in the menu (and by
`--cli status`) so that it can be removed, never removed automatically.

Removing a VM (menu "Remove…", `--cli remove`, `npx unlatch remove`/`uninstall`) stops its
engine, then calls `NSFileProviderManager.remove(_:mode: .preserveDirtyUserData)`. Files on the
VM are not touched; downloaded copies and Mac-only metadata are discarded; files with edits that
had not reached the VM (saved while it was offline, or rejected with `cannotSynchronize`) are
not uploaded but moved by macOS to a folder whose path the menu shows (and reveals in Finder) and
the CLI prints (`"preserved"`). Only after the system has finished are the record and
`domains/<id>/` deleted. If the system call fails, the record stays, the engine restarts, and
the error is shown; a domain the system no longer has counts as removed.

## 8. Known gaps

* Nothing on the Mac side has run on a real Mac from this repository yet. CI compiles it, builds
  it and checks its bundle. The behaviour rules come from measured sshdrive quirks and from the
  review, and each still needs an `unlatch probe` pass on macOS 14, 15 and 26.
* Removing a VM does not upload pending edits or wait for them. It does not delete them either:
  they are moved aside (§7, "Removing a VM"), and copying them back into the VM's folder after
  you add it again uploads them.
* No Finder decorations, custom actions or thumbnails (v1.1).
* `.app`/package uploads are refused with `excludedFromSync` in v1 (review D8).
