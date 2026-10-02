'use strict';
// The site's numbers must be the committed scorecard's, and its
// copy must not contradict the code (polling fallback; the Mac app does show ssh's password /
// passphrase / 2FA prompts; share cannot always know the Mac's address).

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const REPO = path.join(__dirname, '..', '..', '..');
const read = (...p) => fs.readFileSync(path.join(REPO, ...p), 'utf8');

/** { metric: { unlatch, sshfs, local, raw, target, pass } } for one `## <profile>` section. */
function scorecardSection(md, profile) {
  const start = md.indexOf(`\n## ${profile}\n`);
  assert.ok(start >= 0, `no ## ${profile} in SCORECARD.md`);
  const end = md.indexOf('\n## ', start + 1);
  const body = md.slice(start, end < 0 ? undefined : end);
  const rows = {};
  for (const line of body.split('\n')) {
    const c = line.split('|').map((x) => x.trim());
    if (c.length < 11 || !/^T\d+$/.test(c[1])) continue;
    rows[c[2]] = { unlatch: c[4], sshfs: c[5], local: c[6], raw: c[7], target: c[8], pass: c[9] };
  }
  return rows;
}

const num = (cell) => Number(String(cell).replace(/^~/, '').replace(/,/g, ''));

test('benchmarks.json matches the committed SCORECARD.md section it names, value for value', () => {
  const bench = JSON.parse(read('site', 'benchmarks.json'));
  const rows = scorecardSection(read('bench', 'results', 'SCORECARD.md'), bench.profile);
  assert.match(bench._source, new RegExp(`SCORECARD\\.md.*${bench.profile}`));
  let checked = 0;
  for (const [key, b] of Object.entries(bench)) {
    if (!b || typeof b !== 'object' || !b.metric) continue;
    const row = rows[b.metric];
    assert.ok(row, `${key}: ${b.metric} is not in the ${bench.profile} section`);
    for (const [field, col] of [['unlatch', 'unlatch'], ['sshfs', 'sshfs'], ['local', 'local']]) {
      if (b[`${field}_v`] === undefined) continue;
      assert.equal(b[`${field}_v`], num(row[col]), `${key}.${field}_v vs scorecard ${b.metric} ${col} (${row[col]})`);
      // The displayed string starts with the scorecard's own spelling of the number.
      assert.equal(num(String(b[field]).replace(/\s.*$/, '')), num(row[col]), `${key}.${field} "${b[field]}"`);
      checked++;
    }
    if (row.pass === '❌') {
      assert.equal(b.target_met, false, `${key}: the scorecard marks ${b.metric} as failing its target; say so`);
      assert.ok(b.note && /target/.test(b.note), `${key} needs a note naming the missed target`);
    }
  }
  assert.ok(checked >= 10, `only ${checked} values checked`);
});

test('the built site carries those numbers and none of the stale or false claims', () => {
  const bench = JSON.parse(read('site', 'benchmarks.json'));
  for (const file of [['site', 'dist', 'index.html']]) {
    const html = read(...file);
    const where = file.join('/');
    for (const k of ['ls_first', 'initial_sync', 'p99_meta']) {
      assert.ok(html.includes(bench[k].unlatch), `${where} lacks ${k} ${bench[k].unlatch}`);
      assert.ok(html.includes(bench[k].sshfs), `${where} lacks ${k} ${bench[k].sshfs}`);
    }
    assert.ok(html.includes(bench.p99_meta.note), `${where} must say the p99 target was missed`);
    // False before the review: unlatchd polls network filesystems and directories past the watch budget.
    assert.doesNotMatch(html, /Nothing polls|no polling/i, where);
    assert.match(html, /falls back to polling every 1–30 s/, where);
    // False before the review: the Mac app shows ssh's password/passphrase/2FA prompts (askpass).
    assert.doesNotMatch(html, /never asks for or keeps/, where);
    assert.match(html, /never stores passwords, passphrases or keys/, where);
    assert.match(html, /shows ssh's own prompt/, where);
    // Misleading before the review: share cannot always know how the Mac reaches the VM.
    assert.doesNotMatch(html, /prints the exact line/, where);
    assert.match(html, /behind NAT/, where);
    // Unsourced or superseded numbers from the 30 Sep scorecard and earlier.
    for (const stale of ['18.0 ms', '336 ms', '67 ms', '1,609 ms', '2.6 ms', '2.7 ms', '~6 ms', '1.3 s']) {
      assert.ok(!html.includes(`>${stale}<`) && !html.includes(` ${stale} `) && !html.includes(` ${stale},`), `${where} still shows ${stale}`);
    }
  }
});
