#!/usr/bin/env node
// Assemble publishable npm package directories (then `npm pack` / `npm publish` them).
//
//   node npm/scripts/assemble.mjs main     --out DIR [--version V] [--no-darwin]
//   node npm/scripts/assemble.mjs platform --key linux-x64|linux-arm64 --out DIR
//        --unlatchd FILE --unlatch FILE [--version V] [--crate-version V]
//   node npm/scripts/assemble.mjs platform --key darwin-universal --out DIR
//        --app-zip FILE --app-version V [--app-bundle Unlatch.app] [--app-executable Unlatch]
//        [--bundle-id ID] [--unlatch FILE] [--version V]
//
// --no-darwin leaves unlatch-darwin-universal out of the installer's optionalDependencies, for a
// release that ships no Mac app (e.g. a Linux-only pre-release built where the app cannot be): no
// install then asks the registry for a package that does not exist, and on a Mac the installer
// says the app is not published yet (lib/platform.js `released`). Publish no darwin package with
// such a release.
//
// Every package gets the repository's LICENSE-MIT and LICENSE-APACHE.
//
// --version defaults to npm/unlatch/package.json's; every package of a release must share it
// (`main` pins the optionalDependencies to it). --crate-version defaults to the Cargo workspace
// version: it is part of the daemon's file name on the VM (`unlatchd-<crate version>-<sha16>`,
// crates/unlatch-core/src/transport/bootstrap.rs), so it must be the version unlatchd was built as.
//
// The unlatchd binary is copied byte for byte and never stripped here: the Mac app bundles the
// same file, and the VM only skips the upload when the checksums are identical.
//
// manifest.json (schema unlatch-platform/1):
//   { schema, platform, version,
//     daemon?: { file, arch, sha256, size, crate_version, remote_name },
//     cli?:    { file, sha256, size },
//     app?:    { zip, bundle, executable, version, bundle_id, sha256 } }

import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const npmDir = path.resolve(here, '..');
const repo = path.resolve(npmDir, '..');

function args(argv) {
  const o = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith('--')) {
      const k = a.slice(2);
      const v = argv[i + 1];
      if (v === undefined || v.startsWith('--')) o[k] = true;
      else {
        o[k] = v;
        i++;
      }
    } else o._.push(a);
  }
  return o;
}

function die(msg) {
  console.error(`assemble: ${msg}`);
  process.exit(2);
}

const sha256 = (f) => createHash('sha256').update(fs.readFileSync(f)).digest('hex');
const readJson = (f) => JSON.parse(fs.readFileSync(f, 'utf8'));
const writeJson = (f, o) => fs.writeFileSync(f, JSON.stringify(o, null, 2) + '\n');

function cargoVersion() {
  const toml = fs.readFileSync(path.join(repo, 'Cargo.toml'), 'utf8');
  const m = /\[workspace\.package\][^[]*?\bversion\s*=\s*"([^"]+)"/s.exec(toml);
  if (!m) die('cannot find [workspace.package] version in Cargo.toml');
  return m[1];
}

function fresh(out) {
  fs.rmSync(out, { recursive: true, force: true });
  fs.mkdirSync(out, { recursive: true });
}

const LICENSES = ['LICENSE-MIT', 'LICENSE-APACHE'];

function copyLicenses(out) {
  for (const f of LICENSES) {
    const src = path.join(repo, f);
    if (!fs.existsSync(src)) die(`missing ${src}`);
    fs.copyFileSync(src, path.join(out, f));
  }
}

function copyExe(src, dest) {
  if (!fs.existsSync(src)) die(`missing ${src}`);
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(src, dest);
  fs.chmodSync(dest, 0o755);
  return { sha256: sha256(dest), size: fs.statSync(dest).size };
}

const ARCH = { 'linux-x64': 'x86_64', 'linux-arm64': 'aarch64' };

function platformPkg(o) {
  const key = o.key || die('--key is required');
  const out = path.resolve(o.out || die('--out is required'));
  const tpl = path.join(npmDir, 'platforms', key, 'package.json');
  if (!fs.existsSync(tpl)) die(`unknown platform ${key}`);
  const version = o.version || readJson(path.join(npmDir, 'unlatch', 'package.json')).version;
  fresh(out);
  const pkg = readJson(tpl);
  pkg.version = version;
  writeJson(path.join(out, 'package.json'), pkg);
  const manifest = { schema: 'unlatch-platform/1', platform: key, version };
  if (key.startsWith('linux-')) {
    const crate = o['crate-version'] || cargoVersion();
    const d = copyExe(path.resolve(o.unlatchd || die('--unlatchd is required')), path.join(out, 'bin', 'unlatchd'));
    manifest.daemon = {
      file: 'bin/unlatchd',
      arch: ARCH[key],
      sha256: d.sha256,
      size: d.size,
      crate_version: crate,
      remote_name: `unlatchd-${crate}-${d.sha256.slice(0, 16)}`,
    };
    const c = copyExe(path.resolve(o.unlatch || die('--unlatch is required')), path.join(out, 'bin', 'unlatch'));
    manifest.cli = { file: 'bin/unlatch', ...c };
  } else if (key === 'darwin-universal') {
    const zip = path.resolve(o['app-zip'] || die('--app-zip is required'));
    fs.mkdirSync(path.join(out, 'app'), { recursive: true });
    const bundle = o['app-bundle'] || 'Unlatch.app';
    const zdest = path.join(out, 'app', `${bundle}.zip`);
    fs.copyFileSync(zip, zdest);
    manifest.app = {
      zip: `app/${bundle}.zip`,
      bundle,
      executable: o['app-executable'] || bundle.replace(/\.app$/, ''),
      version: o['app-version'] || version,
      bundle_id: o['bundle-id'] || null,
      sha256: sha256(zdest),
    };
    if (o.unlatch) manifest.cli = { file: 'bin/unlatch', ...copyExe(path.resolve(o.unlatch), path.join(out, 'bin', 'unlatch')) };
  }
  fs.writeFileSync(
    path.join(out, 'README.md'),
    `# ${pkg.name}\n\n${pkg.description}\n\nSee https://www.npmjs.com/package/unlatch.\n`
  );
  writeJson(path.join(out, 'manifest.json'), manifest);
  copyLicenses(out);
  console.log(out);
}

function mainPkg(o) {
  const out = path.resolve(o.out || die('--out is required'));
  const src = path.join(npmDir, 'unlatch');
  const pkg = readJson(path.join(src, 'package.json'));
  const version = o.version || pkg.version;
  fresh(out);
  for (const f of pkg.files) {
    const s = path.join(src, f);
    if (fs.existsSync(s)) fs.cpSync(s, path.join(out, f), { recursive: true });
  }
  pkg.version = version;
  if (o['no-darwin']) delete pkg.optionalDependencies['unlatch-darwin-universal'];
  for (const k of Object.keys(pkg.optionalDependencies)) pkg.optionalDependencies[k] = version;
  delete pkg.scripts;
  delete pkg.devDependencies;
  writeJson(path.join(out, 'package.json'), pkg);
  copyLicenses(out);
  fs.chmodSync(path.join(out, 'bin', 'unlatch.js'), 0o755);
  console.log(out);
}

const o = args(process.argv.slice(2));
if (o._[0] === 'platform') platformPkg(o);
else if (o._[0] === 'main') mainPkg(o);
else die('usage: assemble.mjs main|platform …');
