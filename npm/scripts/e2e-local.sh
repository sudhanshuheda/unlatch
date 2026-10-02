#!/usr/bin/env bash
# End-to-end test of `npx unlatch` on one Linux x86_64 box (VM and client are the same machine,
# connected over a real `ssh localhost`).
#
#   npm/scripts/e2e-local.sh            # uses target/release/{unlatchd,unlatch}; builds them if missing
#   E2E_TARBALLS=DIR npm/scripts/e2e-local.sh
#                                       # tests already packed release tarballs instead: DIR holds
#                                       # unlatch-<v>.tgz and unlatch-linux-x64-<v>.tgz (npm pack output)
#
# Needs: node >= 18, npm, cargo (unless the binaries exist), fusermount3 + /dev/fuse, and
# `ssh localhost true` working non-interactively. Never touches ~/.unlatch: the daemon goes into a
# temp UNLATCH_HOME (VM side) and a temp --remote-home (client side), the mount state into a temp
# --state.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET="${CARGO_TARGET_DIR:-$REPO/target}"
T="$(mktemp -d /tmp/unlatch-e2e.XXXXXX)"
PASS=0
FAIL=0
HOME_UNLATCH_BEFORE=$(stat -c '%w %y' "$HOME/.unlatch" 2>/dev/null || echo absent)

ok() { echo "  ok   $*"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL $*"; FAIL=$((FAIL + 1)); }
check() { local what=$1; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }

cleanup() {
  set +e
  grep -q " $T/mnt " /proc/self/mounts && fusermount3 -u "$T/mnt"
  grep -q " $T/mnt2 " /proc/self/mounts && fusermount3 -u "$T/mnt2"
  for b in "$T"/vmhome/unlatchd-*; do [ -x "$b" ] && UNLATCH_HOME="$T/vmhome" "$b" stop >/dev/null; done
  for b in "$T"/npxhome/unlatchd-*; do [ -x "$b" ] && UNLATCH_HOME="$T/npxhome" "$b" stop >/dev/null; done
  [ -n "${KEEP:-}" ] || rm -rf "$T"
}
trap cleanup EXIT

if [ -n "${E2E_TARBALLS:-}" ]; then
  echo "== prebuilt tarballs from $E2E_TARBALLS"
  mkdir -p "$T/tgz"
  cp "$E2E_TARBALLS"/unlatch-[0-9]*.tgz "$E2E_TARBALLS"/unlatch-linux-x64-*.tgz "$T/tgz/"
else
  echo "== build"
  if [ ! -x "$TARGET/release/unlatchd" ] || [ ! -x "$TARGET/release/unlatch" ]; then
    (cd "$REPO" && cargo build --release -p unlatchd -p unlatch-cli)
  fi
  "$TARGET/release/unlatchd" --version

  echo "== assemble + npm pack"
  node "$REPO/npm/scripts/assemble.mjs" main --out "$T/pkg/unlatch" >/dev/null
  node "$REPO/npm/scripts/assemble.mjs" platform --key linux-x64 --out "$T/pkg/linux-x64" \
    --unlatchd "$TARGET/release/unlatchd" --unlatch "$TARGET/release/unlatch" >/dev/null
  mkdir -p "$T/tgz"
  (cd "$T/tgz" && npm pack --silent "$T/pkg/unlatch" "$T/pkg/linux-x64" >/dev/null)
fi
ls -la "$T/tgz"
MAIN_TGZ=$(ls "$T"/tgz/unlatch-[0-9]*.tgz)
PLAT_TGZ=$(ls "$T"/tgz/unlatch-linux-x64-*.tgz)
# The daemon the installed package must put on the VM, byte for byte: the one in the tarball.
mkdir -p "$T/unpacked"
tar -xzf "$PLAT_TGZ" -C "$T/unpacked"
tar -xzf "$MAIN_TGZ" -C "$T/unpacked" package/package.json --transform 's,^package/,main/,'
DAEMON_SRC="$T/unpacked/package/bin/unlatchd"
"$DAEMON_SRC" --version
check "unlatch tarball has no runtime dependencies" \
  node -e 'const p=require(process.argv[1]+"/unpacked/main/package.json");process.exit(p.dependencies?1:0)' "$T"

echo "== install into a temp prefix"
npm install --global --prefix "$T/prefix" --offline --no-audit --no-fund "$MAIN_TGZ" "$PLAT_TGZ" >/dev/null 2>"$T/npm-install.log" || { cat "$T/npm-install.log"; exit 1; }
export PATH="$T/prefix/bin:$PATH"
unlatch version
check "platform package resolved" sh -c 'unlatch version | grep -q "linux-x64"'

echo "== (1) VM mode: unlatch share <dir> --json"
mkdir -p "$T/root/src/pkg" "$T/root/node_modules/dep" "$T/mnt"
echo "hello from the VM" > "$T/root/README.md"
for i in $(seq 1 200); do echo "$i" > "$T/root/src/pkg/f$i.txt"; done
UNLATCH_HOME="$T/vmhome" unlatch share "$T/root" --json > "$T/share.json"
cat "$T/share.json"
check "share JSON has the agent contract fields" node -e '
  const r = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
  const need = ["mac_command", "host_candidates", "path", "daemon_version", "warnings"];
  for (const k of need) if (!(k in r)) throw new Error("missing " + k);
  if (!r.ok || !/^npx unlatch connect \S+@\S+:\S+/.test(r.mac_command)) throw new Error(r.mac_command);
  if (!Array.isArray(r.host_candidates) || !r.host_candidates.length) throw new Error("no hosts");
  if (!r.daemon.server || !r.daemon.server.running) throw new Error("server not running");
' "$T/share.json"
DAEMON=$(node -e 'console.log(JSON.parse(require("fs").readFileSync(process.argv[1])).daemon.path)' "$T/share.json")
check "daemon installed in the temp UNLATCH_HOME" test -x "$DAEMON" -a "$(dirname "$DAEMON")" = "$T/vmhome"
check "daemon file name is unlatchd-<version>-<sha16>" sh -c "basename '$DAEMON' | grep -Eq '^unlatchd-[0-9.]+-[0-9a-f]{16}$'"
check "installed daemon is byte-identical to the package's" cmp -s "$DAEMON" "$DAEMON_SRC"
check "installed daemon sha256 matches the platform manifest" node -e '
  const m = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
  const h = require("crypto").createHash("sha256").update(require("fs").readFileSync(process.argv[2])).digest("hex");
  if (h !== m.daemon.sha256 || !process.argv[2].endsWith(m.daemon.remote_name)) process.exit(1);' "$T/unpacked/package/manifest.json" "$DAEMON"
UNLATCH_HOME="$T/vmhome" unlatch share "$T/root" --json > "$T/share2.json"
check "second share is a no-op (installed_now=false, server reused)" node -e '
  const r = JSON.parse(require("fs").readFileSync(process.argv[1]));
  if (r.daemon.installed_now || r.daemon.server.started) process.exit(1);' "$T/share2.json"

echo "== (1b) the way an agent runs it: npm exec (= npx -y) unlatch share --json"
UNLATCH_HOME="$T/npxhome" npm exec --yes --offline --package="$MAIN_TGZ" --package="$PLAT_TGZ" -- unlatch share "$T/root" --json --no-serve > "$T/npx.json" 2>"$T/npx.err" || cat "$T/npx.err"
check "npx-style run prints valid JSON with mac_command" node -e '
  const r = JSON.parse(require("fs").readFileSync(process.argv[1])); if (!r.ok || !r.mac_command) process.exit(1);' "$T/npx.json"

echo "== (2) Linux client: unlatch connect localhost:<dir> --mount <mnt> over real ssh"
ssh -o BatchMode=yes localhost true || { echo "ssh localhost does not work non-interactively"; exit 1; }
t0=$(date +%s%N)
unlatch connect "localhost:$T/root" --mount "$T/mnt" --state "$T/client" --remote-home "$T/vmhome" --json > "$T/connect.json"
t1=$(date +%s%N)
cat "$T/connect.json"
echo "  connect+mount took $(( (t1 - t0) / 1000000 )) ms"
check "mounted (fuse.unlatch in /proc/self/mounts)" grep -q " $T/mnt fuse" /proc/self/mounts
check "files visible through the mount" test "$(cat "$T/mnt/README.md")" = "hello from the VM"
check "200 files listed" test "$(ls "$T/mnt/src/pkg" | wc -l)" -eq 200
check "lazy dir listed (node_modules)" test -d "$T/mnt/node_modules"
check "client attached to the server share pre-started (one state dir, same pid)" sh -c "
  n=\$(UNLATCH_HOME='$T/vmhome' '$DAEMON' status | grep -c 'running=yes'); [ \"\$n\" -eq 1 ]"
check "no daemon upload needed (share already installed it)" node -e '
  const r = JSON.parse(require("fs").readFileSync(process.argv[1])); if (r.daemon_uploaded) process.exit(1);' "$T/connect.json"

echo "vm-side write $(date +%s%N)" > "$T/root/written-on-vm.txt"
t0=$(date +%s%N)
seen=""
for _ in $(seq 1 400); do
  if [ -f "$T/mnt/written-on-vm.txt" ]; then seen=1; break; fi
  sleep 0.005
done
t1=$(date +%s%N)
check "VM-side write appears in the mount ($(( (t1 - t0) / 1000000 )) ms)" test -n "$seen"
check "VM-side content correct" grep -q "vm-side write" "$T/mnt/written-on-vm.txt"

echo "saved from the client" > "$T/mnt/written-on-client.txt"
sync "$T/mnt/written-on-client.txt" 2>/dev/null || true
for _ in $(seq 1 200); do [ -f "$T/root/written-on-client.txt" ] && break; sleep 0.01; done
check "client write lands on the VM" test "$(cat "$T/root/written-on-client.txt" 2>/dev/null)" = "saved from the client"

rm "$T/root/src/pkg/f1.txt"
for _ in $(seq 1 200); do [ ! -e "$T/mnt/src/pkg/f1.txt" ] && break; sleep 0.01; done
check "VM-side delete disappears from the mount" test ! -e "$T/mnt/src/pkg/f1.txt"

unlatch status --json > "$T/status.json"
check "status lists the mount" grep -q "$T/mnt" "$T/status.json"

echo "== unmount"
unlatch remove --mount "$T/mnt"
check "unmounted cleanly" sh -c "! grep -q ' $T/mnt ' /proc/self/mounts"

echo "== (2b) Linux client against a VM without the daemon: upload over ssh"
mkdir -p "$T/mnt2"
unlatch connect "localhost:$T/root" --mount "$T/mnt2" --state "$T/client2" --remote-home "$T/fresh" --json > "$T/connect2.json"
check "daemon uploaded over ssh" node -e '
  const r = JSON.parse(require("fs").readFileSync(process.argv[1])); if (!r.daemon_uploaded) process.exit(1);' "$T/connect2.json"
check "uploaded daemon is byte-identical" sh -c "cmp -s '$T'/fresh/unlatchd-* '$DAEMON_SRC'"
check "files visible through the second mount" test "$(cat "$T/mnt2/README.md")" = "hello from the VM"
unlatch remove --mount "$T/mnt2" >/dev/null
for b in "$T"/fresh/unlatchd-*; do UNLATCH_HOME="$T/fresh" "$b" stop >/dev/null; done

echo "== uninstall (VM side)"
UNLATCH_HOME="$T/vmhome" unlatch uninstall --yes --json > "$T/uninstall.json"
check "daemon and state removed" test -z "$(ls -A "$T/vmhome")"

check "real ~/.unlatch untouched" test "$(stat -c '%w %y' "$HOME/.unlatch" 2>/dev/null || echo absent)" = "$HOME_UNLATCH_BEFORE"

echo
echo "e2e: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
