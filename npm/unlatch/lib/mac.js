'use strict';
// Mac mode: install/launch the app, then drive it through its command-line entry
//   <App>.app/Contents/MacOS/<Exe> --cli <command> … --json
// (spec: docs/INSTALL.md §"The app's command-line entry"; Swift: mac/Unlatch/CLI/CLIMain.swift).

const path = require('node:path');
const names = require('./names');
const platform = require('./platform');
const ssh = require('./ssh');
const { UsageError, UserActionError } = require('./ui');

class AppCliError extends Error {
  constructor(message, code, result) {
    super(message);
    this.code = code;
    this.result = result;
  }
}

const LOGIN_ITEMS_URL = 'x-apple.systempreferences:com.apple.LoginItems-Settings.extension';
/** Above the app CLI's own 130 s watchdog for `remove` (fileproviderd may move unsynced edits aside). */
const REMOVE_TIMEOUT_MS = 140000;

// ---- the app bundle ------------------------------------------------------------------------

function appFacts(sys) {
  const plat = platform.resolvePlatform(sys);
  const app = platform.appOf(plat);
  if (!app) throw new Error(`platform package ${plat.key} carries no macOS app`);
  return { plat, app };
}

function candidateDirs(sys) {
  return [sys.paths.applications, path.join(sys.home, 'Applications')];
}

function findInstalled(sys, bundle) {
  for (const d of candidateDirs(sys)) {
    const p = path.join(d, bundle);
    if (exists(sys, path.join(p, 'Contents', 'Info.plist'))) return p;
  }
  return null;
}

function exists(sys, p) {
  try {
    sys.fs.accessSync(p);
    return true;
  } catch {
    return false;
  }
}

function plistValue(sys, appPath, key) {
  const r = sys.run('/usr/libexec/PlistBuddy', ['-c', `Print :${key}`, path.join(appPath, 'Contents', 'Info.plist')]);
  return r.status === 0 ? String(r.stdout).trim() : null;
}

function writableDir(sys, dir) {
  try {
    sys.fs.accessSync(dir, sys.fs.constants.W_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * Install or update the app from the platform package's zip. Unzips with `ditto -x -k` (keeps
 * the code signature and the stapled notarization ticket), swaps it in next to the old copy,
 * clears quarantine. Never replaces a newer installed app unless `reinstall` is set.
 * Returns { path, action: 'current'|'installed'|'updated'|'newer', version }.
 */
function installApp(sys, ui, { app }, { reinstall = false } = {}) {
  const existing = findInstalled(sys, app.bundle);
  if (existing && !reinstall) {
    const v = plistValue(sys, existing, 'CFBundleShortVersionString');
    const cmp = v && app.version ? compareVersions(v, app.version) : null;
    if (cmp === 0 || (cmp === null && v && v === app.version)) return { path: existing, action: 'current', version: v };
    if (cmp > 0) {
      // An older unlatch (stale npx cache, old global install, pinned agent) must never quit and
      // replace a newer app: keep it, and say how to get a matching installer.
      const warning =
        `installed ${app.bundle} ${v} is newer than this ${names.CLI} (${app.version}); keeping it. ` +
        `Run \`${names.npx()}@latest …\` to use the matching installer, or --reinstall to go back to ${app.version}.`;
      ui.warn(warning);
      return { path: existing, action: 'newer', version: v, packaged: app.version, warning };
    }
  }
  let destDir = existing ? path.dirname(existing) : sys.paths.applications;
  if (!writableDir(sys, destDir)) {
    destDir = path.join(sys.home, 'Applications');
    sys.fs.mkdirSync(destDir, { recursive: true });
  }
  const dest = path.join(destDir, app.bundle);
  ui.step(`${existing ? 'updating' : 'installing'} ${app.bundle} ${app.version || ''} in ${destDir}`);
  const tmp = sys.fs.mkdtempSync(path.join(destDir, `.${names.CLI}-`));
  try {
    const unzip = sys.run('ditto', ['-x', '-k', app.zip, tmp]);
    if (unzip.status !== 0) throw new Error(`ditto -x -k ${app.zip}: ${String(unzip.stderr).trim()}`);
    const staged = path.join(tmp, app.bundle);
    if (!exists(sys, path.join(staged, 'Contents', 'Info.plist'))) throw new Error(`${app.zip} does not contain ${app.bundle}`);
    if (existing) {
      quitApp(sys, existing, app);
      const old = path.join(tmp, `${app.bundle}.old`);
      sys.fs.renameSync(existing, old);
    }
    sys.fs.renameSync(staged, dest);
  } finally {
    sys.fs.rmSync(tmp, { recursive: true, force: true });
  }
  // npm does not set quarantine, but an unzip tool might have; never fatal.
  sys.run('xattr', ['-dr', 'com.apple.quarantine', dest]);
  return { path: dest, action: existing ? 'updated' : 'installed', version: app.version };
}

/**
 * Compare two version strings (semver: `MAJOR.MINOR.PATCH[-pre][+build]`, missing parts are 0,
 * a leading `v` is ignored). Returns <0, 0 or >0, or null when either is not a version, so the
 * caller can fall back to "different = replace".
 */
function compareVersions(a, b) {
  const parse = (s) => {
    const m = /^v?(\d+(?:\.\d+)*)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(String(s || '').trim());
    if (!m) return null;
    return { nums: m[1].split('.').map(Number), pre: m[2] ? m[2].split('.') : [] };
  };
  const x = parse(a);
  const y = parse(b);
  if (!x || !y) return null;
  for (let i = 0; i < Math.max(x.nums.length, y.nums.length); i++) {
    const d = (x.nums[i] || 0) - (y.nums[i] || 0);
    if (d) return d;
  }
  // A pre-release sorts before its release; identifiers compare numerically when both are numbers.
  if (!x.pre.length || !y.pre.length) return (y.pre.length ? 1 : 0) - (x.pre.length ? 1 : 0);
  for (let i = 0; i < Math.max(x.pre.length, y.pre.length); i++) {
    const p = x.pre[i];
    const q = y.pre[i];
    if (p === undefined) return -1;
    if (q === undefined) return 1;
    const pn = /^\d+$/.test(p);
    const qn = /^\d+$/.test(q);
    if (pn && qn && Number(p) !== Number(q)) return Number(p) - Number(q);
    if (pn !== qn) return pn ? -1 : 1;
    if (!pn && p !== q) return p < q ? -1 : 1;
  }
  return 0;
}

function exePath(appPath, app) {
  return path.join(appPath, 'Contents', 'MacOS', app.executable);
}

function quitApp(sys, appPath, app) {
  sys.run('osascript', ['-e', `quit app ${JSON.stringify(appPath)}`], { timeout: 10000 });
  sys.run('pkill', ['-x', app.executable]);
}

/** MQ-061: an app nobody launched has no extension registered. Launch once, hidden. */
function launch(sys, appPath) {
  const r = sys.run('open', ['-g', appPath]);
  if (r.status !== 0) throw new Error(`open -g ${appPath}: ${String(r.stderr).trim()}`);
}

// ---- the app's --cli -----------------------------------------------------------------------

function appCli(sys, appPath, app, args, { timeout = 60000 } = {}) {
  const r = sys.run(exePath(appPath, app), ['--cli', ...args, '--json'], { timeout });
  let j = null;
  try {
    j = JSON.parse(String(r.stdout).trim());
  } catch {
    j = null;
  }
  if (!j) {
    throw new AppCliError(`${app.executable} --cli ${args[0]} gave no JSON (exit ${r.status}): ${String(r.stderr).trim().slice(-400)}`, 'no_json', null);
  }
  if (j.ok === false) throw new AppCliError(j.error || `${args[0]} failed`, j.code || 'failed', j);
  return j;
}

/** Wait until the agent answers; handle the Login Items approval. Returns the status object. */
function waitAgent(sys, ui, appPath, app, { timeoutMs = 30000, approvalMs = 180000 } = {}) {
  const t0 = sys.now();
  let asked = false;
  let last = null;
  for (;;) {
    try {
      const st = appCli(sys, appPath, app, ['status'], { timeout: 15000 });
      if (st.agent === 'enabled' || st.agent === undefined) return st;
      if (st.agent === 'requires_approval') {
        if (!asked) {
          asked = true;
          ui.warn(`macOS wants your OK to run ${names.PRODUCT} in the background.`);
          ui.warn(`Turn on ${names.PRODUCT} in System Settings → General → Login Items & Extensions (opening it now).`);
          sys.run('open', [LOGIN_ITEMS_URL]);
        }
        if (sys.now() - t0 > approvalMs || !sys.stdinIsTTY) {
          throw new UserActionError(`Allow ${names.PRODUCT} under System Settings → General → Login Items & Extensions, then run this again.`);
        }
      } else if (st.agent === 'not_registered' || st.agent === 'not_found') {
        appCli(sys, appPath, app, ['register-agent']);
      }
      last = st.agent;
    } catch (e) {
      if (e instanceof UserActionError) throw e;
      if (e instanceof AppCliError && e.code === 'requires_approval') {
        last = 'requires_approval';
      } else {
        last = e.message;
      }
      if (sys.now() - t0 > timeoutMs && !asked) throw new Error(`the ${names.PRODUCT} background agent did not answer: ${last}`);
    }
    sys.sleep(1000);
  }
}

/** Poll until the domain is Live. Returns the domain. */
function waitLive(sys, ui, appPath, app, id, { timeoutMs = 180000 } = {}) {
  const t0 = sys.now();
  let shown = '';
  for (;;) {
    const st = appCli(sys, appPath, app, ['status'], { timeout: 15000 });
    const dom = (st.domains || []).find((x) => x.id === id);
    if (!dom) throw new Error(`the app no longer lists ${id}`);
    const line = `${dom.state}${dom.detail ? `: ${dom.detail}` : ''}`;
    if (line !== shown && dom.state !== 'Live') ui.step(line);
    shown = line;
    if (dom.state === 'Live') return dom;
    if (dom.state === 'NeedsUser') throw new UserActionError(`${names.PRODUCT} needs you: ${dom.detail || 'see the menu bar'}`);
    if (dom.state === 'Paused') throw new UserActionError(`Paused: ${dom.detail}. Choose in the ${names.PRODUCT} menu.`);
    if (sys.now() - t0 > timeoutMs) {
      throw new Error(`still ${line} after ${Math.round(timeoutMs / 1000)} s${dom.last_error ? ` (${dom.last_error})` : ''}; it keeps trying in the background`);
    }
    sys.sleep(1000);
  }
}

// ---- commands ------------------------------------------------------------------------------

function defaultName(t) {
  const short = t.host.split('.')[0];
  if (t.path === '~' || t.path === '/') return short;
  return `${short}-${path.basename(t.path)}`;
}

function sameDomain(dom, t) {
  return dom.host === ssh.destination(t) && (dom.port || null) === (t.port || null) && dom.root === t.path;
}

/** Make sure the app is installed, launched and its agent answers. */
function ready(sys, ui, opts = {}) {
  const facts = appFacts(sys);
  const inst = installApp(sys, ui, facts, { reinstall: opts.reinstall });
  if (inst.action === 'installed' || inst.action === 'updated') ui.ok(`${facts.app.bundle} ${inst.action} (${inst.path})`);
  launch(sys, inst.path);
  if (inst.action === 'updated') {
    // MQ-062/063: a replaced bundle needs its agent re-registered (unregister, wait, register).
    ui.step('re-registering the background agent for the new version');
    try {
      appCli(sys, inst.path, facts.app, ['repair-agent'], { timeout: 60000 });
    } catch (e) {
      ui.warn(`agent repair: ${e.message}`);
    }
  }
  const st = waitAgent(sys, ui, inst.path, facts.app, opts);
  return { ...facts, appPath: inst.path, install: inst, status: st };
}

function checkSsh(sys, ui, t, opts) {
  ui.step(`checking ssh ${ssh.destination(t)}`);
  const test = ssh.testSsh(sys, t, { identity: opts.identity });
  if (test.ok) {
    ui.ok(`ssh works (${test.uname || 'remote'})`);
    if (test.uname && !/linux/i.test(test.uname)) ui.warn(`the VM reports ${test.uname}; ${names.PRODUCT} needs Linux on the VM`);
  } else if (ssh.promptable(test.kind)) {
    ui.warn(`${test.message} ${names.PRODUCT} will ask in a dialog; to skip that next time: ${test.fix}`);
  } else if (!opts.force) {
    throw new UserActionError(`${test.message}\n  Fix: ${test.fix}\n  (--force tries anyway)`);
  } else {
    ui.warn(test.message);
  }
}

function connect(sys, ui, opts) {
  let t = opts.target;
  const hosts = ssh.loadSshConfig(sys);
  const pref = ssh.preferAlias(t, hosts);
  if (pref.alias && pref.alias !== t.host) ui.ok(`using your ~/.ssh/config alias "${pref.alias}" for ${t.host}`);
  t = pref.target;

  // The wizard has already tested (and explained) ssh.
  if (!opts.sshTested) checkSsh(sys, ui, t, opts);

  const r = ready(sys, ui, opts);
  const { app, appPath } = r;
  let dom = (r.status.domains || []).find((x) => sameDomain(x, t));
  if (dom) {
    ui.ok(`${ssh.formatTarget(t)} is already added as "${dom.name}"`);
  } else {
    const name = opts.name || defaultName(t);
    ui.step(`adding "${name}" (answer any ssh dialogs that appear)`);
    const args = ['add', '--name', name, '--host', ssh.destination(t), '--root', t.path];
    if (t.port) args.push('--port', String(t.port));
    if (opts.identity) args.push('--identity', opts.identity);
    if (opts.shellAgent) args.push('--use-shell-agent');
    dom = appCli(sys, appPath, app, args, { timeout: 300000 }).domain;
  }
  dom = waitLive(sys, ui, appPath, app, dom.id, opts);
  const opened = appCli(sys, appPath, app, opts.open === false ? ['open', dom.id, '--no-reveal'] : ['open', dom.id]);
  const warnings = r.install.warning ? [r.install.warning] : [];
  const result = { ok: true, mode: 'finder', domain: { ...dom, path: opened.path || dom.path }, app: appPath, target: ssh.formatTarget(t), warnings };
  if (ui.json) ui.emit(result);
  else {
    ui.print('');
    ui.print(`${ui.bold(ssh.formatTarget(t))} is in Finder: ${ui.bold(result.domain.path || dom.name)}`);
    ui.print(ui.dim(`Status: ${names.npx('status')}   Remove: ${names.npx(`remove ${dom.name}`)}`));
  }
  return result;
}

function installedOrThrow(sys) {
  const { app } = appFacts(sys);
  const p = findInstalled(sys, app.bundle);
  if (!p) throw new UserActionError(`${app.bundle} is not installed. Run \`${names.npx('connect <user@vm>:<folder>')}\` or \`${names.npx()}\`.`);
  return { app, appPath: p };
}

function status(sys, ui) {
  const { app, appPath } = installedOrThrow(sys);
  const st = appCli(sys, appPath, app, ['status']);
  if (ui.json) return ui.emit({ ...st, app: appPath });
  ui.print(`${names.PRODUCT} ${st.app_version || ''} (${appPath}), background agent: ${st.agent}`);
  if (!(st.domains || []).length) ui.print(ui.dim('No VMs added yet.'));
  for (const d of st.domains || []) {
    const mark = d.state === 'Live' ? ui.green('●') : d.state === 'NeedsUser' || d.state === 'Paused' ? ui.red('●') : ui.yellow('●');
    ui.print(`${mark} ${d.name}  ${d.host}${d.port ? ':' + d.port : ''}:${d.root}  ${d.state}${d.detail ? ` (${d.detail})` : ''}`);
    if (d.path) ui.print(ui.dim(`    ${d.path}`));
  }
  return st;
}

function pick(st, which) {
  const ds = st.domains || [];
  if (!which) {
    if (ds.length === 1) return ds[0];
    throw new UsageError(ds.length ? `several VMs: name one of ${ds.map((d) => d.name).join(', ')}` : 'no VMs added yet');
  }
  const d = ds.find((x) => x.id === which || x.name === which);
  if (!d) throw new UsageError(`no VM named ${which} (have: ${ds.map((x) => x.name).join(', ') || 'none'})`);
  return d;
}

function open(sys, ui, which) {
  const { app, appPath } = installedOrThrow(sys);
  const d = pick(appCli(sys, appPath, app, ['status']), which);
  const o = appCli(sys, appPath, app, ['open', d.id]);
  if (ui.json) ui.emit({ ok: true, id: d.id, path: o.path });
  else ui.ok(`opened ${o.path}`);
}

function remove(sys, ui, which) {
  const { app, appPath } = installedOrThrow(sys);
  const d = pick(appCli(sys, appPath, app, ['status']), which);
  const r = appCli(sys, appPath, app, ['remove', d.id], { timeout: REMOVE_TIMEOUT_MS });
  if (ui.json) ui.emit({ ok: true, removed: d.id, preserved: r.preserved || null });
  else {
    ui.ok(`removed ${d.name} (files on the VM are untouched)`);
    if (r.preserved) ui.warn(`edits that had not reached the VM were kept in ${r.preserved}`);
  }
}

function update(sys, ui, opts) {
  const r = ready(sys, ui, { ...opts, reinstall: opts.reinstall });
  const res = { ok: true, app: r.appPath, action: r.install.action, version: r.install.version, warnings: r.install.warning ? [r.install.warning] : [] };
  if (ui.json) ui.emit(res);
  else if (r.install.action === 'current') ui.ok(`${r.app.bundle} ${r.install.version} is current (use \`${names.npx()}@latest update\` for newer releases)`);
  else if (r.install.action === 'newer') ui.ok(`${r.app.bundle} ${r.install.version} kept (newer than this installer's ${r.install.packaged})`);
  else ui.ok(`${r.app.bundle} ${r.install.action} to ${r.install.version}`);
  return res;
}

/** MQ-063 order: remove every domain first (the agent must still be alive), then the agent, then the bundle. */
function uninstall(sys, ui, opts) {
  const { app } = appFacts(sys);
  const appPath = findInstalled(sys, app.bundle);
  if (!appPath) {
    if (ui.json) ui.emit({ ok: true, removed: [], app: null });
    else ui.ok(`${app.bundle} is not installed`);
    return;
  }
  if (!opts.yes) throw new UsageError(`This removes every VM from Finder and deletes ${appPath}. Re-run with --yes to confirm.`);
  const removed = [];
  const preserved = [];
  try {
    const st = appCli(sys, appPath, app, ['status']);
    for (const d of st.domains || []) {
      ui.step(`removing ${d.name}`);
      const r = appCli(sys, appPath, app, ['remove', d.id], { timeout: REMOVE_TIMEOUT_MS });
      removed.push(d.id);
      if (r.preserved) {
        preserved.push(r.preserved);
        ui.warn(`edits that had not reached ${d.name} were kept in ${r.preserved}`);
      }
    }
  } catch (e) {
    if (!opts.force) throw new Error(`could not remove the Finder locations first (${e.message}); --force skips this`);
    ui.warn(`skipping domain removal: ${e.message}`);
  }
  try {
    appCli(sys, appPath, app, ['unregister-agent']);
  } catch (e) {
    ui.warn(`unregister agent: ${e.message}`);
  }
  quitApp(sys, appPath, app);
  sys.fs.rmSync(appPath, { recursive: true, force: true });
  if (ui.json) ui.emit({ ok: true, removed, preserved, app: appPath });
  else ui.ok(`removed ${removed.length} VM(s) and ${appPath}. Files on your VMs are untouched.`);
}

/** Mac-side doctor: app, agent, extension registration, ssh to each VM (+ `unlatch doctor`). */
function doctor(sys, ui) {
  const checks = [];
  const add = (id, level, message, fix) => checks.push({ id, level, message, fix });
  const { plat, app } = appFacts(sys);
  const appPath = findInstalled(sys, app.bundle);
  if (!appPath) add('app', 'fail', `${app.bundle} is not installed`, names.npx());
  else {
    add('app', 'ok', `${appPath} (${plistValue(sys, appPath, 'CFBundleShortVersionString') || '?'})`);
    const bid = plistValue(sys, appPath, 'CFBundleIdentifier');
    if (bid) {
      const pk = sys.run('pluginkit', ['-m', '-i', `${bid}.fileprovider`]);
      if (String(pk.stdout).trim()) add('extension', 'ok', 'Finder extension registered');
      else add('extension', 'fail', 'Finder extension is not registered (MQ-061/081)', `open -g ${JSON.stringify(appPath)}   (then check System Settings → Login Items & Extensions)`);
    }
    try {
      const st = appCli(sys, appPath, app, ['status'], { timeout: 15000 });
      add('agent', st.agent === 'enabled' ? 'ok' : 'warn', `background agent: ${st.agent}`, st.agent === 'enabled' ? undefined : 'System Settings → General → Login Items & Extensions');
      const cli = platform.cliOf(plat);
      for (const d of st.domains || []) {
        const t = { ...ssh.parseTarget(`${d.host}:${d.root}`), port: d.port || null };
        const s = ssh.testSsh(sys, t, { identity: d.identity });
        add(`ssh:${d.name}`, s.ok ? 'ok' : 'warn', s.ok ? `${d.name}: ssh ok, ${d.state}` : `${d.name}: ${s.message}`, s.ok ? undefined : s.fix);
        if (s.ok && cli && exists(sys, cli)) {
          const r = sys.run(cli, ['doctor', '--host', ssh.destination(t), '--root', d.root, '--json', ...(t.port ? ['--ssh-arg=-p', `--ssh-arg=${t.port}`] : [])], { timeout: 120000 });
          add(`vm:${d.name}`, r.status === 0 ? 'ok' : 'warn', `${d.name}: \`${names.CLIENT_BIN} doctor\` ${r.status === 0 ? 'passed' : 'found problems'}`, r.status === 0 ? undefined : `${cli} doctor --host ${ssh.destination(t)} --root ${d.root}`);
        }
      }
    } catch (e) {
      add('agent', 'fail', `background agent not answering: ${e.message}`, `open -g ${JSON.stringify(appPath)}`);
    }
  }
  return checks;
}

module.exports = {
  AppCliError,
  installApp,
  compareVersions,
  findInstalled,
  launch,
  appCli,
  waitAgent,
  waitLive,
  ready,
  connect,
  status,
  open,
  remove,
  update,
  uninstall,
  doctor,
  defaultName,
  exePath,
};
