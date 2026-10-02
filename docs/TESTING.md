# Testing Unlatch without a Mac

Unlatch's hardest behaviour lives on the other side of a macOS daemon we cannot run in CI:
`fileproviderd`. This document explains how the Linux verification loop stands in for it, what that
stand-in models, how to run it, and — just as important — what it does **not** tell you.

Authoritative design: `docs/DESIGN.md` as amended by `docs/review/2026-09-30-design-review.md`.
Measured macOS behaviour: `docs/review/sshdrive-macos-quirks.md` (MQ-xxx ids; sshdrive has no
licence, so we cite its measurements and never copy its code).

## 1. Layers

| Layer | Where | Talks to | Needs |
|---|---|---|---|
| Unit tests | every crate | in-process | nothing |
| **fpsim scenarios** | `crates/unlatch-bench/src/fpsim` | scripted engine, or real engine + `unlatchd` | nothing / `unlatchd` |
| **correctness fuzz** | `crates/unlatch-bench/src/fuzz` | scripted engine, or real engine + `unlatchd` + a real directory | nothing / `unlatchd` |
| FUSE fuzz, bench | `unlatch-cli`, `unlatch-bench` | Linux frontend, netns + netem | FUSE |
| `unlatch-probe` | the user's Mac | real Finder + fileproviderd | a signed build |

fpsim and the fuzzer are the only layers that exercise the File Provider semantics; the probe is
the only one that checks them against real macOS.

## 2. fpsim: a model of fileproviderd

fpsim (`FpSim`) plays fileproviderd for one domain. It reaches the engine **only** through the IPC
the Swift shim forwards — `unlatch_core::ipc::IpcClient` over the socket from `Engine::serve_ipc`,
with content passed as file descriptors — and receives the host-app signals through the engine's
`EventHandler` (`WorkingSetChanged` → `signalEnumerator`, `ErrorResolved` →
`signalErrorResolved`, `Reimport` → `reimportItems`). Time is virtual: a 47-minute throttle
costs nothing.

### What it models

**The local side.** A case- and normalization-insensitive namespace (APFS: `casefold(NFD)`),
materialized vs dataless files, and an item database keyed by provider identifier holding the
version the system believes each item has.

**Measured behaviours** (each has a failing-first scenario, §4):

| Rule | fpsim does |
|---|---|
| MQ-001 | enumerates a container once, ever; later changes arrive only through the working set |
| MQ-002/003 | working set is a change stream; a transport failure means a fresh extension connection |
| MQ-004 | an empty change set at the held anchor = up to date; nothing more until the next signal |
| MQ-005 | failing change enumerations back off 30 s × 1.18ⁿ (ceiling 2820 s); signals do not bypass it; `ErrorResolved` lifts it |
| MQ-006 | `AnchorExpired` → continue from `CurrentAnchor`, **no** re-enumeration |
| MQ-009/010/075 | the trash node is asked twice at `add(domain)`; `NSFeatureUnsupported` stops it, `noSuchItem` loops at 1 Hz (answered by the shim model, the IPC has no trash id) |
| MQ-011 | `noSuchItem` from `item(for:)` deletes the local file |
| MQ-012/036 | a failed fetch leaves the item and is never retried |
| MQ-013 | the version in a create/modify reply is believed with the local bytes; only `should_fetch_content` makes it re-download |
| MQ-014 | a create answered `filenameCollision` is retried for ever (0.04 s, 5 s, 15 s, doubling) |
| MQ-015 | collisions inside Finder never reach the provider (`x copy.ext`) |
| MQ-016 | a server-side case/normalization collision renames the **older** item locally, with no call |
| MQ-029 | items whose parent the system does not know are not ingested |
| MQ-035 | queued writes are re-offered for ever (5.5 s × 1.93ⁿ), same base, same bytes, same template id |
| MQ-037 | only `signalErrorResolved` flushes queued writes; a working-set signal does not |
| MQ-042/046/047/048/049 | tags = one `tagData` modify; `.DS_Store` & co never reach the provider; chmod carries owner-exec only (no-op → no call); a rename is one `.filename` modify; an atomic save is one `modifyItem` on the original id after an mtime-only modify of the parent |
| MQ-058 | "Move to Trash" is a delete (trash sync off) |
| MQ-080 | a pending edit on an item reported deleted comes back as `createItem` (`deletion_conflicted`) |
| (a)5 | a directory stays until each child is reported deleted |
| D9 | root policy `downloadLazilyAndEvictOnRemoteUpdate`: a materialized file whose content version changes goes dataless |

Plus: `materializedItemsDidChange` → `MaterializedChanged`; user actions browse, open, edit,
atomic save, create, drag in, mkdir, symlink, rename, move, delete, chmod, tag, evict; Finder
honours item capabilities; `DeletionRejected` restores the item (and re-lists a folder).

**Engine rules of review §2(c)** with a scenario of their own (failing-first against a scripted
flaw, and end to end against the real engine): (c)2 `rule2_create_of_identical_file_merges`,
(c)4 `rule4_concurrent_rename_answers_server_state`, (c)5 `rule5_tags_stay_on_the_mac`, (c)6
`rule6_delete_keeps_unseen_agent_file` and `rule6_retried_delete_keeps_unseen_agent_file`, (c)8 `rule8_mass_deletion_waits_for_the_user` (the
scripted engine models the guard; worlds confirm a pause by themselves unless
`World::set_auto_confirm(false)`), (c)10 `rule10_exec_bit_hidden_by_default`, (c)11
`rule11_moved_twin_keeps_real_name` plus the MQ-016 pair. (c)1, 3 and 9 are covered by the
replay, MQ-013/MQ-035 and symlink scenarios. (c)7 index change runs end to end only
(`rule7_index_change_reimports`, an `E2E_CHECKS` entry: `World::wipe_daemon_index` deletes
`unlatchd`'s `index.bin` and journal while the link is down, so the next daemon builds a new
index; the scripted world cannot, and skips it): pending edits, a pending create and a pending
delete made before the change must neither execute by an id of the old index (untouched files
and the deleted one keep their bytes; no item shows under an old id) nor be lost (the edits'
bytes are on the VM, in place or as conflict copies); `Reimport{below: ROOT}` is emitted and
the trees converge. How fileproviderd merges a pending edit on reimport is unmeasured; fpsim's
model is in §7. Not covered end to end: (c)12 writer lanes (bench T13), (c)13 LWW /
post-snapshot drop (engine unit tests, fuzz).

### Invariants (after quiesce)

`World::converged()` browses everything, lets fpsim run every due retry, then requires:

* the visible tree equals the VM tree — names (modulo the rule-11 `stem (Unlatch N).ext` mapping),
  kinds, sizes, and the bytes of every **materialized** file; VM symlinks appear as symlinks
  exactly when rule 9 says so (absolute in-root targets rewritten), else as read-only files whose
  content is the target text;
* no stuck pending operation, no duplicate identifier on the Mac, no contract violation noticed
  on the IPC (e.g. a fetch path outside `dest_dir`, a modify answered for another item);
* e2e/fuzz additionally: nothing outside the root changed (sentinel fingerprint), and every
  materialized realpath stays inside the domain (the Mac tree is materialized into a scratch
  directory with real symlinks, then canonicalized).

## 3. The correctness fuzzer

Seeded random interleavings (`ChaCha8`, reproducible) of:

* **agent ops on the VM root**: create/overwrite/append, atomic replace (temp + rename over),
  rename/rename-over/`mv` of directories, mkdir, rm, `rm -rf`, in-root and out-of-root symlinks,
  hardlinks, chmod, same-size churn, `git checkout`-like mass change, `mv a b; touch a`,
  `rm f; mkdir f`, an intermediate directory swapped for an out-of-root symlink;
* **Mac ops through fpsim** (browse, open, edit, save, create, drag in, mkdir, rename, move,
  delete, chmod, tag, evict); targets are picks into the *current* tree so every op stays
  meaningful when the shrinker removes others; names include case and NFC/NFD twins;
* **faults**: `Engine::drop_connection()`, SIGKILL of `unlatchd`, link cut/restore, engine
  restart, `die_before_ipc_reply:<kind>` (engine) — plus `die_after_commit:<op>` (daemon) in the
  crash/replay matrix.

At every `Quiesce` and at the end: the fpsim invariants, plus **"no VM-side write is ever
lost"**: every agent write has unique bytes; each must still exist somewhere on the VM (in place,
moved, or in a conflict copy) unless the agent itself replaced it or the Mac deliberately
destroyed it *after having been shown it* (it held exactly those bytes, received a newer version
of that item, or a quiesce passed). Conflict copies and `name N` duplicates are allowed only
where both sides touched that name in the same sync epoch — each copy's bytes carry the tag of
the write that made it, so the check is exact (a `name N` from a Mac rename onto `name` races on
`name`, whatever its bytes were first written as). "Touched" includes what the design counts as
a content change: `link(2)` touches its source (ctime), and a daemon crash touches every name the
agent touched within 1 s before it (the D3 racy window, see §5 #9).

A failure prints the seed, the problems, a ddmin-shrunk op list, and writes it as JSON for
`--replay` (which also dumps every provider call, the Mac tree and, for the scripted engine, its
mutation log).

**Crash/replay matrix** (review §2(f)3): {create, mkdir, modify, rename, delete} × {IPC hop
(`die_before_ipc_reply`), wire hop (`die_after_commit`)}; pass = the op happened exactly once, no
duplicate, no conflict copy, nothing stuck, trees converge.

**Targeted races** (§2(f)4): Mac write racing an agent write (agent bytes survive in place),
rename-over with an already-observed temp (destination id survives), `mv a b; touch a`,
`rm f; mkdir f`, a directory moved during a snapshot (fresh engine state), `rm -rf` in Finder
while the agent writes there, an intermediate directory swapped for an out-of-root symlink,
hardlinks, a Mac save after a daemon restart, an inotify queue overflow
(`UNLATCH_FAULT=overflow_after_events:2`: unlatchd's reader drops every event after the second and
reports `IN_Q_OVERFLOW`; lost creates, moves, deletes and rewrites must converge with ids intact).
Skipped with a reason: bind mounts (need CAP_SYS_ADMIN in the daemon's mount namespace).

## 4. Failing-first: the scripted engine

`fpsim::scripted` is a small in-memory engine + VM that implements the design's rules — and can
break any of them on purpose (`Flaws`, 28 switches). Every fpsim scenario runs twice: against the
correct scripted engine (must pass) and against one that breaks exactly the rule the scenario
defends (must be **caught**). If the second run passes, fpsim does not model the behaviour — a
naive engine would sail through — and the test fails. `tests/fpsim_scenarios.rs` holds one test
per scenario; `tests/fpsim_ipc.rs` runs every scenario again over a real unix socket (frame codec
+ `SCM_RIGHTS`) through a tiny scripted IPC server.

Random fuzzing also finds the flaws on its own. Seeds that fail, out of 40 (200 ops each):

| flaw | caught | flaw | caught |
|---|---|---|---|
| signal_before_commit | 40 | ws_only_materialized_ids | 40 |
| dir_modify_unsupported | 40 | no_display_mapping | 36 |
| dir_tombstone_only | 30 | expire_on_restart | 30 |
| conflict_without_fetch | 23 | no_display_rename_report | 23 |
| non_idempotent | 22 | stale_content_version | 17 |
| item_not_found_offline | 9 | stale_replay_reply | 5 |
| create_returns_exists | 2 | metadata_reply_hides_content_change | 2 |
| no_error_resolved | 1 | modify_missing_ok, no_self_fastforward, tombstone_filter_strict, symlink_depth_not_reevaluated | 0 (targeted scenarios only) |
| | | rename_conflict_errors, delete_ignores_seen_seq, display_name_leaks_to_vm, local_meta_not_merged, no_mass_delete_guard, exec_bit_exposed (added 2026-09-30), delete_retry_widens_seen (added 2026-10-01) | not measured by random fuzz (targeted scenarios only) |

(`changes_fail_offline` alone is not a defect — it is the MQ-005 set-up — and scores 0 by design.)

## 5. Findings: engine requirements the letter of the review leaves out

Found by the fuzzer (or, #10, by a review (c) scenario), turned into scenarios, implemented in
the scripted engine, and checked against the real engine (status as of 2026-09-30 after the
integration round, `unlatchd connect`):

| # | Requirement | Scenario | Real engine |
|---|---|---|---|
| 1 | A VM newcomer that sorts first takes the plain name from an existing case/NFD twin (rule 11): the engine must report the **displaced sibling** too, and before the newcomer, or fileproviderd bounces it locally (MQ-016) | `mq016_newcomer_displaces_existing_display_name` | passes (the engine journals displaced twins before the newcomer, and twins regaining a vacated name after it) |
| 2 | A metadata-only modify (rename, or a rule-4 "server state" reply) whose item has newer content than the request's base must set `should_fetch_content` (MQ-013) | `mq013_rename_reply_with_newer_content` | passes (any reply whose content version differs from the bytes the Mac holds sets `should_fetch_content`) |
| 3 | When a directory tombstone is reported, its descendants' tombstones must be reported too (the Mac can hold a child it never enumerated, moved in through the working set; it keeps a directory until each child is deleted) | `tombstones_follow_reported_dir` | passes (a tombstone is also reported when its parent's later tombstone from the same removal is) |
| 4 | Rule 9 is relative to the link's depth: moving an ancestor directory can turn `../../x` into an escape (or back). Symlinks below a moved directory must be re-evaluated and re-reported (**security**, D12) | `symlink_rule_follows_ancestor_moves` | passes (links whose rule-9 view changes are journalled before the move; a blocked link keeps `Kind::Symlink` + `symlink_blocked` on the wire, per the IPC fixture, so a flip is not a kind change) |
| 5 | A replayed op's stored reply must be refreshed (current version, `should_fetch_content` if content moved on); if the item was deleted since, its tombstone re-emitted | `replay_reply_reflects_later_edit`, `replay_of_since_deleted_item` | passes |
| 6 | A base mismatch caused by the client's own unacknowledged write (reply lost, user saved again) must fast-forward, not make a conflict copy of the user's own save | `second_save_after_lost_reply` | passes |
| 7 | A create replayed with **different bytes** (user re-saved the new file before the replay) must apply them; `op = H(domain, template_id)` alone answers from the ops table and drops them | `replayed_create_carries_newer_content` | passes |
| 8 | (daemon) a write through one hardlink must also update the sibling link's entry (`link(2)` sends `IN_ATTRIB` to the inode's own watches only, never to the parent dir) | race `hardlinks` | passes (unlatchd) |
| 9 | (daemon) after a daemon **crash**, the startup racy-timestamp rule (D3) bumped every file written within 20 ms of the last *commit* — however long the daemon kept watching after it — so the Mac's next save of the last file the agent wrote before any crash conflicted with itself (deterministic on the base build, fuzz seed 475 in 5 ops). The daemon now journals a quiet observation once nothing has happened for 25 ms after a commit (only when inotify covers everything: no polled directory, both event queues empty; not fsync'd — losing it is merely conservative), as a clean stop already did. A crash inside the window still bumps (D3, by design): the fuzzer's tracker treats names the agent touched within 1 s before a `KillUnlatchd` as touched in that epoch | `core::crash_bumps_only_files_changed_within_the_racy_window`; fuzz_e2e regression seed 475 | passes (fixed) |
| 10 | Rule 11 applies to pure moves too: the system sends no filename when a mapped twin (`readme (Unlatch 2).md`) is dragged to another folder, and the engine sent the generated display name to the VM | `rule11_moved_twin_keeps_real_name` | passes (fixed) |
| 11 | (daemon) a folder the Mac creates under a lazy name (`node_modules`) must be scanned and watched at once — fileproviderd never enumerates a folder it created — and published `lazy: false` from the start | fuzz seed 1; `ops::mkdir_of_a_lazy_name_is_watched` | passes (unlatchd) |
| 12 | (daemon) a content change held back by the hot-file throttle must survive a daemon crash for a client resuming past its seq | fuzz seeds 25/31; `core::throttled_change_survives_a_crash_for_resuming_clients` | passes (unlatchd) |
| 13 | (daemon) the startup verify walk must re-list and re-watch a directory it finds *moved* | fuzz seeds 58/59; `session::dir_moved_while_down_is_watched_after_restart` | passes (unlatchd) |
| 14 | (daemon) every descendant of a removed subtree needs a tombstone for Resume (an item moved into it while the client was away) | fuzz seed 72; `lifecycle::resume_removes_an_item_moved_into_a_removed_dir` | passes (unlatchd) |
| 15 | (daemon) a move into a folder that vanished on the VM is `Renamed { applied: false }`, never `NotFound` (which the engine reads as "the item is gone" and deletes it on the Mac) | fuzz seed 13; `ops::move_into_a_vanished_folder_reports_the_item_where_it_is` | passes (unlatchd) |
| 16 | (daemon) Ping must cover polled directories and a pending startup verify walk; links dirtied by names retried after a move, by a removed link, or by a write through a short-lived link must be re-stat'ed; names blocked behind nested moves retried until the moves settle | `tests/stress.rs` (unlatchd-level random churn with hard links and restarts), `core::*` | passes (unlatchd) |
| 17 | (daemon) a change held back by the hot-file throttle is published under a fresh seq (a client whose link was cut meanwhile resumes past its old one) | fuzz seeds 170/134/159; `core::throttled_change_published_offline_reaches_a_resuming_client` | passes (unlatchd) |
| 18 | (daemon) a request by id resolves against the live tree: Write flushes before resolving, and a stale path is re-listed and retried where the id is now — NotFound means "the item is gone" to the engine, which then re-creates the Mac's edit as `name 2` | fuzz seeds 152/111; `ops::write_right_after_its_dir_moved_resolves_live`, `ops::request_by_id_follows_an_unseen_move` | passes (unlatchd) |
| 19 | (daemon) an inode number reused by a new file in the same batch as the old file's deletion must move to the new node's identity (else its moves split and its hard links are never dirtied) | fuzz seed 157; `index::reused_inode_number_goes_to_the_new_node` | passes (unlatchd) |
| 20 | Rule 6 for a **retried** delete: the system retries a delete whose reply was lost after it has consumed newer anchors — for items under a folder it had already deleted locally, so never shown. The retry must keep the first attempt's `seen_seq` (persisted per item and call, released when the system enumerates the folder again); recomputing it gave the retry a new op id and the daemon deleted the agent file the first attempt kept. Likewise an unknown content base (`beforeFirstSyncComponent`) never falls back to the replica's version: only the consumed anchor counts | fuzz seed 186; `rule6_retried_delete_keeps_unseen_agent_file`; engine `retried_folder_delete_keeps_its_first_seen_seq`, `delete_with_unknown_content_base_keeps_unseen_agent_write` | passes (fixed) |
| 21 | (fpsim) a pending folder the system bounced locally (MQ-016) to the very name the create's reply then gave it (rule 2, `README 2`) is settled; fpsim kept the bounce and "undid" it once the agent's README went, showing README for the VM's `README 2`. The engine was right (its replica had `README 2`) | fuzz seed 13; model check `rule2_recreated_folder_takes_numbered_name` (`fpsim_model`, `fpsim_e2e`) | passes (fpsim fixed) |
| 22 | Rule (c)7: a rebuilt index must never hand out an id the lost one issued — fileproviderd reconciles a reimport and retries queued writes **by identifier**, so a reused id silently re-points the Mac's item (and its pending edit or delete) at another file. unlatchd restarted ids at 2 for every new index (seqs already continued above the persisted high-water mark); now ids do too. And a create that may already exist (reimport) whose bytes differ from the VM's file kept nothing of the Mac's bytes (`should_fetch_content` only): the Mac's pending edit was silently replaced; now they land as the file's conflict copy (rule 3) | `rule7_index_change_reimports` (e2e); `core::rebuilt_index_never_reuses_ids`; engine `create_never_surfaces_exists` | passes (fixed; a re-imaged VM that loses `alloc.bin` too still restarts ids — open) |
| 23 | (harness) an upload the VM refused for good (an edit of a hard link the agent had just made read-only: `Permission` → `.cannotSynchronize`, error badge) dropped out of the tree comparison entirely, so its VM file was reported "missing on the Mac"; an errored item now accounts for its name and only its content may differ. Also: after `Core::stop` the actor could still run one iteration and re-checkpoint | fuzz seed 267; `check::errored_items_account_for_their_name_only`; fuzz_e2e regression seed 267 | passes (fixed) |
| 24 | (daemon, **data loss**) a recursive delete judges entries the index does not hold (contents of an unexpanded lazy dir such as `node_modules`) by the wall time of the client's seen seq; the seq→time samples are checkpointed, not journaled, so after a daemon crash a client's seen seq can predate all of them — and an unknown time meant "not newer": the Mac's `rm -rf` of `src` deleted agent files written into `src/node_modules` after the Mac last synced. Unknown now means keep (the delete is partial → `DeletionRejected`, the folder comes back) | fuzz seed 406; `ops::unknown_seen_time_keeps_unindexed_entries` | passes (fixed) |
| 25 | (daemon) a directory that is not at its indexed path when it must be read (it moved, and the MOVED pair is only read by a *later* batch) must be re-listed and re-watched once it is reachable — never dropped as "gone". Three shapes: the startup verify walk listed the parent, then `mv a0 a02` ran before a walker opened `a0`, so this process never watched a0's subtree (persisted `scanned`, no watch, not polled: `dir a02/n3/n5 missing`); a new dir renamed between its stat and its first listing stayed unscanned (published `lazy`) and unwatched; a dirty name under a dir whose move came in the next batch was discarded. Fix: such dirs are recorded stale (with their blocked names and hints) and retried by every later batch and every flush; a batch re-lists a scanned dir it observes that this process does not watch; the verify walk lists non-lazy dirs persisted unscanned | CI `tests/stress.rs` seed 41 (ubuntu-24.04; locally ~50% with `UNLATCHD_DEBOUNCE_MS=0`); `core::dir_moved_during_the_verify_walk_is_watched`, `core::new_dir_renamed_before_its_first_listing_is_scanned`, `core::name_blocked_by_a_move_reported_in_a_later_batch_is_applied`, `reconcile::failed_listing_is_retried_by_a_later_batch`, `reconcile::verify_lists_a_non_lazy_dir_left_unscanned` | passes (unlatchd) |
| 26 | (daemon) journal replay after a crash must leave every node findable by inode: a txn is journalled upserts before removals, so replaying a rename-over (`mv b a`) gave `a` the inode while `b` still held it in the identity map, then `b`'s removal emptied the slot — a new hard link to `a` never dirtied it (`link n5 n1; write n1` left n5's old size). The identity maps are rebuilt once a replay ends | `tests/stress.rs` seed 81 with `UNLATCHD_DEBOUNCE_MS=0` (40/40); `persist::replayed_rename_over_keeps_the_destination_findable_by_inode` | passes (unlatchd) |
| 27 | (daemon) a write through a short-lived hard link whose name was *moved* away must still reach the other links when the landing name loses the inode in the same batch (`ln a/n2/n3 a/n4; echo >> a/n4; mv a/n4 a/n1; mv a/n1.tmp a/n1`): the inode is observed nowhere, so the indexed files are audited, as for a created-then-deleted name. MOVED_FROM/MOVED_TO cookies are followed through further renames, so a plain atomic save (`tmp → f`) still costs no audit. Two saves of one file in one batch (`git` writing index.lock twice) look exactly like this shape (one IN_CREATE either way, and the other link can be in any dir with its nlink back where it was), so the batch re-stats only the files of the dirs those names were in and *owes* one full audit: it runs when events go quiet (300 ms), at the next barrier (a Pong covers it), or 5 s after it fell due — once for any number of such batches (100k-entry tree, 10 s of such saves every 50 ms: 20.0M → 0.2M statx) | `tests/stress.rs` seed 930 (every run; the default run covers seeds 0–59 only); `core::link_written_then_moved_onto_a_name_renamed_over_reaches_the_other_link`, `core::lost_created_inode_follows_renames`, `core::renamed_over_created_inode_audits_its_dir_now_and_every_file_at_the_barrier`, `core::owed_audit_runs_when_events_go_quiet`; cost: `tests/measure.rs` `double_atomic_save_burst_100k` | passes (unlatchd) |
| 28 | (daemon) a name that appears by IN_CREATE (no MOVED_TO) holding a file inode still indexed at another name is a hard link, never a move: its stat can race an unlink and read nlink 1 (`ln d/f x; rm x`), and following the inode moved f's id to x, so `rm x` removed f from the Mac while it was on disk. The two single-link nodes of one inode share the identity map: when the holder goes, the other takes over | `tests/stress.rs` seed 104 with `UNLATCHD_DEBOUNCE_MS=0` (~1/300); `reconcile::created_name_of_an_indexed_inode_is_a_link_not_a_move`, `index::second_node_of_an_inode_takes_over_its_identity` | passes (unlatchd) |
| 29 | (daemon) unlatchd's own install and state dirs inside the served root — the **default** layout: the Mac's "Add VM" root is `~`, `npx unlatch share` serves the current directory (often `~`), and the bootstrap installs into `UNLATCH_HOME=~/.unlatch` with the state under `~/.unlatch/state/<hash>`. They were indexed and watched: every journal append was an event that was committed and journaled again (a loop that never went idle: ~2,200 seqs and ~230 KB of journal per 5 s, Events to the Mac every second), and Finder showed (and could edit or delete) index.bin, the journal and the binaries. They are now excluded by identity — `(dev, ino)` of the state dir, the install dir when it is unlatchd's own (row 43) and an `UNLATCHD_LOG` file, wherever they sit and whatever they are called: treated as absent by every listing and observation (never indexed, never watched; an index persisted with them drops them at the verify walk), and kept by a recursive delete of a folder containing them | `life::own_install_and_state_dirs_are_never_indexed`, `life::persisted_own_dir_is_removed_on_adoption`, `life::recursive_remove_keeps_the_install_dir` | passes (unlatchd) |
| 30 | (daemon) an id/seq block rollover while alloc.bin cannot be rewritten (disk full, state dir not writable) applied the batch to the index but published nothing — and later batches went out above it, so neither live clients nor a Resume ever got it (a fresh client did, with ids that were never durably reserved: a crash could hand them to other items). Now nothing carrying an id or seq above the durable reservation leaves unlatchd — Events, Welcome, snapshot chunks, listings, replies, read versions: while held, Ping fails (`NoSpace`/`Io`), and a client mutation reserves headroom *before* it touches the VM, so it fails cleanly and is retried. Once a reservation succeeds (any commit, a request, or the 1 s retry) everything since the hold is journaled (if an append failed meanwhile) and published, exactly what a Resume from there would send. A `ServerInfo` warning names the state dir | `life::unreservable_ids_are_never_handed_out_and_held_changes_publish_on_recovery`, `core::released_hold_journals_what_it_publishes` | passes (unlatchd) |
| 31 | (daemon) a failing checkpoint was retried on every periodic tick (~10/s), each attempt encoding the whole index under the core lock (~60% of a core on a 200k-entry index) and, on ENOSPC, re-filling a partial `index.tmp*` with the space the agent had just freed. Retries now back off exponentially (5 s doubling to 10 min), a failed atomic write removes its temp file, and a `ServerInfo` warning stands until a checkpoint succeeds | `core::failed_checkpoint_backs_off_and_warns` | passes (unlatchd) |
| 32 | (daemon) `unlatchd stop` signalled whatever pid serve.pid held whenever serve.lock was held — stale after an unclean exit, and a `stdio` session holds the lock without writing it: a reused pid (an editor, a build) was SIGTERMed and SIGKILLed. `stop`/`status` now take the pid of the flock's owner from /proc/locks and require it to be our process, an `unlatchd*` executable with argv[1] `serve`, holding serve.lock open; a stale serve.pid is removed, and "stopped" is printed only once it has | `life::stop_never_signals_a_process_that_is_not_the_serve`, `life::stop_stops_the_lock_holding_serve_despite_a_stale_pid_file` | passes (unlatchd) |
| 33 | (daemon) the abstract socket name was computable from public facts (uid, root, state path) and the abstract namespace has no permissions: another user could bind it first and the victim's serve exited on EADDRINUSE while every connect failed with PermissionDenied — a denial of service for as long as the name was held (live names are listed in /proc/net/unix, so even the idle exit was a window). The name now also hashes a random nonce in the 0700 state dir (`sock.nonce`, 0600); a serve whose name is taken re-rolls it, and a connector treats a foreign-uid listener as no server (SO_PEERCRED still rejects it both ways) and re-reads the name every round | `life::squatted_socket_name_neither_hijacks_nor_denies_service`, `life::predictable_socket_name_squat_is_harmless` (a real second uid via `unshare --map-auto`) | passes (unlatchd) |
| 34 | (daemon) tombstones had only the 30-day age bound: every descendant of a removed folder gets one, so a non-lazy output dir churned under the root grew memory, every checkpoint and the first Resume without limit. They are now capped by count too (`UNLATCHD_TOMB_MAX`, default 1M, like the engine's journal): the oldest go first and raise `gc_seq`, so a client resuming from before them gets a Snapshot; a Resume that would send more removals than the index has entries is a Snapshot as well | `life::tombstone_count_cap_falls_back_to_snapshot` | passes (unlatchd) |
| 35 | (daemon) a directory whose root-relative path reached PATH_MAX (4096 bytes) could not be opened (`openat2` takes one path): it and everything below were silently missing, unwatched, and ListDir said NotFound. Longer paths are now opened in pieces, each `openat2(RESOLVE_BENEATH\|NO_SYMLINKS)` beneath the directory the previous piece opened. (Depth beyond 4096 *components* is still not indexed: the index's cycle guard.) | `life::paths_beyond_path_max_work`, `sys::open_beneath_paths_beyond_path_max` | passes (unlatchd) |
| 36 | (daemon, **data loss**, review 2026-09-30) a recursive Remove of a folder that is itself a mount point (another filesystem, or a bind mount — of a directory on the *same* filesystem too, which has the same `st_dev`) walked the mounted tree: the walk took its device from the removed item, i.e. from the mounted filesystem's root, and deleted every file older than the seen point there or in the bind source outside the root. The walk now takes `st_dev` and the mount id (`STATX_MNT_ID`) from the removed item's **parent** and never enters another mount; a removed mount point (recursive or not) is kept and reported in `kept` | `tests/ops_safety.rs` `recursive_remove_of_a_mount_point_keeps_the_mounted_filesystem`, `recursive_remove_of_a_bind_mount_keeps_its_source` (under `unshare -rm`) | passes (unlatchd) |
| 37 | (daemon, **data loss**) a Write whose reply raced an agent write (between unlatchd publishing/exchanging the Mac's bytes and observing them) got a version that named the *agent's* bytes: the engine cached the Mac's bytes under the item's latest version (no fetch, MQ-013), and the Mac's next save passed the base check and overwrote the agent's write. The reply now carries a fresh seq `a` for the Mac's bytes and the item moves to `b > a` whenever the observation differs from our bytes; the engine's create, like its modify, fetches when the item's version is not the reply's. (The first fix compared against an fstat taken *after* the exchange/publish, which already held an agent write landing in between — see row 42.) | `ops::reply_version_never_names_an_agent_write_racing_the_publish` (create, exchange, in-place); engine `write_reply_older_than_the_item_asks_for_a_fetch` | passes (fixed) |
| 38 | (daemon, **data loss**) a recursive Remove kept a folder the agent moved into the removed folder after the seen point, but judged its files one by one — a move does not touch descendants' seqs — and deleted them. A folder whose meta seq is newer than `seen_seq` (moved, renamed or created after it; unindexed: by ctime) is kept whole | `tests/ops_safety.rs` `recursive_remove_keeps_a_folder_moved_in_after_the_seen_point` (indexed and lazy destination) | passes (unlatchd) |
| 39 | (daemon, **data loss**) base checks in **polled** directories (network/virtual-filesystem roots, dirs beyond the watch budget, `UNLATCHD_POLL=1`) compared against an index that lags the disk by up to a poll interval: a Mac save or delete based on the version it saw replaced/unlinked an agent's newer bytes with no conflict copy. Write and Remove now compare the live stat (ino, size, mtime, ctime, mode) with the index and observe the item first when they differ; the recursive walk treats an indexed file whose live stat differs as newer | `tests/ops_safety.rs` `polled_replace_keeps_an_unpolled_agent_edit`, `polled_remove_keeps_an_unpolled_agent_edit` | passes (unlatchd) |
| 40 | (daemon) `EXDEV` (and `ELOOP`) mapped to `NotFound`, which the engine reads as "the item is gone": a Finder move onto a mount point below the root deleted the item (or a folder subtree) from the Mac while it stayed on the VM. A move the kernel refuses across filesystems is now `Renamed { applied: false }` (the item where it is), a Write's `move_to` there saves in place; elsewhere EXDEV is `CannotSync` and ELOOP `Io` (a stale path's ELOOP is still retried as NotFound once by `resolve_dir`) | `tests/ops_safety.rs` `move_into_a_mount_point_reports_the_item_where_it_is` (under `unshare -rm`) | passes (unlatchd) |
| 41 | (daemon, **data loss**) a step of the VM's wall clock **backwards** (NTP after a resume, a leap second, `date -s`) left the seq→time samples later than files the agent wrote after it; a recursive Remove judged such files in an unexpanded lazy dir older than the seen point and deleted them (row 24 covered only an *unknown* seen time). The samples now follow the step (wall vs. monotonic clock: all move back by it), and samples later than now are dropped (→ an earlier sample or unknown, both keep) | `ops::clock_step_back_keeps_unseen_unindexed_entries`, `index::seq_time_samples_follow_a_wall_clock_step_back` | passes (unlatchd; a step that happened while unlatchd was down *and* that the clock has caught up with since is not measurable) |
| 42 | (daemon, **data loss**, re-review of row 37) the race fix described the Mac's bytes by an fstat taken *after* publishing them: an agent write between the RENAME_EXCHANGE (or the create's link, or the in-place write) and that fstat landed inside "ours", the observation matched it, and the reply's version named the agent's bytes again — for the exchange a regression against the parent. Pinned to one CPU the review's probe saw 8/10 runs with a mismatch and 2/10 losing the agent's write ("conflict_copy=None; agent bytes survive anywhere: false"). Our bytes are now described by facts fixed before the publish — the staged inode's stat (ino, size, mtime; ctime before) and the request's size and blake3 hash (in place: the target's identity, size, hash, requested mtime) — and after unlatchd observed its own change the reply names our bytes only if the observation, a post-publish re-hash of our inode (through a read-only handle opened while the staged file was still ours alone; for files over 64 MiB only when the stat cannot prove writes: our mtime older than the staged inode's ctime) and a fstat after it all agree; any difference is a race (fresh `a`, item to `b > a`). A write after these checks is an ordinary later change (its event, or the live-stat check before the next base compare, moves the item on). An in-place write also re-checks the target after the base check and keeps both versions if it changed. Known limit: a polled directory cannot see a same-size write within the same timestamp tick once it has observed the file (any polled file) | `ops::reply_version_never_names_an_agent_write_racing_the_publish` (create, exchange, in-place × hook at Published / BeforeObserve / BeforeFstat / AfterFstat × append / same-size-same-mtime write × inotify / polled: 45 cases), `ops::large_upload_race_is_seen_by_stat_alone`, `ops::in_place_write_after_the_base_check_keeps_both`, `ops::reply_version_is_current_without_a_racing_write`; probe `tests/race_probe.rs` (ignored; `taskset -c 0`) | passes (fixed) |
| 43 | (daemon) with `UNLATCH_HOME` unset the install dir falls back to the folder holding the binary, and row 29 excluded it whole: unlatchd run from `<root>/bin` (or `~/bin`, `~/.local/bin`, `~/.cargo/bin` under the root `~`) hid every other file of the user's there from Finder. The install dir is now excluded whole only when it is unlatchd's own — `$UNLATCH_HOME` (the bootstrap and `npx unlatch` always export it), or the binary's folder when it is one of the bootstrap's probed dirs (`$XDG_DATA_HOME/unlatch`, `~/.unlatch`, `/var/tmp/unlatch-$UID`, `/tmp/unlatch-$UID`); otherwise only the state dir (with the default `<dir>/state` container it sits in) and the running binary's own inode | `life::binary_in_a_user_folder_hides_only_itself` | passes (unlatchd) |
| 44 | (daemon, **data loss**, found by a race probe: 2 of 60 runs pinned to one CPU) a replace keeps the exchanged-out old inode as a conflict copy only if it changed by the time of one fstat right after the exchange; a process that had the file open before the exchange (the agent between its `open` and `write`, an editor, a log writer) could write through its handle after that fstat, onto an inode unlatchd then unlinked — the bytes were gone. The old inode is now unlinked at once only when an exclusive lease (`F_SETLEASE F_WRLCK`, granted only while no other open file description refers to it) proves nobody else can write to it; otherwise it is *parked* under its staging name (hidden) and swept by the actor every 50 ms: written since → kept as a conflict copy, closed by everyone → unlinked, no lease possible (not the owner, no lease support) → unlinked after 10 s unchanged; at most 256 parked (the oldest becomes a conflict copy), and a stopping daemon keeps what is still open elsewhere as conflict copies. Known limit: a crash leaves parked inodes under staging names, which the startup walk removes | `ops::agent_handle_on_the_replaced_inode_never_loses_its_writes` (written at Published / BeforeObserve / BeforeFstat / AfterFstat / after the reply / never), `ops::reply_version_is_current_without_a_racing_write` (nothing parked when nobody else holds the file); probe `tests/race_probe.rs` | passes (fixed) Follow-up: the stat is taken again while holding the lease and compared on size, mtime and ctime, in replace and in the parked sweep; 0 silent losses in 1,440 pinned probe rounds at load 290–540. Remaining limit: a same-size write that restores the mtime within one timestamp tick on a still-open old inode. |
| 45 | (engine, CI flake 2026-10-01) a create/modify reply that keeps the Mac's bytes as a conflict copy must have the working-set signal **delivered to the host before the reply returns**: the system believes the reply at once (MQ-013) and the copy, a second item, only arrives by enumerating the working set. The engine only *requested* the signal; the coalescing signal thread (≤ 5 ms gap) and the event thread could deliver it after the reply, so on a busy machine the Mac held `f.txt` at the VM's version but not `f (conflict from …).txt` until some later, unrelated signal (`mq013_returned_version_is_believed`: "on the VM, missing on the Mac", ~0.5% of runs). Now both conflict paths signal and flush (`Shared::signal_working_set_now`) before replying. `FPSIM_SLOW_HOST_MS=20` makes the e2e ordering certain (pre-fix 10/10 failures) | `mq013_returned_version_is_believed`; engine `conflict_copy_is_signalled_before_the_modify_reply`, `create_conflict_copy_is_signalled_before_the_reply` | passes (fixed) Note: the copy was never lost — the requested signal always arrived, a few ms after the reply; on a real Mac the host is asynchronous, so the fix makes fpsim's ordering deterministic and only narrows the window there. |
| 46 | (daemon, **denial of service**) `unlatchd connect` used a blocking connect(2) on the predictable abstract socket name: a foreign-uid squatter that binds it and lets its accept queue fill parks connect in `unix_wait_for_peer` forever. Now a non-blocking connect; a full queue is answered like a foreign owner ("not served by us") and `attach` moves on to a server it starts. | `squat_backlog::squatter_with_a_full_backlog_cannot_block_connect` (fails without the fix: "no Welcome: Timeout"; passes in ~1 s) | passes |
| 47 | (daemon) `unlatchd stop` could misread /proc/locks (a seq_file read in chunks can skip a line while other processes lock and unlock) and refuse with "not a server" (1 in 480 under 25× oversubscription). A miss is retried 5× over 80 ms. | life tests under load | passes |
| 48 | (daemon, **open, analysed — not reproduced**) D3 racy-timestamp rule: `commit()` stamps `observed_ns` at commit time, after the batch's stats. A rewrite in the same timestamp tick between the stat and a commit more than RACY_WINDOW (20 ms) later, followed by a crash before the queued event is read, would not be re-versioned on restart. Sound fixes: stamp the time taken before the batch's stats, or advance `observed_ns` only at quiet marks (queue empty) and stop. | — | open |
| 49 | (daemon, **interactive latency**, perf3 2026-10-01) row 42's post-publish re-hash read the whole upload (≤ 64 MiB) while the core lock was held, so every Stat / ListDir / Ping of every session waited behind each large Write: two sessions on one `unlatchd serve` on tmpfs, 64 MiB create + replace, the longest Stat during a Write was 30–63 ms (median over the writes 40–50 ms) against 1–16 ms before row 42, bisected to row 42's fix; T13 cannot see it (its upload never completes inside the sampling window). The staged O_TMPFILE now takes an exclusive write lease (`F_SETLEASE F_WRLCK`) right after it is created, while it is unnamed and ours alone, and keeps it — `tmp` stays open — until the reply is decided: any open of the inode by anyone waits for us, so no other write can land on our bytes and the re-hash is not needed. `F_GETLEASE` still `F_WRLCK` before the post-publish fstat proves no open was even attempted; a lease being broken (someone is waiting to open it) is reported as a race (fresh `a`, item to `b > a`) without reading anything. Closing the leased `tmp` after the publish raises its write-close event under the O_TMPFILE's own name (`#<ino>`), never under the published name. The re-hash stays as the fallback: no leases on the filesystem, a network filesystem (a lease holds back only this kernel's openers, not another machine's), a named staging file (no O_TMPFILE), a write in place (hard-linked target). After: 4–15 ms, as before row 42 | `ops::leased_write_rehashes_nothing_under_the_core_lock` (bytes re-hashed under the lock: 0 with the lease; the fallback still runs); `ops::reply_version_never_names_an_agent_write_racing_the_publish` and `ops::large_upload_race_is_seen_by_stat_alone` now run with and without the lease (agents on their own thread: under the lease their open waits); `tests/write_stall.rs` (two sessions, tmpfs; median over 6 Writes of the longest Stat < 25 ms; fails on the unfixed daemon with 34–63 ms) | passes (fixed) |
| 50 | (daemon, **spurious conflict copies**, verify of row 49, 2026-10-01) row 49's exclusive write lease breaks on *any* open of the inode, read-only included: an editor auto-reloading the file, an LSP or an indexer reading it the moment the Mac's save lands made the Write report a race (fresh `a`, item to `b > a`), so the Mac's next save became a conflict copy — and the reader waited for the reply. A read-only-agent probe: 13 of 360 rounds; `readonly_open_vs_reply` on the write-lease build, 6 runs × 120 rounds interleaved (load 23–60): 135 of 720 rounds (ext4 38 + 37, xfs 28 + 25, tmpfs 2 + 5), reader open+read up to 6.9 ms. Now a **read lease**: once the upload's bytes are written and synced, the staged O_TMPFILE's writable descriptor — its only one, the file is unnamed — is traded for a read-only reopen holding `F_SETLEASE F_RDLCK` (granted only while the inode is open for writing nowhere: EAGAIN with our own writer open), then the publish (`linkat`, or link + `RENAME_EXCHANGE`) and the reply as before; `F_GETLEASE` must still be `F_RDLCK` before the post-publish fstat, else race. Mode, owner, xattrs and mtime are set through the read-only descriptor. Kernel facts, Linux 6.8 on tmpfs, ext4 and xfs (`sys::read_lease_breaks_on_writers_only`): our own chmod/chown/xattr/utimens, `linkat`, `RENAME_EXCHANGE`, unlink of the old name, other processes' `O_RDONLY` opens + reads, `stat`, `chmod`, `rename` keep it; `O_WRONLY`, `O_RDWR`, `O_APPEND`, `O_WRONLY\|O_TRUNC` opens and `truncate(2)` break it (F_GETLEASE → `F_UNLCK`) and wait until we close. **One content change gets past a read lease**: `open(O_RDONLY\|O_TRUNC)` truncates to 0 bytes without a break — it always changes the size, so the existing stat checks (index vs ours, fstat vs index) report it. The re-hash fallback is unchanged (no leases, network fs, named staging, in place); a filesystem without leases now also closes the writer before the publish (no write-close event under the published name either way). After: 0 of 720 (same interleaved runs), reader ≤ 0.55 ms; `race_probe` pinned × 20 (tmpfs/ext4/xfs): 0 unreported mismatches, 0 silent losses in 2,400 rounds; write_stall median 10.0 ms (write lease 10.3, before row 49 39.7) | `ops::a_reader_racing_the_publish_is_no_race_and_never_waits` (a reader at every point, create/exchange/in-place, inotify/polled, lease/fallback, with and without keeping its handle across the next save: no race, no copy, never waits; fails on the write-lease build: "a reader waited for the Write"); `ops::reply_version_never_names_an_agent_write_racing_the_publish` adds the `O_RDONLY\|O_TRUNC` agent (45 writing + 24 truncating cases per lease mode); `sys::read_lease_breaks_on_writers_only`; `tests/race_probe.rs` `readonly_open_vs_reply` (ignored probe, asserts 0 conflicts / 0 reported / 0 next-save copies) | passes (fixed) |
| 51 | (daemon, **identity**, CI flake 2026-10-01: `identity::mv_a_b_then_touch_a`, b got ItemId(3) for a's ItemId(2)) rename(2) queues IN_MOVED_FROM and IN_MOVED_TO one after the other, not atomically; when the renaming thread is preempted between the two (a busy or single-CPU VM), a batch can be taken holding the IN_MOVED_FROM alone. Reconciled alone, the old name's node is missing with its inode found nowhere: if the new `a` is not there yet the node is removed (tombstone) and the next batch's MOVED_TO makes `b` a new item (the CI shape); if it is, rule 4 merges the old id into the new `a` and `b` is new — the old item silently re-pointed at another file. Every other cut of `mv a b; echo > a` (after MOVED_TO, CREATE, MODIFY) was already right. Fix: a batch never ends between the two halves of a rename. Not a pairing timeout: rename holds the inode lock of both parents until both events are queued, so for each IN_MOVED_FROM without its IN_MOVED_TO the batch takes the source dir's lock (one getdents64), then takes the queued events up to that MOVED_TO into the batch (rounds bounded at 8); still unmatched = moved out of the watched tree. Before: `mv_a_b_then_touch_a_on_one_cpu` (300 rounds, test and daemon on one CPU, no debounce) failed 10/10 runs on tmpfs and ext4 (3–5 % of rounds; 431/2000 rounds with `taskset -c 0`); the single-shot test 0/300 locally. After: 0/50 runs (15,000 rounds), 0/4,000 rounds on two CPUs. | `core::rename_cut_after_its_moved_from_keeps_the_id` (file and dir × new occupant before/after the cut batch; 4/4 fail without the fix), `core::unpaired_moves_names_moved_from_without_its_moved_to`, `identity::mv_a_b_then_touch_a_on_one_cpu` | passes (fixed) |
| 52 | (daemon, identity, **open, analysed — pre-existing**) `ln a h; mv a b; echo new > a` in one batch: reconcile rule 4 lets the new `a` reuse the old id although the old inode is still alive at `b` and `h` (multi-link), and `b` gets a new id. Deterministic (300/300 live). No data loss — contents and versions stay correct — but the Mac sees replace + new file instead of rename + new file. Fix: rule 4 must not reuse an id whose inode is found anywhere (including other links) in the batch or still indexed. | reproduced by an out-of-tree harness (one batch, link shape); no in-tree test yet | open |
| 53 | (daemon, identity, **open, analysed — pre-existing**) `mv a b; echo new > a; mv b b2` with `UNLATCHD_DEBOUNCE_MS=0`: the batch's stats run after the later `mv b b2`, so `b` is missing and rule 4 reuses the old id for the new `a`; the next batch's `MOVED_TO b2` creates a new item. ~4% of rounds at debounce 0, 0/300 at the default. No data loss. Fix: rule 4 should consult events still queued (`queued_names`) before reusing an id. | reproduced by an out-of-tree every-cut test (cuts 1–5 of 7); no in-tree test yet | open |

`tests/stress.rs` runs seeds 0–59 by default (`UNLATCHD_STRESS_SEED=<n>` one seed, `UNLATCHD_STRESS_SEEDS=<a>..<b>` a range: every seed runs, failures are listed at the end and, with `UNLATCHD_STRESS_FAIL_DIR`, written one file per seed). Its check is two-way: disk → replica (every file and dir, with sizes) and replica → disk (every entry reachable from the root exists with the same kind, no duplicate sibling names, no dir published lazy unless lazy by name; path lookups fail on duplicates). Rows 20–23 came from a campaign over seeds 1–3000, 1000 seeds under `taskset -c 0` and 500 with `UNLATCHD_DEBOUNCE_MS=0`; `.github/workflows/nightly-stress.yml` repeats such a campaign every night (6000 seeds in each of the three timing shapes, plus `unlatch-bench fuzz` over 300 seeds and its matrix), uploading the failing seeds' logs.

Also: fpsim sends `MaterializedChanged` synchronously after each enumeration; the real host
forwards `materializedItemsDidChange` asynchronously. Changes inside a container that land
between its `Enumerate` and the engine learning it is materialized are filtered out while the
anchor may advance past them — the engine should re-report a container's children changed since
its enumeration when it joins M. fpsim cannot show this window (see §7).

## 6. Running it

```sh
source ~/.cargo/env
# model + scripted engine (no unlatchd needed), ~20 s:
cargo test -p unlatch-bench --test fpsim_scenarios --test fpsim_model --test fpsim_ipc --test fuzz_scripted
cargo run -p unlatch-bench -- fpsim --scripted [--verbose]          # failing-first table
cargo run -p unlatch-bench -- fuzz --scripted --seeds 300 --ops 250  # random, shrinks failures
cargo run -p unlatch-bench -- fuzz --scripted-flaw no_display_mapping --seeds 20   # mutation run (expected to FAIL)

# end to end, by default: tests/fpsim_e2e.rs builds unlatchd from this checkout into the test's
# target dir (or uses $UNLATCHD_BIN; with neither it fails with instructions), and uses the
# unlatch-bench binary cargo builds for the test as the engine host — nothing skips silently
# (the one allowed skip, mq005, is listed with its reason in the test file):
cargo test -p unlatch-bench --test fpsim_e2e
# the CLI runner and the fuzzer:
cargo build -p unlatchd -p unlatch-bench
D=target/debug
$D/unlatch-bench fpsim --unlatchd $D/unlatchd --engine-host $D/unlatch-bench [--only NAME] [--verbose]
$D/unlatch-bench fuzz  --unlatchd $D/unlatchd --engine-host $D/unlatch-bench --seeds 5 --ops 150
$D/unlatch-bench fuzz  --unlatchd $D/unlatchd --replay /tmp/unlatch-fuzz-seed7-minimal.json
cargo test -p unlatch-bench --test fuzz_e2e     # runs by default; finds target/<profile>/unlatchd or $UNLATCHD_BIN
$D/unlatch-bench fuzz --unlatchd $D/unlatchd --only race/inotify    # just the matching matrix/race cells
$D/unlatch-bench fuzz --unlatchd $D/unlatchd --replay F.json --shrink-runs 200   # shrink a replay again
```

Knobs: `$UNLATCHD_BIN`, `$UNLATCH_E2E_NO_BUILD=1` (fpsim_e2e: do not run `cargo build -p unlatchd`;
use an existing `target/<profile>/unlatchd`, with a staleness warning), `$UNLATCH_ENGINE_HOST` (the `unlatch-bench` binary; `unlatch-bench fpsim
engine-host` runs one engine per process so `UNLATCH_FAULT=die_before_ipc_reply:<kind>` stays
scoped to it), `$UNLATCH_TMP` (world directories; keep it short — socket paths ≤ 108 bytes),
`$UNLATCH_E2E_DAEMON=stdio` (one in-process `unlatchd stdio` per connection instead of
`connect`/`serve`; every reconnect is then a daemon restart), `--timeout SECS` (per-seed
watchdog; a hang exits 3 with the seed).

How e2e faults are injected: the engine's `Transport::Command` is a small shell wrapper that
refuses to connect while `ctl/offline` exists (link cut), and exports the contents of
`ctl/wire-fault` as `UNLATCH_FAULT` for the **next** daemon only (then deletes it), so
`die_after_commit:<op>` is one-shot. Killing the daemon SIGKILLs this world's `unlatchd` processes
by state dir. An engine-side reply fault restarts the engine as a child process with
`UNLATCH_FAULT` set; the first dropped connection restarts it clean — the "extension/agent kill".

## 7. What this does NOT cover (versus a real Mac)

* **Anything fileproviderd does that is not in the table above**, and anything measured only on
  macOS 26.x (the whole catalog; 14/15 are unmeasured). fpsim is written from the same
  assumptions as the design; the MQ measurements are what keeps it honest, and `unlatch-probe` is
  the only check against the real thing.
* **Modelling assumptions without a measurement**, chosen conservatively and listed so they can
  be probed: removals in a change set are applied before updates; a local bounce is undone once
  its collision is gone — unless the provider has since named the item exactly as the bounce
  did (then the two agree and nothing is undone); on `reimportItems(below:)` the system first
  lists every container it had enumerated, re-offers the items whose identifiers the provider
  no longer has *and that carry local work* (pending edits, pending creates below them) as
  creates that may already exist — parents first, before it ingests the fresh listings — and
  drops the clean ones (the listings bring them back); a replayed create whose item already arrived through the working set is
  merged by identifier (the side with newer local edits wins); a modify answered `NotFound`
  re-offers a content edit as a create (MQ-080 path); `item(for:)` is called before
  materializing a dataless file; a folder restored after `DeletionRejected` is re-enumerated; a
  file dragged onto a never-opened folder creates there without enumerating it; the
  materialized set is reported synchronously (the real host is asynchronous, §5).
* **XPC, extension launch/kill timing, code signing, TCC, LaunchServices** (MQ-003's fresh
  instances are modelled only as reconnects; MQ-060..070 not at all).
* **Latency as the user perceives it**: fileproviderd's own scheduling (MQ-071: 8–90 s on an
  idle Mac), FSEvents delivery, Finder redraw. Virtual time says nothing about wall-clock.
* **Eviction, pinning and content policies** beyond evict-on-update (MQ-017..034), atime,
  xattr syncing (MQ-044), decorations and menus (MQ-053..059), `clonefile` on APFS.
* **The scripted VM** models hardlinks as copies and has no inodes; real-filesystem ops
  (hardlinks, out-of-root symlinks, directory swaps) are exercised only in e2e runs.
* Bind mounts inside the root are not exercised (see §3); queue overflow only as injected by
  `overflow_after_events` (the kernel's `max_queued_events` sysctl needs root).
