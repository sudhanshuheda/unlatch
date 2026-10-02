#!/usr/bin/env bash
# Build the harness and run the benchmark: scripts/bench.sh [unlatch-bench run args…]
#   scripts/bench.sh --quick
#   scripts/bench.sh --full --out target/bench/full.json
#   scripts/bench.sh --profile rtt0-bw200,rtt100-bw20 --only T3,T4 --systems sshfs,local
# Binaries: target/verify/release/{unlatch-bench,unlatchd,unlatch} (CARGO_TARGET_DIR overrides).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/verify}"
if ! unshare -rn true 2>/dev/null; then
  echo "bench.sh: unprivileged user+net namespaces are unavailable (unshare -rn failed)" >&2
  exit 2
fi
cargo build --release -p unlatch-bench
cargo build --release -p unlatchd || echo "bench.sh: unlatchd did not build; Unlatch rows will be n/a" >&2
cargo build --release -p unlatch-cli || echo "bench.sh: unlatch CLI did not build; FUSE rows will be n/a" >&2
BIN="$CARGO_TARGET_DIR/release"
args=()
[ -x "$BIN/unlatchd" ] && args+=(--unlatchd "$BIN/unlatchd")
[ -x "$BIN/unlatch" ] && args+=(--unlatch "$BIN/unlatch")
exec "$BIN/unlatch-bench" run "${args[@]}" "$@"
