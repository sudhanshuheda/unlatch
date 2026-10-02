'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const ssh = require('../lib/ssh');
const { parseArgs } = require('../lib/cli');
const { fakeSys } = require('./helpers');

test('parseTarget: scp style, IPv6, ssh:// URLs, defaults', () => {
  const t = (s) => ssh.parseTarget(s);
  assert.deepEqual(t('me@vm:~/code'), { user: 'me', host: 'vm', port: null, path: '~/code' });
  assert.deepEqual(t('vm'), { user: null, host: 'vm', port: null, path: '~' });
  assert.deepEqual(t('vm:'), { user: null, host: 'vm', port: null, path: '~' });
  assert.deepEqual(t('vm:code/x/'), { user: null, host: 'vm', port: null, path: '~/code/x' });
  assert.deepEqual(t('me@vm:/srv/a b'), { user: 'me', host: 'vm', port: null, path: '/srv/a b' });
  assert.deepEqual(t('me@[fd7a::32]:/srv'), { user: 'me', host: 'fd7a::32', port: null, path: '/srv' });
  assert.deepEqual(t('ssh://me@vm:2222/~/code'), { user: 'me', host: 'vm', port: 2222, path: '~/code' });
  assert.deepEqual(t('ssh://vm/srv'), { user: null, host: 'vm', port: null, path: '/srv' });
  assert.deepEqual(t('me@devbox.ts.net:~/a@b'), { user: 'me', host: 'devbox.ts.net', port: null, path: '~/a@b' });
  assert.throws(() => t('-oProxyCommand=x:~'), /may not start/);
  assert.throws(() => t(''), /missing/);
  assert.throws(() => t('a;b:~'), /invalid host/);
  assert.equal(ssh.formatTarget(t('me@[fd7a::32]:/srv')), 'me@[fd7a::32]:/srv');
});

test('ssh config: aliases, Include, wildcards and Match skipped, first value wins', () => {
  const sys = fakeSys({ platform: 'linux' });
  fs.mkdirSync(path.join(sys.home, '.ssh', 'conf.d'), { recursive: true });
  fs.writeFileSync(
    path.join(sys.home, '.ssh', 'config'),
    'Include conf.d/*.conf\nHost a b # two\n  HostName=10.0.0.1\n  User u\n  Port 2200\n  User ignored\nHost *.corp !x\n  User c\nMatch all\n  User m\nHost "q q"\n'
  );
  fs.writeFileSync(path.join(sys.home, '.ssh', 'conf.d', 'x.conf'), 'Host inc\n  HostName inc.example\n');
  const hosts = ssh.loadSshConfig(sys);
  assert.deepEqual(hosts.map((h) => h.alias), ['inc', 'a', 'b', 'q q']);
  assert.deepEqual(hosts[1], { alias: 'a', hostName: '10.0.0.1', user: 'u', port: 2200 });
});

test('preferAlias: exact HostName match, user and port must agree', () => {
  const hosts = [
    { alias: 'dev', hostName: 'devbox.tail1.ts.net', user: 'sam' },
    { alias: 'ip', hostName: '10.0.0.7', port: 2222 },
  ];
  const p = (s, port) => ssh.preferAlias({ ...ssh.parseTarget(s), port: port || null }, hosts);
  assert.equal(p('sam@devbox.tail1.ts.net:~').target.host, 'dev');
  assert.equal(p('sam@devbox.tail1.ts.net:~').target.user, null);
  assert.equal(p('bob@devbox.tail1.ts.net:~').alias, null);
  assert.equal(p('10.0.0.7:/x', 2222).target.host, 'ip');
  assert.equal(p('10.0.0.70:/x').alias, null);
  assert.equal(p('dev:~').alias, 'dev');
});

// A short HostName must not capture an FQDN with the same first label —
// not even a MagicDNS one (a LAN box called `web` is not the tailnet's `web`).
test('preferAlias: no short-name matches, exact matches only', () => {
  const p = (s, hosts) => ssh.preferAlias(ssh.parseTarget(s), hosts);
  assert.equal(p('me@web.tail1234.ts.net:~/api', [{ alias: 'oldweb', hostName: 'web' }]).alias, null);
  assert.equal(p('me@web.tail1234.ts.net:~/api', [{ alias: 'w', hostName: 'web' }, { alias: 'tw', hostName: 'web.tail1234.ts.net' }]).alias, 'tw');
  assert.equal(p('me@dev.corp.example.com:~', [{ alias: 'x', hostName: 'dev' }]).alias, null);
  assert.equal(p('ubuntu:~', [{ alias: 'pi', hostName: 'ubuntu.local' }]).alias, null);
  assert.equal(p('me@web.example.org:~', [{ alias: 'w', hostName: 'web.example.net' }]).alias, null);
  assert.equal(p('web:~', [{ alias: 'tw', hostName: 'web.tail1234.ts.net.' }]).alias, null);
  assert.equal(p('me@web.tail1234.ts.net:~', [{ alias: 'tw', hostName: 'web.tail1234.ts.net.' }]).alias, 'tw');
  // Exact matches are unaffected.
  assert.equal(p('me@dev.corp.example.com:~', [{ alias: 'd', hostName: 'DEV.corp.example.com' }]).alias, 'd');
});

// share prints `<your-ssh-host>` when it cannot tell how the Mac reaches the VM; pasting that
// unedited must explain itself instead of failing inside ssh.
test('parseTarget: an unreplaced placeholder host is a clear usage error', () => {
  assert.throws(() => ssh.parseTarget('me@<your-ssh-host>:~/api'), /replace <your-ssh-host>.*~\/\.ssh\/config/);
});

test('classify ssh failures into fixes', () => {
  const t = { user: 'me', host: 'vm' };
  assert.equal(ssh.classify('me@vm: Permission denied (publickey).', t).kind, 'auth');
  assert.equal(ssh.classify('Host key verification failed.', t).kind, 'hostkey');
  assert.equal(ssh.classify('ssh: Could not resolve hostname vm: Name or service not known', t).kind, 'dns');
  assert.equal(ssh.classify('ssh: connect to host vm port 22: Connection timed out', t).kind, 'timeout');
  assert.equal(ssh.classify('ssh: connect to host vm port 22: Connection refused', t).kind, 'refused');
  assert.match(ssh.classify('Permission denied', t).fix, /ssh-copy-id me@vm/);
});

test('cli argument parsing', () => {
  assert.deepEqual(parseArgs(['connect', 'vm:~', '--port=22', '--json', '-y']), { cmd: 'connect', args: ['vm:~'], flags: { port: '22', json: true, yes: true } });
  assert.throws(() => parseArgs(['share', '--bogus']), /unknown option/);
  assert.throws(() => parseArgs(['connect', '--mount']), /needs a value/);
  assert.deepEqual(parseArgs(['share', '--', '--weird-dir']).args, ['--weird-dir']);
});
