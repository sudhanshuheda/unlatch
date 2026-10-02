'use strict';
// "What should the Mac call this VM?" — ranked guesses, best first, and whether the best one is
// a guess the user has to check.
//
// The hard case is a cloud VM behind 1:1 NAT (AWS, GCP, Azure) without Tailscale: sshd sees the
// VPC address (10.x), `hostname -f` is a cloud-internal name, and neither works from the Mac.
// There the public address comes from the cloud's instance metadata service (link-local, short
// timeouts); if that has no answer, the Mac command carries a `<your-ssh-host>` placeholder and
// a hint, instead of an address that cannot work.

const PLACEHOLDER = '<your-ssh-host>';
const IMDS = 'http://169.254.169.254';
const METADATA_TIMEOUT_MS = 800;

function inTailscaleRange(ip) {
  const m = /^(\d+)\.(\d+)\./.exec(ip || '');
  return !!m && Number(m[1]) === 100 && Number(m[2]) >= 64 && Number(m[2]) <= 127;
}

function isPrivate(ip) {
  return /^(10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.|127\.|169\.254\.)/.test(ip || '') || ip === '::1' || /^f[cd][0-9a-f]{2}:/i.test(ip || '') || /^fe80:/i.test(ip || '');
}

function isIpv4(s) {
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(s || '');
  return !!m && m.slice(1).every((x) => Number(x) <= 255);
}

/** A routable public IPv4 address (not private, link-local, loopback, CGNAT/Tailscale, multicast). */
function isPublicIpv4(s) {
  return isIpv4(s) && !isPrivate(s) && !inTailscaleRange(s) && !/^(0\.|22[4-9]\.|2[3-5]\d\.)/.test(s);
}

/** The ssh client came from the internet (not a private network or the tailnet). */
function isPublicAddr(ip) {
  return !!ip && !isPrivate(ip) && !inTailscaleRange(ip);
}

/**
 * The VM's public IPv4 address from its cloud's instance metadata service, or null.
 * AWS (IMDSv2: PUT a session token, then GET with it), Google Cloud and Azure, all at the
 * link-local 169.254.169.254, in parallel, each capped at METADATA_TIMEOUT_MS.
 * Returns { ip, cloud } or null.
 */
function cloudPublicAddress(sys) {
  if (typeof sys.fetchSync !== 'function') return null;
  const opts = { timeoutMs: METADATA_TIMEOUT_MS };
  const ok = (r) => (r && r.status === 200 && isPublicIpv4(String(r.body).trim()) ? String(r.body).trim() : null);
  const [awsToken, gcp, azure] = sys.fetchSync(
    [
      { url: `${IMDS}/latest/api/token`, method: 'PUT', headers: { 'X-aws-ec2-metadata-token-ttl-seconds': '60' } },
      { url: `${IMDS}/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip`, headers: { 'Metadata-Flavor': 'Google' } },
      { url: `${IMDS}/metadata/instance/network/interface/0/ipv4/ipAddress/0/publicIpAddress?api-version=2021-02-01&format=text`, headers: { Metadata: 'true' } },
    ],
    opts
  );
  if (ok(gcp)) return { ip: ok(gcp), cloud: 'Google Cloud' };
  if (ok(azure)) return { ip: ok(azure), cloud: 'Azure' };
  const token = awsToken && awsToken.status === 200 ? String(awsToken.body).trim() : '';
  if (token && /^[\w=+/-]+$/.test(token)) {
    const [ip] = sys.fetchSync([{ url: `${IMDS}/latest/meta-data/public-ipv4`, headers: { 'X-aws-ec2-metadata-token': token } }], opts);
    if (ok(ip)) return { ip: ok(ip), cloud: 'AWS' };
  }
  return null;
}

/** `ip route get` for an internet address: the source address this VM uses outbound. */
function routeSource(sys) {
  if (!sys.which('ip')) return null;
  const r = sys.run('ip', ['-o', 'route', 'get', '1.1.1.1'], { timeout: 2000 });
  if (r.status !== 0) return null;
  const m = /\bsrc\s+(\S+)/.exec(String(r.stdout));
  return m ? m[1] : null;
}

/**
 * Returns { candidates: [{host, source, note}], port, guess, hint }:
 *   port   sshd's port if it is not 22 (from SSH_CONNECTION)
 *   guess  true when the first candidate may well not work from the Mac (the user must check)
 *   hint   what to do about it (null when there is nothing to say)
 * Sources: tailscale, tailscale-ip, cloud-metadata, ssh-connection, route, hostname, placeholder.
 */
function hostCandidates(sys) {
  const out = [];
  const add = (host, source, note, { first = false } = {}) => {
    if (!host) return;
    host = String(host).trim().replace(/\.$/, '');
    if (!host || out.some((c) => c.host.toLowerCase() === host.toLowerCase())) return;
    if (first) out.unshift({ host, source, note });
    else out.push({ host, source, note });
  };

  // 1. Tailscale MagicDNS name: works from any machine on the tailnet, survives IP changes.
  const ts = sys.which('tailscale') ? sys.run('tailscale', ['status', '--json'], { timeout: 4000 }) : null;
  if (ts && ts.status === 0) {
    try {
      const j = JSON.parse(ts.stdout);
      if (j.BackendState === 'Running' && j.Self) {
        add(j.Self.DNSName, 'tailscale', 'Tailscale MagicDNS name (works from any device on your tailnet)');
        const ip4 = (j.Self.TailscaleIPs || []).find((ip) => ip.includes('.'));
        add(ip4, 'tailscale-ip', 'Tailscale IP');
      }
    } catch {
      /* not JSON: ignore */
    }
  }
  const haveTailscale = out.length > 0;

  // 2. The address this ssh session came in on. Under 1:1 NAT that is the VPC address, which the
  //    Mac cannot reach; the client's address tells the cases apart. Without SSH_CONNECTION (a
  //    detached tmux, a service), the outbound source address from `ip route get` stands in.
  let port = null;
  let local = null; // { ip, source }
  let clientIp = null;
  const sc = (sys.env.SSH_CONNECTION || '').trim().split(/\s+/);
  if (sc.length === 4) {
    const [cip, , serverIp, serverPort] = sc;
    clientIp = cip;
    if (serverPort && serverPort !== '22') port = Number(serverPort);
    local = { ip: serverIp, source: 'ssh-connection' };
  } else if (!haveTailscale) {
    const src = routeSource(sys);
    if (src) local = { ip: src, source: 'route' };
  }

  const localPrivate = !!local && isPrivate(local.ip) && !inTailscaleRange(local.ip);
  // NAT: the client came from the internet (or we cannot tell), but this VM has only a private address.
  const behindNat = localPrivate && (local.source === 'route' || isPublicAddr(clientIp));
  let cloud = null;
  if (behindNat && !haveTailscale) {
    cloud = cloudPublicAddress(sys);
    if (cloud) add(cloud.ip, 'cloud-metadata', `public address from ${cloud.cloud} instance metadata (this VM sits behind 1:1 NAT)`);
  }

  if (local) {
    const what = local.source === 'ssh-connection' ? 'address of your current ssh session' : "this VM's outbound address";
    const note = inTailscaleRange(local.ip)
      ? `${what} (Tailscale)`
      : behindNat
        ? `${what}: a private cloud network address, which your Mac cannot reach from the internet`
        : localPrivate
          ? `${what} (private network: works if your Mac is on the same network or VPN, not through a jump host)`
          : what;
    add(local.ip, local.source, note);
  }

  // 3. The VM's own idea of its name.
  const hf = sys.run('hostname', ['-f'], { timeout: 3000 });
  const fqdn = hf.status === 0 ? String(hf.stdout).trim() : '';
  const internal = /\.(internal|local|localdomain|lan)$|\.internal\.cloudapp\.net$/i;
  if (fqdn && fqdn !== 'localhost' && fqdn.includes('.')) {
    add(fqdn, 'hostname', internal.test(fqdn) ? "this machine's cloud-internal name (resolves only inside its own network)" : "this machine's fully qualified name");
  }
  if (sys.hostname && sys.hostname !== 'localhost') add(sys.hostname, 'hostname', "this machine's host name (works if your Mac resolves it, or has a Host alias with this name)");

  // 4. Can the Mac actually use the first one? Say so when it is a guess, and how to fix it.
  const aliasTip = 'If you ssh here by a ~/.ssh/config Host alias or a DNS name, use that instead (connect also switches to an alias whose HostName matches).';
  const replaceTip = `Replace ${PLACEHOLDER} with the address or ~/.ssh/config Host alias you use to ssh to this VM (what you type after \`ssh\`).`;
  let guess = false;
  let hint = null;
  const best = out[0];
  if (!best) {
    add(PLACEHOLDER, 'placeholder', 'the address or ~/.ssh/config Host alias you use to ssh to this VM');
    guess = true;
    hint = `Could not find any address for this VM. ${replaceTip}`;
  } else if (best.source === 'tailscale' || best.source === 'tailscale-ip') {
    guess = false;
  } else if (best.source === 'cloud-metadata') {
    hint = `${best.host} is this VM's public address according to ${cloud.cloud}. ${aliasTip}`;
  } else if (behindNat) {
    add(PLACEHOLDER, 'placeholder', 'the address or ~/.ssh/config Host alias you use to ssh to this VM', { first: true });
    guess = true;
    const why =
      local.source === 'ssh-connection'
        ? `this ssh session arrived from the internet on the private address ${local.ip} (so the VM is behind NAT)`
        : `there is no ssh session to learn from and this VM's own address, ${local.ip}, is private (behind NAT, or a LAN)`;
    hint =
      `Could not tell how your Mac reaches this VM: ${why}, and no cloud metadata service gave a public address. ${replaceTip} ` +
      'If a port other than 22 is forwarded to this VM, add --port <that port>.';
  } else if (localPrivate) {
    guess = true;
    hint =
      `${best.host} is a private address. It works if your Mac is on the same network or VPN as this VM. ` +
      `If you reach it through a jump host or by another name, use your ~/.ssh/config Host alias instead (connect also switches to an alias whose HostName matches).`;
  } else if (best.source === 'hostname') {
    guess = true;
    hint = `${best.host} is this machine's own name, which your Mac may not resolve. ${aliasTip}`;
  }
  return { candidates: out, port, guess, hint };
}

module.exports = { hostCandidates, cloudPublicAddress, inTailscaleRange, isPrivate, isPublicIpv4, PLACEHOLDER };
