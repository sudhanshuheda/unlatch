#!/usr/bin/env bash
# Build the Rust half of the macOS app.
#
#   build-rust.sh                 libunlatch.a (universal) + unlatchd (linux-musl x86_64/aarch64)
#   build-rust.sh --ffi-only      only libunlatch.a            (macOS: universal via lipo;
#                                 elsewhere: a host build for the portable Swift tests)
#   build-rust.sh --unlatchd-only   only unlatchd                (any host with cargo-zigbuild + zig)
#   build-rust.sh --xcode         Xcode "RustLib" phase: libunlatch.a, plus unlatchd when it can
#   build-rust.sh --stage-bundle <Unlatch.app>
#                                 copy staged unlatchd + .sha256 into <app>/Contents/Resources/unlatchd
#   --debug                       debug profile instead of release
#
# Outputs (git-ignored):
#   mac/build/rust/libunlatch.a                 universal static library (aarch64 + x86_64)
#   mac/build/rust/native-libs.xcconfig       UNLATCH_RUST_LDFLAGS from `--print native-static-libs`
#   mac/build/unlatchd/unlatchd-<arch>{,.sha256}  static VM daemons, <arch> = `uname -m` on the VM
#
# Environment:
#   UNLATCHD_PREBUILT_DIR   take unlatchd-<arch> from here instead of building (CI: the musl job)
#   UNLATCH_REQUIRE_UNLATCHD  1 = fail when unlatchd cannot be built or found (release builds)
#   UNLATCH_SKIP_RUST       1 = --xcode does nothing if libunlatch.a already exists (CI prebuilt it)
#   CARGO_TARGET_DIR      respected
set -Eeuo pipefail
# Under `set -e` a failing command substitution exits without a word; name the culprit.
trap 'echo "error: build-rust: line $LINENO: \"$BASH_COMMAND\" exited with status $?" >&2' ERR

MAC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "$MAC_DIR/.." && pwd)"
RUST_OUT="$MAC_DIR/build/rust"
UNLATCHD_OUT="$MAC_DIR/build/unlatchd"
HEADER="$REPO_DIR/crates/unlatch-ffi/include/unlatch.h"
DARWIN_TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
# VM arch (uname -m) -> Rust target
UNLATCHD_ARCHES=(x86_64 aarch64)

mode=all
profile=release
stage_bundle=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --ffi-only) mode=ffi ;;
    --unlatchd-only) mode=unlatchd ;;
    --xcode) mode=xcode ;;
    --stage-bundle) mode=stage; stage_bundle="${2:?--stage-bundle needs the .app path}"; shift ;;
    --debug) profile=debug ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "build-rust.sh: unknown argument $1" >&2; exit 2 ;;
  esac
  shift
done

log() { echo "build-rust: $*" >&2; }
warn() {
  # Xcode shows "warning: …" lines in the issue navigator.
  echo "warning: build-rust: $*" >&2
}
die() { echo "error: build-rust: $*" >&2; exit 1; }

# Xcode runs scripts with a minimal PATH and exports its own build settings; cargo and the cc
# crate must not pick those up (CC/CFLAGS/LDFLAGS belong to the Swift targets, not to Rust).
prepare_env() {
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
  unset CC CXX LD CFLAGS CXXFLAGS LDFLAGS CPATH LIBRARY_PATH RUSTFLAGS || true
  command -v cargo >/dev/null || die "cargo not found; install Rust from https://rustup.rs"
  TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_DIR/target}"
}

cargo_profile_flag() {
  if [[ "$profile" == release ]]; then echo "--release"; fi
}

sha256_of() {
  if command -v shasum >/dev/null; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

ensure_rust_targets() {
  command -v rustup >/dev/null || return 0
  local installed
  installed="$(rustup target list --installed)"
  local t
  for t in "$@"; do
    grep -qx "$t" <<<"$installed" || rustup target add "$t"
  done
}

# One libunlatch.a build. Always `cargo rustc --crate-type staticlib`, never `cargo build`:
# unlatch-ffi declares crate-type = ["staticlib", "rlib"], and when rustc emits an rlib in the same
# invocation it does not run the release profile's (thin) LTO, so the staticlib is just every
# crate's un-optimised-across-crates objects, each with an embedded LLVM bitcode copy. With only
# the staticlib requested, rustc performs the LTO the profile asks for and ships its output.
# The same invocation prints the native libraries the Swift targets must link.
#   cargo_staticlib <target-or-empty> <log>
cargo_staticlib() {
  local target="$1" out_log="$2"
  # --color never: CI sets CARGO_TERM_COLOR=always, and the colour codes would end up in the
  # native-static-libs line (ld: library 'm\e[0m' not found).
  local args=(rustc --color never -p unlatch-ffi --lib --crate-type staticlib)
  [[ "$profile" == release ]] && args+=(--release)
  [[ -n "$target" ]] && args+=(--target "$target")
  log "cargo ${args[*]} ($profile)"
  # Streamed (cargo's own progress stays visible) and kept for the native-static-libs line.
  if ! (cd "$REPO_DIR" && cargo "${args[@]}" -- --print native-static-libs) 2>&1 | tee "$out_log" >&2; then
    die "libunlatch build failed (log: $out_log)"
  fi
}

# The native-static-libs line of a cargo_staticlib log -> native-libs.xcconfig.
write_native_libs() {
  local native
  native="$(sed -e $'s/\x1b\\[[0-9;]*m//g' -n -e 's/.*native-static-libs: //p' "$1" | tail -1)"
  # clang always links libSystem; listing it again only earns "ld: warning: ignoring duplicate
  # libraries: '-lSystem'".
  native="$(tr ' ' '\n' <<<"$native" | grep -vx -- '-lSystem' | grep -v '^$' | tr '\n' ' ' | sed 's/ *$//')" || true
  if [[ -z "$native" ]]; then
    warn "rustc printed no native-static-libs line; using the defaults in Project.xcconfig"
    return 0
  fi
  printf '// Generated by mac/scripts/build-rust.sh — do not edit.\nUNLATCH_RUST_LDFLAGS = -lunlatch %s\n' "$native" \
    > "$RUST_OUT/native-libs.xcconfig"
}

build_ffi_host() {
  # Not macOS: a host libunlatch.a, so `swift test --package-path mac/UnlatchShared` can run the
  # portable Swift tests (protocol fixtures) on Linux.
  log "host libunlatch.a ($profile) — not a macOS build"
  mkdir -p "$RUST_OUT"
  cargo_staticlib "" "$RUST_OUT/cargo-host.log"
  cp -f "$TARGET_DIR/$profile/libunlatch.a" "$RUST_OUT/libunlatch.a"
  log "host libunlatch.a ready ($RUST_OUT)"
}

build_ffi() {
  if [[ "$(uname -s)" != Darwin ]]; then
    build_ffi_host
    return
  fi
  # Match the app's deployment target, or the linker warns about newer object files.
  export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
  ensure_rust_targets "${DARWIN_TARGETS[@]}"
  mkdir -p "$RUST_OUT"
  local libs=() t
  for t in "${DARWIN_TARGETS[@]}"; do
    cargo_staticlib "$t" "$RUST_OUT/cargo-$t.log"
    libs+=("$TARGET_DIR/$t/$profile/libunlatch.a")
  done
  write_native_libs "$RUST_OUT/cargo-${DARWIN_TARGETS[0]}.log"
  lipo -create -output "$RUST_OUT/libunlatch.a.tmp" "${libs[@]}"
  mv -f "$RUST_OUT/libunlatch.a.tmp" "$RUST_OUT/libunlatch.a"

  # Every function in unlatch.h must be exported (a renamed Rust fn would otherwise surface as an
  # undefined symbol deep inside the Xcode link). Checked per slice, so each architecture's
  # archive is judged on its own. --no-llvm-bc: Xcode's nm (llvm-nm) otherwise reads the
  # symbols from the __LLVM,__bitcode section that Rust objects embed (std's compiler_builtins
  # always does), and Xcode's LLVM is older than rustc's ("Unknown attribute kind").
  local missing=0 sym lib exported nm_err
  nm_err="$(mktemp)"
  for t in "${DARWIN_TARGETS[@]}"; do
    lib="$TARGET_DIR/$t/$profile/libunlatch.a"
    if ! exported="$(nm --no-llvm-bc -gU "$lib" 2>"$nm_err" | awk 'NF >= 3 && $2 ~ /^[TDSB]$/ {print $3}' | sed 's/^_//' | sort -u)"; then
      echo "error: build-rust: nm cannot read the $t libunlatch.a:" >&2
      head -5 "$nm_err" >&2 || true
      missing=1
      continue
    fi
    for sym in $(grep -oE 'unlatch_[a-z0-9_]+\(' "$HEADER" | tr -d '(' | sort -u); do
      if ! grep -qx "$sym" <<<"$exported"; then
        echo "error: build-rust: $sym is declared in unlatch.h but not exported by the $t libunlatch.a" >&2
        missing=1
      fi
    done
  done
  rm -f "$nm_err"
  [[ $missing == 0 ]] || exit 1
  log "libunlatch.a ready: $(lipo -archs "$RUST_OUT/libunlatch.a") ($RUST_OUT)"
}

stage_unlatchd_file() {
  local arch="$1" src="$2"
  mkdir -p "$UNLATCHD_OUT"
  cp -f "$src" "$UNLATCHD_OUT/unlatchd-$arch.tmp"
  chmod 0755 "$UNLATCHD_OUT/unlatchd-$arch.tmp"
  mv -f "$UNLATCHD_OUT/unlatchd-$arch.tmp" "$UNLATCHD_OUT/unlatchd-$arch"
  printf '%s  unlatchd-%s\n' "$(sha256_of "$UNLATCHD_OUT/unlatchd-$arch")" "$arch" > "$UNLATCHD_OUT/unlatchd-$arch.sha256"
}

build_unlatchd() {
  local strict="${1:-${UNLATCH_REQUIRE_UNLATCHD:-0}}" arch
  if [[ -n "${UNLATCHD_PREBUILT_DIR:-}" ]]; then
    for arch in "${UNLATCHD_ARCHES[@]}"; do
      [[ -f "$UNLATCHD_PREBUILT_DIR/unlatchd-$arch" ]] || die "missing $UNLATCHD_PREBUILT_DIR/unlatchd-$arch"
      stage_unlatchd_file "$arch" "$UNLATCHD_PREBUILT_DIR/unlatchd-$arch"
    done
    log "unlatchd staged from $UNLATCHD_PREBUILT_DIR"
    return 0
  fi
  if ! command -v cargo-zigbuild >/dev/null || ! command -v zig >/dev/null; then
    if [[ -f "$UNLATCHD_OUT/unlatchd-x86_64" && -f "$UNLATCHD_OUT/unlatchd-aarch64" ]]; then
      warn "cargo-zigbuild/zig not installed; keeping the unlatchd binaries already in $UNLATCHD_OUT"
      return 0
    fi
    if [[ "$strict" == 1 ]]; then
      die "unlatchd needs cargo-zigbuild and zig (brew install zig && cargo install cargo-zigbuild)"
    fi
    warn "unlatchd not built (install zig + cargo-zigbuild); the app can then only use VMs that already run unlatchd"
    return 0
  fi
  for arch in "${UNLATCHD_ARCHES[@]}"; do
    local target="$arch-unknown-linux-musl"
    ensure_rust_targets "$target"
    log "cargo zigbuild -p unlatchd --target $target ($profile)"
    (cd "$REPO_DIR" && cargo zigbuild -p unlatchd $(cargo_profile_flag) --target "$target")
    stage_unlatchd_file "$arch" "$TARGET_DIR/$target/$profile/unlatchd"
  done
  log "unlatchd staged in $UNLATCHD_OUT"
}

stage_into_bundle() {
  local app="$1"
  [[ -d "$app/Contents" ]] || die "$app is not an app bundle"
  local dest="$app/Contents/Resources/unlatchd"
  rm -rf "$dest"
  if [[ ! -f "$UNLATCHD_OUT/unlatchd-x86_64" && ! -f "$UNLATCHD_OUT/unlatchd-aarch64" ]]; then
    [[ "${UNLATCH_REQUIRE_UNLATCHD:-0}" == 1 ]] && die "no unlatchd binaries in $UNLATCHD_OUT to stage"
    warn "no unlatchd binaries staged in $UNLATCHD_OUT; the app bundle will not carry unlatchd"
    return 0
  fi
  mkdir -p "$dest"
  local arch
  for arch in "${UNLATCHD_ARCHES[@]}"; do
    [[ -f "$UNLATCHD_OUT/unlatchd-$arch" ]] || continue
    # Re-check the checksum we ship against the file we ship.
    local want have
    want="$(awk '{print $1}' "$UNLATCHD_OUT/unlatchd-$arch.sha256")"
    have="$(sha256_of "$UNLATCHD_OUT/unlatchd-$arch")"
    [[ "$want" == "$have" ]] || die "unlatchd-$arch checksum mismatch ($want != $have)"
    cp -f "$UNLATCHD_OUT/unlatchd-$arch" "$UNLATCHD_OUT/unlatchd-$arch.sha256" "$dest/"
  done
  log "unlatchd copied into $dest"
}

case "$mode" in
  all)
    prepare_env
    build_ffi
    build_unlatchd
    ;;
  ffi)
    prepare_env
    build_ffi
    ;;
  unlatchd)
    prepare_env
    build_unlatchd 1
    ;;
  xcode)
    if [[ "${UNLATCH_SKIP_RUST:-0}" == 1 && -f "$RUST_OUT/libunlatch.a" ]]; then
      log "UNLATCH_SKIP_RUST=1 and libunlatch.a exists; skipping"
      exit 0
    fi
    # Xcode's own setting (e.g. 13.0) wins over the default above.
    prepare_env
    build_ffi
    build_unlatchd
    ;;
  stage)
    stage_into_bundle "$stage_bundle"
    ;;
esac
