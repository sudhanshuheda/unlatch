'use strict';
// `npx unlatch` on a Mac with a terminal: pick a VM, pick a folder, test ssh, connect.

const names = require('./names');
const ssh = require('./ssh');
const mac = require('./mac');

async function runWizard(sys, ui, opts = {}) {
  ui.print(`${ui.bold(names.PRODUCT)} — ${names.TAGLINE}`);
  ui.print(ui.dim("Show a folder from your Linux VM in Finder. Uses your existing ssh; nothing to configure on the VM."));
  ui.print('');

  const hosts = ssh.loadSshConfig(sys);
  let target = null;
  while (!target) {
    if (hosts.length) {
      ui.print('Hosts in ~/.ssh/config:');
      hosts.forEach((h, i) => {
        const where = [h.user && `${h.user}@`, h.hostName].filter(Boolean).join('');
        ui.print(`  ${String(i + 1).padStart(2)}. ${h.alias}${where ? ui.dim(`  (${where})`) : ''}`);
      });
    }
    const q = hosts.length ? 'VM: number, alias or user@host › ' : 'VM (user@host or ssh alias) › ';
    const a = (await sys.ask(q)).trim();
    if (!a) continue;
    const n = Number(a);
    const raw = Number.isInteger(n) && n >= 1 && n <= hosts.length ? hosts[n - 1].alias : a;
    try {
      target = ssh.parseTarget(raw.includes(':') ? raw : `${raw}:~`);
    } catch (e) {
      ui.warn(e.message);
      continue;
    }
    if (!raw.includes(':') || raw.endsWith(':')) {
      const f = (await sys.ask('Folder on the VM [~] › ')).trim();
      if (f) target = ssh.parseTarget(`${raw.replace(/:$/, '')}:${f}`);
    }

    ui.step(`checking ssh ${ssh.destination(target)}`);
    const test = ssh.testSsh(sys, target, { identity: opts.identity });
    if (test.ok) {
      ui.ok(`ssh works (${test.uname})`);
      break;
    }
    ui.warn(test.message);
    ui.print(`  Fix: ${test.fix}`);
    const choices = ssh.promptable(test.kind) ? '[c]ontinue (the app will ask in a dialog), [r]etry, [q]uit' : '[r]etry, [q]uit';
    const c = (await sys.ask(`${choices} › `)).trim().toLowerCase();
    if (c.startsWith('c') && ssh.promptable(test.kind)) break;
    if (c.startsWith('q')) return null;
    target = null;
  }

  const name = (await sys.ask(`Name in Finder [${mac.defaultName(target)}] › `)).trim();
  return mac.connect(sys, ui, { ...opts, target, name: name || undefined, force: true, sshTested: true });
}

module.exports = { runWizard };
