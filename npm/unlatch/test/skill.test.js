'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { fakeSys } = require('./helpers');
const { main } = require('../lib/cli');

test('skill --all writes Claude Code and Codex locations', async () => {
  const sys = fakeSys({ platform: 'linux' });
  assert.equal(await main(['skill', '--all', '--json'], sys), 0);
  const r = JSON.parse(sys.stdout());
  const paths = r.installed.map((i) => i.path);
  assert.deepEqual(paths, [
    path.join(sys.home, '.claude', 'skills', 'unlatch', 'SKILL.md'),
    path.join(sys.home, '.agents', 'skills', 'unlatch', 'SKILL.md'),
  ]);
  const text = fs.readFileSync(paths[0], 'utf8');
  assert.match(text, /^---\nname: unlatch\ndescription: .+\n---\n/);
  assert.match(text, /npx -y unlatch share --json/);
  assert.match(text, /mac_command/);
});

test('skill with no flag installs for the agents that are present; legacy ~/.codex/skills too', async () => {
  const sys = fakeSys({ platform: 'linux' });
  fs.mkdirSync(path.join(sys.home, '.codex', 'skills'), { recursive: true });
  assert.equal(await main(['skill', '--json'], sys), 0);
  const agents = JSON.parse(sys.stdout()).installed.map((i) => i.agent);
  assert.deepEqual(agents, ['codex', 'codex (legacy dir)']);
});

test('skill --print prints the text only', async () => {
  const sys = fakeSys({ platform: 'darwin' });
  assert.equal(await main(['skill', '--print'], sys), 0);
  assert.match(sys.stdout(), /^---\nname: unlatch/);
  assert.ok(!fs.existsSync(path.join(sys.home, '.claude')));
});

// The skill `unlatch skill` installs must be the site's SKILL.md (one
// source, site/src/SKILL.md, rendered by site/build.sh), not a drifting hand-kept copy.
const REPO = path.join(__dirname, '..', '..', '..');
function renderBrand(text) {
  const brand = JSON.parse(fs.readFileSync(path.join(REPO, 'site', 'brand.json'), 'utf8'));
  return text.replace(/\{\{\s*([\w.]+)\s*\}\}/g, (_, k) => {
    let o = brand;
    for (const p of k.split('.')) o = o[p];
    if (o === undefined) throw new Error(`unknown placeholder ${k}`);
    return String(o);
  });
}

test('the npm skill is site/src/SKILL.md rendered with brand.json (run site/build.sh after editing it)', () => {
  const want = renderBrand(fs.readFileSync(path.join(REPO, 'site', 'src', 'SKILL.md'), 'utf8'));
  assert.equal(fs.readFileSync(path.join(__dirname, '..', 'skill', 'SKILL.md'), 'utf8'), want, 'npm/unlatch/skill/SKILL.md drifted; run site/build.sh');
  assert.equal(fs.readFileSync(path.join(REPO, 'site', 'dist', 'SKILL.md'), 'utf8'), want, 'site/dist/SKILL.md drifted; run site/build.sh');
});

test('the skill carries the rules and the host_guess handling', () => {
  const text = fs.readFileSync(path.join(__dirname, '..', 'skill', 'SKILL.md'), 'utf8');
  assert.match(text, /^---\nname: unlatch\ndescription: .+\n---\n/);
  assert.match(text, /Never ask the user for a password, private key or token/);
  assert.match(text, /Never open ports/);
  assert.match(text, /Never run the Mac command yourself on the VM/);
  assert.match(text, /doctor --json/);
  assert.match(text, /`host_guess`/);
  assert.match(text, /`host_hint`/);
  assert.match(text, /<your-ssh-host>/);
});

test('README links point at the default branch (dev), never main', () => {
  const readme = fs.readFileSync(path.join(__dirname, '..', 'README.md'), 'utf8');
  assert.doesNotMatch(readme, /\/(blob|tree)\/main\//);
  assert.match(readme, /https:\/\/github\.com\/sudhanshuheda\/unlatch\/blob\/dev\/docs\/INSTALL\.md/);
  assert.match(fs.readFileSync(path.join(__dirname, '..', 'lib', 'skill.js'), 'utf8'), /Generated from site\/src\/SKILL\.md/);
});
