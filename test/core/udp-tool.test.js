// UDP through the page's network from a tool of the Debian image rather than
// a probe: bash's /dev/udp (a connected socket, a write, a read), and its
// /dev/tcp beside it, against a network in the page that echoes.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser'], images: ['slim'], timeout: 180 };

const enc = new TextEncoder();
const dec = new TextDecoder();

/** Each chunk back, prefixed with where it was going. */
function echo(host, port) {
  let push;
  const readable = new ReadableStream({ start: (c) => { push = c; } });
  const writable = new WritableStream({ write: (b) => push.enqueue(enc.encode(`${host}:${port} ${dec.decode(b)}`)) });
  return { readable, writable };
}

export default async function (t) {
  const { eq, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('slim/debian-slim.tar.zst'));
  const asked = [];
  sys.network = {
    connect: (host, port) => { asked.push(`tcp ${host}:${port}`); return echo(host, port); },
    datagram: (host, port) => { asked.push(`udp ${host}:${port}`); return echo(host, port); },
  };
  await timeout(sys.boot(), 120000, 'boot');
  const env = { PATH: '/usr/bin:/bin', HOME: '/root' };
  const bash = (script) => timeout(sys.run(['bash', '-c', script], { env }), 30000, script);
  // One datagram out and its echo back: 15 characters, "10.1.2.3:7 ping".
  const udp = await bash('exec 3<>/dev/udp/10.1.2.3/7; printf ping >&3; IFS= read -r -t 5 -N 15 l <&3; echo "$l"');
  eq('bash /dev/udp: the echo', [udp.code, udp.stdout], [0, '10.1.2.3:7 ping\n']);
  const tcp = await bash('exec 3<>/dev/tcp/10.1.2.3/8; printf ping >&3; IFS= read -r -t 5 -N 15 l <&3; echo "$l"');
  eq('bash /dev/tcp: the echo', [tcp.code, tcp.stdout], [0, '10.1.2.3:8 ping\n']);
  eq('each went to the network as its kind', asked, ['udp 10.1.2.3:7', 'tcp 10.1.2.3:8']);
  await sys.shutdown();
}
