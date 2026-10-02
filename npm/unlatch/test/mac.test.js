'use strict';
// Mac mode with a mocked platform, file system layout and child processes: install order,
// the app's --cli calls, the wizard, update and uninstall order.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { fakeSys, fakePlatform } = require('./helpers');
const { main } = require('../lib/cli');

const SSH_CONFIG = `
Host *
  ServerAliveInterval 30
Host dev devbox
  HostName devbox.tail1234.ts.net
  User sam
  IdentityFile ~/.ssh/id_dev
Host gpu-*
  User root
Match host foo
  User nobody
Host build
  HostName 10.0.0.7
  Port 2222
Include config.d/*
`;

/** Emulates Unlatch.app --cli: domains progress Connecting → Syncing → Live. */
function fakeApp() {
  const domains = [];
  const log = [];
  let n = 0;
  const handler = (cmd, args) => {
    const [, sub, ...rest] = args; // '--cli', sub, ...
    log.push([sub, ...rest.filter((a) => a !== '--json')]);
    const ok = (o) => ({ stdout: JSON.stringify({ ok: true, ...o }) });
    switch (sub) {
      case 'status':
        for (const d of domains) {
          d.ticks++;
          d.state = d.ticks < 2 ? 'Connecting' : d.ticks < 3 ? 'Syncing' : 'Live';
          d.path = d.state === 'Live' ? `/Users/me/Library/CloudStorage/Unlatch-${d.name}` : null;
        }
        return ok({ app_version: '0.2.0', agent: 'enabled', domains: domains.map(({ ticks, ...d }) => d) });
      case 'add': {
        const get = (k) => rest[rest.indexOf(`--${k}`) + 1];
        const d = { id: `${get('name')}-${++n}`, name: get('name'), host: get('host'), port: rest.includes('--port') ? Number(get('port')) : null, root: get('root'), state: 'Connecting', ticks: 0 };
        domains.push(d);
        return ok({ domain: { ...d, ticks: undefined } });
      }
      case 'open': {
        const d = domains.find((x) => x.id === rest[0]);
        return ok({ id: d.id, path: `/Users/me/Library/CloudStorage/Unlatch-${d.name}` });
      }
      case 'remove': {
        const i = domains.findIndex((x) => x.id === rest[0]);
        const [d] = domains.splice(i, 1);
        return ok({ id: rest[0], preserved: d.dirty ? `/Users/me/Library/CloudStorage/Unlatch-${d.name} (unsynced)` : null });
      }
      case 'register-agent':
      case 'repair-agent':
      case 'unregister-agent':
        return ok({ agent: sub === 'unregister-agent' ? 'not_registered' : 'enabled' });
      default:
        return { status: 2, stdout: JSON.stringify({ ok: false, error: 'usage', code: 'usage' }) };
    }
  };
  return { domains, log, handler };
}

function macSys({ answers = [], tty = false, installed = null } = {}) {
  const plat = fakePlatform('darwin');
  const app = fakeApp();
  const handlers = [
    [(c) => c === 'ditto', (c, args) => {
      const plist = path.join(args[3], 'Unlatch.app', 'Contents', 'Info.plist');
      fs.mkdirSync(path.dirname(plist), { recursive: true });
      fs.writeFileSync(plist, '0.2.0');
      return {};
    }],
    [(c) => c === '/usr/libexec/PlistBuddy', (c, args) => {
      const f = args[2];
      if (!fs.existsSync(f)) return { status: 1 };
      return { stdout: args[1].includes('CFBundleIdentifier') ? 'dev.example.unlatch\n' : fs.readFileSync(f, 'utf8') + '\n' };
    }],
    [(c, a) => c === 'ssh' && a.includes('__unlatch_ok__;'), { stdout: '__unlatch_ok__\nLinux x86_64\n' }],
    [(c) => c.endsWith('/Contents/MacOS/Unlatch'), app.handler],
    [(c) => ['open', 'xattr', 'osascript', 'pkill'].includes(c), {}],
  ];
  const sys = fakeSys({ platform: 'darwin', handlers, answers, tty, env: { UNLATCH_PLATFORM_DIR: plat } });
  fs.mkdirSync(path.join(sys.home, '.ssh', 'config.d'), { recursive: true });
  fs.writeFileSync(path.join(sys.home, '.ssh', 'config'), SSH_CONFIG);
  fs.writeFileSync(path.join(sys.home, '.ssh', 'config.d', 'work'), 'Host work-vm\n  HostName 203.0.113.9\n  User ci\n');
  if (installed) {
    const plist = path.join(sys.paths.applications, 'Unlatch.app', 'Contents', 'Info.plist');
    fs.mkdirSync(path.dirname(plist), { recursive: true });
    fs.writeFileSync(plist, installed);
  }
  return { sys, app };
}

/** Condensed call log: 'ditto -x -k', 'open -g', 'cli add', … */
function steps(sys) {
  return sys.calls
    .map(({ cmd, args }) => {
      if (cmd.endsWith('/Contents/MacOS/Unlatch')) return `cli ${args[1]}`;
      if (cmd === 'ditto') return `ditto ${args.slice(0, 2).join(' ')}`;
      if (cmd === 'open') return `open ${args[0]}`;
      if (cmd === 'ssh') return 'ssh test';
      return cmd.split('/').pop();
    })
    .filter((s) => s !== 'PlistBuddy')
    .filter((s, i, a) => !(s === 'cli status' && a[i - 1] === 'cli status')); // collapse polling
}

test('connect: fresh install → launch → add → wait live → open, in that order', async () => {
  const { sys, app } = macSys();
  const code = await main(['connect', 'sam@devbox.tail1234.ts.net:~/code', '--json'], sys);
  assert.equal(code, 0, sys.stderr() + sys.stdout());
  assert.deepEqual(steps(sys), ['ssh test', 'ditto -x -k', 'xattr', 'open -g', 'cli status', 'cli add', 'cli status', 'cli open']);
  // The ~/.ssh/config alias whose HostName matches is used, so its User/IdentityFile apply.
  const add = app.log.find((l) => l[0] === 'add');
  assert.deepEqual(add, ['add', '--name', 'dev-code', '--host', 'dev', '--root', '~/code']);
  const res = JSON.parse(sys.stdout());
  assert.equal(res.ok, true);
  assert.equal(res.domain.state, 'Live');
  assert.equal(res.domain.path, '/Users/me/Library/CloudStorage/Unlatch-dev-code');
  assert.ok(fs.existsSync(path.join(sys.paths.applications, 'Unlatch.app', 'Contents', 'Info.plist')));
  // ssh is tested non-interactively first.
  const sshCall = sys.calls.find((c) => c.cmd === 'ssh');
  assert.ok(sshCall.args.includes('BatchMode=yes'));
});

test('connect: builds --port/--identity, and is idempotent for an existing domain', async () => {
  const { sys, app } = macSys();
  assert.equal(await main(['connect', 'ops@198.51.100.4:/srv/app', '--port', '2200', '--identity', '/k/id', '--name', 'prod', '--json'], sys), 0, sys.stderr());
  const add = app.log.find((l) => l[0] === 'add');
  assert.deepEqual(add, ['add', '--name', 'prod', '--host', 'ops@198.51.100.4', '--root', '/srv/app', '--port', '2200', '--identity', '/k/id']);
  const before = app.log.filter((l) => l[0] === 'add').length;
  assert.equal(await main(['connect', 'ops@198.51.100.4:/srv/app', '--port', '2200', '--json'], sys), 0);
  assert.equal(app.log.filter((l) => l[0] === 'add').length, before, 'second connect must not add again');
});

test('connect: the app is not reinstalled when the same version is present', async () => {
  const { sys } = macSys({ installed: '0.2.0' });
  assert.equal(await main(['connect', 'dev:~', '--json'], sys), 0, sys.stderr());
  assert.ok(!sys.calls.some((c) => c.cmd === 'ditto'));
  assert.equal(steps(sys)[1], 'open -g');
});

test('update: an older app is replaced, then the agent is repaired (MQ-062/063)', async () => {
  const { sys, app } = macSys({ installed: '0.1.0' });
  assert.equal(await main(['update', '--json'], sys), 0, sys.stderr());
  assert.deepEqual(steps(sys), ['ditto -x -k', 'osascript', 'pkill', 'xattr', 'open -g', 'cli repair-agent', 'cli status']);
  assert.ok(app.log.some((l) => l[0] === 'repair-agent'));
  assert.equal(fs.readFileSync(path.join(sys.paths.applications, 'Unlatch.app', 'Contents', 'Info.plist'), 'utf8'), '0.2.0');
  assert.equal(JSON.parse(sys.stdout()).action, 'updated');
});

test('app goes to ~/Applications when /Applications is not writable', async () => {
  const { sys } = macSys();
  fs.chmodSync(sys.paths.applications, 0o555);
  try {
    assert.equal(await main(['connect', 'dev:~', '--json'], sys), 0, sys.stderr());
    assert.ok(fs.existsSync(path.join(sys.home, 'Applications', 'Unlatch.app', 'Contents', 'Info.plist')));
  } finally {
    fs.chmodSync(sys.paths.applications, 0o755);
  }
});

test('uninstall: domains first, then the agent, then the app (MQ-063)', async () => {
  const { sys, app } = macSys();
  await main(['connect', 'dev:~/a', '--json'], sys);
  await main(['connect', 'build:/srv', '--json'], sys);
  assert.equal(app.domains.length, 2);
  sys.calls.length = 0;
  assert.equal(await main(['uninstall', '--json'], sys), 2, 'needs --yes');
  sys.calls.length = 0;
  assert.equal(await main(['uninstall', '--yes', '--json'], sys), 0, sys.stderr());
  assert.deepEqual(steps(sys), ['cli status', 'cli remove', 'cli remove', 'cli unregister-agent', 'osascript', 'pkill']);
  assert.equal(app.domains.length, 0);
  assert.ok(!fs.existsSync(path.join(sys.paths.applications, 'Unlatch.app')));
});

test('wizard: parses ~/.ssh/config, picks a host by number, asks for the folder and a name', async () => {
  // Hosts listed: dev, devbox, build, work-vm (wildcards and Match skipped, Include followed).
  const { sys, app } = macSys({ tty: true, answers: ['3', '~/code', ''] });
  const code = await main([], sys);
  assert.equal(code, 0, sys.stderr());
  const listing = sys.stdout();
  for (const h of ['dev', 'devbox', 'build', 'work-vm']) assert.match(listing, new RegExp(`\\b${h}\\b`));
  assert.doesNotMatch(listing, /gpu-\*/);
  assert.deepEqual(app.log.find((l) => l[0] === 'add'), ['add', '--name', 'build-code', '--host', 'build', '--root', '~/code']);
});

test('non-TTY bare invocation on a Mac asks for connect instead of hanging', async () => {
  const { sys } = macSys();
  assert.equal(await main(['--json'], sys), 2);
  assert.match(JSON.parse(sys.stdout()).error, /connect/);
});

test('status, open and remove go through --cli', async () => {
  const { sys, app } = macSys();
  await main(['connect', 'dev:~/a', '--json'], sys);
  sys.calls.length = 0;
  assert.equal(await main(['status', '--json'], sys), 0);
  assert.equal(await main(['open', 'dev-a', '--json'], sys), 0);
  assert.equal(await main(['remove', 'dev-a', '--json'], sys), 0);
  assert.equal(app.domains.length, 0);
  assert.equal(await main(['remove', 'nope', '--json'], sys), 2);
});

test('remove and uninstall say where unsynced edits were kept', async () => {
  const { sys, app } = macSys();
  await main(['connect', 'dev:~/a', '--json'], sys);
  await main(['connect', 'build:/srv', '--json'], sys);
  app.domains[0].dirty = true;
  const kept = `/Users/me/Library/CloudStorage/Unlatch-${app.domains[0].name} (unsynced)`;
  sys.calls.length = 0;
  const before = sys.stdout().length;
  assert.equal(await main(['remove', app.domains[0].id, '--json'], sys), 0, sys.stderr());
  assert.equal(JSON.parse(sys.stdout().slice(before)).preserved, kept);
  const errBefore = sys.stderr().length;
  app.domains.push({ id: 'x-9', name: 'x', host: 'x', root: '/', state: 'Live', ticks: 5, dirty: true });
  assert.equal(await main(['uninstall', '--yes'], sys), 0, sys.stderr());
  assert.match(sys.stdout() + sys.stderr().slice(errBefore), /Unlatch-x \(unsynced\)/);
});

test('ssh network failures stop before installing anything', async () => {
  const { sys } = macSys();
  sys.calls.length = 0;
  const bad = fakeSys({
    platform: 'darwin',
    env: sys.env,
    home: sys.home,
    handlers: [[(c) => c === 'ssh', { status: 255, stderr: 'ssh: Could not resolve hostname nope: nodename nor servname provided\n' }]],
  });
  assert.equal(await main(['connect', 'nope:~', '--json'], bad), 3);
  const r = JSON.parse(bad.stdout());
  assert.equal(r.code, 'needs_user');
  assert.match(r.error, /does not resolve/);
  assert.ok(!bad.calls.some((c) => c.cmd === 'ditto'));
});

// A newer installed app must never be replaced by an older package.
test('connect/update never downgrade a newer installed app (semver, not string equality)', async () => {
  const { sys, app } = macSys({ installed: '0.3.0' });
  assert.equal(await main(['connect', 'dev:~', '--json'], sys), 0, sys.stderr());
  const s = steps(sys);
  for (const forbidden of ['ditto -x -k', 'osascript', 'pkill', 'cli repair-agent']) assert.ok(!s.includes(forbidden), `${forbidden} in ${s}`);
  assert.equal(fs.readFileSync(path.join(sys.paths.applications, 'Unlatch.app', 'Contents', 'Info.plist'), 'utf8'), '0.3.0');
  assert.match(JSON.parse(sys.stdout()).warnings.join('\n'), /newer than this unlatch[\s\S]*npx unlatch@latest/);
  assert.ok(app.log.some((l) => l[0] === 'add'));
  const h = macSys({ installed: '0.3.0' });
  assert.equal(await main(['connect', 'dev:~'], h.sys), 0, h.sys.stderr());
  assert.match(h.sys.stderr(), /Unlatch\.app 0\.3\.0 is newer than this unlatch \(0\.2\.0\); keeping it/);

  const u = macSys({ installed: '0.10.0' }); // 0.10.0 > 0.2.0 numerically, < as strings
  assert.equal(await main(['update', '--json'], u.sys), 0, u.sys.stderr());
  const r = JSON.parse(u.sys.stdout());
  assert.equal(r.action, 'newer');
  assert.equal(r.version, '0.10.0');
  assert.ok(!u.sys.calls.some((c) => c.cmd === 'ditto'));

  // --reinstall is the explicit way to go back.
  const f = macSys({ installed: '0.3.0' });
  assert.equal(await main(['update', '--reinstall', '--json'], f.sys), 0, f.sys.stderr());
  assert.equal(JSON.parse(f.sys.stdout()).action, 'updated');
});

test('compareVersions orders semver numerically, pre-releases first', () => {
  const { compareVersions } = require('../lib/mac');
  assert.ok(compareVersions('0.10.0', '0.2.0') > 0);
  assert.ok(compareVersions('0.2.0', '0.3.0') < 0);
  assert.equal(compareVersions('0.2', '0.2.0'), 0);
  assert.equal(compareVersions('v1.2.3', '1.2.3'), 0);
  assert.ok(compareVersions('1.0.0-rc.1', '1.0.0') < 0);
  assert.ok(compareVersions('1.0.0-rc.2', '1.0.0-rc.10') < 0);
  assert.ok(compareVersions('1.0.0+build.5', '1.0.0') === 0);
  assert.equal(compareVersions('garbage', '1.0.0'), null);
});

test('connect with the unedited `<your-ssh-host>` placeholder is a usage error that says what to type', async () => {
  const { sys } = macSys();
  assert.equal(await main(['connect', "me@<your-ssh-host>:~/proj", '--json'], sys), 2);
  const r = JSON.parse(sys.stdout());
  assert.equal(r.code, 'usage');
  assert.match(r.error, /replace <your-ssh-host> with the address or ~\/\.ssh\/config Host alias/);
  assert.ok(!sys.calls.length, 'nothing may run before the target is fixed');
});
