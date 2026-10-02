'use strict';
// The release publish step must be idempotent (a re-run after a partial
// failure finishes the remaining packages instead of failing on E403 for the first one) and must
// publish the platform packages before the `unlatch` installer that pins them.
// npm/scripts/publish.mjs runs against a fake `npm` that keeps a registry in a JSON file.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const cp = require('node:child_process');
const { tmpdir } = require('./helpers');

const SCRIPT = path.join(__dirname, '..', '..', 'scripts', 'publish.mjs');

const FAKE_NPM = `#!/usr/bin/env node
const fs = require('fs');
const reg = process.env.FAKE_REGISTRY;
const db = fs.existsSync(reg) ? JSON.parse(fs.readFileSync(reg, 'utf8')) : { published: [], log: [] };
const save = () => fs.writeFileSync(reg, JSON.stringify(db));
const a = process.argv.slice(2);
db.log.push(a.join(' ') + (a[0] === 'publish' ? ' @' + process.cwd() : ''));
if (a[0] === 'view') {
  const spec = a[1];
  save();
  if ((process.env.FAKE_VIEW_FAIL || '') === spec) { process.stderr.write('npm error code ECONNRESET\\n'); process.exit(1); }
  const [name] = spec.split(/(?<=.)@/);
  if (db.published.includes(spec)) { process.stdout.write(JSON.stringify(spec.split(/(?<=.)@/)[1])); process.exit(0); }
  if (db.published.some((p) => p.startsWith(name + '@'))) process.exit(0); // package exists, version does not: empty output
  process.stderr.write('npm error code E404\\nnpm error 404 Not Found - GET https://registry.npmjs.org/' + name + '\\n');
  process.exit(1);
}
if (a[0] === 'publish') {
  const pkg = JSON.parse(fs.readFileSync('package.json', 'utf8'));
  const spec = pkg.name + '@' + pkg.version;
  if (db.published.includes(spec)) { save(); process.stderr.write('npm error code E403\\nnpm error 403 You cannot publish over the previously published versions\\n'); process.exit(1); }
  if ((process.env.FAKE_PUBLISH_FAIL || '') === pkg.name) { save(); process.stderr.write('npm error code ECONNRESET\\n'); process.exit(1); }
  db.published.push(spec);
  save();
  process.exit(0);
}
save();
process.exit(1);
`;

function setup() {
  const base = tmpdir('unlatch-publish-');
  const bin = path.join(base, 'bin');
  fs.mkdirSync(bin);
  fs.writeFileSync(path.join(bin, 'npm'), FAKE_NPM);
  fs.chmodSync(path.join(bin, 'npm'), 0o755);
  const dist = path.join(base, 'dist');
  const pkgs = {
    'linux-x64': { name: 'unlatch-linux-x64', version: '0.2.0' },
    'linux-arm64': { name: 'unlatch-linux-arm64', version: '0.2.0' },
    'darwin-universal': { name: 'unlatch-darwin-universal', version: '0.2.0' },
    unlatch: {
      name: 'unlatch',
      version: '0.2.0',
      optionalDependencies: { 'unlatch-darwin-universal': '0.2.0', 'unlatch-linux-arm64': '0.2.0', 'unlatch-linux-x64': '0.2.0' },
    },
  };
  for (const [d, pj] of Object.entries(pkgs)) {
    fs.mkdirSync(path.join(dist, d), { recursive: true });
    fs.writeFileSync(path.join(dist, d, 'package.json'), JSON.stringify(pj));
  }
  const registry = path.join(base, 'registry.json');
  const run = (dirs, env = {}) =>
    cp.spawnSync(process.execPath, [SCRIPT, '--tag', 'latest', '--provenance', ...dirs.map((d) => path.join(dist, d))], {
      encoding: 'utf8',
      env: { PATH: `${bin}:${process.env.PATH}`, FAKE_REGISTRY: registry, ...env },
    });
  const db = () => JSON.parse(fs.readFileSync(registry, 'utf8'));
  return { run, db, dist };
}

const ALL = ['unlatch', 'linux-x64', 'linux-arm64', 'darwin-universal']; // main first on purpose

test('publish: platform packages first, then the installer, with tag/provenance/public access', () => {
  const s = setup();
  const r = s.run(ALL);
  assert.equal(r.status, 0, r.stderr + r.stdout);
  assert.deepEqual(s.db().published, ['unlatch-linux-x64@0.2.0', 'unlatch-linux-arm64@0.2.0', 'unlatch-darwin-universal@0.2.0', 'unlatch@0.2.0']);
  const pub = s.db().log.filter((l) => l.startsWith('publish'));
  for (const l of pub) assert.match(l, /^publish --access public --tag latest --provenance @/);
});

test('publish: a re-run after a partial failure skips what is live and finishes the rest', () => {
  const s = setup();
  const r1 = s.run(ALL, { FAKE_PUBLISH_FAIL: 'unlatch' });
  assert.notEqual(r1.status, 0);
  assert.deepEqual(s.db().published, ['unlatch-linux-x64@0.2.0', 'unlatch-linux-arm64@0.2.0', 'unlatch-darwin-universal@0.2.0']);
  const r2 = s.run(ALL);
  assert.equal(r2.status, 0, r2.stderr + r2.stdout);
  assert.deepEqual(s.db().published.slice(-1), ['unlatch@0.2.0']);
  assert.match(r2.stdout, /skip unlatch-linux-x64@0\.2\.0 \(already published\)/);
  // A third run is a no-op.
  const before = s.db().log.filter((l) => l.startsWith('publish')).length;
  assert.equal(s.run(ALL).status, 0);
  assert.equal(s.db().log.filter((l) => l.startsWith('publish')).length, before);
});

test('publish: a platform failure stops before the installer; an unknown registry answer is an error, not a skip', () => {
  const s = setup();
  const r = s.run(ALL, { FAKE_PUBLISH_FAIL: 'unlatch-linux-arm64' });
  assert.notEqual(r.status, 0);
  assert.ok(!s.db().published.includes('unlatch@0.2.0'), 'unlatch must not go out without its platform packages');

  const t = setup();
  const r2 = t.run(ALL, { FAKE_VIEW_FAIL: 'unlatch-linux-x64@0.2.0' });
  assert.notEqual(r2.status, 0);
  assert.match(r2.stderr, /cannot tell whether unlatch-linux-x64@0\.2\.0 is published/);
  assert.deepEqual(t.db().published, []);
});

test('publish: the installer refuses to go out when a pinned platform package is missing from the registry', () => {
  const s = setup();
  const r = s.run(['unlatch']);
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /unlatch-darwin-universal@0\.2\.0 is not on the registry/);
  assert.deepEqual(s.db().published, []);
});

test('npm-release.yml publishes through publish.mjs', () => {
  const wf = fs.readFileSync(path.join(__dirname, '..', '..', '..', '.github', 'workflows', 'npm-release.yml'), 'utf8');
  assert.match(wf, /node npm\/scripts\/publish\.mjs --tag "\$NPM_TAG" --provenance dist\/linux-x64 dist\/linux-arm64 dist\/darwin-universal dist\/unlatch/);
  assert.doesNotMatch(wf, /\(cd "\$d" && npm publish/);
});
