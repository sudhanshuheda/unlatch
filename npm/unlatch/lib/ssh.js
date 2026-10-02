'use strict';
// ssh targets, ~/.ssh/config, and a non-interactive reachability test with fix-it advice.

const path = require('node:path');

class TargetError extends Error {}

/**
 * Parse `[user@]host[:path]`, `[user@][v6addr][:path]` or `ssh://[user@]host[:port][/path]`.
 * The path defaults to `~`. Returns { user, host, port, path }.
 */
function parseTarget(raw) {
  let s = String(raw || '').trim();
  if (!s) throw new TargetError('missing ssh target, e.g. me@my-vm:~/code');
  if (s.startsWith('-')) throw new TargetError(`ssh target may not start with '-': ${s}`);
  if (/\s/.test(s.replace(/:.*$/, ''))) throw new TargetError(`whitespace in ssh target: ${s}`);
  let port = null;
  let p = null;
  if (s.startsWith('ssh://')) {
    s = s.slice(6);
    const slash = s.indexOf('/');
    if (slash >= 0) {
      p = s.slice(slash);
      s = s.slice(0, slash);
      // ssh://host/~/x means ~/x, ssh://host/abs means /abs.
      if (p.startsWith('/~')) p = p.slice(1);
    }
    const m = /^(.*?)(?::(\d+))?$/.exec(s);
    s = m[1];
    if (m[2]) port = Number(m[2]);
    const { user, host } = splitUser(s);
    return finish(user, host, port, p);
  }
  const at = s.lastIndexOf('@', s.startsWith('[') ? -1 : indexOfPathColon(s));
  const user = at > 0 ? s.slice(0, at) : null;
  let rest = at > 0 ? s.slice(at + 1) : s;
  let host;
  if (rest.startsWith('[')) {
    const close = rest.indexOf(']');
    if (close < 0) throw new TargetError(`unclosed '[' in ${raw}`);
    host = rest.slice(1, close);
    rest = rest.slice(close + 1);
    if (rest.startsWith(':')) p = rest.slice(1);
    else if (rest) throw new TargetError(`unexpected ${rest} after ] in ${raw}`);
  } else {
    const c = rest.indexOf(':');
    host = c < 0 ? rest : rest.slice(0, c);
    if (c >= 0) p = rest.slice(c + 1);
  }
  return finish(user, host, port, p);
}

function indexOfPathColon(s) {
  const c = s.indexOf(':');
  return c < 0 ? s.length : c;
}

function splitUser(s) {
  const at = s.lastIndexOf('@');
  return at > 0 ? { user: s.slice(0, at), host: s.slice(at + 1) } : { user: null, host: s };
}

function finish(user, host, port, p) {
  if (!host) throw new TargetError('missing host');
  if (/[<>]/.test(host)) {
    throw new TargetError(
      `replace ${host} with the address or ~/.ssh/config Host alias you use to ssh to the VM (what you type after \`ssh\`)`
    );
  }
  if (!/^[A-Za-z0-9._%:+-]+$/.test(host)) throw new TargetError(`invalid host ${host}`);
  if (user !== null && !/^[^\s@/:]+$/.test(user)) throw new TargetError(`invalid user ${user}`);
  if (port !== null && !(port > 0 && port < 65536)) throw new TargetError(`invalid port ${port}`);
  p = p === null || p === '' ? '~' : p;
  if (!p.startsWith('/') && p !== '~' && !p.startsWith('~/')) p = `~/${p}`; // scp semantics
  if (p.length > 1) p = p.replace(/\/+$/, '');
  return { user, host, port, path: p };
}

function destination(t) {
  return t.user ? `${t.user}@${t.host}` : t.host;
}

/** scp-style spelling, for printing. */
function formatTarget(t) {
  const h = t.host.includes(':') ? `[${t.host}]` : t.host;
  return `${t.user ? t.user + '@' : ''}${h}:${t.path}`;
}

// ---- ~/.ssh/config ----------------------------------------------------------------------

/**
 * Concrete `Host` aliases (no wildcards/negations) with the options that matter to us.
 * Follows `Include` (relative to ~/.ssh, `*` globs). `Match` blocks are skipped.
 */
function parseSshConfig(sys, file, depth = 0, seen = new Set()) {
  const hosts = [];
  if (depth > 8 || seen.has(file)) return hosts;
  seen.add(file);
  let text;
  try {
    text = sys.fs.readFileSync(file, 'utf8');
  } catch {
    return hosts;
  }
  let current = [];
  let inMatch = false;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.replace(/(^|\s)#.*$/, '').trim();
    if (!line) continue;
    const m = /^(\S+?)(?:\s*=\s*|\s+)(.*)$/.exec(line);
    if (!m) continue;
    const key = m[1].toLowerCase();
    const val = m[2].trim();
    if (key === 'host') {
      inMatch = false;
      current = [];
      for (const pat of splitWords(val)) {
        if (/[*?!]/.test(pat)) continue;
        const h = { alias: pat };
        hosts.push(h);
        current.push(h);
      }
    } else if (key === 'match') {
      inMatch = true;
      current = [];
    } else if (key === 'include') {
      for (const inc of splitWords(val)) {
        for (const f of expandInclude(sys, inc)) hosts.push(...parseSshConfig(sys, f, depth + 1, seen));
      }
    } else if (!inMatch) {
      const field = { hostname: 'hostName', user: 'user', port: 'port', identityfile: 'identityFile', proxyjump: 'proxyJump' }[key];
      if (!field) continue;
      for (const h of current) if (h[field] === undefined) h[field] = field === 'port' ? Number(val) : unquote(val);
    }
  }
  // First definition wins in ssh; keep the first of duplicate aliases.
  const byAlias = new Map();
  for (const h of hosts) if (!byAlias.has(h.alias)) byAlias.set(h.alias, h);
  return [...byAlias.values()];
}

function splitWords(s) {
  const out = [];
  const re = /"([^"]*)"|(\S+)/g;
  let m;
  while ((m = re.exec(s))) out.push(m[1] !== undefined ? m[1] : m[2]);
  return out;
}

function unquote(s) {
  return s.startsWith('"') && s.endsWith('"') ? s.slice(1, -1) : s;
}

function expandInclude(sys, pat) {
  const sshDir = path.join(sys.home, '.ssh');
  let p = pat.startsWith('~/') ? path.join(sys.home, pat.slice(2)) : pat;
  if (!path.isAbsolute(p)) p = path.join(sshDir, p);
  if (!/[*?]/.test(path.basename(p))) return [p];
  const dir = path.dirname(p);
  const re = new RegExp('^' + path.basename(p).replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.') + '$');
  try {
    return sys.fs
      .readdirSync(dir)
      .filter((f) => re.test(f))
      .sort()
      .map((f) => path.join(dir, f));
  } catch {
    return [];
  }
}

function loadSshConfig(sys) {
  return parseSshConfig(sys, path.join(sys.home, '.ssh', 'config'));
}

/**
 * If the Mac's ~/.ssh/config has an alias for this host, prefer it: the alias carries the
 * user's User, IdentityFile, ProxyJump and Port. Returns { target, alias } (alias may be null).
 */
function preferAlias(target, hosts) {
  if (hosts.some((h) => h.alias === target.host)) return { target, alias: target.host };
  const want = norm(target.host);
  const match = hosts.find((h) => {
    if (!h.hostName || !sameHost(norm(h.hostName), want)) return false;
    if (target.user && h.user && h.user !== target.user) return false;
    if (target.port && h.port && h.port !== target.port) return false;
    return true;
  });
  if (!match) return { target, alias: null };
  const user = target.user && target.user !== match.user ? target.user : null;
  return { target: { ...target, host: match.alias, user, port: target.port && target.port !== match.port ? target.port : null }, alias: match.alias };
}

function norm(h) {
  return h.toLowerCase().replace(/\.$/, '');
}

function isIp(h) {
  return /^[\d.]+$/.test(h) || h.includes(':');
}

/**
 * Only an exact (case-insensitive, trailing-dot-insensitive) HostName match reuses an alias. Any
 * short/long pair (`web` vs `web.tail1234.ts.net`, `dev` vs `dev.corp.example.com`) may name a
 * different machine — a LAN box called `web` is not the tailnet's `web` — and swapping in the wrong
 * alias would attach Finder to another host. A user who wants their alias types it.
 */
function sameHost(a, b) {
  return a === b;
}

// ---- running ssh ----------------------------------------------------------------------------

/** ssh argv (without the remote command) for a target plus options. */
function sshArgv(t, { batch = true, identity, extra = [] } = {}) {
  const a = ['-T', '-o', 'ConnectTimeout=15'];
  if (batch) a.push('-o', 'BatchMode=yes');
  if (t.port) a.push('-p', String(t.port));
  if (identity) a.push('-i', identity);
  a.push(...extra, destination(t));
  return a;
}

/** A function (remoteArgs, opts) → run result, bound to one target. */
function sshRunner(sys, t, opts = {}) {
  return (remote, runOpts = {}) => sys.run('ssh', [...sshArgv(t, opts), '--', ...remote], runOpts);
}

/**
 * Try the target without prompting. Returns { ok, kind, message, fix, uname }.
 * kind: ok | auth | hostkey | dns | timeout | refused | noroute | other
 */
function testSsh(sys, t, opts = {}) {
  const r = sys.run('ssh', [...sshArgv(t, opts), '--', 'echo', '__unlatch_ok__;', 'uname', '-sm'], { timeout: 30000 });
  if (r.status === 0 && String(r.stdout).includes('__unlatch_ok__')) {
    const uname = String(r.stdout).split('__unlatch_ok__')[1].trim().split('\n')[0] || '';
    return { ok: true, kind: 'ok', uname };
  }
  return { ok: false, ...classify(String(r.stderr), t) };
}

function classify(stderr, t) {
  const dest = destination(t);
  const s = stderr.toLowerCase();
  const tail = stderr.trim().split('\n').slice(-2).join(' ').trim();
  if (s.includes('host key verification failed') || s.includes('remote host identification has changed')) {
    return {
      kind: 'hostkey',
      message: `ssh does not trust ${t.host}'s host key yet (or it changed).`,
      fix: `Run \`ssh ${dest}\` once in Terminal and answer "yes" (if it says the key CHANGED, check with the VM's owner first).`,
    };
  }
  if (s.includes('permission denied') || s.includes('too many authentication failures') || s.includes('no supported authentication')) {
    return {
      kind: 'auth',
      message: `ssh reached ${t.host} but could not log in without a prompt.`,
      fix:
        `Use a key: \`ssh-copy-id ${dest}\` (or add your public key to ~/.ssh/authorized_keys on the VM), ` +
        'load it into your agent (`ssh-add`), or name it with --identity <file>. ' +
        'Passwords and 2FA prompts also work: the Mac app asks in a dialog.',
    };
  }
  if (s.includes('could not resolve hostname') || s.includes('name or service not known') || s.includes('nodename nor servname')) {
    return {
      kind: 'dns',
      message: `${t.host} does not resolve from this machine.`,
      fix: 'Check the spelling, your VPN/Tailscale connection (`tailscale status`), or use the IP address.',
    };
  }
  if (s.includes('timed out')) {
    return { kind: 'timeout', message: `No answer from ${t.host}.`, fix: 'Is the VM running, and is port 22 reachable (firewall, security group, VPN)?' };
  }
  if (s.includes('connection refused')) {
    return { kind: 'refused', message: `${t.host} refused the connection.`, fix: 'Is sshd running on the VM, on this port?' };
  }
  if (s.includes('no route to host') || s.includes('network is unreachable')) {
    return { kind: 'noroute', message: `No route to ${t.host}.`, fix: 'Check your network/VPN.' };
  }
  return { kind: 'other', message: `ssh ${dest} failed: ${tail || 'no output'}`, fix: `Try \`ssh ${dest}\` in Terminal to see what it needs.` };
}

/** Failures that a password/host-key dialog in the Mac app can get past. */
function promptable(kind) {
  return kind === 'auth' || kind === 'hostkey' || kind === 'other';
}

module.exports = {
  TargetError,
  parseTarget,
  destination,
  formatTarget,
  parseSshConfig,
  loadSshConfig,
  preferAlias,
  sshArgv,
  sshRunner,
  testSsh,
  classify,
  promptable,
};
