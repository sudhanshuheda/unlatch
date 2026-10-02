'use strict';
// VM mode (share) and the daemon install paths, with a shell stand-in for unlatchd and the real
// POSIX probe script. Everything happens under a temp UNLATCH_HOME.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { realSys } = require('../lib/sys');
const { main } = require('../lib/cli');
const daemon = require('../lib/daemon');
const platform = require('../lib/platform');
const { tmpdir, fakePlatform, FAKE_UNLATCHD } = require('./helpers');

function vmSys({ env = {}, which, fetchSync } = {}) {
  const base = tmpdir('unlatch-vm-');
  const home = path.join(base, 'home');
  const root = path.join(home, 'proj');
  fs.mkdirSync(path.join(root, 'src', 'deep'), { recursive: true });
  fs.mkdirSync(path.join(root, 'node_modules', 'x', 'y'), { recursive: true });
  const plat = fakePlatform('linux', { daemonScript: FAKE_UNLATCHD });
  const out = [];
  const err = [];
  const real = realSys();
  const sys = realSys({
    platform: 'linux',
    arch: 'x64',
    env: {
      PATH: process.env.PATH,
      HOME: home,
      UNLATCH_HOME: path.join(base, 'install'),
      UNLATCH_PLATFORM_DIR: plat,
      SSH_CONNECTION: '198.51.100.20 50000 203.0.113.7 2222',
      ...env,
    },
    home,
    cwd: root,
    username: 'dev',
    which: which || ((c) => (c === 'tailscale' ? null : real.which(c))),
    fetchSync: fetchSync || (() => {
      throw new Error('tests must not reach a real metadata service');
    }),
    out: (s) => out.push(s),
    err: (s) => err.push(s),
  });
  return { sys, base, home, root, plat, stdout: () => out.join(''), stderr: () => err.join('') };
}

test('share --json: installs into UNLATCH_HOME, starts the server, prints the Mac command', async () => {
  const v = vmSys();
  const code = await main(['share', '--json'], v.sys);
  assert.equal(code, 0, v.stderr() + v.stdout());
  const r = JSON.parse(v.stdout());
  const man = JSON.parse(fs.readFileSync(path.join(v.plat, 'manifest.json'), 'utf8'));
  assert.equal(r.ok, true);
  assert.equal(r.path, fs.realpathSync(v.root));
  assert.equal(r.remote_path, '~/proj');
  assert.equal(r.daemon_version, '0.1.0');
  assert.equal(r.daemon.install_dir, path.join(v.base, 'install'));
  assert.equal(r.daemon.path, path.join(v.base, 'install', man.daemon.remote_name));
  assert.equal(r.daemon.installed_now, true);
  assert.equal(r.daemon.server.running, true);
  assert.equal(fs.statSync(r.daemon.path).mode & 0o777, 0o700);
  // SSH_CONNECTION's server address and non-default port.
  assert.equal(r.host_candidates[0].source, 'ssh-connection');
  assert.equal(r.mac_command, 'npx unlatch connect dev@203.0.113.7:~/proj --port 2222');
  assert.equal(r.port, 2222);
  assert.ok(Array.isArray(r.warnings));
  for (const k of ['mac_command', 'host_candidates', 'path', 'daemon_version', 'warnings']) assert.ok(k in r, k);

  // Second run: nothing to install, server already up.
  const v2 = { ...v };
  const out = [];
  v2.sys = { ...v.sys, out: (s) => out.push(s) };
  assert.equal(await main(['share', v.root, '--json'], v2.sys), 0);
  const r2 = JSON.parse(out.join(''));
  assert.equal(r2.daemon.installed_now, false);
  assert.equal(r2.daemon.server.started, false);
});

test('share: Tailscale MagicDNS name wins when tailscale answers', async () => {
  const bin = tmpdir('unlatch-bin-');
  fs.writeFileSync(
    path.join(bin, 'tailscale'),
    `#!/bin/sh\necho '{"BackendState":"Running","Self":{"DNSName":"box.tail9.ts.net.","TailscaleIPs":["100.101.1.2","fd7a::2"]}}'\n`
  );
  fs.chmodSync(path.join(bin, 'tailscale'), 0o755);
  const v = vmSys({ env: { PATH: `${bin}:${process.env.PATH}`, SSH_CONNECTION: '' }, which: (c) => realSys({ env: { PATH: `${bin}:${process.env.PATH}` } }).which(c) });
  assert.equal(await main(['share', '--json', '--no-serve'], v.sys), 0, v.stderr());
  const r = JSON.parse(v.stdout());
  assert.equal(r.mac_command, 'npx unlatch connect dev@box.tail9.ts.net:~/proj');
  assert.deepEqual(r.host_candidates.slice(0, 2).map((c) => c.source), ['tailscale', 'tailscale-ip']);
  assert.equal(r.daemon.server, null);
});

test('share: human output leads with the Mac command; errors are usage errors', async () => {
  const v = vmSys();
  assert.equal(await main(['share', '--no-serve'], v.sys), 0, v.stderr());
  assert.match(v.stdout(), /On your Mac, run this in Terminal:\n\n {4}npx unlatch connect dev@203\.0\.113\.7:~\/proj --port 2222/);
  const e = vmSys();
  assert.equal(await main(['share', '/definitely/not/here', '--json'], e.sys), 2);
  assert.equal(JSON.parse(e.stdout()).code, 'usage');
});

test('share refuses to run on a Mac', async () => {
  const v = vmSys();
  v.sys.platform = 'darwin';
  assert.equal(await main(['share', '--json'], v.sys), 2);
});

test('missing platform package explains --omit=optional', () => {
  const v = vmSys({ env: { UNLATCH_PLATFORM_DIR: '' } });
  assert.throws(
    () => platform.resolvePlatform(v.sys, { resolver: () => { throw new Error('nope'); } }),
    /unlatch-linux-x64 is not installed[\s\S]*--omit=optional/
  );
});

test('ensureRemote over a (local) "ssh": probes, uploads, verifies, reuses', () => {
  const v = vmSys();
  const plat = platform.resolvePlatform(v.sys);
  const d = platform.daemonOf(plat);
  const remoteHome = path.join(v.base, 'remote-install');
  const calls = [];
  // Stand-in for `ssh host -- <args>`: run the remote command with sh locally.
  const ssh = (args, opts = {}) => {
    calls.push(args.join(' '));
    const cmd = args[0] === 'sh' && args[1] === '-s' ? ['sh', ['-s']] : ['sh', ['-c', args.join(' ')]];
    return v.sys.run(cmd[0], cmd[1], { ...opts, env: { PATH: process.env.PATH, HOME: v.home } });
  };
  const r1 = daemon.ensureRemote(v.sys, ssh, d, { override: remoteHome, root: '~/proj' });
  assert.equal(r1.uploaded, true);
  assert.equal(r1.path, path.join(remoteHome, d.remoteName));
  assert.equal(r1.root, fs.realpathSync(v.root));
  assert.equal(daemon.sha256File(v.sys, r1.path), d.sha256);
  assert.equal(calls.length, 3);
  const r2 = daemon.ensureRemote(v.sys, ssh, d, { override: remoteHome });
  assert.equal(r2.uploaded, false);
  // A VM of another architecture: fall back to a daemon `share` installed there.
  const r3 = daemon.ensureRemote(v.sys, ssh, { ...d, arch: 'riscv64' }, { override: remoteHome });
  assert.equal(r3.path, r1.path);
  assert.throws(() => daemon.ensureRemote(v.sys, ssh, d, { override: remoteHome, root: '~/nope' }), /does not exist/);
});

test('status/doctor never create the install directory', async () => {
  const v = vmSys({ env: { UNLATCH_HOME: '' } });
  const out = [];
  v.sys.out = (s) => out.push(s);
  assert.equal(await main(['doctor', '--json'], v.sys), 0);
  assert.equal(await main(['status', '--json'], v.sys), 0);
  assert.ok(!fs.existsSync(path.join(v.home, '.unlatch')), 'doctor/status created ~/.unlatch');
});

test('uninstall on the VM: needs --yes, stops servers, removes daemon and state', async () => {
  const v = vmSys();
  assert.equal(await main(['share', '--json'], v.sys), 0);
  const inst = path.join(v.base, 'install');
  assert.equal(await main(['uninstall', '--json'], v.sys), 2);
  assert.equal(await main(['uninstall', '--yes', '--json'], v.sys), 0);
  assert.deepEqual(fs.readdirSync(inst), []);
});

// --install-dir must reach status/doctor/uninstall, and share must
// say that the Mac's bootstrap will not look there.
test('share/status/doctor/uninstall all honour --install-dir; share warns the Mac will not see it', async () => {
  const v = vmSys({ env: { UNLATCH_HOME: '' } });
  const dir = path.join(v.base, 'custom');
  const run = async (args) => {
    const out = [];
    v.sys.out = (s) => out.push(s);
    const code = await main(args, v.sys);
    return { code, r: JSON.parse(out.join('')) };
  };
  const s = await run(['share', '--json', '--install-dir', dir]);
  assert.equal(s.code, 0);
  assert.equal(s.r.daemon.install_dir, dir);
  assert.ok(s.r.warnings.some((w) => /--install-dir/.test(w) && /UNLATCH_HOME/.test(w)), JSON.stringify(s.r.warnings));

  const st = await run(['status', '--json', '--install-dir', dir]);
  assert.equal(st.r.daemon && st.r.daemon.path, s.r.daemon.path);
  assert.equal(st.r.servers.length, 1);
  assert.equal(st.r.servers[0].running, true);

  const doc = await run(['doctor', '--json', '--install-dir', dir]);
  assert.match(doc.r.checks.find((c) => c.id === 'install').message, new RegExp(`install directory ${dir.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`));

  const un = await run(['uninstall', '--yes', '--json', '--install-dir', dir]);
  assert.equal(un.code, 0);
  assert.equal(un.r.install_dir, dir);
  assert.equal(un.r.removed.length, 1);
  assert.deepEqual(fs.readdirSync(dir), []);

  // A missing --install-dir is "nothing there", never a fallback to the default directory.
  fs.mkdirSync(path.join(v.home, '.unlatch'), { mode: 0o700 });
  fs.writeFileSync(path.join(v.home, '.unlatch', 'unlatchd-0.1.0-keepme'), '');
  const none = await run(['uninstall', '--yes', '--json', '--install-dir', path.join(v.base, 'nope')]);
  assert.deepEqual(none.r.removed, []);
  assert.ok(fs.existsSync(path.join(v.home, '.unlatch', 'unlatchd-0.1.0-keepme')));
});

test('share warns when UNLATCH_HOME picks the install dir (non-interactive ssh may not see it)', async () => {
  const v = vmSys();
  assert.equal(await main(['share', '--json', '--no-serve'], v.sys), 0);
  const r = JSON.parse(v.stdout());
  assert.ok(r.warnings.some((w) => /UNLATCH_HOME/.test(w) && /non-interactive/.test(w)), JSON.stringify(r.warnings));
});

test('share on a NAT cloud VM without metadata: placeholder command, host_guess and a hint, human output says so', async () => {
  const v = vmSys({ env: { SSH_CONNECTION: '203.0.113.5 50000 10.0.1.23 22' }, fetchSync: (reqs) => reqs.map(() => null) });
  assert.equal(await main(['share', '--json', '--no-serve'], v.sys), 0, v.stderr());
  const r = JSON.parse(v.stdout());
  assert.equal(r.mac_command, "npx unlatch connect 'dev@<your-ssh-host>:~/proj'");
  assert.equal(r.host_guess, true);
  assert.match(r.host_hint, /Replace <your-ssh-host>/);
  assert.ok(r.host_candidates.some((c) => c.host === '10.0.1.23'));

  const h = vmSys({ env: { SSH_CONNECTION: '203.0.113.5 50000 10.0.1.23 22' }, fetchSync: (reqs) => reqs.map(() => null) });
  assert.equal(await main(['share', '--no-serve'], h.sys), 0, h.stderr());
  assert.match(h.stdout(), /npx unlatch connect 'dev@<your-ssh-host>:~\/proj'\n\n! Could not tell how your Mac reaches this VM/);
});

test('share on a NAT cloud VM with metadata: the public address is the Mac command', async () => {
  const fetchSync = (reqs) =>
    reqs.map((q) => (q.url.includes('access-configs/0/external-ip') ? { status: 200, body: '35.9.8.7\n' } : null));
  const v = vmSys({ env: { SSH_CONNECTION: '203.0.113.5 50000 10.0.1.23 22' }, fetchSync });
  assert.equal(await main(['share', '--json', '--no-serve'], v.sys), 0, v.stderr());
  const r = JSON.parse(v.stdout());
  assert.equal(r.mac_command, 'npx unlatch connect dev@35.9.8.7:~/proj');
  assert.equal(r.host_guess, false);
  assert.equal(r.host_candidates[0].source, 'cloud-metadata');
});
