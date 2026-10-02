'use strict';
// VM mode: `npx unlatch` / `npx unlatch share [path]` on the Linux VM.

const path = require('node:path');
const names = require('./names');
const platform = require('./platform');
const daemon = require('./daemon');
const { hostCandidates } = require('./hosts');
const { vmChecks, warningsOf } = require('./vmcheck');
const { shQuote } = require('./sys');
const { UsageError } = require('./ui');

/** How the Mac should spell the folder: `~/…` under $HOME (portable across user names), else absolute. */
function macPath(sys, root) {
  const home = sys.home ? safeReal(sys, sys.home) : null;
  if (home && root === home) return '~';
  if (home && root.startsWith(home + '/')) return '~/' + root.slice(home.length + 1);
  return root;
}

function safeReal(sys, p) {
  try {
    return sys.fs.realpathSync(p);
  } catch {
    return p;
  }
}

const NONINTERACTIVE_HINT =
  'for bash, above the "If not running interactively" line in ~/.bashrc; for zsh, in ~/.zshenv';

/**
 * The Mac (and `connect --mount`) find the daemon by probing $UNLATCH_HOME, $XDG_DATA_HOME/unlatch,
 * ~/.unlatch, /var/tmp/unlatch-$UID and /tmp/unlatch-$UID in a NON-interactive ssh session. A directory
 * chosen any other way (--install-dir, or a variable exported only for interactive shells) is
 * invisible to it: it would upload its own daemon and start a second server for the same folder.
 */
function installDirWarning(sys, dir, override) {
  const env = names.INSTALL_ENV;
  if (override) {
    return (
      `--install-dir ${dir}: the Mac's connect does not look there (it checks $${env}, ` +
      `$XDG_DATA_HOME/${names.INSTALL_XDG_SUBDIR}, ~/${names.INSTALL_HOME_DIR}, /var/tmp and /tmp as a non-interactive ssh ` +
      `session sees them), so it would install its own daemon and start a second server. Set ${env}=${dir} where ` +
      `non-interactive ssh sessions see it (${NONINTERACTIVE_HINT}), or leave out --install-dir. ` +
      `status, doctor and uninstall on this VM need the same --install-dir.`
    );
  }
  const viaEnv = sys.env[env] && path.resolve(sys.env[env]) === dir ? `${env}=${sys.env[env]}` : null;
  const xdg = sys.env.XDG_DATA_HOME ? path.join(path.resolve(sys.env.XDG_DATA_HOME), names.INSTALL_XDG_SUBDIR) : null;
  const viaXdg = !viaEnv && xdg === dir ? `XDG_DATA_HOME=${sys.env.XDG_DATA_HOME}` : null;
  const via = viaEnv || viaXdg;
  if (!via) return null;
  return (
    `${via} chose the install directory ${dir}. The Mac connects with a non-interactive ssh command; if that ` +
    `variable is only set for interactive shells, it installs into a default directory and starts a second server. ` +
    `Make sure non-interactive ssh sessions see it too (${NONINTERACTIVE_HINT}).`
  );
}

function connectCommand(user, host, p, port, extra = '') {
  const h = host.includes(':') ? `[${host}]` : host;
  return names.npx(`connect ${shQuote(`${user}@${h}:${p}`)}${port ? ` --port ${port}` : ''}${extra}`);
}

function share(sys, ui, opts) {
  if (sys.platform !== 'linux') {
    throw new UsageError(`\`share\` runs on the Linux VM. On this ${sys.platform === 'darwin' ? 'Mac' : 'machine'}, run \`${names.npx('connect <user@vm>:<folder>')}\`.`);
  }
  const want = path.resolve(sys.cwd, opts.path || '.');
  let root;
  try {
    root = sys.fs.realpathSync(want);
    if (!sys.fs.statSync(root).isDirectory()) throw new Error('not a directory');
  } catch (e) {
    throw new UsageError(`${want}: ${e.code === 'ENOENT' ? 'no such folder' : e.message}`);
  }

  const plat = platform.resolvePlatform(sys);
  const d = platform.daemonOf(plat);
  ui.step(`installing the ${names.PRODUCT} daemon`);
  const inst = daemon.installLocal(sys, d, { override: opts.installDir });
  const version = daemon.version(sys, inst) || d.version;
  ui.ok(`daemon ${names.DAEMON_BIN} ${version} ${inst.installed ? 'installed in' : 'already in'} ${inst.dir}`);

  ui.step('checking this VM');
  const checks = vmChecks(sys, root, inst.dir);
  const fail = checks.find((c) => c.level === 'fail');
  if (fail) throw new UsageError(fail.message);
  const warnings = warningsOf(checks);
  const dirWarning = installDirWarning(sys, inst.dir, opts.installDir);
  if (dirWarning) warnings.push(dirWarning);

  let server = null;
  if (opts.serve !== false) {
    ui.step('starting the index server for this folder');
    try {
      server = daemon.startServe(sys, inst, root);
      ui.ok(`index server ${server.started ? 'started' : 'already running'} (pid ${server.pid})`);
    } catch (e) {
      warnings.push(`could not pre-start the index server (${e.message}); the first connect from the Mac starts it instead`);
    }
  }

  const { candidates, port, guess, hint } = hostCandidates(sys);
  const p = macPath(sys, root);
  const user = sys.username;
  for (const c of candidates) c.mac_command = connectCommand(user, c.host, p, port);
  const best = candidates[0];
  const linuxCmd = connectCommand(user, best.host, p, port, ` --mount ~/${names.CLI}`);
  const macPublished = platform.released(sys, 'darwin-universal');
  const sourceRef = `${names.BUILD_FROM_SOURCE_URL}${names.SOURCE_NOTE ? ` (${names.SOURCE_NOTE})` : ''}`;
  const macWarning = macPublished
    ? null
    : `the ${names.PRODUCT} Mac app is not published yet (this is a Linux-only pre-release), so mac_command does not work on a Mac today: ` +
      `build the app from source: ${sourceRef}. ` +
      `A Linux desktop can mount this folder now: ${linuxCmd}`;
  if (macWarning) warnings.push(macWarning);

  const result = {
    ok: true,
    product: names.PRODUCT,
    mac_command: best.mac_command,
    host_guess: guess,
    host_hint: hint,
    host_candidates: candidates,
    path: root,
    remote_path: p,
    user,
    port: port || null,
    daemon_version: version,
    daemon: {
      path: inst.path,
      install_dir: inst.dir,
      installed_now: inst.installed,
      server: server ? { running: server.running, started: server.started, pid: server.pid, state_dir: server.state } : null,
    },
    linux_command: linuxCmd,
    mac_app_published: macPublished,
    warnings,
    checks,
  };

  if (ui.json) {
    ui.emit(result);
    return result;
  }
  for (const w of warnings) if (w !== macWarning) ui.warn(w);
  ui.print('');
  ui.print(`${ui.bold(names.PRODUCT)} is sharing ${ui.bold(root)}`);
  ui.print('');
  if (!macPublished) {
    // Lead with what works today; the Mac line below would only print "not published yet".
    ui.print('On a Linux desktop, run this in a terminal:');
    ui.print('');
    ui.print(`    ${ui.cyan(linuxCmd)}`);
    ui.print('');
    ui.print(`${ui.yellow('!')} The ${names.PRODUCT} Mac app is not published yet (Linux-only pre-release).`);
    ui.print(ui.dim(`  To use a Mac today, build the app from source: ${sourceRef}`));
    ui.print(ui.dim('  Once the Mac app ships, the Mac command for this folder is:'));
    ui.print('');
  } else {
    ui.print('On your Mac, run this in Terminal:');
    ui.print('');
  }
  ui.print(`    ${macPublished ? ui.cyan(best.mac_command) : ui.dim(best.mac_command)}`);
  ui.print('');
  if (guess) ui.print(`${ui.yellow('!')} ${hint}`);
  else ui.print(ui.dim(`(${best.host}: ${best.note})`));
  if (!guess && hint) ui.print(ui.dim(hint));
  if (candidates.length > 1) {
    ui.print('');
    ui.print(ui.dim('If your Mac cannot reach that name, use one of these instead:'));
    for (const c of candidates.slice(1)) ui.print(ui.dim(`    ${c.mac_command}    # ${c.note}`));
  }
  if (macPublished) {
    ui.print('');
    ui.print(ui.dim(`Linux desktop: ${linuxCmd}`));
  }
  return result;
}

module.exports = { share, macPath, connectCommand };
