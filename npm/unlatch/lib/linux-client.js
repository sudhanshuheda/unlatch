'use strict';
// Linux desktop client: `unlatch connect <target>:<path> --mount <dir>` → FUSE mount through
// `unlatch mount`. Also how the whole round trip is tested on a Linux box.

const path = require('node:path');
const names = require('./names');
const platform = require('./platform');
const daemon = require('./daemon');
const ssh = require('./ssh');
const { shQuote } = require('./sys');
const { UsageError, UserActionError } = require('./ui');

function expandHome(sys, p) {
  if (p === '~') return sys.home;
  if (p.startsWith('~/')) return path.join(sys.home, p.slice(2));
  return path.resolve(sys.cwd, p);
}

/** Mounted `unlatch` filesystems from /proc/self/mounts: [{ source, mountpoint }]. */
function listMounts(sys) {
  let text = '';
  try {
    text = sys.fs.readFileSync('/proc/self/mounts', 'utf8');
  } catch {
    return [];
  }
  return text
    .split('\n')
    .map((l) => l.split(' '))
    .filter((f) => f.length > 2 && f[0].startsWith(names.FUSE_FSNAME_PREFIX) && f[2].startsWith('fuse'))
    .map((f) => ({ source: unescapeMount(f[0]), mountpoint: unescapeMount(f[1]) }));
}

function unescapeMount(s) {
  return s.replace(/\\([0-7]{3})/g, (_, o) => String.fromCharCode(parseInt(o, 8)));
}

function connectMount(sys, ui, opts) {
  const t = opts.target;
  if (!sys.which('fusermount3') && !sys.which('fusermount')) {
    throw new UserActionError('FUSE is not installed. Install it (e.g. sudo apt install fuse3) and try again.');
  }
  const plat = platform.resolvePlatform(sys);
  const cli = platform.cliOf(plat);
  const d = platform.daemonOf(plat);
  const mnt = expandHome(sys, opts.mount);

  ui.step(`checking ssh ${ssh.destination(t)}`);
  const test = ssh.testSsh(sys, t, { identity: opts.identity });
  if (!test.ok) throw new UserActionError(`${test.message}\n  Fix: ${test.fix}`);
  ui.ok(`ssh works (${test.uname || 'remote'})`);

  ui.step('making sure the VM has the daemon');
  const run = ssh.sshRunner(sys, t, { identity: opts.identity });
  const rem = daemon.ensureRemote(sys, run, d, { override: opts.remoteHome, root: t.path });
  ui.ok(`daemon ${rem.uploaded ? 'uploaded to' : 'found at'} ${rem.path}`);

  sys.fs.mkdirSync(mnt, { recursive: true });
  const args = ['mount', mnt, '--host', ssh.destination(t), '--root', t.path, '--unlatchd', shQuote(rem.path)];
  if (t.port) args.push('--port', String(t.port));
  if (opts.identity) args.push('--identity', opts.identity);
  if (opts.state) args.push('--state', expandHome(sys, opts.state));
  if (opts.name) args.push('--name', opts.name);
  if (opts.foreground) args.push('--foreground');
  ui.step(`mounting on ${mnt}`);
  const code = opts.foreground || !ui.json ? sys.runInherit(cli, args) : sys.run(cli, args).status;
  if (code !== 0) throw new Error(`${names.CLIENT_BIN} mount exited with ${code}`);
  const result = {
    ok: true,
    mode: 'fuse',
    mountpoint: mnt,
    target: ssh.formatTarget(t),
    daemon_path: rem.path,
    daemon_uploaded: rem.uploaded,
    unmount_command: names.npx(`remove --mount ${shQuote(mnt)}`),
  };
  if (ui.json) ui.emit(result);
  else {
    ui.print('');
    ui.print(`${ui.bold(ssh.formatTarget(t))} is mounted at ${ui.bold(mnt)}`);
    ui.print(ui.dim(`Unmount: ${result.unmount_command}   (or fusermount3 -u ${shQuote(mnt)})`));
  }
  return result;
}

function unmount(sys, ui, mount) {
  const mnt = expandHome(sys, mount);
  const tool = sys.which('fusermount3') ? 'fusermount3' : 'fusermount';
  const r = sys.run(tool, ['-u', mnt]);
  if (r.status !== 0) throw new Error(`${tool} -u ${mnt}: ${String(r.stderr).trim()}`);
  if (ui.json) ui.emit({ ok: true, unmounted: mnt });
  else ui.ok(`unmounted ${mnt}`);
}

function requireMountFlag(opts) {
  if (!opts.mount) {
    throw new UsageError(
      `On Linux, \`connect\` mounts the VM folder with FUSE: add --mount <dir>, e.g.\n  ${names.npx(`connect ${ssh.formatTarget(opts.target)} --mount ~/${names.CLI}`)}`
    );
  }
}

module.exports = { connectMount, unmount, listMounts, requireMountFlag, expandHome };
