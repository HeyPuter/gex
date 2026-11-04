// A process killed while it holds connections out lets go of them: every
// connection and UDP flow the page's network made is closed once its holders
// are gone, for more processes than the page has receive rings (64): killed
// alone, killed with a forked child holding the same sockets (moved to the
// global kernel), and crashed.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 900 };

const ROUNDS = 40;
const FORK_ROUNDS = 30;
const TRAP_ROUNDS = 10;
const EACH = 2;

export default async function (t) {
  const { say, eq, timeout, until, lines } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };

  // Connections and flows that stay open until the page's end of them is
  // closed; at the end, ones that send back what they are sent.
  const count = { tcp: { opened: 0, closed: 0 }, udp: { opened: 0, closed: 0 } };
  let echo = false;
  const held = (c) => {
    c.opened++;
    let done = false;
    const end = () => { if (!done) { done = true; c.closed++; } };
    const readable = new ReadableStream({ cancel: end });
    const writable = new WritableStream({ close: end, abort: end });
    return { readable, writable };
  };
  const echoing = (once) => {
    let push;
    const readable = new ReadableStream({ start: (c) => { push = c; } });
    const writable = new WritableStream({ write: (b) => { push.enqueue(b.slice()); if (once) push.close(); } });
    return { readable, writable };
  };
  sys.network = {
    connect: () => (echo ? echoing(true) : held(count.tcp)),
    datagram: () => (echo ? echoing(false) : held(count.udp)),
  };

  const hold = async (how, proto) => {
    const p = sys.spawn(['rt-probe', 'hold', '10.1.2.3', '80', String(EACH), ...(how ? [how] : []), ...(proto === 'udp' ? ['udp'] : [])], { env });
    const said = lines(p);
    await until(() => said.some((l) => l.startsWith('held')), 'rt-probe hold');
    const child = said.map((l) => /^child (\d+)$/.exec(l)?.[1]).find(Boolean);
    if (child) {
      const k = await timeout(sys.run(['rt-probe', 'kill', child, '9'], { env }), 30000, 'kill child');
      if (k.stdout !== '0\n') throw new Error(`kill ${child}: ${JSON.stringify(k)}`);
    }
    if (how !== 'trap') p.kill('SIGKILL');
    await timeout(p.exited, 20000, 'holder exits');
  };

  for (const proto of ['tcp', 'udp']) {
    const c = count[proto];
    for (let i = 0; i < ROUNDS; i++) await hold(null, proto);
    for (let i = 0; i < FORK_ROUNDS; i++) await hold('fork', proto);
    const killed = c.closed;
    for (let i = 0; i < TRAP_ROUNDS; i++) await hold('trap', proto);
    say(`  ${proto} killed: ${(ROUNDS + FORK_ROUNDS) * EACH} opened, ${killed} closed; crashed: ${TRAP_ROUNDS * EACH} opened, ${c.closed - killed} closed`);
    const want = (ROUNDS + FORK_ROUNDS + TRAP_ROUNDS) * EACH;
    eq(`every ${proto} ${proto === 'tcp' ? 'connection' : 'flow'} was made`, c.opened, want);
    await until(() => c.closed === c.opened, `all ${c.opened} ${proto} closed`, 15000).catch(() => {});
    eq(`every killed holder's ${proto} ${proto === 'tcp' ? 'connection' : 'flow'} was closed`, c.closed, c.opened);
    say(`  ${proto}: ${c.opened} opened, ${c.closed} closed`);
  }

  // And the network still carries a connection and a datagram.
  echo = true;
  const out = await timeout(sys.run(['rt-probe', 'connect', '10.1.2.3', '80', 'after'], { env }), 30000, 'connect after');
  eq('a connection after them works', [out.code, out.stdout], [0, 'after\n']);
  const u = await timeout(sys.run(['rt-probe', 'udp', '10.1.2.3', '80', 'after'], { env }), 30000, 'udp after');
  eq('a datagram after them works', [u.code, u.stdout.trim()], [0, 'after']);
}
