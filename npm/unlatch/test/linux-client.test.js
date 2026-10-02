'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const { fakeSys, fakePlatform } = require('./helpers');
const { main } = require('../lib/cli');

test('connect --mount: ssh test, remote probe, then `unlatch mount … --unlatchd <path>`', async () => {
  const plat = fakePlatform('linux');
  const handlers = [
    [(c, a) => c === 'ssh' && a.includes('__unlatch_ok__;'), { stdout: '__unlatch_ok__\nLinux x86_64\n' }],
    [(c, a) => c === 'ssh' && a.includes('-s'), (c, a, o) => {
      const nonce = /N='?(UNLATCH-[0-9a-f]+)/.exec(o.input)[1];
      const man = require('node:fs').readFileSync(`${plat}/manifest.json`, 'utf8');
      const sha = JSON.parse(man).daemon.sha256;
      return { stdout: `${nonce} arch x86_64\n${nonce} dir /home/dev/.unlatch\n${nonce} hashtool yes\n${nonce} have ${sha}\n${nonce} root /home/dev/code\n` };
    }],
    [(c) => c.endsWith('/bin/unlatch'), {}],
  ];
  const sys = fakeSys({ platform: 'linux', arch: 'x64', handlers, env: { UNLATCH_PLATFORM_DIR: plat } });
  const code = await main(['connect', 'dev@vm:~/code', '--mount', '~/vm', '--port', '2200', '--json'], sys);
  assert.equal(code, 0, sys.stderr() + sys.stdout());
  const mount = sys.calls.find((c) => c.cmd.endsWith('/bin/unlatch'));
  const a = mount.args;
  assert.equal(a[0], 'mount');
  assert.equal(a[1], `${sys.home}/vm`);
  assert.deepEqual(a.slice(2, 6), ['--host', 'dev@vm', '--root', '~/code']);
  assert.equal(a[6], '--unlatchd');
  assert.match(a[7], /^\/home\/dev\/\.unlatch\/unlatchd-0\.1\.0-[0-9a-f]{16}$/);
  assert.deepEqual(a.slice(8), ['--port', '2200']);
  const ssh = sys.calls.find((c) => c.cmd === 'ssh');
  assert.ok(ssh.args.includes('-p') && ssh.args.includes('2200'));
  assert.equal(JSON.parse(sys.stdout()).mountpoint, `${sys.home}/vm`);
});

test('connect on Linux without --mount explains the flag', async () => {
  const sys = fakeSys({ platform: 'linux', arch: 'x64' });
  assert.equal(await main(['connect', 'vm:~/x', '--json'], sys), 2);
  assert.match(JSON.parse(sys.stdout()).error, /--mount/);
});
