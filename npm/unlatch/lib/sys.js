'use strict';
// The one seam between the installer and the machine: processes, files, environment, time and
// the terminal. Everything else takes a `sys` and never touches `process`, `child_process` or
// `fs` directly, so node:test can swap in a fake (see test/helpers.js).

const cp = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const readline = require('node:readline');

function realSys(overrides = {}) {
  const env = overrides.env || process.env;
  const sys = {
    platform: process.platform,
    arch: process.arch,
    env,
    fs,
    home: env.HOME || os.homedir(),
    uid: typeof process.getuid === 'function' ? process.getuid() : -1,
    username: safe(() => os.userInfo().username) || env.USER || env.LOGNAME || 'user',
    hostname: os.hostname(),
    cwd: process.cwd(),
    stdinIsTTY: !!process.stdin.isTTY,
    stdoutIsTTY: !!process.stdout.isTTY,
    paths: { applications: '/Applications' },
    /** This installer's own package.json: its optionalDependencies name the platform packages of the release. */
    installerPackage: safe(() => require('../package.json')) || null,

    /** Run to completion, capturing output. Never throws; `error` is set if it could not run. */
    run(cmd, args = [], opts = {}) {
      const r = cp.spawnSync(cmd, args, {
        encoding: opts.encoding === undefined ? 'utf8' : opts.encoding,
        input: opts.input,
        env: opts.env || env,
        cwd: opts.cwd,
        timeout: opts.timeout,
        maxBuffer: 64 * 1024 * 1024,
        stdio: opts.stdio,
      });
      return {
        status: r.status === null ? (r.error ? 127 : 128) : r.status,
        signal: r.signal,
        stdout: r.stdout == null ? '' : r.stdout,
        stderr: r.stderr == null ? '' : r.stderr,
        error: r.error,
      };
    },

    /** Run with the terminal attached (ssh prompts, long-running tools). Returns the exit code. */
    runInherit(cmd, args = [], opts = {}) {
      const r = cp.spawnSync(cmd, args, { stdio: 'inherit', env: opts.env || env, cwd: opts.cwd });
      return r.status === null ? (r.error ? 127 : 128) : r.status;
    },

    which(cmd) {
      const dirs = (env.PATH || '').split(':').concat(['/usr/sbin', '/sbin', '/usr/local/sbin']);
      for (const d of dirs) {
        if (!d) continue;
        const p = `${d}/${cmd}`;
        try {
          fs.accessSync(p, fs.constants.X_OK);
          if (fs.statSync(p).isFile()) return p;
        } catch {
          /* next */
        }
      }
      return null;
    },

    /**
     * Plain-HTTP requests in parallel, synchronously (a short-lived child node process), each
     * capped at `timeoutMs`. `requests`: [{ url, method?, headers? }]. Returns one
     * { status, body } per request, or null for any that failed or timed out. Used only for
     * link-local cloud metadata services, never for the internet.
     */
    fetchSync(requests, { timeoutMs = 800 } = {}) {
      const r = cp.spawnSync(process.execPath, ['-e', FETCH_CHILD, String(timeoutMs)], {
        input: JSON.stringify(requests),
        encoding: 'utf8',
        timeout: timeoutMs + 2000,
        env: { PATH: env.PATH || '' },
      });
      try {
        const out = JSON.parse(r.stdout);
        return Array.isArray(out) && out.length === requests.length ? out : requests.map(() => null);
      } catch {
        return requests.map(() => null);
      }
    },

    sleep(ms) {
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
    },
    now: () => Date.now(),

    out: (s) => process.stdout.write(s),
    err: (s) => process.stderr.write(s),

    /** Ask one question on the terminal. */
    async ask(question) {
      const rl = readline.createInterface({ input: process.stdin, output: process.stderr });
      try {
        return await new Promise((resolve) => rl.question(question, (a) => resolve(a)));
      } finally {
        rl.close();
      }
    },
  };
  return Object.assign(sys, overrides);
}

// The child behind fetchSync: http only (the metadata services are plain HTTP on link-local
// addresses), no proxies, no redirects, small bodies.
const FETCH_CHILD = `
const http = require('node:http');
const ms = Number(process.argv[1]) || 800;
let input = '';
process.stdin.on('data', (d) => (input += d)).on('end', async () => {
  const reqs = JSON.parse(input || '[]');
  const one = (q) => new Promise((resolve) => {
    let done = false;
    const fin = (v) => { if (!done) { done = true; resolve(v); } };
    try {
      const req = http.request(q.url, { method: q.method || 'GET', headers: q.headers || {}, timeout: ms }, (res) => {
        let body = '';
        res.setEncoding('utf8');
        res.on('data', (c) => { body += c; if (body.length > 65536) req.destroy(); });
        res.on('end', () => fin({ status: res.statusCode, body }));
        res.on('error', () => fin(null));
      });
      req.on('timeout', () => { req.destroy(); fin(null); });
      req.on('error', () => fin(null));
      setTimeout(() => { req.destroy(); fin(null); }, ms).unref();
      req.end();
    } catch { fin(null); }
  });
  process.stdout.write(JSON.stringify(await Promise.all(reqs.map(one))));
});
`;

function safe(f) {
  try {
    return f();
  } catch {
    return undefined;
  }
}

/** POSIX sh single-quoting. */
function shQuote(s) {
  s = String(s);
  if (s !== '' && /^[A-Za-z0-9@%_+=:,./~-]+$/.test(s) && !s.startsWith('~')) return s;
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

module.exports = { realSys, shQuote };
