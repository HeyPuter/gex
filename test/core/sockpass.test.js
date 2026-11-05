// TCP sockets passed over SCM_RIGHTS and inherited across fork between
// processes that each have their own memory (fixtures/native/wasm32/sockpass.c,
// the busybox image's rt-sockpass): carried by name and rebuilt on the same
// connection ("inet:"), or, unconnected, moved to the global kernel and held by
// both ("inetg:"). Its remote cases go out through the page's own network
// (sys.network), which answers each HTTP/1.0 GET.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 600 };

const enc = new TextEncoder();
const dec = new TextDecoder();

/** A connection that answers the first request it is sent with an HTTP
 *  response and closes. */
function http() {
  let push;
  let seen = '';
  const readable = new ReadableStream({ start: (c) => { push = c; } });
  const writable = new WritableStream({
    write: (b) => {
      seen += dec.decode(b, { stream: true });
      if (!seen.includes('\r\n\r\n') || !push) return;
      push.enqueue(enc.encode(`HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 4096\r\n\r\n${'x'.repeat(4096)}`));
      push.close();
      push = null;
    },
  });
  return { readable, writable };
}

export default async function (t) {
  const { check, eq, say, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const asked = [];
  sys.network = { connect: (host, port) => { asked.push(`${host}:${port}`); return http(); } };
  await timeout(sys.boot(), 60000, 'boot');
  const r = await timeout(sys.run(['rt-sockpass', '10.1.2.3', '80'], { env: { PATH: '/bin:/usr/bin', HOME: '/root' } }), 300000, 'rt-sockpass');
  const lines = r.stdout.split('\n');
  for (const l of lines) {
    const m = /^(ok|FAIL)\s+(.*)$/.exec(l);
    if (m) check(m[2], m[1] === 'ok', lines.filter((x) => x.startsWith('  ')).join(' / '));
  }
  eq('every case ran (11)', lines.filter((l) => /^(ok|FAIL)\s/.test(l)).length, 11);
  eq('exits 0', r.code, 0);
  check('the remote cases went through the page\'s network', asked.length >= 3, asked.join(', '));
  say(`  connections out: ${asked.length}`);
}
