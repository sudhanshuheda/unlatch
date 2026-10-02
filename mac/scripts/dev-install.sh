#!/usr/bin/env bash
# Build Unlatch.app from this checkout, sign it with your Apple Development certificate, install it
# in /Applications and open it: the from-source way to try Unlatch on your own Mac.
#
#   mac/scripts/dev-install.sh [--prebuilt DIR] [--no-open]
#   mac/scripts/dev-install.sh --check            # only check the prerequisites
#   mac/scripts/dev-install.sh --unsigned --no-install   # compile check (CI); cannot run in Finder
#
# --prebuilt DIR  unlatchd-x86_64 and unlatchd-aarch64 built elsewhere (for example copied from a
#                 Linux box). Without it the script builds them with zig + cargo-zigbuild when both
#                 are installed; otherwise the app can only use VMs that already have unlatchd.
# Environment: UNLATCH_TEAM_ID / UNLATCH_BUNDLE_PREFIX override what goes into
# mac/Signing.local.xcconfig; XCODEGEN names the xcodegen binary; UNLATCHD_PREBUILT_DIR = --prebuilt.
#
# Runs with the bash 3.2 that ships with macOS. Stops at the first problem and prints the fix.
set -euo pipefail

MAC_DIR="$(cd "$(dirname "$0")/.." && pwd)"
REPO_DIR="$(cd "$MAC_DIR/.." && pwd)"
BUILD_DIR="$MAC_DIR/build"
DERIVED="$BUILD_DIR/dd"
APP_DEST="/Applications/Unlatch.app"
MIN_RUST_MINOR=85 # 1.85: edition-2024 dependencies (Cargo.toml rust-version)

check_only=0; unsigned=0; install=1; open_app=1
prebuilt="${UNLATCHD_PREBUILT_DIR:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    --check) check_only=1 ;;
    --unsigned) unsigned=1 ;;
    --no-install) install=0 ;;
    --no-open) open_app=0 ;;
    --prebuilt) shift; prebuilt="${1:-}"; [ -n "$prebuilt" ] || { echo "--prebuilt needs a directory" >&2; exit 2; } ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
  esac
  shift
done
if [ "$unsigned" = 1 ] && [ "$install" = 1 ]; then
  echo "--unsigned builds cannot run the Finder integration; add --no-install" >&2; exit 2
fi

step() { printf '\n== %s\n' "$*"; }
ok() { printf '   ok: %s\n' "$*"; }
fail() {
  # fail <what went wrong> <fix line>...
  printf '\n   PROBLEM: %s\n' "$1" >&2
  shift
  if [ $# -gt 0 ]; then
    printf '   FIX:\n' >&2
    for l in "$@"; do printf '     %s\n' "$l" >&2; done
  fi
  exit 1
}

# ---- prerequisites ---------------------------------------------------------------------------
step "Checking prerequisites"
[ "$(uname -s)" = Darwin ] || fail "this script builds the Mac app; run it on a Mac"

dev_dir="$(xcode-select -p 2>/dev/null || true)"
case "$dev_dir" in
  ""|*CommandLineTools*)
    if [ -d /Applications/Xcode.app ]; then
      fail "Xcode is installed but not selected (active developer directory: ${dev_dir:-none})" \
        "sudo xcode-select -s /Applications/Xcode.app/Contents/Developer" \
        "sudo xcodebuild -license accept" \
        "sudo xcodebuild -runFirstLaunch"
    fi
    fail "Xcode is not installed (only the Command Line Tools are). The Finder extension needs full Xcode 16 or later" \
      "open \"macappstore://apps.apple.com/app/xcode/id497799835\"   # install, open Xcode once, then:" \
      "sudo xcode-select -s /Applications/Xcode.app/Contents/Developer" \
      "sudo xcodebuild -license accept" \
      "sudo xcodebuild -runFirstLaunch"
    ;;
esac
if ! xcode_version="$(xcodebuild -version 2>&1)"; then
  fail "xcodebuild does not run: $(printf '%s' "$xcode_version" | head -3 | tr '\n' ' ')" \
    "sudo xcodebuild -license accept" \
    "sudo xcodebuild -runFirstLaunch"
fi
xcode_major="$(printf '%s\n' "$xcode_version" | sed -n 's/^Xcode \([0-9][0-9]*\).*/\1/p' | head -1)"
if [ -n "$xcode_major" ] && [ "$xcode_major" -lt 16 ]; then
  fail "$(printf '%s' "$xcode_version" | head -1) is too old; Unlatch needs Xcode 16 or later" \
    "open \"macappstore://apps.apple.com/app/xcode/id497799835\""
fi
xcodebuild -checkFirstLaunchStatus >/dev/null 2>&1 ||
  fail "Xcode has not finished its first-launch setup" "sudo xcodebuild -runFirstLaunch"
ok "$(printf '%s' "$xcode_version" | head -1) at $dev_dir"

if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
command -v cargo >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1 ||
  fail "Rust (rustup) is not installed" \
    "curl -sSf https://sh.rustup.rs | sh -s -- -y && . \"\$HOME/.cargo/env\""
rust_minor="$(rustc --version 2>/dev/null | sed -n 's/^rustc 1\.\([0-9][0-9]*\).*/\1/p')"
if [ -z "$rust_minor" ] || [ "$rust_minor" -lt "$MIN_RUST_MINOR" ]; then
  fail "Rust 1.$MIN_RUST_MINOR or later is needed (found: $(rustc --version 2>/dev/null || echo none))" \
    "rustup update stable && rustup default stable"
fi
ok "$(rustc --version)"

xcodegen="${XCODEGEN:-$(command -v xcodegen || true)}"
[ -n "$xcodegen" ] && [ -x "$xcodegen" ] || fail "XcodeGen is not installed" "brew install xcodegen"
ok "xcodegen $("$xcodegen" --version 2>/dev/null | sed 's/^Version: //')"

# ---- signing team ----------------------------------------------------------------------------
local_cfg="$MAC_DIR/Signing.local.xcconfig"
cfg_value() { # cfg_value KEY: the value set in Signing.local.xcconfig, if any
  [ -f "$local_cfg" ] || return 0
  sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*\([^[:space:]]*\).*/\1/p" "$local_cfg" | tail -1
}
valid_team() { printf '%s' "$1" | grep -Eq '^[A-Z0-9]{10}$' && [ "$1" != XXXXXXXXXX ]; }

if [ "$unsigned" = 1 ]; then
  ok "unsigned build: skipping the signing team"
else
  step "Finding your signing team"
  # Teams of valid (unexpired, private key present) Apple Development identities.
  identities="$(security find-identity -v -p codesigning 2>/dev/null |
    sed -n 's/.*"\(Apple Development: [^"]*\)".*/\1/p' || true)"
  teams=""
  if [ -n "$identities" ]; then
    while IFS= read -r ident; do
      [ -n "$ident" ] || continue
      t="$(security find-certificate -c "$ident" -p 2>/dev/null |
        openssl x509 -noout -subject 2>/dev/null |
        sed -n 's/.*OU *= *\([A-Z0-9]\{10\}\).*/\1/p' | head -1 || true)"
      if [ -n "$t" ] && ! printf '%s\n' "$teams" | grep -qx "$t"; then
        teams="${teams:+$teams
}$t"
      fi
    done <<EOF
$identities
EOF
  fi
  [ -n "$teams" ] || fail "no valid \"Apple Development\" signing certificate in your keychain" \
    "Open Xcode > Settings > Accounts, sign in with your Apple ID, select your team," \
    "click Manage Certificates..., then + > Apple Development. Run this script again."

  team="${UNLATCH_TEAM_ID:-}"
  [ -n "$team" ] || team="$(cfg_value UNLATCH_TEAM_ID)"
  if valid_team "$team" && printf '%s\n' "$teams" | grep -qx "$team"; then
    ok "team $team (from ${UNLATCH_TEAM_ID:+UNLATCH_TEAM_ID}${UNLATCH_TEAM_ID:-mac/Signing.local.xcconfig})"
  else
    if valid_team "$team"; then
      echo "   note: team $team has no valid Apple Development certificate here; using one that does"
    fi
    team="$(printf '%s\n' "$teams" | head -1)"
    ok "team $team"
    if [ "$(printf '%s\n' "$teams" | wc -l | tr -d ' ')" -gt 1 ]; then
      echo "   note: certificates for several teams: $(printf '%s' "$teams" | tr '\n' ' ')"
      echo "         set UNLATCH_TEAM_ID=<team> to choose another one"
    fi
  fi

  prefix="${UNLATCH_BUNDLE_PREFIX:-}"
  [ -n "$prefix" ] || prefix="$(cfg_value UNLATCH_BUNDLE_PREFIX)"
  if [ -z "$prefix" ] || [ "$prefix" = dev.unlatch.example ]; then
    prefix="local.$(id -un | tr -cd 'a-z0-9')"
  fi
  printf '%s' "$prefix" | grep -Eq '^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+$' ||
    fail "UNLATCH_BUNDLE_PREFIX \"$prefix\" is not a reverse-DNS prefix like com.example"
  if [ "$check_only" = 0 ]; then
    printf 'UNLATCH_TEAM_ID = %s\nUNLATCH_BUNDLE_PREFIX = %s\n' "$team" "$prefix" > "$local_cfg"
    ok "wrote mac/Signing.local.xcconfig (team $team, bundle prefix $prefix)"
  else
    ok "would use team $team and bundle prefix $prefix"
  fi
fi

# ---- unlatchd for the VM ---------------------------------------------------------------------
step "Linux daemon (unlatchd) to bundle"
if [ -n "$prebuilt" ]; then
  for arch in x86_64 aarch64; do
    [ -f "$prebuilt/unlatchd-$arch" ] || fail "missing $prebuilt/unlatchd-$arch" \
      "copy both static binaries there, named unlatchd-x86_64 and unlatchd-aarch64"
  done
  prebuilt="$(cd "$prebuilt" && pwd)"
  ok "using $prebuilt"
elif command -v zig >/dev/null 2>&1 && command -v cargo-zigbuild >/dev/null 2>&1; then
  ok "building with zig + cargo-zigbuild"
else
  echo "   note: no --prebuilt directory and no zig + cargo-zigbuild: the app will not carry unlatchd"
  echo "         and can only use VMs where unlatchd is already on the PATH of a non-interactive ssh."
  echo "         To bundle it: brew install zig && cargo install cargo-zigbuild, or pass --prebuilt DIR."
fi

if [ "$check_only" = 1 ]; then
  step "Prerequisites look fine (--check: nothing built)"
  exit 0
fi

# ---- build -----------------------------------------------------------------------------------
mkdir -p "$BUILD_DIR"
step "Building libunlatch.a$( [ -n "$prebuilt" ] && echo " and staging unlatchd" )"
rust_log="$BUILD_DIR/dev-install-rust.log"
if [ -n "$prebuilt" ]; then
  UNLATCHD_PREBUILT_DIR="$prebuilt" UNLATCH_REQUIRE_UNLATCHD=1 "$MAC_DIR/scripts/build-rust.sh" > "$rust_log" 2>&1 ||
    { tail -25 "$rust_log" >&2; fail "build-rust.sh failed (full log: $rust_log)"; }
else
  "$MAC_DIR/scripts/build-rust.sh" > "$rust_log" 2>&1 ||
    { tail -25 "$rust_log" >&2; fail "build-rust.sh failed (full log: $rust_log)"; }
fi
grep -E '^(warning: build-rust|build-rust: libunlatch.a ready|build-rust: unlatchd)' "$rust_log" | sed 's/^/   /' || true

step "Generating the Xcode project"
(cd "$MAC_DIR" && "$xcodegen" generate) > "$BUILD_DIR/xcodegen.log" 2>&1 ||
  { tail -20 "$BUILD_DIR/xcodegen.log" >&2; fail "xcodegen generate failed (log: $BUILD_DIR/xcodegen.log)"; }
ok "mac/Unlatch.xcodeproj"

step "Building Unlatch.app ($( [ "$unsigned" = 1 ] && echo unsigned || echo "signed, team $team" ); a few minutes)"
xc_log="$BUILD_DIR/xcodebuild.log"
sign_args=""
[ "$unsigned" = 1 ] && sign_args="CODE_SIGNING_ALLOWED=NO"
# With a prebuilt daemon, a bundle without it is an error, not a warning.
[ -n "$prebuilt" ] && sign_args="$sign_args UNLATCH_REQUIRE_UNLATCHD=1"
# UNLATCH_SKIP_RUST: the Rust step above already built libunlatch.a.
# shellcheck disable=SC2086
if ! xcodebuild -project "$MAC_DIR/Unlatch.xcodeproj" -scheme Unlatch -configuration Debug \
  -derivedDataPath "$DERIVED" -destination 'generic/platform=macOS' \
  UNLATCH_SKIP_RUST=1 $sign_args build > "$xc_log" 2>&1; then
  echo "   errors (deduplicated):" >&2
  grep -E "error: " "$xc_log" | sed "s|$REPO_DIR/||" | sort -u | head -40 | sed 's/^/     /' >&2 || true
  if grep -q "errSecInternalComponent" "$xc_log"; then
    fail "codesign could not use your signing key (full log: $xc_log)" \
      "Run this script at the Mac's own screen and click \"Always Allow\" when the keychain asks."
  fi
  fail "xcodebuild failed (full log: $xc_log)" "Paste the errors above into the conversation."
fi
if grep -q "nearly matches optional requirement" "$xc_log"; then
  grep "nearly matches optional requirement" "$xc_log" | sort -u | head -5 >&2
  fail "a File Provider method no longer matches this Xcode's SDK, so the system would never call it (log: $xc_log)" \
    "Paste the lines above into the conversation."
fi
built="$DERIVED/Build/Products/Debug/Unlatch.app"
[ -x "$built/Contents/MacOS/Unlatch" ] || fail "xcodebuild succeeded but $built has no executable (log: $xc_log)"
ok "$built"

if [ "$install" = 0 ]; then
  step "Built (not installed: --no-install)"
  exit 0
fi

# ---- install ---------------------------------------------------------------------------------
step "Installing $APP_DEST"
replacing=0
if [ -e "$APP_DEST" ]; then
  replacing=1
  osascript -e 'tell application id "'"$prefix"'.unlatch" to quit' >/dev/null 2>&1 || true
  rm -rf "$APP_DEST" 2>/dev/null || fail "cannot remove the old $APP_DEST" "sudo rm -rf \"$APP_DEST\"   # then run this script again"
fi
ditto "$built" "$APP_DEST" 2>/dev/null || fail "cannot write to /Applications (is this an admin account?)" \
  "sudo ditto \"$built\" \"$APP_DEST\""
ok "installed"
if [ "$replacing" = 1 ]; then
  # A replaced bundle leaves the old agent registration dead (docs/MACOS.md §4): re-register it.
  "$APP_DEST/Contents/MacOS/Unlatch" --cli repair-agent --json >/dev/null 2>&1 &&
    ok "background agent re-registered" ||
    echo "   note: if the menu says the agent is not responding, choose \"Repair Background Agent\""
fi

if [ "$open_app" = 1 ]; then
  open "$APP_DEST"
  ok "opened Unlatch (menu bar). If macOS asks, allow it under System Settings > General > Login Items & Extensions."
fi

cat <<EOF

Next: add a VM folder (use what you type after "ssh" as the host):
  "$APP_DEST/Contents/MacOS/Unlatch" --cli add --name my-vm --host you@your-vm --root /home/you/code --json
It then shows up in Finder under Locations.
EOF
