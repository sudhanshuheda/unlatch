'use strict';
// On a cloud VM behind 1:1 NAT (AWS/GCP/Azure) without Tailscale, the
// address sshd sees is private and unreachable from the Mac. `share` must find the public
// address (cloud metadata), or say plainly that the host is a guess and how to fix it, never
// lead with a private address that cannot work.

const test = require('node:test');
const assert = require('node:assert/strict');
const cp = require('node:child_process');
const { hostCandidates } = require('../lib/hosts');
const { realSys } = require('../lib/sys');

const IMDS = 'http://169.254.169.254';

/** A fake sys for hostCandidates: scripted metadata, `ip route get`, `hostname -f`. */
function hostSys({ ssh, metadata = {}, route = null, fqdn = 'ip-10-0-1-23.ec2.internal', hostname = 'ip-10-0-1-23', tailscale = null }) {
  const fetches = [];
  return {
    fetches,
    env: ssh === undefined ? {} : { SSH_CONNECTION: ssh },
    hostname,
    which: (c) => (c === 'tailscale' && tailscale ? '/usr/bin/tailscale' : c === 'ip' ? '/usr/sbin/ip' : null),
    run(cmd, args) {
      if (cmd === 'tailscale') return { status: 0, stdout: JSON.stringify(tailscale) };
      if (cmd === 'hostname') return { status: 0, stdout: fqdn + '\n' };
      if (cmd === 'ip') return route ? { status: 0, stdout: route } : { status: 1, stdout: '' };
      return { status: 127, stdout: '' };
    },
    fetchSync(reqs, opts) {
      fetches.push({ reqs, opts });
      return reqs.map((q) => {
        for (const [k, v] of Object.entries(metadata)) {
          const [method, url] = k.split(' ');
          if (method === (q.method || 'GET') && q.url === url) return typeof v === 'function' ? v(q) : v;
        }
        return null;
      });
    },
  };
}

const AWS = {
  [`PUT ${IMDS}/latest/api/token`]: (q) =>
    q.headers['X-aws-ec2-metadata-token-ttl-seconds'] ? { status: 200, body: 'TOKEN' } : { status: 400, body: '' },
  [`GET ${IMDS}/latest/meta-data/public-ipv4`]: (q) =>
    q.headers['X-aws-ec2-metadata-token'] === 'TOKEN' ? { status: 200, body: '54.1.2.3' } : { status: 401, body: '' },
};
const GCP = {
  [`GET ${IMDS}/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip`]: (q) =>
    q.headers['Metadata-Flavor'] === 'Google' ? { status: 200, body: '35.9.8.7' } : { status: 403, body: '' },
};
const AZURE = {
  [`GET ${IMDS}/metadata/instance/network/interface/0/ipv4/ipAddress/0/publicIpAddress?api-version=2021-02-01&format=text`]: (q) =>
    q.headers.Metadata === 'true' ? { status: 200, body: '20.30.40.50' } : { status: 400, body: '' },
};

const NAT = '203.0.113.5 50000 10.0.1.23 22'; // public client → private server: 1:1 NAT

for (const [cloud, md, ip] of [['aws', AWS, '54.1.2.3'], ['gcp', GCP, '35.9.8.7'], ['azure', AZURE, '20.30.40.50']]) {
  test(`NAT on ${cloud}: the metadata public address leads, the private one is marked unreachable`, () => {
    const sys = hostSys({ ssh: NAT, metadata: md });
    const r = hostCandidates(sys);
    assert.equal(r.candidates[0].host, ip);
    assert.equal(r.candidates[0].source, 'cloud-metadata');
    assert.match(r.candidates[0].note, new RegExp(cloud === 'aws' ? 'AWS' : cloud === 'gcp' ? 'Google Cloud' : 'Azure'));
    const priv = r.candidates.find((c) => c.host === '10.0.1.23');
    assert.match(priv.note, /private/);
    assert.match(priv.note, /cannot reach/);
    assert.equal(r.guess, false);
    assert.match(r.hint, /~\/\.ssh\/config/);
    // Short timeouts: never stall share on a machine without a metadata service.
    for (const f of sys.fetches) assert.ok(f.opts.timeoutMs <= 1000, `timeout ${f.opts.timeoutMs}`);
  });
}

test('NAT, no metadata answer: the first candidate is a placeholder and the hint says how to fix it', () => {
  const sys = hostSys({ ssh: NAT });
  const r = hostCandidates(sys);
  assert.equal(r.candidates[0].source, 'placeholder');
  assert.equal(r.candidates[0].host, '<your-ssh-host>');
  assert.equal(r.guess, true);
  assert.match(r.hint, /10\.0\.1\.23/);
  assert.match(r.hint, /private/);
  assert.match(r.hint, /Replace <your-ssh-host> with the address or ~\/\.ssh\/config Host alias/);
  assert.match(r.hint, /--port/);
  // The private address and the .internal name are still listed, but explained.
  assert.ok(r.candidates.some((c) => c.host === '10.0.1.23' && /cannot reach/.test(c.note)));
  assert.ok(r.candidates.some((c) => c.host === 'ip-10-0-1-23.ec2.internal' && /cloud-internal/.test(c.note)));
});

test('metadata answers that are not public IPv4 addresses are ignored', () => {
  for (const body of ['<html>nope</html>', '10.0.0.9', '', '169.254.1.1', '100.100.1.1']) {
    const md = { [`GET ${IMDS}/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip`]: { status: 200, body } };
    const r = hostCandidates(hostSys({ ssh: NAT, metadata: md }));
    assert.equal(r.candidates[0].source, 'placeholder', body);
  }
  const md404 = { [`GET ${IMDS}/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip`]: { status: 404, body: '35.9.8.7' } };
  assert.equal(hostCandidates(hostSys({ ssh: NAT, metadata: md404 })).candidates[0].source, 'placeholder');
});

test('public sshd address: used as is, no metadata lookup, not a guess', () => {
  const sys = hostSys({ ssh: '198.51.100.20 50000 203.0.113.7 2222' });
  const r = hostCandidates(sys);
  assert.equal(r.candidates[0].host, '203.0.113.7');
  assert.equal(r.port, 2222);
  assert.equal(r.guess, false);
  assert.equal(sys.fetches.length, 0);
});

test('private client and private server (LAN, VPN or jump host): private address first, flagged as a guess', () => {
  const sys = hostSys({ ssh: '192.168.1.10 50000 10.0.1.23 22' });
  const r = hostCandidates(sys);
  assert.equal(r.candidates[0].host, '10.0.1.23');
  assert.equal(r.guess, true);
  assert.match(r.hint, /same network or VPN/);
  assert.match(r.hint, /jump host/);
  assert.equal(sys.fetches.length, 0, 'no metadata lookup when the client is on the private side too');
});

test('Tailscale wins and no metadata is fetched', () => {
  const sys = hostSys({ ssh: NAT, metadata: AWS, tailscale: { BackendState: 'Running', Self: { DNSName: 'box.tail9.ts.net.', TailscaleIPs: ['100.101.1.2'] } } });
  const r = hostCandidates(sys);
  assert.equal(r.candidates[0].host, 'box.tail9.ts.net');
  assert.equal(r.guess, false);
  assert.equal(sys.fetches.length, 0);
});

test('no SSH_CONNECTION: `ip route get` gives the address; private → metadata, public → used', () => {
  const pub = hostSys({ route: '1.1.1.1 via 203.0.113.1 dev eth0 src 203.0.113.50 uid 1000 \\    cache' });
  const r1 = hostCandidates(pub);
  assert.equal(r1.candidates[0].host, '203.0.113.50');
  assert.equal(r1.candidates[0].source, 'route');
  assert.equal(r1.guess, false);
  assert.equal(pub.fetches.length, 0);

  const priv = hostSys({ route: '1.1.1.1 via 10.0.0.1 dev ens5 src 10.0.1.23 uid 1000', metadata: AWS });
  const r2 = hostCandidates(priv);
  assert.equal(r2.candidates[0].host, '54.1.2.3');
  assert.equal(r2.candidates[0].source, 'cloud-metadata');

  const none = hostSys({ route: '1.1.1.1 via 10.0.0.1 dev ens5 src 10.0.1.23 uid 1000' });
  const r3 = hostCandidates(none);
  assert.equal(r3.candidates[0].source, 'placeholder');
  assert.equal(r3.guess, true);
});

test('fetchSync: parallel requests with method/headers, failures and timeouts are null', async () => {
  const server = cp.spawn(process.execPath, [
    '-e',
    `require('http').createServer((q, r) => { if (q.url === '/slow') return; r.end(q.method + ' ' + (q.headers['x-a'] || '')); })
       .listen(0, '127.0.0.1', function () { console.log(this.address().port); });`,
  ], { env: { PATH: process.env.PATH } }); // not NODE_TEST_CONTEXT: the child is not a test file
  try {
    const port = await new Promise((resolve) => server.stdout.once('data', (d) => resolve(Number(String(d).trim()))));
    const t0 = Date.now();
    const out = realSys().fetchSync(
      [
        { url: `http://127.0.0.1:${port}/x`, method: 'PUT', headers: { 'X-A': '1' } },
        { url: `http://127.0.0.1:${port}/slow` },
        { url: 'http://127.0.0.1:1/' },
      ],
      { timeoutMs: 400 }
    );
    assert.deepEqual(out[0], { status: 200, body: 'PUT 1' });
    assert.equal(out[1], null);
    assert.equal(out[2], null);
    assert.ok(Date.now() - t0 < 3000, 'parallel, bounded by the timeout');
  } finally {
    server.kill();
  }
});
