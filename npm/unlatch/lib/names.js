'use strict';
// Every user-facing name in the npm installer lives here, and only here.
//
// The product, the npm packages and the Rust binaries all carry the one name "Unlatch"
// (`unlatchd`, `unlatch`, `$UNLATCH_HOME`, `~/.unlatch`). To rename, change the values below
// (and the matching constants in crates/unlatch-core/src/transport/bootstrap.rs, which
// test/names.test.js cross-checks) and nothing else in npm/ needs to move.

const PRODUCT = 'Unlatch';
const TAGLINE = 'Unlatch your VM.';
const HOMEPAGE = 'https://unlatch.dev'; // intended home; not registered yet, never claim it is live
const REPO_URL = 'https://github.com/sudhanshuheda/unlatch';
const BUILD_FROM_SOURCE_URL = `${REPO_URL}#building-from-source`;
// Said next to REPO_URL links while the repository is private; set to null once it is public.
const SOURCE_NOTE = null;

// npm
const CLI = 'unlatch'; // the unscoped package and its bin
const PLATFORM_PREFIX = 'unlatch-'; // platform packages (unscoped): unlatch-linux-x64, ...

// Binaries shipped inside the platform packages.
const DAEMON_BIN = 'unlatchd'; // VM daemon (static musl)
const CLIENT_BIN = 'unlatch'; // CLI: mount (FUSE), doctor, probe, agent

// VM install directory (must equal the probe order in the Mac-side bootstrap, review D22).
const INSTALL_ENV = 'UNLATCH_HOME'; // explicit override, also exported to unlatchd
const INSTALL_XDG_SUBDIR = 'unlatch'; // $XDG_DATA_HOME/<this>
const INSTALL_HOME_DIR = '.unlatch'; // $HOME/<this>
const INSTALL_TMP_PREFIX = 'unlatch-'; // /var/tmp/<this>$UID, /tmp/<this>$UID
const DAEMON_FILE_PREFIX = 'unlatchd-'; // <prefix><crate version>-<first 16 hex of sha256>
const FUSE_FSNAME_PREFIX = 'unlatch:'; // `unlatch mount` sets fsname=unlatch:<name>

// macOS app. A platform package's manifest.json may override these; the defaults are the
// names the app ships under.
const APP_BUNDLE = 'Unlatch.app';
const APP_EXECUTABLE = 'Unlatch';
const CLOUD_STORAGE_PREFIX = 'Unlatch-'; // ~/Library/CloudStorage/<prefix><Name>

// Agent integration
const SKILL_NAME = 'unlatch';

// Environment knobs of the installer itself.
const ENV = {
  platformDir: 'UNLATCH_PLATFORM_DIR', // use this unpacked platform package (tests, dev builds)
  debug: 'UNLATCH_DEBUG',
};

module.exports = {
  PRODUCT,
  TAGLINE,
  HOMEPAGE,
  REPO_URL,
  BUILD_FROM_SOURCE_URL,
  SOURCE_NOTE,
  CLI,
  PLATFORM_PREFIX,
  DAEMON_BIN,
  CLIENT_BIN,
  INSTALL_ENV,
  INSTALL_XDG_SUBDIR,
  INSTALL_HOME_DIR,
  INSTALL_TMP_PREFIX,
  DAEMON_FILE_PREFIX,
  FUSE_FSNAME_PREFIX,
  APP_BUNDLE,
  APP_EXECUTABLE,
  CLOUD_STORAGE_PREFIX,
  SKILL_NAME,
  ENV,
  platformPackage(key) {
    return `${PLATFORM_PREFIX}${key}`;
  },
  npx(args) {
    return `npx ${CLI}${args ? ' ' + args : ''}`;
  },
};
