# Unlatch bench scorecard

Host `bench-vm` (kernel 6.8.0-139-generic, 124 CPUs, shared), quick mode. Shaping: unprivileged netns + `tc` on `lo` (MTU 1500, offloads off), one bottleneck per direction (`netem` drop-tail, or `netem` delay → `tbf` → `fq_codel` for `-fqcodel`); sshfs and Unlatch cross the same shaped TCP path, through the raw bridge or, for `-ssh`, real `ssh` ⇄ `sshd`. Values are p50 unless the metric says otherwise. `n/a` = system not available yet; `~` = extrapolated. Speedup = sshfs ÷ unlatch for latencies, unlatch ÷ sshfs for throughput. The `raw` column is `cat` of the T8 file through the same client path (`ssh bench cat` for `-ssh`), and for T13 the link's own floor (plain-TCP bulk down+up, probe on its own connection).

**Unlatch targets:** 161 judged, 22 failed, 0 unlatch rows not available.

## rtt0-bw200

Host load average (1/5/15 min): 38.0/41.5/42.1 at start, 52.2/44.4/43.1 at end.

Link: RTT p50 0.11 ms (nominal 0), down 190.8 / up 191.2 Mbit/s (nominal 200), 256 KiB fetch 11.4 ms warm vs 11.7 ms after 2 s idle.

| # | metric | unit | unlatch | sshfs | local | raw | target | pass | × vs sshfs |
|---|---|---|---:|---:|---:|---:|---|:-:|---:|
| T1 | list_1000_p50_ms | ms | 0.15 |  | 2.17 |  | ≤ 0.5 ms (engine API) | ✅ |  |
| T2 | stat_p50_us | us | 0.13 |  | 3.35 |  | ≤ 20 µs (engine API) | ✅ |  |
| T3 | ls_la_cold_ms | ms | 22.2 | 46.9 |  |  |  |  | 2.1× |
| T3 | ls_la_ms | ms | 6.15 | 6.69 | 10.1 |  | ≤ 2× local | ✅ | 1.1× |
| T4 | visible_event_p50_ms | ms | 2.54 |  |  |  | ≤ RTT/2 + 15 ms = 15 ms | ✅ |  |
| T4 | visible_p50_ms | ms | 2.63 | 0.28 |  |  | ≤ RTT/2 + 15 ms = 15 ms | ✅ | 0.1× |
| T4 | visible_readdir_p50_ms | ms | 1.56 | skip |  |  | ≤ RTT/2 + 15 ms = 15 ms | ✅ |  |
| T5 | burst_all_visible_ms | ms | 13.4 | skip |  |  | ≤ 300 ms + RTT = 300 ms | ✅ |  |
| T6 | open_small_warm_p50_ms | ms | 0.30 | 0.80 | 0.0163 |  | ≤ 1 ms (prefetched, engine API) | ✅ | 2.7× |
| T7 | open_256k_cold_idle_ms | ms | 12.6 | 16.4 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 15 ms | ✅ | 1.3× |
| T7 | open_256k_cold_ms | ms | 13.7 | 12.5 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 15 ms | ✅ | 0.9× |
| T7 | open_4k_cold_idle_ms | ms | 1.49 | 1.63 |  |  | ≤ 1 RTT + 5 ms = 5 ms | ✅ | 1.1× |
| T7 | open_4k_cold_ms | ms | 0.95 | 1.44 |  |  | ≤ 1 RTT + 5 ms = 5 ms | ✅ | 1.5× |
| T8 | throughput_mbit | Mbit/s | 187 | 190 | 8429 | 191 | ≥ 80% of raw `cat` over the same link | ✅ | 1.0× |
| T9 | upload_4k_p50_ms | ms | 4.68 | 2.88 | 1.09 |  | ≤ 1 RTT + 5 ms = 5 ms | ✅ | 0.6× |
| T10 | reconnect_catchup_ms | ms | 291 |  |  |  | ≤ 1.5 s (RTT ≤ 40 ms) | ✅ |  |
| T11 | full_tree_known_s | s | 0.72 | ~20.9 | 0.55 |  | ≤ 3 s at RTT 40 ms (100k + 30k lazy) | ✅ | 29.0× |
| T11 | initial_sync_bytes | bytes | 2777034 |  |  |  |  |  |  |
| T11 | initial_sync_cold_daemon_s | s | 0.76 |  |  |  |  |  |  |
| T12 | daemon_rss_bytes_per_entry | B/entry | 183 |  |  |  | ≤ 250 B/entry | ✅ |  |
| T13 | p99_interactive_under_load_ms | ms | skip | skip |  | skip | ≤ RTT + 30 ms = 30 ms |  |  |
| T14 | bytes_moved_ratio | ratio | 0.0009 |  |  |  | ≤ 2× appended bytes | ✅ |  |

<details><summary>not measured (5)</summary>

- T13 — Skipped unlatch: T13 runs at 20 and 50 Mbit/s only
- T13 — Skipped raw: T13 runs at 20 and 50 Mbit/s only
- T13 — Skipped sshfs: T13 runs at 20 and 50 Mbit/s only
- T5 — Skipped sshfs: quick mode (bounded by sshfs's 20 s dir cache; run --full)
- T4 — Skipped sshfs: quick mode (sshfs listing refresh is bounded by its 20 s dir cache; run --full)

</details>

## rtt40-bw50

Host load average (1/5/15 min): 47.5/43.7/42.8 at start, 39.1/42.6/42.6 at end.

Link: RTT p50 40.38 ms (nominal 40), down 46.9 / up 46.9 Mbit/s (nominal 50), 256 KiB fetch 84.2 ms warm vs 211.0 ms after 2 s idle.

| # | metric | unit | unlatch | sshfs | local | raw | target | pass | × vs sshfs |
|---|---|---|---:|---:|---:|---:|---|:-:|---:|
| T1 | list_1000_p50_ms | ms | 0.13 |  | 2.26 |  | ≤ 0.5 ms (engine API) | ✅ |  |
| T2 | stat_p50_us | us | 0.14 |  | 1.89 |  | ≤ 20 µs (engine API) | ✅ |  |
| T3 | ls_la_cold_ms | ms | 23.2 | 329 |  |  |  |  | 14.2× |
| T3 | ls_la_ms | ms | 10.4 | 6.93 | 10.6 |  | ≤ 2× local; ≥ 10× faster than sshfs (cold) at RTT 40 | ✅ | 0.7× |
| T4 | visible_event_p50_ms | ms | 22.8 |  |  |  | ≤ RTT/2 + 15 ms = 35 ms | ✅ |  |
| T4 | visible_p50_ms | ms | 22.8 | 41.0 |  |  | ≤ RTT/2 + 15 ms = 35 ms | ✅ | 1.8× |
| T4 | visible_readdir_p50_ms | ms | 23.2 | skip |  |  | ≤ RTT/2 + 15 ms = 35 ms | ✅ |  |
| T5 | burst_all_visible_ms | ms | 36.2 | skip |  |  | ≤ 300 ms + RTT = 340 ms | ✅ |  |
| T6 | open_small_warm_p50_ms | ms | 0.31 | 82.5 | 0.0166 |  | ≤ 1 ms (prefetched, engine API) | ✅ | 269.3× |
| T7 | open_256k_cold_idle_ms | ms | 220 | 313 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 287 ms | ✅ | 1.4× |
| T7 | open_256k_cold_ms | ms | 86.1 | 213 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 287 ms | ✅ | 2.5× |
| T7 | open_4k_cold_idle_ms | ms | 43.0 | 124 |  |  | ≤ 1 RTT + 5 ms = 45 ms | ✅ | 2.9× |
| T7 | open_4k_cold_ms | ms | 42.3 | 124 |  |  | ≤ 1 RTT + 5 ms = 45 ms | ✅ | 2.9× |
| T8 | throughput_mbit | Mbit/s | 44.4 | 30.9 | 10630 | 44.8 | ≥ 80% of raw `cat` over the same link | ✅ | 1.4× |
| T9 | upload_4k_p50_ms | ms | 46.3 | 213 | 1.27 |  | ≤ 1 RTT + 5 ms = 45 ms | ❌ | 4.6× |
| T10 | reconnect_catchup_ms | ms | 396 |  |  |  | ≤ 1.5 s (RTT ≤ 40 ms) | ✅ |  |
| T11 | full_tree_known_s | s | 1.18 | ~338 | 0.51 |  | ≤ 3 s at RTT 40 ms (100k + 30k lazy) | ✅ | 287.4× |
| T11 | initial_sync_bytes | bytes | 2773076 |  |  |  |  |  |  |
| T11 | initial_sync_cold_daemon_s | s | 1.15 |  |  |  |  |  |  |
| T12 | daemon_rss_bytes_per_entry | B/entry | 185 |  |  |  | ≤ 250 B/entry | ✅ |  |
| T13 | p99_interactive_under_load_ms | ms | 89.1 | 1597 |  | 272 | ≤ RTT + 30 ms = 70 ms | ❌ | 17.9× |
| T13 | p99_listdir_under_load_ms | ms | 89.1 |  |  |  | ≤ RTT + 30 ms = 70 ms | ❌ |  |
| T13 | p99_pong_under_load_ms | ms | 81.0 |  |  |  | ≤ RTT + 30 ms = 70 ms | ❌ |  |
| T14 | bytes_moved_ratio | ratio | 0.0009 |  |  |  | ≤ 2× appended bytes | ✅ |  |

<details><summary>not measured (2)</summary>

- T5 — Skipped sshfs: quick mode (bounded by sshfs's 20 s dir cache; run --full)
- T4 — Skipped sshfs: quick mode (sshfs listing refresh is bounded by its 20 s dir cache; run --full)

</details>

## rtt100-bw20

Host load average (1/5/15 min): 37.1/42.0/42.4 at start, 25.4/37.3/40.7 at end.

Link: RTT p50 100.42 ms (nominal 100), down 18.3 / up 18.3 Mbit/s (nominal 20), 256 KiB fetch 210.1 ms warm vs 524.5 ms after 2 s idle.

| # | metric | unit | unlatch | sshfs | local | raw | target | pass | × vs sshfs |
|---|---|---|---:|---:|---:|---:|---|:-:|---:|
| T1 | list_1000_p50_ms | ms | 0.13 |  | 2.35 |  | ≤ 0.5 ms (engine API) | ✅ |  |
| T2 | stat_p50_us | us | 0.13 |  | 1.89 |  | ≤ 20 µs (engine API) | ✅ |  |
| T3 | ls_la_cold_ms | ms | 27.8 | 761 |  |  |  |  | 27.4× |
| T3 | ls_la_ms | ms | 5.72 | 7.48 | 10.2 |  | ≤ 2× local | ✅ | 1.3× |
| T4 | visible_event_p50_ms | ms | 52.7 |  |  |  | ≤ RTT/2 + 15 ms = 65 ms | ✅ |  |
| T4 | visible_p50_ms | ms | 52.7 | 101 |  |  | ≤ RTT/2 + 15 ms = 65 ms | ✅ | 1.9× |
| T4 | visible_readdir_p50_ms | ms | 54.8 | skip |  |  | ≤ RTT/2 + 15 ms = 65 ms | ✅ |  |
| T5 | burst_all_visible_ms | ms | 64.3 | skip |  |  | ≤ 300 ms + RTT = 400 ms | ✅ |  |
| T6 | open_small_warm_p50_ms | ms | 0.34 | 204 | 0.0201 |  | ≤ 1 ms (prefetched, engine API) | ✅ | 605.8× |
| T7 | open_256k_cold_idle_ms | ms | 537 | 775 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 710 ms | ✅ | 1.4× |
| T7 | open_256k_cold_ms | ms | 218 | 515 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 710 ms | ✅ | 2.4× |
| T7 | open_4k_cold_idle_ms | ms | 104 | 304 |  |  | ≤ 1 RTT + 5 ms = 105 ms | ✅ | 2.9× |
| T7 | open_4k_cold_ms | ms | 103 | 307 |  |  | ≤ 1 RTT + 5 ms = 105 ms | ✅ | 3.0× |
| T8 | throughput_mbit | Mbit/s | 14.1 | 11.3 | 8506 | 16.4 | ≥ 80% of raw `cat` over the same link | ✅ | 1.2× |
| T9 | upload_4k_p50_ms | ms | 106 | 510 | 1.07 |  | ≤ 1 RTT + 5 ms = 105 ms | ❌ | 4.8× |
| T10 | reconnect_catchup_ms | ms | 624 |  |  |  | ≤ 1.5 s at RTT 40 ms (informational here) |  |  |
| T11 | full_tree_known_s | s | 2.22 | ~376 | 0.54 |  | ≤ 3 s at RTT 40 ms (informational here) |  | 169.5× |
| T11 | initial_sync_bytes | bytes | 2777751 |  |  |  |  |  |  |
| T11 | initial_sync_cold_daemon_s | s | 2.18 |  |  |  |  |  |  |
| T12 | daemon_rss_bytes_per_entry | B/entry | 185 |  |  |  | ≤ 250 B/entry | ✅ |  |
| T13 | p99_interactive_under_load_ms | ms | 141 | 3856 |  | 256 | ≤ RTT + 30 ms = 130 ms | ❌ | 27.4× |
| T13 | p99_listdir_under_load_ms | ms | 141 |  |  |  | ≤ RTT + 30 ms = 130 ms | ❌ |  |
| T13 | p99_pong_under_load_ms | ms | 126 |  |  |  | ≤ RTT + 30 ms = 130 ms | ✅ |  |
| T14 | bytes_moved_ratio | ratio | 0.0009 |  |  |  | ≤ 2× appended bytes | ✅ |  |

<details><summary>not measured (2)</summary>

- T5 — Skipped sshfs: quick mode (bounded by sshfs's 20 s dir cache; run --full)
- T4 — Skipped sshfs: quick mode (sshfs listing refresh is bounded by its 20 s dir cache; run --full)

</details>

## rtt0-bw200-ssh

Host load average (1/5/15 min): 31.7/38.4/41.0 at start, 32.5/37.5/40.6 at end.

Link: RTT p50 0.11 ms (nominal 0), down 191.2 / up 191.2 Mbit/s (nominal 200), 256 KiB fetch 11.3 ms warm vs 11.7 ms after 2 s idle.

| # | metric | unit | unlatch | sshfs | local | raw | target | pass | × vs sshfs |
|---|---|---|---:|---:|---:|---:|---|:-:|---:|
| T1 | list_1000_p50_ms | ms | 0.21 |  | 2.24 |  | ≤ 0.5 ms (engine API) | ✅ |  |
| T2 | stat_p50_us | us | 0.13 |  | 1.88 |  | ≤ 20 µs (engine API) | ✅ |  |
| T3 | ls_la_cold_ms | ms | 28.8 | 42.5 |  |  |  |  | 1.5× |
| T3 | ls_la_ms | ms | 6.38 | 6.62 | 10.1 |  | ≤ 2× local | ✅ | 1.0× |
| T4 | visible_event_p50_ms | ms | 2.83 |  |  |  | ≤ RTT/2 + 15 ms = 15 ms | ✅ |  |
| T4 | visible_p50_ms | ms | 2.81 | 0.52 |  |  | ≤ RTT/2 + 15 ms = 15 ms | ✅ | 0.2× |
| T4 | visible_readdir_p50_ms | ms | 1.81 | skip |  |  | ≤ RTT/2 + 15 ms = 15 ms | ✅ |  |
| T5 | burst_all_visible_ms | ms | 16.2 | skip |  |  | ≤ 300 ms + RTT = 300 ms | ✅ |  |
| T6 | open_small_warm_p50_ms | ms | 0.0567 | 1.08 | 0.0168 |  | ≤ 1 ms (prefetched, engine API) | ✅ | 19.0× |
| T7 | open_256k_cold_idle_ms | ms | 15.3 | 14.9 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 15 ms | ✅ | 1.0× |
| T7 | open_256k_cold_ms | ms | 12.8 | 23.6 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 15 ms | ✅ | 1.8× |
| T7 | open_4k_cold_idle_ms | ms | 2.15 | 4.79 |  |  | ≤ 1 RTT + 5 ms = 5 ms | ✅ | 2.2× |
| T7 | open_4k_cold_ms | ms | 1.11 | 2.57 |  |  | ≤ 1 RTT + 5 ms = 5 ms | ✅ | 2.3× |
| T8 | throughput_mbit | Mbit/s | 180 | 184 | 7038 | 184 | ≥ 80% of raw `cat` over the same link | ✅ | 1.0× |
| T9 | upload_4k_p50_ms | ms | 4.86 | 3.62 | 1.13 |  | ≤ 1 RTT + 5 ms = 5 ms | ✅ | 0.7× |
| T10 | reconnect_catchup_ms | ms | 392 |  |  |  | ≤ 1.5 s (RTT ≤ 40 ms) | ✅ |  |
| T11 | full_tree_known_s | s | 0.89 | ~36.0 | 0.50 |  | ≤ 3 s at RTT 40 ms (100k + 30k lazy) | ✅ | 40.3× |
| T11 | initial_sync_bytes | bytes | 2787919 |  |  |  |  |  |  |
| T11 | initial_sync_cold_daemon_s | s | 0.95 |  |  |  |  |  |  |
| T12 | daemon_rss_bytes_per_entry | B/entry | 185 |  |  |  | ≤ 250 B/entry | ✅ |  |
| T13 | p99_interactive_under_load_ms | ms | skip | skip |  | skip | ≤ RTT + 30 ms = 30 ms |  |  |
| T14 | bytes_moved_ratio | ratio | 0.0014 |  |  |  | ≤ 2× appended bytes | ✅ |  |

<details><summary>not measured (5)</summary>

- T13 — Skipped unlatch: T13 runs at 20 and 50 Mbit/s only
- T13 — Skipped raw: T13 runs at 20 and 50 Mbit/s only
- T13 — Skipped sshfs: T13 runs at 20 and 50 Mbit/s only
- T5 — Skipped sshfs: quick mode (bounded by sshfs's 20 s dir cache; run --full)
- T4 — Skipped sshfs: quick mode (sshfs listing refresh is bounded by its 20 s dir cache; run --full)

</details>

## rtt40-bw50-ssh

Host load average (1/5/15 min): 29.4/36.6/40.3 at start, 38.6/37.4/40.2 at end.

Link: RTT p50 40.34 ms (nominal 40), down 46.9 / up 46.9 Mbit/s (nominal 50), 256 KiB fetch 85.5 ms warm vs 211.0 ms after 2 s idle.

| # | metric | unit | unlatch | sshfs | local | raw | target | pass | × vs sshfs |
|---|---|---|---:|---:|---:|---:|---|:-:|---:|
| T1 | list_1000_p50_ms | ms | 0.14 |  | 2.74 |  | ≤ 0.5 ms (engine API) | ✅ |  |
| T2 | stat_p50_us | us | 0.13 |  | 1.95 |  | ≤ 20 µs (engine API) | ✅ |  |
| T3 | ls_la_cold_ms | ms | 17.0 | 345 |  |  |  |  | 20.3× |
| T3 | ls_la_ms | ms | 6.11 | 6.77 | 10.6 |  | ≤ 2× local; ≥ 10× faster than sshfs (cold) at RTT 40 | ✅ | 1.1× |
| T4 | visible_event_p50_ms | ms | 22.8 |  |  |  | ≤ RTT/2 + 15 ms = 35 ms | ✅ |  |
| T4 | visible_p50_ms | ms | 22.9 | 42.0 |  |  | ≤ RTT/2 + 15 ms = 35 ms | ✅ | 1.8× |
| T4 | visible_readdir_p50_ms | ms | 24.1 | skip |  |  | ≤ RTT/2 + 15 ms = 35 ms | ✅ |  |
| T5 | burst_all_visible_ms | ms | 33.2 | skip |  |  | ≤ 300 ms + RTT = 340 ms | ✅ |  |
| T6 | open_small_warm_p50_ms | ms | 0.29 | 82.8 | 0.0167 |  | ≤ 1 ms (prefetched, engine API) | ✅ | 281.6× |
| T7 | open_256k_cold_idle_ms | ms | 212 | 324 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 287 ms | ✅ | 1.5× |
| T7 | open_256k_cold_ms | ms | 86.1 | 212 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 287 ms | ✅ | 2.5× |
| T7 | open_4k_cold_idle_ms | ms | 42.7 | 126 |  |  | ≤ 1 RTT + 5 ms = 45 ms | ✅ | 2.9× |
| T7 | open_4k_cold_ms | ms | 43.4 | 124 |  |  | ≤ 1 RTT + 5 ms = 45 ms | ✅ | 2.9× |
| T8 | throughput_mbit | Mbit/s | 44.1 | 30.6 | 10291 | 39.8 | ≥ 80% of raw `cat` over the same link | ✅ | 1.4× |
| T9 | upload_4k_p50_ms | ms | 50.7 | 218 | 1.44 |  | ≤ 1 RTT + 5 ms = 45 ms | ❌ | 4.3× |
| T10 | reconnect_catchup_ms | ms | 843 |  |  |  | ≤ 1.5 s (RTT ≤ 40 ms) | ✅ |  |
| T11 | full_tree_known_s | s | 1.62 | ~336 | 0.55 |  | ≤ 3 s at RTT 40 ms (100k + 30k lazy) | ✅ | 207.1× |
| T11 | initial_sync_bytes | bytes | 2788435 |  |  |  |  |  |  |
| T11 | initial_sync_cold_daemon_s | s | 1.61 |  |  |  |  |  |  |
| T12 | daemon_rss_bytes_per_entry | B/entry | 189 |  |  |  | ≤ 250 B/entry | ✅ |  |
| T13 | p99_interactive_under_load_ms | ms | 72.4 | 1141 |  | 327 | ≤ RTT + 30 ms = 70 ms | ❌ | 15.8× |
| T13 | p99_listdir_under_load_ms | ms | 72.4 |  |  |  | ≤ RTT + 30 ms = 70 ms | ❌ |  |
| T13 | p99_pong_under_load_ms | ms | 65.7 |  |  |  | ≤ RTT + 30 ms = 70 ms | ✅ |  |
| T14 | bytes_moved_ratio | ratio | 0.0016 |  |  |  | ≤ 2× appended bytes | ✅ |  |

<details><summary>not measured (2)</summary>

- T5 — Skipped sshfs: quick mode (bounded by sshfs's 20 s dir cache; run --full)
- T4 — Skipped sshfs: quick mode (sshfs listing refresh is bounded by its 20 s dir cache; run --full)

</details>

## rtt100-bw20-ssh

Host load average (1/5/15 min): 35.9/36.9/40.0 at start, 48.2/43.7/42.2 at end.

Link: RTT p50 100.40 ms (nominal 100), down 18.2 / up 18.3 Mbit/s (nominal 20), 256 KiB fetch 210.2 ms warm vs 524.4 ms after 2 s idle.

| # | metric | unit | unlatch | sshfs | local | raw | target | pass | × vs sshfs |
|---|---|---|---:|---:|---:|---:|---|:-:|---:|
| T1 | list_1000_p50_ms | ms | 0.13 |  | 2.25 |  | ≤ 0.5 ms (engine API) | ✅ |  |
| T2 | stat_p50_us | us | 0.14 |  | 2.03 |  | ≤ 20 µs (engine API) | ✅ |  |
| T3 | ls_la_cold_ms | ms | 30.5 | 759 |  |  |  |  | 24.9× |
| T3 | ls_la_ms | ms | 6.10 | 6.21 | 10.1 |  | ≤ 2× local | ✅ | 1.0× |
| T4 | visible_event_p50_ms | ms | 53.3 |  |  |  | ≤ RTT/2 + 15 ms = 65 ms | ✅ |  |
| T4 | visible_p50_ms | ms | 53.3 | 101 |  |  | ≤ RTT/2 + 15 ms = 65 ms | ✅ | 1.9× |
| T4 | visible_readdir_p50_ms | ms | 62.3 | skip |  |  | ≤ RTT/2 + 15 ms = 65 ms | ✅ |  |
| T5 | burst_all_visible_ms | ms | 69.6 | skip |  |  | ≤ 300 ms + RTT = 400 ms | ✅ |  |
| T6 | open_small_warm_p50_ms | ms | 0.0591 | 205 | 0.0171 |  | ≤ 1 ms (prefetched, engine API) | ✅ | 3472.5× |
| T7 | open_256k_cold_idle_ms | ms | 528 | 780 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 710 ms | ✅ | 1.5× |
| T7 | open_256k_cold_ms | ms | 213 | 521 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 710 ms | ✅ | 2.4× |
| T7 | open_4k_cold_idle_ms | ms | 103 | 305 |  |  | ≤ 1 RTT + 5 ms = 105 ms | ✅ | 3.0× |
| T7 | open_4k_cold_ms | ms | 104 | 309 |  |  | ≤ 1 RTT + 5 ms = 105 ms | ✅ | 3.0× |
| T8 | throughput_mbit | Mbit/s | 14.4 | 11.1 | 8163 | 13.4 | ≥ 80% of raw `cat` over the same link | ✅ | 1.3× |
| T9 | upload_4k_p50_ms | ms | 107 | 515 | 1.32 |  | ≤ 1 RTT + 5 ms = 105 ms | ❌ | 4.8× |
| T10 | reconnect_catchup_ms | ms | 1448 |  |  |  | ≤ 1.5 s at RTT 40 ms (informational here) |  |  |
| T11 | full_tree_known_s | s | 3.04 | ~377 | 0.47 |  | ≤ 3 s at RTT 40 ms (informational here) |  | 123.9× |
| T11 | initial_sync_bytes | bytes | 2787875 |  |  |  |  |  |  |
| T11 | initial_sync_cold_daemon_s | s | 3.01 |  |  |  |  |  |  |
| T12 | daemon_rss_bytes_per_entry | B/entry | 185 |  |  |  | ≤ 250 B/entry | ✅ |  |
| T13 | p99_interactive_under_load_ms | ms | 146 | 2642 |  | 295 | ≤ RTT + 30 ms = 130 ms | ❌ | 18.1× |
| T13 | p99_listdir_under_load_ms | ms | 146 |  |  |  | ≤ RTT + 30 ms = 130 ms | ❌ |  |
| T13 | p99_pong_under_load_ms | ms | 120 |  |  |  | ≤ RTT + 30 ms = 130 ms | ✅ |  |
| T14 | bytes_moved_ratio | ratio | 0.0016 |  |  |  | ≤ 2× appended bytes | ✅ |  |

<details><summary>not measured (2)</summary>

- T5 — Skipped sshfs: quick mode (bounded by sshfs's 20 s dir cache; run --full)
- T4 — Skipped sshfs: quick mode (sshfs listing refresh is bounded by its 20 s dir cache; run --full)

</details>

## rtt40-bw50-fqcodel

Host load average (1/5/15 min): 43.8/42.9/41.9 at start, 72.1/49.1/44.0 at end.

Link: RTT p50 40.37 ms (nominal 40), down 44.7 / up 44.8 Mbit/s (nominal 50), 256 KiB fetch 87.1 ms warm vs 208.3 ms after 2 s idle.

| # | metric | unit | unlatch | sshfs | local | raw | target | pass | × vs sshfs |
|---|---|---|---:|---:|---:|---:|---|:-:|---:|
| T1 | list_1000_p50_ms | ms | 0.19 |  | 2.32 |  | ≤ 0.5 ms (engine API) | ✅ |  |
| T2 | stat_p50_us | us | 0.13 |  | 1.89 |  | ≤ 20 µs (engine API) | ✅ |  |
| T3 | ls_la_cold_ms | ms | 13.5 | 368 |  |  |  |  | 27.4× |
| T3 | ls_la_ms | ms | 5.85 | 6.66 | 9.95 |  | ≤ 2× local; ≥ 10× faster than sshfs (cold) at RTT 40 | ✅ | 1.1× |
| T4 | visible_event_p50_ms | ms | 24.0 |  |  |  | ≤ RTT/2 + 15 ms = 35 ms | ✅ |  |
| T4 | visible_p50_ms | ms | 23.5 | 40.7 |  |  | ≤ RTT/2 + 15 ms = 35 ms | ✅ | 1.7× |
| T4 | visible_readdir_p50_ms | ms | 26.3 | skip |  |  | ≤ RTT/2 + 15 ms = 35 ms | ✅ |  |
| T5 | burst_all_visible_ms | ms | 31.8 | skip |  |  | ≤ 300 ms + RTT = 340 ms | ✅ |  |
| T6 | open_small_warm_p50_ms | ms | 0.36 | 82.5 | 0.0165 |  | ≤ 1 ms (prefetched, engine API) | ✅ | 230.2× |
| T7 | open_256k_cold_idle_ms | ms | 209 | 334 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 287 ms | ✅ | 1.6× |
| T7 | open_256k_cold_ms | ms | 84.7 | 209 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 287 ms | ✅ | 2.5× |
| T7 | open_4k_cold_idle_ms | ms | 45.0 | 123 |  |  | ≤ 1 RTT + 5 ms = 45 ms | ❌ | 2.7× |
| T7 | open_4k_cold_ms | ms | 44.3 | 125 |  |  | ≤ 1 RTT + 5 ms = 45 ms | ✅ | 2.8× |
| T8 | throughput_mbit | Mbit/s | 40.7 | 31.2 | 6514 | 41.8 | ≥ 80% of raw `cat` over the same link | ✅ | 1.3× |
| T9 | upload_4k_p50_ms | ms | 47.8 | 207 | 1.15 |  | ≤ 1 RTT + 5 ms = 45 ms | ❌ | 4.3× |
| T10 | reconnect_catchup_ms | ms | 403 |  |  |  | ≤ 1.5 s (RTT ≤ 40 ms) | ✅ |  |
| T11 | full_tree_known_s | s | 1.11 | ~337 | 0.50 |  | ≤ 3 s at RTT 40 ms (100k + 30k lazy) | ✅ | 302.8× |
| T11 | initial_sync_bytes | bytes | 2773051 |  |  |  |  |  |  |
| T11 | initial_sync_cold_daemon_s | s | 1.29 |  |  |  |  |  |  |
| T12 | daemon_rss_bytes_per_entry | B/entry | 183 |  |  |  | ≤ 250 B/entry | ✅ |  |
| T13 | p99_interactive_under_load_ms | ms | 138 | 1191 |  | 45.0 | ≤ RTT + 30 ms = 70 ms | ❌ | 8.7× |
| T13 | p99_listdir_under_load_ms | ms | 138 |  |  |  | ≤ RTT + 30 ms = 70 ms | ❌ |  |
| T13 | p99_pong_under_load_ms | ms | 98.6 |  |  |  | ≤ RTT + 30 ms = 70 ms | ❌ |  |
| T14 | bytes_moved_ratio | ratio | 0.0009 |  |  |  | ≤ 2× appended bytes | ✅ |  |

<details><summary>not measured (2)</summary>

- T5 — Skipped sshfs: quick mode (bounded by sshfs's 20 s dir cache; run --full)
- T4 — Skipped sshfs: quick mode (sshfs listing refresh is bounded by its 20 s dir cache; run --full)

</details>

## rtt100-bw20-fqcodel

Host load average (1/5/15 min): 80.3/52.0/45.0 at start, 60.1/54.2/46.8 at end.

Link: RTT p50 102.64 ms (nominal 100), down 15.4 / up 14.1 Mbit/s (nominal 20), 256 KiB fetch 240.8 ms warm vs 535.3 ms after 2 s idle.

| # | metric | unit | unlatch | sshfs | local | raw | target | pass | × vs sshfs |
|---|---|---|---:|---:|---:|---:|---|:-:|---:|
| T1 | list_1000_p50_ms | ms | 0.14 |  | 2.41 |  | ≤ 0.5 ms (engine API) | ✅ |  |
| T2 | stat_p50_us | us | 0.14 |  | 1.97 |  | ≤ 20 µs (engine API) | ✅ |  |
| T3 | ls_la_cold_ms | ms | 16.2 | 771 |  |  |  |  | 47.7× |
| T3 | ls_la_ms | ms | 5.98 | 6.61 | 11.7 |  | ≤ 2× local | ✅ | 1.1× |
| T4 | visible_event_p50_ms | ms | 52.8 |  |  |  | ≤ RTT/2 + 15 ms = 65 ms | ✅ |  |
| T4 | visible_p50_ms | ms | 52.6 | 101 |  |  | ≤ RTT/2 + 15 ms = 65 ms | ✅ | 1.9× |
| T4 | visible_readdir_p50_ms | ms | 54.3 | skip |  |  | ≤ RTT/2 + 15 ms = 65 ms | ✅ |  |
| T5 | burst_all_visible_ms | ms | 65.6 | skip |  |  | ≤ 300 ms + RTT = 400 ms | ✅ |  |
| T6 | open_small_warm_p50_ms | ms | 0.28 | 204 | 0.0165 |  | ≤ 1 ms (prefetched, engine API) | ✅ | 718.1× |
| T7 | open_256k_cold_idle_ms | ms | 530 | 772 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 710 ms | ✅ | 1.5× |
| T7 | open_256k_cold_ms | ms | 316 | 517 |  |  | ≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = 710 ms | ✅ | 1.6× |
| T7 | open_4k_cold_idle_ms | ms | 101 | 302 |  |  | ≤ 1 RTT + 5 ms = 105 ms | ✅ | 3.0× |
| T7 | open_4k_cold_ms | ms | 104 | 303 |  |  | ≤ 1 RTT + 5 ms = 105 ms | ✅ | 2.9× |
| T8 | throughput_mbit | Mbit/s | 13.5 | 11.3 | 8687 | 14.7 | ≥ 80% of raw `cat` over the same link | ✅ | 1.2× |
| T9 | upload_4k_p50_ms | ms | 107 | 509 | 1.15 |  | ≤ 1 RTT + 5 ms = 105 ms | ❌ | 4.7× |
| T10 | reconnect_catchup_ms | ms | 623 |  |  |  | ≤ 1.5 s at RTT 40 ms (informational here) |  |  |
| T11 | full_tree_known_s | s | 2.53 | ~375 | 0.53 |  | ≤ 3 s at RTT 40 ms (informational here) |  | 148.4× |
| T11 | initial_sync_bytes | bytes | 2777362 |  |  |  |  |  |  |
| T11 | initial_sync_cold_daemon_s | s | 2.43 |  |  |  |  |  |  |
| T12 | daemon_rss_bytes_per_entry | B/entry | 185 |  |  |  | ≤ 250 B/entry | ✅ |  |
| T13 | p99_interactive_under_load_ms | ms | 209 | 2775 |  | 105 | ≤ RTT + 30 ms = 130 ms | ❌ | 13.3× |
| T13 | p99_listdir_under_load_ms | ms | 209 |  |  |  | ≤ RTT + 30 ms = 130 ms | ❌ |  |
| T13 | p99_pong_under_load_ms | ms | 133 |  |  |  | ≤ RTT + 30 ms = 130 ms | ❌ |  |
| T14 | bytes_moved_ratio | ratio | 0.0009 |  |  |  | ≤ 2× appended bytes | ✅ |  |

<details><summary>not measured (2)</summary>

- T5 — Skipped sshfs: quick mode (bounded by sshfs's 20 s dir cache; run --full)
- T4 — Skipped sshfs: quick mode (sshfs listing refresh is bounded by its 20 s dir cache; run --full)

</details>

## daemon

| # | metric | unit | unlatch | sshfs | local | raw | target | pass | × vs sshfs |
|---|---|---|---:|---:|---:|---:|---|:-:|---:|
| T15 | listdir_entries_sent | entries | 200 |  |  |  | = 1 level (200 entries), child dirs lazy | ✅ |  |
| T15 | watches_added | watches | 1.00 |  |  |  | ≤ 1 + child dirs of that level (201) | ✅ |  |
| T16 | restart_snapshot_bytes | bytes | 0 |  |  |  | 0 snapshot bytes | ✅ |  |
| T16 | restart_welcome_ms | ms | 65.5 |  |  |  | ≤ 200 ms, Resume mode, same index | ✅ |  |
| T17 | same_size_rewrite_new_version_pct | % | 100 |  |  |  | 100% new content version | ✅ |  |

## Notes

- binaries: unlatchd=<workdir>/target/release/unlatchd unlatch=<workdir>/target/release/unlatch (release builds; the project was then codenamed hatch) sshfs=~/.local/bin/sshfs sftp-server=/usr/lib/openssh/sftp-server
- layout: VM side (tree, unlatchd state) under <workdir>/target/unlatch-bench-work; Mac side (engine replica, cache, staging, FUSE state) under the same directory
- tree: seed 0x4a7c4b3e20260930, 101006 eager entries + 30007 lazy, 793 MiB
