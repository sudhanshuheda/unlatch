'use strict';
// Find the platform package (esbuild/turbo pattern): `unlatch` lists every unlatch-<key> as an
// optionalDependency with `os`/`cpu` set, so npm installs exactly the one for this machine.

const path = require('node:path');
const names = require('./names');

class PlatformError extends Error {}
/** The platform package for this machine is not part of this release (e.g. the Mac app in a Linux-only pre-release). */
class NotPublishedError extends PlatformError {}

/** The platform package key for this machine, or null if unsupported. */
function platformKey(platform, arch) {
  if (platform === 'linux' && arch === 'x64') return 'linux-x64';
  if (platform === 'linux' && arch === 'arm64') return 'linux-arm64';
  if (platform === 'darwin' && (arch === 'arm64' || arch === 'x64')) return 'darwin-universal';
  return null;
}

/** `uname -m` spelling used by the VM bootstrap. */
function unameArch(nodeArch) {
  return { x64: 'x86_64', arm64: 'aarch64' }[nodeArch] || nodeArch;
}

/**
 * Whether this installer's release ships the platform package for `key`. A release that leaves a
 * platform out (`assemble.mjs main --no-darwin`) drops it from the optionalDependencies, so an
 * install never asks the registry for a package that does not exist. Unknown (no package.json):
 * assume it ships.
 */
function released(sys, key) {
  const pkg = sys.installerPackage;
  if (!pkg || !pkg.optionalDependencies) return true;
  return Object.prototype.hasOwnProperty.call(pkg.optionalDependencies, names.platformPackage(key));
}

function notPublishedMessage(sys, key) {
  const version = (sys.installerPackage && sys.installerPackage.version) || '';
  const release = `${names.CLI}${version ? ` ${version}` : ''}`;
  const source = `  ${names.BUILD_FROM_SOURCE_URL}${names.SOURCE_NOTE ? `\n  (${names.SOURCE_NOTE})` : ''}`;
  if (key === 'darwin-universal') {
    return (
      `The ${names.PRODUCT} Mac app is not published yet. ${release} is a pre-release with the Linux side only:\n` +
      `  on the VM:            ${names.npx('share')}\n` +
      `  on a Linux desktop:   ${names.npx('connect <user@vm>:<folder> --mount <dir>')}\n` +
      `To use ${names.PRODUCT} from a Mac today, build the app from source:\n${source}`
    );
  }
  return `${names.platformPackage(key)} is not published for ${release}. Build ${names.PRODUCT} from source:\n${source}`;
}

/**
 * Locate and load the platform package. Returns { dir, manifest, key } where `manifest` is the
 * package's manifest.json (see npm/scripts/assemble.mjs for its schema).
 */
function resolvePlatform(sys, { resolver = require.resolve } = {}) {
  const key = platformKey(sys.platform, sys.arch);
  const override = sys.env[names.ENV.platformDir];
  let dir = null;
  if (override) {
    dir = path.resolve(override);
  } else {
    if (!key) {
      throw new PlatformError(
        `${names.PRODUCT} has no build for ${sys.platform}/${sys.arch}. ` +
          'Supported: Linux x64/arm64 (VM daemon, FUSE client) and macOS (Finder app).'
      );
    }
    const pkg = names.platformPackage(key);
    try {
      dir = path.dirname(resolver(`${pkg}/package.json`, { paths: [__dirname] }));
    } catch {
      if (!released(sys, key)) throw new NotPublishedError(notPublishedMessage(sys, key));
      throw new PlatformError(
        `The platform package ${pkg} is not installed.\n` +
          'It is an optional dependency, so it is skipped by `--omit=optional`, `--no-optional`\n' +
          'or an npm config with `optional=false`. Fix:\n' +
          `  npx -y ${names.CLI}@latest            (fresh npx cache)\n` +
          `  npm i -g ${names.CLI} --include=optional\n` +
          `  npm i -g ${pkg}                        (install it by hand)`
      );
    }
  }
  let manifest;
  try {
    manifest = JSON.parse(sys.fs.readFileSync(path.join(dir, 'manifest.json'), 'utf8'));
  } catch (e) {
    throw new PlatformError(`${dir}/manifest.json is missing or invalid: ${e.message}`);
  }
  return { dir, manifest, key: manifest.platform || key };
}

/** Absolute path of a file named in the manifest. */
function asset(plat, rel) {
  return path.join(plat.dir, rel);
}

/** The VM daemon from a Linux platform package: { path, arch, sha256, size, remoteName, version }. */
function daemonOf(plat) {
  const d = plat.manifest.daemon;
  if (!d) return null;
  return {
    path: asset(plat, d.file),
    arch: d.arch,
    sha256: d.sha256,
    size: d.size,
    version: d.crate_version,
    remoteName: d.remote_name,
  };
}

function cliOf(plat) {
  const c = plat.manifest.cli;
  return c ? asset(plat, c.file) : null;
}

/** macOS app facts, with names.js defaults for anything the manifest leaves out. */
function appOf(plat) {
  const a = plat.manifest.app;
  if (!a) return null;
  return {
    zip: asset(plat, a.zip),
    bundle: a.bundle || names.APP_BUNDLE,
    executable: a.executable || names.APP_EXECUTABLE,
    version: a.version,
    bundleId: a.bundle_id || null,
  };
}

module.exports = { PlatformError, NotPublishedError, released, platformKey, unameArch, resolvePlatform, daemonOf, cliOf, appOf };
