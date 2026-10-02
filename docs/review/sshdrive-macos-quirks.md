> Historical record — the project was codenamed Hatch at the time.

# sshdrive macOS File Provider quirk catalog (MQ-001..MQ-081)

Source: <https://github.com/alecdwm/sshdrive/blob/2840985bb6871bc7aff4d7fa6cf59790235182fc/docs/quirks/macos.md>
(commit `2840985`, 2026-09-24). Catalog format/rules: <https://github.com/alecdwm/sshdrive/blob/2840985bb6871bc7aff4d7fa6cf59790235182fc/docs/quirks/README.md>.
Captured 2026-09-30 for Hatch design review.

**Licence note:** the sshdrive repo has **no LICENSE file**, so its text is all-rights-reserved by
default. This file therefore contains only our own one-line paraphrases of the findings, with the
quirk ids, as a reading guide; it does not reproduce the upstream catalog. Read the original at the
source link above. Anything Unlatch relies on is re-derived in our own tests (fpsim, docs/TESTING.md).

**Measurement context:** almost every row was measured on macOS 26.4.1 (25E253) on a headless arm64
build VM, a few on 26.6.2 / 27.0 on the author's Mac. **Nothing has been measured on macOS 14 or 15**
(the project's stated minimum). The catalog README says 80 entries; the file actually has 81 rows
(MQ-081 was added 2026-09-23). Ids are stable, not sequential.

## Quick index (one line each, paraphrased)

### Enumeration, working set, anchors, trash
- **MQ-001** A folder is enumerated once, ever; revisits/reopens/remote changes never re-call the container enumerator. All later changes must arrive via the working set (~20 s observed).
- **MQ-002** The working set is only a change stream; `enumerateItems` on it returns/ingests nothing.
- **MQ-003** A fresh extension instance is launched per working-set signal and per queued-write retry.
- **MQ-073** The system kills idle extension instances and their XPC connection invalidates; do not treat invalidation as "agent gone" (calling `disconnect` there bricks the domain, FP -1004).
- **MQ-004** An empty change set at the anchor the system already holds = "up to date"; the change is dropped until something else signals (deleted file stayed 10 min).
- **MQ-005** A change enumeration that keeps failing is throttled: 27 errors -> 47-min retry (7 errors -> 94 s); server changes stop reaching Finder while status looks green.
- **MQ-006** `.syncAnchorExpired` makes the system re-ask from a fresh anchor (working set only).
- **MQ-007** An `enumerateItems` held a full 60 s is not timed out.
- **MQ-008** `supportsSyncingTrash` defaults to YES.
- **MQ-075** The system creates the trash node itself at `add(domain)` and asks for its children even with `supportsSyncingTrash = false`.
- **MQ-009** Answering the trash enumerator `.noSuchItem` -> ~1 Hz delete/rematerialize loop forever; `ls -la` of the mount hangs.
- **MQ-010** Answering it `NSFeatureUnsupportedError` (3328) -> system gives up after 2 attempts and (usually) removes `.Trash`.

### Errors
- **MQ-011** `.noSuchItem` from `item(for:)` makes the system delete the item from disk.
- **MQ-012** From `fetchContents`, `.noSuchItem` and `.cannotSynchronize` both leave the item in place (reader sees ESTALE vs ETIMEDOUT); reversible.
- **MQ-013** The system believes whatever version a `modifyItem` reply carries; never re-fetches, no conflict flag.
- **MQ-014** `.filenameCollision` from `createItem` is retried forever, silently, on doubling backoff; caller told it succeeded.
- **MQ-080** A pending edit on an item reported deleted is re-offered as `createItem` -> collides forever (MQ-014); new identifier loses pins/tags.
- **MQ-015** Name collisions inside Finder never reach the provider (Finder renames "x copy").
- **MQ-016** A case-only collision from the server is silently renamed on the replica only (older item gets " 2").
- **MQ-052** `add(domain)` can report NSCocoaErrorDomain 4099 after the call actually landed.

### Eviction, content policy, pinning, atime
- **MQ-020** `evictItem` is recursive and works on `.rootContainer`.
- **MQ-017** `evictItem` straight after a `modifyItem` reply is refused -2008; first retry succeeds.
- **MQ-018** -2008 is returned for both pending uploads and kept items (indistinguishable); -2007 never seen.
- **MQ-019** Evicting the parent of a pending item fails opaquely (4101, contentVersionMismatch).
- **MQ-024** The effective (inherited) eager `contentPolicy` is what refuses eviction, not `allowsEvicting`.
- **MQ-025** The system ignores served `allowsEvicting` and reports it from `isDownloaded` (API deprecated since 13).
- **MQ-026** `contentPolicy = .inherited` is neutral; a new domain fetches nothing.
- **MQ-027** An explicit `.downloadLazily` child overrides an eager ancestor.
- **MQ-028** An eager policy downloads subfolders nothing has ever listed.
- **MQ-029** New ancestors reported via the working set are not ingested; a replica path lookup (`getUserVisibleURL` + `lstat`) is what triggers it.
- **MQ-030** Eager on the root downloads the whole location.
- **MQ-031** At most 6 concurrent `fetchContents` for an eager subtree.
- **MQ-032** Foreground opens bypass that ceiling (8 opens -> 8 simultaneous fetches).
- **MQ-033** `evictItem(root)` fails wholesale if anything under it is kept.
- **MQ-034** For 5-10 s after unpin (root: >60 s) eviction fails with an unexplained error.
- **MQ-021** Eviction moves atime (read atime before evicting).
- **MQ-022** Something (likely the domain indexer) advances a materialized file's atime minutes after fetch.
- **MQ-023** APFS atime follows relatime and is written deferred (~1 min).
- **MQ-045** Extended attributes survive eviction.

### Writes, saves, offline
- **MQ-035** Queued writes are re-offered forever on a doubling backoff with no visible ceiling.
- **MQ-036** A failed `fetchContents` is never re-issued by the system.
- **MQ-037** Only `signalErrorResolved(.serverUnreachable)` flushes queued writes (working-set signal / reconnect do not).
- **MQ-038** Requests keep reaching the extension while the domain is connected even when all fail fast.
- **MQ-039** `readdir`/`lstat` walks of the mount are served from the replica and never reach the extension.
- **MQ-040** `disconnect(reason:)` works from inside the extension (permanent, state 4).
- **MQ-041** Through a permanent disconnect the listing and queued writes survive; only fetches fail.
- **MQ-074** Relaunching the app (`open -g`) lifts a permanent disconnect; queued write then lands.
- **MQ-049** Atomic saves (TextEdit, tmp+mv) arrive as one `modifyItem` on the original identifier.
- **MQ-047** `chmod` arrives as `modifyItem` 0x100; only owner-execute is carried.
- **MQ-048** A Finder rename is one `modifyItem` with `.filename` (0x2).

### Attributes, tags, names, symlinks
- **MQ-042** Finder tags arrive only as `tagData` (NSKeyedArchiver blob), not as the xattr; nothing reaches the server unless you send it.
- **MQ-043** Tags are wiped on re-download unless the item returns `tagData`.
- **MQ-079** The system asks once per tag change, no retry loop even with an unchanged version.
- **MQ-044** The system decides which xattrs the extension sees (`XATTR_FLAG_SYNCABLE`; widen via Info.plist key).
- **MQ-046** `.DS_Store` never reaches the extension; kept locally, never uploaded.
- **MQ-076** Items served as symlinks become real symlinks in the mount; Finder shows Kind "Alias".
- **MQ-077** A dangling symlink looks identical to a live one.
- **MQ-078** A refused create is invisible (`ln -s` exits 0); only the item's `uploadingError` carries it.
- **MQ-050** `displayName` must be the bare nickname; mount dir/label are derived from app name + it.
- **MQ-051** Re-`add(domain)` with a new `displayName` renames in place, losing nothing.

### Menus, decorations, drawing, timing
- **MQ-053** Finder's own FP menu entries are only Download Now / Remove Download, chosen by `isDownloaded`.
- **MQ-054** Custom actions appear at the bottom of the context menu, never on the sidebar row.
- **MQ-055** Action activation rules must bind `fileproviderItems` (lower-case p); mistakes fail silently.
- **MQ-056** Decoration Info.plist keys are bare (`Identifier`, `BadgeImageType`, ...); `BadgeImageType` is a UTI; mistakes silent.
- **MQ-057** Badge decorations are drawn at the trailing edge of the Name column, not on the icon.
- **MQ-058** "Move to Bin" is offered without `allowsTrashing`; delete-dialog wording fixed; AppleScript delete shows no dialog.
- **MQ-059** Finder gives third-party downloads no cancel control.
- **MQ-071** On an idle headless Mac eager fetch starts after 8-90 s and working-set fetch is throttled; no latency claims from such a machine.
- **MQ-072** `waitForStabilization` is not a download barrier.

### launchd, LaunchServices, signing, install, TCC
- **MQ-061** LaunchServices registers no plugin from a quarantined, never-launched bundle (26.6; passed on 26.4).
- **MQ-081** A stale LaunchServices record (old DMG mount) blocks appex registration after upgrade (27.0).
- **MQ-062** `SMAppService.register()` does not repair a replaced bundle; needs unregister + launch.
- **MQ-063** `SMAppService.unregister()` returns before launchd drops the job; re-registering too soon -> launch-constraint SIGKILL loop (wait ~5 s).
- **MQ-064** A restricted entitlement needs a provisioning profile naming the exact signing certificate.
- **MQ-065** Ad-hoc signature + `keychain-access-groups` -> killed at exec.
- **MQ-066** `com.apple.application-identifier` in the launchd agent's entitlements -> AMFI refuses launch.
- **MQ-067** `launchctl setenv` does not reach a launchd agent on macOS 26.
- **MQ-068** Fresh user: one `open -g` registers the login item already enabled; domain not user-disabled.
- **MQ-069** Local Network privacy prompt on first LAN connect, in the app's name; unavoidable.
- **MQ-070** IOKit sleep/wake constants and `kCGAnyInputEventType` do not import into Swift.
- **MQ-060** A launchd agent's `stat`/`open` inside its own domain's mount draws no TCC prompt.
