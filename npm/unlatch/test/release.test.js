'use strict';
// A release that ships no Mac app (`assemble.mjs main --no-darwin`, e.g. the Linux-only
// 0.1.0-alpha.1): installs never reference unlatch-darwin-universal, a Mac gets a clear "not
// published yet, build from source" message and a non-zero exit before anything else runs, and
// the Linux modes do not depend on the darwin package.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const cp = require('node:child_process');
const { tmpdir, fakeSys, fakePlatform, FAKE_UNLATCHD } = require('./helpers');
const { main } = require('../lib/cli');
const platform = require('../lib/platform');
const names = require('../lib/names');
const { realSys } = require('../lib/sys');

const ASSEMBLE = path.join(__dirname, '..', '..', 'scripts', 'assemble.mjs');
const SRC_PKG = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'package.json'), 'utf8'));
const LINUX_ONLY = { name: 'unlatch', version: '9.9.9-alpha.1', optionalDependencies: { 'unlatch-linux-arm64': '9.9.9-alpha.1', 'unlatch-linux-x64': '9.9.9-alpha.1' } };
const WITH_DARWIN = { ...LINUX_ONLY, optionalDependencies: { ...LINUX_ONLY.optionalDependencies, 'unlatch-darwin-universal': '9.9.9-alpha.1' } };

const noResolve = () => {
  throw Object.assign(new Error('not found'), { code: 'MODULE_NOT_FOUND' });
};

function assertMacMessage(text) {
  assert.match(text, /Mac app is not published yet/);
  assert.match(text, /9\.9\.9-alpha\.1|0\.1\.0-alpha\.1/);
  assert.ok(text.includes('https://github.com/sudhanshuheda/unlatch#building-from-source'), text);
  assert.match(text, /--mount <dir>/);
  if (names.SOURCE_NOTE) assert.ok(text.includes(names.SOURCE_NOTE), text);
  assert.doesNotMatch(text, /--include=optional|npm i -g unlatch-darwin/, 'must not tell people to install a package that does not exist');
}

test('platform.released follows the installer package optionalDependencies', () => {
  assert.equal(platform.released({ installerPackage: LINUX_ONLY }, 'darwin-universal'), false);
  assert.equal(platform.released({ installerPackage: LINUX_ONLY }, 'linux-x64'), true);
  assert.equal(platform.released({ installerPackage: WITH_DARWIN }, 'darwin-universal'), true);
  assert.equal(platform.released({ installerPackage: null }, 'darwin-universal'), true, 'unknown: assume it ships');
  // The real sys reads this package's own package.json.
  assert.deepEqual(realSys().installerPackage.optionalDependencies, SRC_PKG.optionalDependencies);
});

test('resolvePlatform on a Mac: not in this release → NotPublishedError; in the release but skipped → the --include=optional hint', () => {
  const sys = fakeSys({ platform: 'darwin', installerPackage: LINUX_ONLY });
  assert.throws(() => platform.resolvePlatform(sys, { resolver: noResolve }), (e) => {
    assert.ok(e instanceof platform.NotPublishedError && e instanceof platform.PlatformError);
    assertMacMessage(e.message);
    return true;
  });
  const sys2 = fakeSys({ platform: 'darwin', installerPackage: WITH_DARWIN });
  assert.throws(() => platform.resolvePlatform(sys2, { resolver: noResolve }), (e) => {
    assert.ok(!(e instanceof platform.NotPublishedError));
    assert.match(e.message, /unlatch-darwin-universal is not installed/);
    assert.match(e.message, /--include=optional/);
    return true;
  });
});

for (const [label, argv] of [
  ['connect', ['connect', 'sam@dev-vm:~/code']],
  ['the wizard (plain `npx unlatch` in a terminal)', []],
  ['status', ['status']],
  ['doctor', ['doctor']],
]) {
  test(`Mac without the app in this release: ${label} prints the message and exits 1 before running anything`, async () => {
    const sys = fakeSys({ platform: 'darwin', tty: true, installerPackage: LINUX_ONLY });
    const code = await main(argv, sys);
    assert.equal(code, 1);
    assertMacMessage(sys.stderr());
    assert.deepEqual(sys.calls, [], 'no ssh test, no app install');
    assert.equal(sys.stdout(), '');
  });
}

test('Mac without the app in this release: --json gives {ok:false, code:"not_published"}', async () => {
  const sys = fakeSys({ platform: 'darwin', installerPackage: LINUX_ONLY });
  const code = await main(['connect', 'sam@dev-vm:~/code', '--json'], sys);
  assert.equal(code, 1);
  const r = JSON.parse(sys.stdout());
  assert.equal(r.ok, false);
  assert.equal(r.code, 'not_published');
  assertMacMessage(r.error);
});

test('Mac without the app: version and skill still work', async () => {
  const sys = fakeSys({ platform: 'darwin', installerPackage: LINUX_ONLY });
  assert.equal(await main(['--version'], sys), 0);
  assert.match(sys.stdout(), /no platform package/);
  const s2 = fakeSys({ platform: 'darwin', installerPackage: LINUX_ONLY });
  assert.equal(await main(['skill', '--print'], s2), 0);
});

test('Linux share does not need the darwin package, and says the Mac app is not published', async () => {
  const base = tmpdir('unlatch-rel-');
  const home = path.join(base, 'home');
  fs.mkdirSync(path.join(home, 'proj'), { recursive: true });
  const plat = fakePlatform('linux', { daemonScript: FAKE_UNLATCHD });
  const out = [];
  const real = realSys();
  const sys = realSys({
    platform: 'linux',
    arch: 'x64',
    env: { PATH: process.env.PATH, HOME: home, UNLATCH_HOME: path.join(base, 'install'), UNLATCH_PLATFORM_DIR: plat, SSH_CONNECTION: '198.51.100.20 50000 203.0.113.7 22' },
    home,
    cwd: path.join(home, 'proj'),
    username: 'dev',
    installerPackage: LINUX_ONLY,
    which: (c) => (c === 'tailscale' ? null : real.which(c)),
    fetchSync: () => {
      throw new Error('tests must not reach a real metadata service');
    },
    out: (s) => out.push(s),
    err: () => {},
  });
  const code = await main(['share', '--json', '--no-serve'], sys);
  assert.equal(code, 0);
  const r = JSON.parse(out.join(''));
  assert.equal(r.ok, true);
  assert.equal(r.mac_app_published, false);
  assert.ok(r.warnings.some((w) => /Mac app is not published yet/.test(w) && w.includes(names.BUILD_FROM_SOURCE_URL) && w.includes(r.linux_command)), JSON.stringify(r.warnings));

  // With the Mac app in the release: no such warning.
  const out2 = [];
  Object.assign(sys, { installerPackage: WITH_DARWIN, out: (s) => out2.push(s) });
  assert.equal(await main(['share', '--json', '--no-serve'], sys), 0);
  const r2 = JSON.parse(out2.join(''));
  assert.equal(r2.mac_app_published, true);
  assert.ok(!r2.warnings.some((w) => /not published/.test(w)));

  // Human output without the Mac app: the Linux command leads, the Mac line is marked as future.
  const out3 = [];
  Object.assign(sys, { installerPackage: LINUX_ONLY, out: (s) => out3.push(s) });
  assert.equal(await main(['share', '--no-serve'], sys), 0);
  const text = out3.join('');
  assert.ok(!/On your Mac, run this/.test(text), text);
  const lead = text.indexOf('On a Linux desktop, run this');
  assert.ok(lead >= 0 && lead < text.indexOf('not published yet') && text.indexOf('not published yet') < text.indexOf('Once the Mac app ships'), text);
  assert.ok(text.includes(r.linux_command) && text.includes(r.mac_command) && text.includes(names.BUILD_FROM_SOURCE_URL), text);
  assert.equal(text.split('not published yet').length - 1, 1, 'the Mac notice is printed once, not also as a warning');
});

function assembleMain(extra) {
  const out = path.join(tmpdir('unlatch-asm-'), 'unlatch');
  const r = cp.spawnSync(process.execPath, [ASSEMBLE, 'main', '--out', out, '--version', '9.9.9-alpha.1', ...extra], { encoding: 'utf8' });
  assert.equal(r.status, 0, r.stderr);
  return out;
}

test('assemble main --no-darwin: no darwin optionalDependency, the rest pinned, licenses included, no scripts', () => {
  const out = assembleMain(['--no-darwin']);
  const pj = JSON.parse(fs.readFileSync(path.join(out, 'package.json'), 'utf8'));
  assert.equal(pj.version, '9.9.9-alpha.1');
  assert.deepEqual(pj.optionalDependencies, { 'unlatch-linux-arm64': '9.9.9-alpha.1', 'unlatch-linux-x64': '9.9.9-alpha.1' });
  assert.equal(pj.scripts, undefined);
  assert.equal(pj.dependencies, undefined);
  for (const f of ['LICENSE-MIT', 'LICENSE-APACHE', 'README.md', 'bin/unlatch.js', 'skill/SKILL.md']) assert.ok(fs.existsSync(path.join(out, f)), f);
  assert.ok(!fs.existsSync(path.join(out, 'test')));
  assert.equal(fs.statSync(path.join(out, 'bin', 'unlatch.js')).mode & 0o111, 0o111);
  assert.match(fs.readFileSync(path.join(out, 'bin', 'unlatch.js'), 'utf8'), /^#!\/usr\/bin\/env node\n/);

  // Without the flag the darwin package stays (a full release).
  const full = JSON.parse(fs.readFileSync(path.join(assembleMain([]), 'package.json'), 'utf8'));
  assert.equal(full.optionalDependencies['unlatch-darwin-universal'], '9.9.9-alpha.1');
});

test('the assembled --no-darwin installer, run as a real process on a (pretend) Mac, prints the message and exits 1', () => {
  const out = assembleMain(['--no-darwin']);
  const fake = path.join(tmpdir('unlatch-darwin-'), 'pretend-darwin.js');
  fs.writeFileSync(fake, "Object.defineProperty(process, 'platform', { value: 'darwin' });\n");
  for (const argv of [['connect', 'sam@dev-vm:~/code'], []]) {
    const r = cp.spawnSync(process.execPath, ['--require', fake, path.join(out, 'bin', 'unlatch.js'), ...argv], {
      encoding: 'utf8',
      env: { PATH: process.env.PATH, HOME: tmpdir('unlatch-home-') },
    });
    assert.equal(r.status, 1, r.stdout + r.stderr);
    assertMacMessage(r.stderr);
    assert.doesNotMatch(r.stderr, /at .*\.js:\d+/, 'no stack trace');
  }
});
