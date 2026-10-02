#!/usr/bin/env node
// Publish assembled npm package directories, idempotently and in a safe order.
//
//   node npm/scripts/publish.mjs [--tag TAG] [--provenance] DIR...
//
// * Order: every platform package (unlatch-<platform>) first, the installer last,
//   whatever order the directories are given in. The installer pins the platform packages in
//   optionalDependencies, so it must never be live without them.
// * Idempotent: a `name@version` that is already on the registry is skipped (npm refuses to
//   publish over a version, E403). So "Re-run failed jobs" after a partial failure, or a
//   workflow_dispatch for the same tag, finishes the remaining packages.
// * Fail closed: if the registry cannot say whether a version exists (network, auth), stop
//   rather than guess. Before the installer goes out, every pinned platform package must be on
//   the registry.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

function die(msg) {
  console.error(`publish: ${msg}`);
  process.exit(1);
}

const argv = process.argv.slice(2);
let tag = null;
let provenance = false;
const dirs = [];
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  if (a === '--tag') tag = argv[++i];
  else if (a === '--provenance') provenance = true;
  else if (a.startsWith('--')) die(`unknown option ${a}`);
  else dirs.push(a);
}
if (!dirs.length) die('usage: publish.mjs [--tag TAG] [--provenance] DIR...');

const pkgs = dirs.map((dir) => {
  const pj = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8'));
  return { dir, name: pj.name, version: pj.version, pins: pj.optionalDependencies || {} };
});
// Platform packages (they pin nothing) first, the installer (it pins them) last; stable
// within each group.
const pinsAny = (p) => Number(Object.keys(p.pins).length > 0);
pkgs.sort((a, b) => pinsAny(a) - pinsAny(b));

/** true / false, or exits when the registry gives no clear answer. */
function published(name, version) {
  const spec = `${name}@${version}`;
  const r = spawnSync('npm', ['view', spec, 'version', '--json'], { encoding: 'utf8' });
  // Older npm: exit 0 with empty output for a missing version of an existing package.
  if (r.status === 0) return String(r.stdout).trim() !== '';
  // Current npm: E404 for a missing version ("No match found for version") or package.
  if (/\bE404\b/.test(String(r.stderr) + String(r.stdout))) return false;
  die(`cannot tell whether ${spec} is published (npm view exited ${r.status}): ${String(r.stderr).trim().split('\n').slice(-2).join(' ')}`);
}

// Published by this run: trusted without asking again (the registry's read side can lag a
// fresh publish by a while).
const live = new Set();
for (const p of pkgs) {
  const spec = `${p.name}@${p.version}`;
  if (published(p.name, p.version)) {
    console.log(`skip ${spec} (already published)`);
    live.add(spec);
    continue;
  }
  for (const [dep, ver] of Object.entries(p.pins)) {
    if (!live.has(`${dep}@${ver}`) && !published(dep, ver)) die(`not publishing ${spec}: its pinned ${dep}@${ver} is not on the registry`);
  }
  const args = ['publish', '--access', 'public'];
  if (tag) args.push('--tag', tag);
  if (provenance) args.push('--provenance');
  console.log(`publish ${spec} from ${p.dir}`);
  const r = spawnSync('npm', args, { cwd: p.dir, stdio: 'inherit' });
  if (r.status !== 0) die(`npm publish ${spec} failed (exit ${r.status}); re-run to finish the rest`);
  live.add(spec);
}
