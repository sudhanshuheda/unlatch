'use strict';
// The npm installer must pick the same VM install directory, with the same rules, as the Mac-side
// bootstrap in crates/unlatch-core/src/transport/bootstrap.rs. Fails when the two drift.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const daemon = require('../lib/daemon');
const names = require('../lib/names');

const BOOTSTRAP = path.resolve(__dirname, '../../../crates/unlatch-core/src/transport/bootstrap.rs');
const have = fs.existsSync(BOOTSTRAP);
const rust = have ? fs.readFileSync(BOOTSTRAP, 'utf8') : '';

function shFunction(text, name) {
  const start = text.indexOf(`${name}() {`);
  assert.ok(start >= 0, `${name}() not found`);
  const end = text.indexOf('\n}\n', start);
  return text
    .slice(start, end + 2)
    .split('\n')
    .map((l) => l.trim().replace('[ "$RO" = 1 ] || ', ''))
    // Our read-only mode (status/doctor) adds guard lines the bootstrap does not need.
    .filter((l) => l && !l.startsWith('#') && !l.includes('"$RO"'))
    .join('\n');
}

const js = daemon.probeScript({ nonce: 'N', override: '', want: '', prefix: 'unlatchd-' });

test('install-directory candidates match the bootstrap, in order', { skip: !have && 'not in the repo' }, () => {
  const line = `for c in ${daemon.candidateWords().join(' ')}; do`;
  assert.ok(rust.includes(line), `bootstrap.rs no longer contains:\n${line}`);
});

test('usable() and remote_fs() rules match the bootstrap', { skip: !have && 'not in the repo' }, () => {
  // bootstrap.rs holds the script in a raw string; compare the function bodies line by line.
  for (const f of ['usable', 'remote_fs', 'hash_of']) assert.equal(shFunction(js, f), shFunction(rust, f), `${f}() drifted`);
});

test('daemon file name matches the bootstrap: unlatchd-<crate version>-<sha256[..16]>', { skip: !have && 'not in the repo' }, () => {
  assert.match(rust, /"unlatchd-\{\}-\{\}",\s*env!\("CARGO_PKG_VERSION"\),\s*&self\.sha256\[\.\.16\]/);
  assert.equal(names.DAEMON_FILE_PREFIX, 'unlatchd-');
  assert.ok(rust.includes(`UNLATCH_HOME=$dir`) && names.INSTALL_ENV === 'UNLATCH_HOME');
});

test('user-facing names live in names.js only', () => {
  const lib = path.resolve(__dirname, '../lib');
  for (const f of fs.readdirSync(lib)) {
    if (f === 'names.js') continue;
    const src = fs.readFileSync(path.join(lib, f), 'utf8');
    // No hard-coded product/app/scope strings outside names.js (comments are fine).
    const code = src.replace(/\/\/.*$/gm, '').replace(/\/\*[\s\S]*?\*\//g, '');
    for (const word of ['Unlatch.app', "'unlatch-linux", "'unlatch-darwin", "'Unlatch'"]) {
      assert.ok(!code.includes(word), `${f} hard-codes ${word}`);
    }
  }
});
