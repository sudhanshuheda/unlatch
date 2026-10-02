'use strict';
// VM-side checks run by `unlatch share` / `unlatch doctor` on Linux. Each check returns
// { id, level: 'ok'|'warn'|'fail', message, fix? }.

const path = require('node:path');

// Directories the daemon lists but does not scan or watch until opened (DESIGN §4).
const LAZY = new Set(['node_modules', '.git', 'target', '.venv', '__pycache__', '.next', 'dist', 'build', '.cache']);
const NETWORK_FS = /^(nfs.*|cifs|smb.*|fuse.*|9p|v9fs|afs|virtiofs|ceph|glusterfs|lustre|gpfs)$/;

function readInt(sys, p) {
  try {
    const n = parseInt(sys.fs.readFileSync(p, 'utf8').trim(), 10);
    return Number.isFinite(n) ? n : null;
  } catch {
    return null;
  }
}

/** Count directories the daemon would watch (lazy dirs excluded), with a time/size cap. */
function countWatchedDirs(sys, root, { maxMs = 4000, maxDirs = 2000000 } = {}) {
  const t0 = sys.now();
  let dirs = 0;
  let files = 0;
  let complete = true;
  const stack = [root];
  while (stack.length) {
    if (dirs >= maxDirs || sys.now() - t0 > maxMs) {
      complete = false;
      break;
    }
    const d = stack.pop();
    dirs++;
    let ents;
    try {
      ents = sys.fs.readdirSync(d, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const e of ents) {
      if (e.isDirectory()) {
        if (!LAZY.has(e.name)) stack.push(path.join(d, e.name));
      } else files++;
    }
  }
  return { dirs, files, complete };
}

function checkRoot(sys, root) {
  try {
    if (!sys.fs.statSync(root).isDirectory()) return { id: 'root', level: 'fail', message: `${root} is not a directory` };
  } catch (e) {
    return { id: 'root', level: 'fail', message: `${root}: ${e.code || e.message}` };
  }
  try {
    sys.fs.accessSync(root, sys.fs.constants.W_OK);
  } catch {
    return { id: 'root', level: 'warn', message: `${root} is read-only for you: the Mac can browse but not save`, fix: 'Share a folder you own.' };
  }
  return { id: 'root', level: 'ok', message: `${root} exists and is writable` };
}

function checkFsType(sys, root) {
  const r = sys.run('stat', ['-f', '-c', '%T', root], { timeout: 5000 });
  const t = String(r.stdout).trim();
  if (r.status !== 0 || !t) return { id: 'fstype', level: 'ok', message: 'filesystem type unknown' };
  if (NETWORK_FS.test(t)) {
    return {
      id: 'fstype',
      level: 'warn',
      message: `${root} is on ${t}, a network filesystem: inotify does not see changes made by other machines, so this folder is polled (changes show up within 1–30 s instead of instantly)`,
      fix: 'Share a folder on a local disk for instant updates.',
    };
  }
  return { id: 'fstype', level: 'ok', message: `filesystem: ${t}` };
}

function checkInotify(sys, root, opts) {
  const max = readInt(sys, '/proc/sys/fs/inotify/max_user_watches');
  const inst = readInt(sys, '/proc/sys/fs/inotify/max_user_instances');
  if (max === null) return { id: 'inotify', level: 'warn', message: 'cannot read inotify limits; the daemon will poll if it has to' };
  const c = countWatchedDirs(sys, root, opts);
  const n = c.complete ? `${c.dirs}` : `more than ${c.dirs}`;
  const budget = Math.floor(max / 2); // unlatchd uses at most 50% of the per-user watches (D14)
  const fix =
    'Raise the limit: echo fs.inotify.max_user_watches=1048576 | sudo tee /etc/sysctl.d/60-unlatch.conf && sudo sysctl --system';
  if (c.dirs > budget) {
    return {
      id: 'inotify',
      level: 'warn',
      message: `${n} folders to watch but the inotify budget is about ${budget} (max_user_watches=${max}); folders beyond it are polled every 1–30 s`,
      fix,
      dirs: c.dirs,
      max_user_watches: max,
    };
  }
  if (inst !== null && inst < 8) {
    return { id: 'inotify', level: 'warn', message: `max_user_instances=${inst} is low`, fix: 'sudo sysctl fs.inotify.max_user_instances=128', dirs: c.dirs, max_user_watches: max };
  }
  return { id: 'inotify', level: 'ok', message: `${n} folders to watch, limit ${max} (lazy folders like node_modules and .git are only watched once opened)`, dirs: c.dirs, max_user_watches: max };
}

function freeBytes(sys, p) {
  try {
    const s = sys.fs.statfsSync(p);
    return s.bavail * s.bsize;
  } catch {
    return null;
  }
}

function checkDisk(sys, root, installDir) {
  const out = [];
  const inst = installDir ? freeBytes(sys, installDir) : null;
  if (inst !== null && inst < 200 * 1024 * 1024) {
    out.push({ id: 'disk', level: 'warn', message: `only ${mb(inst)} free for the daemon's index in ${installDir}`, fix: 'Free some space, or set UNLATCH_HOME to a roomier local directory.' });
  }
  const r = freeBytes(sys, root);
  if (r !== null && r < 100 * 1024 * 1024) {
    out.push({ id: 'disk', level: 'warn', message: `only ${mb(r)} free under ${root}: saves from the Mac may fail`, fix: 'Free some space on the VM.' });
  }
  if (!out.length) out.push({ id: 'disk', level: 'ok', message: `disk space ok${r !== null ? ` (${mb(r)} free)` : ''}` });
  return out;
}

function mb(b) {
  return b >= 1024 ** 3 ? `${(b / 1024 ** 3).toFixed(1)} GB` : `${Math.round(b / 1024 ** 2)} MB`;
}

/** Something listening on TCP port 22 (sshd, or its systemd socket)? */
function listening22(sys) {
  for (const f of ['/proc/net/tcp', '/proc/net/tcp6']) {
    try {
      for (const line of sys.fs.readFileSync(f, 'utf8').split('\n').slice(1)) {
        const c = line.trim().split(/\s+/);
        if (c[3] === '0A' && /:0016$/.test(c[1] || '')) return true;
      }
    } catch {
      /* no procfs */
    }
  }
  return false;
}

function checkSshd(sys) {
  if (sys.env.SSH_CONNECTION) return { id: 'sshd', level: 'ok', message: 'you are logged in over ssh' };
  if (listening22(sys)) return { id: 'sshd', level: 'ok', message: 'an ssh server is listening on port 22' };
  if (sys.which('systemctl')) {
    for (const unit of ['ssh', 'sshd', 'ssh.socket']) {
      if (sys.run('systemctl', ['is-active', '--quiet', unit], { timeout: 3000 }).status === 0) {
        return { id: 'sshd', level: 'ok', message: `ssh server running (${unit})` };
      }
    }
  }
  if (sys.which('tailscale') && /"RunSSH"\s*:\s*true/.test(sys.run('tailscale', ['debug', 'prefs'], { timeout: 3000 }).stdout)) {
    return { id: 'sshd', level: 'ok', message: 'Tailscale SSH is on' };
  }
  const bin = sys.which('sshd') || (exists(sys, '/usr/sbin/sshd') ? '/usr/sbin/sshd' : null);
  if (bin) {
    return { id: 'sshd', level: 'warn', message: 'no ssh server seems to be listening (sshd is installed; it may use another port)', fix: 'sudo systemctl enable --now ssh' };
  }
  return {
    id: 'sshd',
    level: 'warn',
    message: 'no ssh server found on this machine; the Mac connects over ssh',
    fix: 'Install one (e.g. sudo apt install openssh-server) or turn on Tailscale SSH (tailscale up --ssh).',
  };
}

function exists(sys, p) {
  try {
    sys.fs.accessSync(p);
    return true;
  } catch {
    return false;
  }
}

/** logind with KillUserProcesses=yes kills the background daemon at logout unless lingering. */
function checkLinger(sys) {
  if (!sys.which('loginctl')) return null;
  let kill = false;
  const files = ['/etc/systemd/logind.conf'];
  try {
    for (const f of sys.fs.readdirSync('/etc/systemd/logind.conf.d')) if (f.endsWith('.conf')) files.push(`/etc/systemd/logind.conf.d/${f}`);
  } catch {
    /* none */
  }
  for (const f of files) {
    try {
      const m = /^\s*KillUserProcesses\s*=\s*(\S+)/im.exec(sys.fs.readFileSync(f, 'utf8'));
      if (m) kill = /^(yes|true|1|on)$/i.test(m[1]);
    } catch {
      /* unreadable */
    }
  }
  if (!kill) return null;
  const r = sys.run('loginctl', ['show-user', sys.username, '-p', 'Linger'], { timeout: 3000 });
  if (/Linger=yes/.test(r.stdout)) return null;
  return {
    id: 'linger',
    level: 'warn',
    message: 'logind kills your processes at logout (KillUserProcesses=yes), so the background daemon stops when you log out; the next connect restarts it with a rescan',
    fix: `sudo loginctl enable-linger ${sys.username}`,
  };
}

function vmChecks(sys, root, installDir, opts = {}) {
  const checks = [checkRoot(sys, root)];
  if (checks[0].level === 'fail') return checks;
  checks.push(checkFsType(sys, root), checkInotify(sys, root, opts), ...checkDisk(sys, root, installDir), checkSshd(sys));
  const l = checkLinger(sys);
  if (l) checks.push(l);
  return checks;
}

function warningsOf(checks) {
  return checks.filter((c) => c.level !== 'ok').map((c) => (c.fix ? `${c.message}. Fix: ${c.fix}` : c.message));
}

module.exports = { vmChecks, warningsOf, countWatchedDirs, LAZY, NETWORK_FS };
