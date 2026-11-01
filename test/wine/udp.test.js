// Winsock UDP out through the page's network: a Wine program's sockets are
// the wineserver's and the program's at once (moved to the global kernel),
// and each datagram's flow is opened in whichever process sends it, the
// replies read by the others (fixtures/wine/src/udpecho.c).
import { bootWine } from '../lib/wine.mjs';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 900 };

const enc = new TextEncoder();
const dec = new TextDecoder();

export default async function (t) {
  const w = await bootWine(t);
  const flows = [];
  w.sys.network = {
    connect: () => { throw new Error('no TCP here'); },
    datagram: (host, port) => {
      flows.push(`${host}:${port}`);
      let push;
      const readable = new ReadableStream({ start: (c) => { push = c; } });
      const writable = new WritableStream({ write: (b) => push.enqueue(enc.encode(`${host}:${port} ${dec.decode(b)}`)) });
      return { readable, writable };
    },
  };
  const r = await t.time('udpecho', () => w.run('udpecho.exe')).catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  t.check('udpecho: every check', r.code === 0 && /^(ok [^\n]*\n)+0 failures\n?$/.test(r.stdout),
    `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-400))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  t.eq('one flow per far address', flows, ['10.1.2.3:7', '10.1.2.3:9']);
  await w.sys.shutdown();
}
