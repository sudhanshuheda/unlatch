#!/usr/bin/env bash
# Unlatch self-verification loop (DESIGN §7): fmt → clippy → tests → fpsim → fuzz → bench →
# compare against bench/baseline.json → PASS/FAIL summary. Exits non-zero on any failure.
#
#   scripts/verify.sh            quick mode (default; < 5 min): 1 bench profile, few seeds
#   scripts/verify.sh --full     all 9 bench profiles, more seeds
#   options: --seeds N  --skip stage[,stage]  --only stage[,stage]  --no-bench  --jobs N
#   stages:  fmt clippy test build fpsim fuzz bench compare
#
# Every stage's output goes to target/bench/logs/<stage>.log. Environment:
#   CARGO_TARGET_DIR   build dir (default: target/verify)
#   UNLATCH_BENCH_WORK   bench work dir (synthetic tree cache; default: target/unlatch-bench-work)
#   VERIFY_BENCH_ARGS  extra args for `unlatch-bench run`
#   VERIFY_RESULTS     results/logs dir (default: target/bench)
#   VERIFY_LIMIT_SCALE multiply quick-mode stage time limits (default 1)
#   UNLATCH_NO_NETNS=1   no unprivileged netns here (e.g. a CI runner): skip the netns + tc netem
#                      bench and its baseline comparison (reported as SKIPPED, never as PASS);
#                      every correctness gate (fmt, clippy, test, build, fpsim, fuzz) still runs
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

MODE=quick
SEEDS=""
SKIP=""
ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --full) MODE=full ;;
    --quick) MODE=quick ;;
    --seeds) SEEDS="$2"; shift ;;
    --skip) SKIP="$2"; shift ;;
    --only) ONLY="$2"; shift ;;
    --no-bench) SKIP="${SKIP:+$SKIP,}bench,compare" ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "verify.sh: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
if [ -z "$SEEDS" ]; then
  if [ "$MODE" = full ]; then SEEDS=200; else SEEDS=5; fi
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/verify}"
export UNLATCH_BENCH_WORK="${UNLATCH_BENCH_WORK:-$ROOT/target/unlatch-bench-work}"
RESULTS="${VERIFY_RESULTS:-$ROOT/target/bench}"
LOGS="$RESULTS/logs"
mkdir -p "$LOGS"
BIN="$CARGO_TARGET_DIR/release"

declare -a NAMES=() RESULTS_=() SECS=() NOTES=()
FAILED=0

want() { # stage selected?
  local s="$1"
  if [ -n "$ONLY" ] && [[ ",$ONLY," != *",$s,"* ]]; then return 1; fi
  if [[ ",$SKIP," == *",$s,"* ]]; then return 1; fi
  return 0
}

# Stages that need unprivileged user + network namespaces (`unshare -rn`, `tc netem`).
NETNS_STAGES=",bench,compare,"
NO_NETNS=0
case "${UNLATCH_NO_NETNS:-}" in 1|true|yes) NO_NETNS=1 ;; esac
if [ $NO_NETNS -eq 1 ]; then
  echo "UNLATCH_NO_NETNS=1: the netns/netem bench (and its baseline comparison) will be SKIPPED;" \
       "correctness gates still run"
fi

# run_stage NAME CMD... — runs CMD with output to logs/NAME.log, records PASS/FAIL.
# Per-stage wall-clock limits (seconds; 0 = none); a stage that hits its limit FAILs. Quick-mode
# limits are sized from measured runs on a loaded 124-core box: the fpsim suite takes ~190 s
# (MQ-037/MQ-080 wait out real offline retries), a fuzz seed usually takes ~2 s but a seed with a
# long offline stretch can take minutes. VERIFY_LIMIT_SCALE multiplies them (e.g. 2 on CI runners).
if [ "$MODE" = full ]; then
  declare -A LIMIT=([fpsim]=0 [fuzz]=0 [bench]=0)
else
  declare -A LIMIT=([fpsim]=420 [fuzz]=300 [bench]=300)
  scale="${VERIFY_LIMIT_SCALE:-1}"
  for k in "${!LIMIT[@]}"; do LIMIT[$k]=$(awk -v l="${LIMIT[$k]}" -v s="$scale" 'BEGIN { printf "%d", l * s }'); done
fi

run_stage() {
  local name="$1"; shift
  if ! want "$name"; then
    NAMES+=("$name"); RESULTS_+=("SKIP"); SECS+=("-"); NOTES+=("skipped by option")
    return 0
  fi
  if [ $NO_NETNS -eq 1 ] && [[ "$NETNS_STAGES" == *",$name,"* ]]; then
    printf '== %-8s SKIPPED (UNLATCH_NO_NETNS=1: no unprivileged netns, netem bench not run)\n' "$name"
    echo "# $(date -Is) SKIPPED: UNLATCH_NO_NETNS=1 (no unprivileged netns; netem bench not run)" \
      > "$LOGS/$name.log"
    NAMES+=("$name"); RESULTS_+=("SKIP"); SECS+=("-"); NOTES+=("SKIPPED: UNLATCH_NO_NETNS=1 (no netns)")
    return 0
  fi
  local log="$LOGS/$name.log"
  local t0=$(date +%s)
  printf '== %-8s ' "$name"
  { echo "# $(date -Is) $*"; } > "$log"
  local limit="${LIMIT[$name]:-0}" rc
  if [ "$limit" -gt 0 ]; then
    # `timeout` cannot run shell functions: run the stage in a subshell under a watchdog.
    ( "$@" ) >> "$log" 2>&1 &
    local pid=$!
    ( sleep "$limit"
      echo "stage $name: time limit ${limit}s reached" >> "$log"
      pkill -TERM -P "$pid" 2>/dev/null; kill -TERM "$pid" 2>/dev/null ) &
    local dog=$!
    wait "$pid"; rc=$?
    kill "$dog" 2>/dev/null; wait "$dog" 2>/dev/null
  else
    "$@" >> "$log" 2>&1; rc=$?
  fi
  local dt=$(( $(date +%s) - t0 ))
  local note
  note="$(grep -E '^(error|FAIL|REGRESSION|test result: FAILED|thread .* panicked)' "$log" | head -1 | cut -c1-90)"
  NAMES+=("$name"); SECS+=("$dt")
  if [ $rc -eq 0 ]; then
    RESULTS_+=("PASS"); NOTES+=("$(tail -1 "$log" | cut -c1-90)")
    echo "PASS (${dt}s)"
  else
    RESULTS_+=("FAIL"); NOTES+=("rc=$rc ${note:-see $log}")
    FAILED=1
    echo "FAIL (${dt}s) — $log"
  fi
  return $rc
}

run_stage fmt cargo fmt --all -- --check
run_stage clippy cargo clippy --workspace --all-targets -- -D warnings
run_stage test cargo test --workspace
# Release binaries for the harness (unlatchd must also build static for musl; checked when the
# target is installed).
build_bins() {
  cargo build --release -p unlatch-bench || return 1
  local rc=0
  cargo build --release -p unlatchd || rc=1
  cargo build --release -p unlatch-cli || rc=1
  if rustup target list --installed 2>/dev/null | grep -q x86_64-unknown-linux-musl; then
    cargo build --release -p unlatchd --target x86_64-unknown-linux-musl || rc=1
  fi
  return $rc
}
run_stage build build_bins
BENCH="$BIN/unlatch-bench"
# fpsim/fuzz/bench find the binaries under test through these.
[ -x "$BIN/unlatchd" ] && export UNLATCHD_BIN="$BIN/unlatchd"
[ -x "$BIN/unlatch" ] && export UNLATCH_BIN="$BIN/unlatch"
have_bench() { [ -x "$BENCH" ] || { echo "unlatch-bench binary missing ($BENCH): build stage failed"; return 1; }; }
fpsim_run() { have_bench && "$BENCH" fpsim --seeds "$SEEDS"; }
fuzz_run() { have_bench && "$BENCH" fuzz --seeds "$SEEDS"; }
run_stage fpsim fpsim_run
run_stage fuzz fuzz_run

OUT="$RESULTS/latest.json"
bench_run() {
  have_bench || return 1
  local args=(run --out "$OUT" --scorecard "$RESULTS/SCORECARD.md" --work "$UNLATCH_BENCH_WORK")
  [ -x "$BIN/unlatchd" ] && args+=(--unlatchd "$BIN/unlatchd")
  [ -x "$BIN/unlatch" ] && args+=(--unlatch "$BIN/unlatch")
  if [ "$MODE" = full ]; then args+=(--full); else args+=(--quick); fi
  # shellcheck disable=SC2086
  "$BENCH" "${args[@]}" ${VERIFY_BENCH_ARGS:-}
}
run_stage bench bench_run

compare_run() {
  have_bench || return 1
  if [ ! -f "$ROOT/bench/baseline.json" ]; then
    echo "no bench/baseline.json — nothing to compare (copy $OUT there to set one)"
    return 0
  fi
  [ -f "$OUT" ] || { echo "no $OUT (bench stage did not produce results)"; return 1; }
  "$BENCH" compare "$ROOT/bench/baseline.json" "$OUT"
}
run_stage compare compare_run

echo
printf '%-9s %-5s %6s  %s\n' STAGE RESULT SECS NOTE
for i in "${!NAMES[@]}"; do
  printf '%-9s %-5s %6s  %s\n' "${NAMES[$i]}" "${RESULTS_[$i]}" "${SECS[$i]}" "${NOTES[$i]}"
done
if [ $NO_NETNS -eq 1 ] && want bench; then
  echo
  echo "Scorecard: SKIPPED — UNLATCH_NO_NETNS=1, the netns/netem bench did not run (performance" \
       "targets NOT verified by this run)"
elif [ -f "$RESULTS/SCORECARD.md" ] && want bench; then
  echo
  grep -m1 '^\*\*Unlatch targets' "$RESULTS/SCORECARD.md" || true
fi
echo
SUFFIX=""
[ $NO_NETNS -eq 1 ] && SUFFIX=", bench SKIPPED: UNLATCH_NO_NETNS=1"
if [ $FAILED -eq 0 ]; then echo "VERIFY: PASS ($MODE$SUFFIX)"; else echo "VERIFY: FAIL ($MODE$SUFFIX) — logs in $LOGS"; fi
exit $FAILED
