'use strict';
// Argument parsing and dispatch. `main(argv, sys)` resolves to the process exit code:
// 0 ok, 1 failed, 2 usage, 3 the user has to do something first.

const path = require('node:path');
const names = require('./names');
const platform = require('./platform');
const daemon = require('./daemon');
const ssh = require('./ssh');
const { share } = require('./share');
const linux = require('./linux-client');
const mac = require('./mac');
const { vmChecks } = require('./vmcheck');
const { installSkill } = require('./skill');
const { runWizard } = require('./wizard');
const { makeUi, UsageError, UserActionError } = require('./ui');

const VALUE_FLAGS = new Set(['port', 'identity', 'name', 'mount', 'state', 'remote-home', 'install-dir', 'timeout']);
const BOOL_FLAGS = new Set([
  'json', 'yes', 'force', 'foreground', 'no-serve', 'no-open', 'reinstall', 'use-shell-agent',
  'claude', 'codex', 'all', 'print', 'help', 'version', 'quiet',
]);
const MAC_APP_CMDS = new Set([undefined, 'connect', 'status', 'open', 'remove', 'disconnect', 'doctor', 'update', 'uninstall']);
const ALIASES = { y: 'yes', h: 'help', v: 'version', q: 'quiet' };

function parseArgs(argv) {
  const pos = [];
  const flags = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--') {
      pos.push(...argv.slice(i + 1));
      break;
    }
    if (a.startsWith('--') || (a.startsWith('-') && a.length === 2)) {
      let key = a.startsWith('--') ? a.slice(2) : ALIASES[a.slice(1)] || a.slice(1);
      let val;
      const eq = key.indexOf('=');
      if (eq >= 0) {
        val = key.slice(eq + 1);
        key = key.slice(0, eq);
      }
      if (VALUE_FLAGS.has(key)) {
        if (val === undefined) {
          if (i + 1 >= argv.length) throw new UsageError(`--${key} needs a value`);
          val = argv[++i];
        }
        flags[key] = val;
      } else if (BOOL_FLAGS.has(key)) {
        if (val !== undefined) throw new UsageError(`--${key} takes no value`);
        flags[key] = true;
      } else {
        throw new UsageError(`unknown option ${a}`);
      }
    } else {
      pos.push(a);
    }
  }
  return { cmd: pos[0], args: pos.slice(1), flags };
}

const HELP = `${names.PRODUCT}: ${names.TAGLINE}
Your cloud VM's files in Finder, as snappy as a local folder.

On the Linux VM (over ssh, or ask your coding agent to run it):
  npx ${names.CLI}                          share the current folder; prints the command for your Mac
  npx ${names.CLI} share [folder] [--json]  the same, for another folder (--json for agents)

On your Mac:
  npx ${names.CLI}                          guided setup (pick a VM and a folder)
  npx ${names.CLI} connect <user@vm>:<folder>  add it to Finder and open it
        [--port N] [--identity FILE] [--name NAME] [--no-open] [--use-shell-agent]
  npx ${names.CLI} status | open [name] | remove <name> | doctor | update | uninstall --yes

On a Linux desktop:
  npx ${names.CLI} connect <user@vm>:<folder> --mount <dir>   FUSE mount
  npx ${names.CLI} remove --mount <dir>

For your coding agent:
  npx ${names.CLI} skill [--claude|--codex|--all|--print]   install the "${names.SKILL_NAME}" skill

Every command takes --json. Exit codes: 0 ok, 1 failed, 2 usage, 3 you need to act.
`;

function packageVersion() {
  try {
    return require('../package.json').version;
  } catch {
    return 'dev';
  }
}

function targetFrom(args, flags) {
  if (!args[0]) throw new UsageError(`usage: ${names.npx('connect <user@vm>:<folder>')}`);
  let t;
  try {
    t = ssh.parseTarget(args[0]);
  } catch (e) {
    if (e instanceof ssh.TargetError) throw new UsageError(e.message);
    throw e;
  }
  if (flags.port) {
    const p = Number(flags.port);
    if (!(p > 0 && p < 65536)) throw new UsageError(`invalid --port ${flags.port}`);
    t.port = p;
  }
  return t;
}

function linuxStatus(sys, ui, flags = {}) {
  const plat = platform.resolvePlatform(sys);
  const d = platform.daemonOf(plat);
  let servers = [];
  let inst = null;
  if (d) {
    const probe = daemon.tryProbe(sys, d, flags['install-dir']);
    if (probe && probe.have) {
      inst = { dir: probe.dir, path: path.join(probe.dir, d.remoteName) };
      servers = daemon.status(sys, inst);
    }
  }
  const mounts = linux.listMounts(sys);
  const res = { ok: true, daemon: inst, servers, mounts };
  if (ui.json) return ui.emit(res);
  ui.print(inst ? `daemon: ${inst.path}` : `daemon: not installed (run \`${names.npx('share')}\`)`);
  for (const s of servers) ui.print(`  ${s.running ? ui.green('●') : ui.dim('○')} ${s.root}${s.running ? `  pid ${s.pid}` : '  stopped'}`);
  for (const m of mounts) ui.print(`mount: ${m.mountpoint}  (${m.source})`);
  if (!mounts.length && !servers.length) ui.print(ui.dim('nothing shared or mounted'));
  return res;
}

function linuxUninstall(sys, ui, flags) {
  const plat = platform.resolvePlatform(sys);
  const d = platform.daemonOf(plat);
  const probe = d && daemon.tryProbe(sys, d, flags['install-dir']);
  if (!probe || !probe.dir) {
    if (ui.json) ui.emit({ ok: true, removed: [] });
    else ui.ok('nothing installed');
    return;
  }
  const files = probe.found.map((f) => path.join(probe.dir, f));
  if (!flags.yes) {
    throw new UsageError(`This stops the ${names.PRODUCT} servers and deletes ${files.join(', ') || 'nothing'} and ${path.join(probe.dir, 'state')}. Re-run with --yes.`);
  }
  const bin = files.find((f) => f.endsWith(d.remoteName)) || files[0];
  if (bin) sys.run(bin, ['stop'], { env: daemon.daemonEnv(sys, probe.dir), timeout: 20000 });
  for (const f of files) sys.fs.rmSync(f, { force: true });
  sys.fs.rmSync(path.join(probe.dir, 'state'), { recursive: true, force: true });
  if (ui.json) ui.emit({ ok: true, removed: files, install_dir: probe.dir });
  else ui.ok(`stopped and removed ${files.length} daemon file(s) and the index state in ${probe.dir}`);
}

function printChecks(ui, checks) {
  for (const c of checks) {
    const mark = c.level === 'ok' ? ui.green('✓') : c.level === 'warn' ? ui.yellow('!') : ui.red('✗');
    ui.print(`${mark} ${c.message}`);
    if (c.fix && c.level !== 'ok') ui.print(ui.dim(`    fix: ${c.fix}`));
  }
}

async function dispatch(sys, ui, { cmd, args, flags }) {
  const isMac = sys.platform === 'darwin';
  const isLinux = sys.platform === 'linux';
  if (flags['install-dir']) flags['install-dir'] = path.resolve(sys.cwd, flags['install-dir']);
  const common = {
    identity: flags.identity,
    name: flags.name,
    force: !!flags.force,
    reinstall: !!flags.reinstall,
    shellAgent: !!flags['use-shell-agent'],
    open: !flags['no-open'],
    yes: !!flags.yes,
  };
  // Mac commands that need the app: fail first, with the reason, when this release has no Mac app.
  if (isMac && MAC_APP_CMDS.has(cmd) && !flags.mount) platform.resolvePlatform(sys);
  switch (cmd) {
    case undefined:
      if (isLinux) return share(sys, ui, { path: '.', serve: !flags['no-serve'], installDir: flags['install-dir'] });
      if (isMac) {
        if (sys.stdinIsTTY && !ui.json) return runWizard(sys, ui, common);
        throw new UsageError(`no terminal to ask in. Run \`${names.npx('connect <user@vm>:<folder>')}\` instead.`);
      }
      throw new UsageError(HELP);
    case 'share':
      return share(sys, ui, { path: args[0] || '.', serve: !flags['no-serve'], installDir: flags['install-dir'] });
    case 'connect': {
      const target = targetFrom(args, flags);
      if (flags.mount || isLinux) {
        linux.requireMountFlag({ ...flags, target });
        return linux.connectMount(sys, ui, {
          ...common,
          target,
          mount: flags.mount,
          state: flags.state,
          remoteHome: flags['remote-home'],
          foreground: !!flags.foreground,
        });
      }
      if (!isMac) throw new UsageError('connect needs macOS (Finder) or --mount <dir> on Linux');
      return mac.connect(sys, ui, { ...common, target });
    }
    case 'status':
      return isMac ? mac.status(sys, ui) : linuxStatus(sys, ui, flags);
    case 'open':
      if (!isMac) throw new UsageError('open is for the Mac app; on Linux, cd into your --mount directory');
      return mac.open(sys, ui, args[0]);
    case 'remove':
    case 'disconnect':
      if (flags.mount) return linux.unmount(sys, ui, flags.mount);
      if (!isMac) throw new UsageError('on Linux: remove --mount <dir>');
      return mac.remove(sys, ui, args[0]);
    case 'doctor': {
      let checks;
      if (isMac) checks = mac.doctor(sys, ui);
      else {
        const root = path.resolve(sys.cwd, args[0] || '.');
        const plat = platform.resolvePlatform(sys);
        const probe = daemon.tryProbe(sys, platform.daemonOf(plat), flags['install-dir']);
        checks = vmChecks(sys, root, probe && probe.dir);
        checks.unshift(
          probe && probe.dir
            ? { id: 'install', level: 'ok', message: `install directory ${probe.dir}${probe.have ? ' (daemon installed)' : ''}` }
            : { id: 'install', level: 'warn', message: 'the daemon is not installed yet', fix: `${names.npx('share')} installs it (or the Mac uploads it on first connect)` }
        );
      }
      const failed = checks.some((c) => c.level === 'fail');
      if (ui.json) ui.emit({ ok: !failed, checks });
      else printChecks(ui, checks);
      return { exit: failed ? 1 : 0 };
    }
    case 'update':
      if (isMac) return mac.update(sys, ui, common);
      return share(sys, ui, { path: args[0] || '.', serve: false, installDir: flags['install-dir'] });
    case 'uninstall':
      return isMac ? mac.uninstall(sys, ui, common) : linuxUninstall(sys, ui, flags);
    case 'skill':
      return installSkill(sys, ui, flags);
    case 'version':
      return printVersion(sys, ui);
    case 'help':
      sys.out(HELP);
      return null;
    default:
      throw new UsageError(`unknown command ${cmd}\n\n${HELP}`);
  }
}

function printVersion(sys, ui) {
  let plat = null;
  try {
    plat = platform.resolvePlatform(sys);
  } catch {
    /* not installed */
  }
  const res = { ok: true, version: packageVersion(), platform: plat ? { key: plat.key, version: plat.manifest.version, dir: plat.dir } : null };
  if (ui.json) ui.emit(res);
  else sys.out(`${names.CLI} ${res.version}${plat ? ` (${plat.key} ${plat.manifest.version})` : ' (no platform package)'}\n`);
  return res;
}

async function main(argv, sys) {
  let parsed;
  const jsonWanted = argv.includes('--json');
  let ui = makeUi(sys, { json: jsonWanted });
  try {
    parsed = parseArgs(argv);
    ui = makeUi(sys, { json: !!parsed.flags.json, quiet: !!parsed.flags.quiet });
    if (parsed.flags.help) {
      sys.out(HELP);
      return 0;
    }
    if (parsed.flags.version && !parsed.cmd) {
      printVersion(sys, ui);
      return 0;
    }
    const r = await dispatch(sys, ui, parsed);
    return r && typeof r.exit === 'number' ? r.exit : 0;
  } catch (e) {
    const code = e instanceof UsageError ? 2 : e instanceof UserActionError ? 3 : 1;
    const kind =
      e instanceof UsageError ? 'usage'
      : e instanceof UserActionError ? 'needs_user'
      : e instanceof platform.NotPublishedError ? 'not_published'
      : e instanceof platform.PlatformError ? 'platform'
      : 'failed';
    if (ui.json) ui.emit({ ok: false, error: e.message, code: kind });
    else sys.err(`${ui.red(`${names.CLI}:`)} ${e.message}\n`);
    if (sys.env[names.ENV.debug] && e.stack) sys.err(e.stack + '\n');
    return code;
  }
}

module.exports = { main, parseArgs, HELP };
