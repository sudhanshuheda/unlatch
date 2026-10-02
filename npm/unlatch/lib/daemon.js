'use strict';
// Installing `unlatchd` into the VM install directory, locally (VM mode) or over ssh (Linux client
// mode), and pre-starting its per-root server.
//
// The directory probe below is a copy of the one the Mac-side bootstrap runs
// (crates/unlatch-core/src/transport/bootstrap.rs, review D22): same candidates, same order, same
// "usable" rules. That is what makes `npx unlatch share` on the VM and the Mac app agree on the
// directory, so the Mac finds `unlatchd-<version>-<sha16>` already there and never uploads it, and
// both end up talking to the same `unlatchd serve`. test/names.test.js fails if the two drift.

const crypto = require('node:crypto');
const path = require('node:path');
const names = require('./names');
const { shQuote } = require('./sys');

class DaemonError extends Error {}

/** The candidate list, as sh words (the Rust bootstrap has the same list after $H_OVERRIDE). */
function candidateWords() {
  const e = names.INSTALL_ENV;
  return [
    '"$H_OVERRIDE"',
    `"\${${e}:-}"`,
    `"\${XDG_DATA_HOME:+$XDG_DATA_HOME/${names.INSTALL_XDG_SUBDIR}}"`,
    `"\${HOME:+$HOME/${names.INSTALL_HOME_DIR}}"`,
    `"/var/tmp/${names.INSTALL_TMP_PREFIX}$uid"`,
    `"/tmp/${names.INSTALL_TMP_PREFIX}$uid"`,
  ];
}

/**
 * POSIX sh that picks the install directory and reports what is in it. Output lines are
 * `<nonce> <key> <value>` so a noisy .bashrc/motd can never be mistaken for a result.
 */
function probeScript({ nonce, override, want, prefix, root, readonly = false }) {
  return `{
N=${shQuote(nonce)}
H_OVERRIDE=${shQuote(override || '')}
WANT=${shQuote(want || '')}
PREFIX=${shQuote(prefix)}
RO=${readonly ? 1 : 0}
ROOT=${root ? rootExpr(root) : "''"}
m() { printf '%s %s\\n' "$N" "$*"; }
umask 077
arch=$(uname -m 2>/dev/null) || arch=unknown
case "$arch" in amd64|x86_64) arch=x86_64 ;; arm64|aarch64) arch=aarch64 ;; esac
m arch "$arch"
hash_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d ' ' -f 1
  elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | sed 's/^.*= *//'
  else echo none; fi
}
remote_fs() {
  t=$(stat -f -c %T "$1" 2>/dev/null) || return 1
  case "$t" in nfs*|cifs|smb*|fuse*|9p|v9fs|afs|virtiofs|ceph|glusterfs|lustre|gpfs) return 0 ;; esac
  return 1
}
usable() {
  d=$1
  [ -n "$d" ] || return 1
  case "$d" in /*) ;; *) return 1 ;; esac
  # Read-only probes (status/doctor/uninstall) never create or chmod anything.
  if [ "$RO" = 1 ]; then [ -d "$d" ] || return 1; fi
  if [ ! -e "$d" ] && [ ! -L "$d" ]; then mkdir -p "$d" 2>/dev/null || return 1; fi
  [ -d "$d" ] && [ ! -L "$d" ] && [ -O "$d" ] || return 1
  [ "$RO" = 1 ] || chmod 700 "$d" 2>/dev/null || return 1
  case "$(ls -ld "$d" 2>/dev/null)" in 'drwx------ '*|'drwx------.'*) ;; *) return 1 ;; esac
  if remote_fs "$d"; then return 1; fi
  [ "$RO" = 1 ] && return 0
  t="$d/.unlatch-exec-test.$$"
  printf '#!/bin/sh\\nexit 0\\n' > "$t" 2>/dev/null && chmod 700 "$t" 2>/dev/null && "$t" 2>/dev/null
  r=$?
  rm -f "$t"
  return $r
}
uid=$(id -u 2>/dev/null) || uid=unknown
dir=
for c in ${candidateWords().join(' ')}; do
  if usable "$c"; then dir=$c; break; fi
done
if [ -z "$dir" ]; then
  m error "no usable install directory (needs a local, exec-capable directory owned by you, mode 0700)"
else
  m dir "$dir"
  [ "$(hash_of /dev/null)" != none ] && m hashtool yes || m hashtool no
  if [ -n "$WANT" ] && [ -f "$dir/$WANT" ] && [ ! -L "$dir/$WANT" ]; then m have "$(hash_of "$dir/$WANT")"; fi
  for f in "$dir"/"$PREFIX"*; do
    case "$f" in *.tmp) continue ;; esac
    [ -f "$f" ] && [ ! -L "$f" ] && m found "\${f##*/}"
  done
fi
if [ -n "$ROOT" ]; then
  if [ -d "$ROOT" ]; then m root "$(cd "$ROOT" && pwd -P)"; else m noroot "$ROOT"; fi
fi
}
`;
}

/** Shell expression for a root on the VM: `~` and `~/…` via $HOME, like the bootstrap. */
function rootExpr(root) {
  if (root === '~') return '"$HOME"';
  if (root.startsWith('~/')) return `"$HOME"/${shQuote(root.slice(2))}`;
  return shQuote(root);
}

function parseProbe(stdout, nonce) {
  const r = { found: [] };
  for (const line of String(stdout).split('\n')) {
    if (!line.startsWith(nonce + ' ')) continue;
    const rest = line.slice(nonce.length + 1);
    const sp = rest.indexOf(' ');
    const key = sp < 0 ? rest : rest.slice(0, sp);
    const val = sp < 0 ? '' : rest.slice(sp + 1);
    if (key === 'found') r.found.push(val);
    else r[key] = val;
  }
  return r;
}

function nonce() {
  return 'UNLATCH-' + crypto.randomBytes(8).toString('hex');
}

function sha256File(sys, p) {
  return crypto.createHash('sha256').update(sys.fs.readFileSync(p)).digest('hex');
}

/** Run the probe locally (VM mode). */
function probeLocal(sys, { override, daemon, root, readonly }) {
  const n = nonce();
  const script = probeScript({
    nonce: n,
    override,
    want: daemon && daemon.remoteName,
    prefix: prefixFor(daemon),
    root,
    readonly,
  });
  const r = sys.run('sh', ['-s'], { input: script, timeout: 30000 });
  if (r.status !== 0 && !r.stdout) throw new DaemonError(`install-directory probe failed: ${r.stderr || r.status}`);
  const p = parseProbe(r.stdout, n);
  if (p.error) throw new DaemonError(p.error);
  if (!p.dir) throw new DaemonError(`install-directory probe printed nothing (${r.stderr.trim()})`);
  return p;
}

/**
 * Probe for status/doctor/uninstall: the directory and what is in it, or null. With `override`
 * (`--install-dir`), only that directory counts: if it is missing or unusable the answer is
 * null, never a fallback to a default directory (uninstall must not empty ~/.unlatch instead).
 */
function tryProbe(sys, d, override) {
  try {
    const p = probeLocal(sys, { daemon: d, readonly: true, override });
    if (override && p.dir !== path.resolve(sys.cwd || '/', override)) return null;
    return p;
  } catch {
    return null;
  }
}

function prefixFor(daemon) {
  return `${names.DAEMON_FILE_PREFIX}${daemon && daemon.version ? daemon.version + '-' : ''}`;
}

/**
 * VM mode: make sure `<dir>/unlatchd-<version>-<sha16>` exists and verifies. Returns
 * { dir, path, installed: bool, arch }.
 */
function installLocal(sys, daemon, { override } = {}) {
  if (!daemon) throw new DaemonError('this platform package carries no VM daemon');
  const probe = probeLocal(sys, { override, daemon });
  if (probe.arch !== daemon.arch) {
    throw new DaemonError(`this machine is ${probe.arch}, the packaged daemon is ${daemon.arch}`);
  }
  const dest = path.join(probe.dir, daemon.remoteName);
  if (probe.have === daemon.sha256) return { dir: probe.dir, path: dest, installed: false, arch: probe.arch };
  const got = sha256File(sys, daemon.path);
  if (got !== daemon.sha256) {
    throw new DaemonError(`packaged daemon ${daemon.path} fails its checksum (${got} != ${daemon.sha256})`);
  }
  const tmp = `${dest}.${process.pid}.tmp`;
  const fs = sys.fs;
  try {
    fs.copyFileSync(daemon.path, tmp);
    fs.chmodSync(tmp, 0o700);
    const fd = fs.openSync(tmp, 'r');
    try {
      fs.fsyncSync(fd);
    } finally {
      fs.closeSync(fd);
    }
    if (sha256File(sys, tmp) !== daemon.sha256) throw new DaemonError('copied daemon failed verification');
    fs.renameSync(tmp, dest);
  } catch (e) {
    try {
      fs.rmSync(tmp, { force: true });
    } catch {
      /* ignore */
    }
    throw e;
  }
  return { dir: probe.dir, path: dest, installed: true, arch: probe.arch };
}

/**
 * Linux client mode: resolve (and if needed upload) the daemon on the VM over ssh.
 * `ssh` is a function (remoteArgs, opts) → run result that runs `ssh … <target> remoteArgs…`.
 * Returns { dir, path, uploaded, arch, root }.
 */
function ensureRemote(sys, ssh, daemon, { override, root } = {}) {
  const n = nonce();
  const script = probeScript({
    nonce: n,
    override,
    want: daemon && daemon.remoteName,
    prefix: prefixFor(daemon),
    root,
  });
  const r = ssh(['sh', '-s'], { input: script, timeout: 60000 });
  const p = parseProbe(r.stdout, n);
  if (p.error) throw new DaemonError(`on the VM: ${p.error}`);
  if (!p.dir) throw new DaemonError(`could not probe the VM (ssh exit ${r.status}): ${String(r.stderr).trim()}`);
  if (root && p.noroot) throw new DaemonError(`the folder ${root} does not exist on the VM`);
  const base = { dir: p.dir, arch: p.arch, root: p.root };
  if (daemon && daemon.arch === p.arch) {
    const dest = `${p.dir}/${daemon.remoteName}`;
    if (p.have === daemon.sha256) return { ...base, path: dest, uploaded: false };
    if (p.hashtool !== 'yes') throw new DaemonError('the VM has no sha256 tool (sha256sum, shasum or openssl)');
    const tmp = `${dest}.${n}.tmp`;
    const bin = sys.fs.readFileSync(daemon.path);
    const up = ssh([`cat > ${shQuote(tmp)}`], { input: bin, encoding: null, timeout: 300000 });
    if (up.status !== 0) throw new DaemonError(`upload failed: ${String(up.stderr).trim()}`);
    const fin = ssh(['sh', '-s'], {
      input: `{
tmp=${shQuote(tmp)}; dest=${shQuote(dest)}; want=${shQuote(daemon.sha256)}
if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp" | cut -d ' ' -f 1)
elif command -v shasum >/dev/null 2>&1; then got=$(shasum -a 256 "$tmp" | cut -d ' ' -f 1)
else got=$(openssl dgst -sha256 "$tmp" | sed 's/^.*= *//'); fi
if [ "$got" != "$want" ]; then rm -f "$tmp"; echo "${n} error uploaded daemon failed verification ($got)"; exit 0; fi
chmod 700 "$tmp" && mv -f "$tmp" "$dest" && echo "${n} ok"
}
`,
      timeout: 60000,
    });
    const f = parseProbe(fin.stdout, n);
    if (f.error) throw new DaemonError(f.error);
    if (!('ok' in f)) throw new DaemonError(`install on the VM failed: ${String(fin.stderr).trim()}`);
    return { ...base, path: dest, uploaded: true };
  }
  // No packaged daemon for the VM's architecture: use one `npx unlatch share` put there.
  const same = p.found.filter((f) => !daemon || !daemon.version || f.startsWith(prefixFor(daemon)));
  if (same.length) return { ...base, path: `${p.dir}/${same.sort().pop()}`, uploaded: false };
  throw new DaemonError(
    `no ${names.PRODUCT} daemon for a ${p.arch} VM here. Run \`${names.npx('share')}\` on the VM first.`
  );
}

/** Parse `unlatchd status` lines: `<state>\troot=<r>\tpid=<p>\trunning=yes|no\tindex_bytes=<n>`. */
function parseStatus(stdout) {
  return String(stdout)
    .split('\n')
    .filter(Boolean)
    .map((line) => {
      const [state, ...kv] = line.split('\t');
      const o = { state };
      for (const f of kv) {
        const i = f.indexOf('=');
        if (i > 0) o[f.slice(0, i)] = f.slice(i + 1);
      }
      return {
        state: o.state,
        root: o.root,
        pid: o.pid && o.pid !== '-' ? Number(o.pid) : null,
        running: o.running === 'yes',
        indexBytes: Number(o.index_bytes || 0),
      };
    });
}

function daemonEnv(sys, dir) {
  return { ...sys.env, [names.INSTALL_ENV]: dir };
}

function status(sys, inst) {
  const r = sys.run(inst.path, ['status'], { env: daemonEnv(sys, inst.dir), timeout: 10000 });
  return parseStatus(r.stdout);
}

/**
 * Start (or find) the per-root `unlatchd serve` exactly as a Mac's `unlatchd connect` would: same
 * binary, same UNLATCH_HOME, same canonical root, default state dir. It daemonizes itself and
 * exits after 24 h without clients.
 */
function startServe(sys, inst, root, { waitMs = 8000 } = {}) {
  const canon = sys.fs.realpathSync(root);
  let st = status(sys, inst).find((s) => s.root === canon);
  if (st && st.running) return { ...st, started: false };
  const r = sys.run(inst.path, ['serve', '--root', canon], { env: daemonEnv(sys, inst.dir), timeout: 15000 });
  if (r.status !== 0) throw new DaemonError(`unlatchd serve failed: ${String(r.stderr).trim() || r.status}`);
  const t0 = sys.now();
  while (sys.now() - t0 < waitMs) {
    st = status(sys, inst).find((s) => s.root === canon);
    if (st && st.running) return { ...st, started: true };
    sys.sleep(100);
  }
  throw new DaemonError('unlatchd serve did not come up (see serve.log in its state directory)');
}

function version(sys, inst) {
  const r = sys.run(inst.path, ['--version'], { timeout: 10000 });
  const m = /^\S+\s+(\S+)/.exec(String(r.stdout).trim());
  return m ? m[1] : null;
}

module.exports = {
  DaemonError,
  candidateWords,
  probeScript,
  parseProbe,
  rootExpr,
  installLocal,
  probeLocal,
  tryProbe,
  ensureRemote,
  parseStatus,
  status,
  startServe,
  version,
  daemonEnv,
  sha256File,
};
