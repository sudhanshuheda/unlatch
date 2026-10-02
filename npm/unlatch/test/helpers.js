'use strict';
// Test doubles: a `sys` with scripted child processes and captured output, over a real temp dir.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { realSys } = require('../lib/sys');

function tmpdir(prefix = 'unlatch-test-') {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

/**
 * fakeSys({ platform, home, handlers, answers, env, installerPackage }) — `handlers` is a list of
 * [predicate(cmd, args, opts) , result | (cmd, args, opts) => result]; the first match answers.
 * Every call is recorded in `sys.calls` as { cmd, args, opts }.
 */
function fakeSys({ platform = 'darwin', arch = 'arm64', home, handlers = [], answers = [], env = {}, tty = false, installerPackage } = {}) {
  home = home || tmpdir('unlatch-home-');
  const out = [];
  const err = [];
  let clock = 0;
  const calls = [];
  const answer = (cmd, args, opts) => {
    calls.push({ cmd, args, opts });
    for (const [pred, res] of handlers) {
      if (pred(cmd, args, opts)) {
        const r = typeof res === 'function' ? res(cmd, args, opts) : res;
        return { status: 0, stdout: '', stderr: '', ...r };
      }
    }
    return { status: 127, stdout: '', stderr: `fake: no handler for ${cmd} ${args.join(' ')}` };
  };
  const sys = realSys({
    platform,
    arch,
    env: { PATH: '/usr/bin:/bin', HOME: home, ...env },
    home,
    cwd: home,
    username: 'me',
    hostname: 'laptop',
    stdinIsTTY: tty,
    stdoutIsTTY: false,
    paths: { applications: path.join(home, 'SystemApplications') },
    ...(installerPackage !== undefined ? { installerPackage } : {}),
    run: answer,
    runInherit: (cmd, args, opts) => answer(cmd, args, opts).status,
    which: (cmd) => (['ssh', 'fusermount3'].includes(cmd) ? `/usr/bin/${cmd}` : null),
    fetchSync: (reqs) => reqs.map(() => null), // no metadata service, and never the network
    sleep: (ms) => {
      clock += ms;
    },
    now: () => clock,
    out: (s) => out.push(s),
    err: (s) => err.push(s),
    ask: async (q) => {
      err.push(q);
      if (!answers.length) throw new Error(`unexpected question: ${q}`);
      return answers.shift();
    },
  });
  fs.mkdirSync(sys.paths.applications, { recursive: true });
  sys.calls = calls;
  sys.stdout = () => out.join('');
  sys.stderr = () => err.join('');
  return sys;
}

/** A platform package on disk. kind: 'darwin' | 'linux'. Returns its dir. */
function fakePlatform(kind, { daemonScript, appVersion = '0.2.0' } = {}) {
  const dir = tmpdir('unlatch-plat-');
  const manifest = { schema: 'unlatch-platform/1', version: '0.2.0' };
  if (kind === 'darwin') {
    manifest.platform = 'darwin-universal';
    fs.mkdirSync(path.join(dir, 'app'));
    fs.writeFileSync(path.join(dir, 'app', 'Unlatch.app.zip'), 'zip');
    manifest.app = { zip: 'app/Unlatch.app.zip', bundle: 'Unlatch.app', executable: 'Unlatch', version: appVersion };
    manifest.cli = { file: 'bin/unlatch' };
  } else {
    manifest.platform = 'linux-x64';
    fs.mkdirSync(path.join(dir, 'bin'));
    const p = path.join(dir, 'bin', 'unlatchd');
    fs.writeFileSync(p, daemonScript || '#!/bin/sh\necho unlatchd 0.1.0\n');
    fs.chmodSync(p, 0o755);
    const sha = crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');
    manifest.daemon = {
      file: 'bin/unlatchd',
      arch: 'x86_64',
      sha256: sha,
      size: fs.statSync(p).size,
      crate_version: '0.1.0',
      remote_name: `unlatchd-0.1.0-${sha.slice(0, 16)}`,
    };
    fs.writeFileSync(path.join(dir, 'bin', 'unlatch'), '#!/bin/sh\nexit 0\n');
    fs.chmodSync(path.join(dir, 'bin', 'unlatch'), 0o755);
    manifest.cli = { file: 'bin/unlatch' };
  }
  fs.writeFileSync(path.join(dir, 'manifest.json'), JSON.stringify(manifest));
  return dir;
}

/** A shell stand-in for unlatchd: --version, serve (records the root), status, stop. */
const FAKE_UNLATCHD = `#!/bin/sh
case "$1" in
  --version) echo "unlatchd 0.1.0 (proto 1)" ;;
  serve) d="$UNLATCH_HOME/state/fake"; mkdir -p "$d"; printf '%s' "$3" > "$d/root"; : > "$d/running" ;;
  status) for d in "$UNLATCH_HOME"/state/*; do [ -f "$d/root" ] || continue
            r=no; [ -f "$d/running" ] && r=yes
            printf '%s\\troot=%s\\tpid=4242\\trunning=%s\\tindex_bytes=0\\n' "$d" "$(cat "$d/root")" "$r"; done ;;
  stop) rm -f "$UNLATCH_HOME"/state/*/running ;;
esac
`;

module.exports = { tmpdir, fakeSys, fakePlatform, FAKE_UNLATCHD };
