// The guest's network (a page's own Network), the page's connections into
// the guest (sys.net.connectGuest) and the guest's calls to the page
// (hostCalls, gex-host).
import { System } from '../src/index.js';
import { Filesystem } from '../src/fs.js';
import { Network } from '../src/net.js';
import { main, say, check, eq, timeout, testDebug, lines, until } from './lib.js';

const enc = new TextEncoder();
const dec = new TextDecoder();

/** A duplex in the page: what is first written comes back reversed,
 *  prefixed with where it was going, and the page closes it. */
function reverser(host, port) {
  let push;
  const readable = new ReadableStream({ start: (c) => { push = c; } });
  const writable = new WritableStream({
    write: (b) => {
      push.enqueue(enc.encode(`${host}:${port} ${[...dec.decode(b)].reverse().join('')}`));
      push.close();
    },
  });
  return { readable, writable };
}

async function text(readable) {
  let s = '';
  for (const r = readable.getReader(); ;) {
    const { value, done } = await r.read();
    if (done) return s;
    s += dec.decode(value, { stream: true });
  }
}

main(async () => {
  const sys = new System({ baseUrl: '/', debug: testDebug() });
  const fs = new Filesystem(sys);
  fs.addTar('/__rt/bb.tar.zst');
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };
  const run = (argv, o = {}) => timeout(sys.run(argv, { env, ...o }), 30000, argv.join(' '));

  // No network yet: a connection out fails rather than hangs.
  const none = await run(['rt-probe', 'connect', '10.1.2.3', '80', 'x']);
  check('with no network, a connection out fails', none.code !== 0, JSON.stringify(none));

  // A page's own network.
  const asked = [];
  const net = new Network(sys, {
    connect: (host, port) => {
      if (port === 81) throw new Error('refused');
      return reverser(host, port);
    },
  });
  net.addEventListener('connect', (e) => asked.push(`${e.host}:${e.port}`));
  await timeout(net.ready, 10000, 'network ready');
  const out = await run(['rt-probe', 'connect', '10.1.2.3', '80', 'hello']);
  eq('a connection out reaches the page\'s connect', [out.code, out.stdout], [0, '10.1.2.3:80 olleh\n']);
  const named = await run(['rt-probe', 'connect', 'example.test', '8080', 'abc']);
  check('a name reaches it as a name or an address', named.code === 0 && /^\S+:8080 cba\n$/.test(named.stdout), JSON.stringify(named));
  say(`  connect saw ${asked.join(', ')}`);
  const refused = await run(['rt-probe', 'connect', '10.1.2.3', '81', 'x']);
  check('a throw in connect refuses it', refused.code !== 0, JSON.stringify(refused));

  // Into the guest.
  const srv = sys.spawn(['rt-probe', 'serve', '8080'], { env });
  const said = lines(srv);
  await until(() => said.includes('listening'), 'rt-probe serve');
  const c = await sys.net.connectGuest(8080);
  const w = c.writable.getWriter();
  await w.write(enc.encode('to the '));
  await w.write('guest');
  await w.close();
  eq('connectGuest: the program answers', await timeout(text(c.readable), 10000, 'reply'), 'TO THE GUEST');
  eq('the program saw it close', await timeout(srv.exited, 10000, 'serve exit'), { code: 0, signal: null });
  check('and said so', said.includes('served'), JSON.stringify(said));
  const nobody = await sys.net.connectGuest(8081).then(() => null, (e) => e);
  check('connectGuest with nothing listening rejects', nobody && /ECONNREFUSED/.test(nobody.message), String(nobody));

  // The guest's calls to the page.
  const off = sys.hostCalls.register('add', ({ a, b }) => a + b);
  sys.hostCalls.register('slow', async (v) => { await new Promise((r) => setTimeout(r, 200)); return { echoed: v }; });
  sys.hostCalls.register('fails', () => { throw new Error('it failed'); });
  const add = await run(['gex-host', 'add', '{"a":1,"b":2}']);
  eq('gex-host add', [add.code, add.stdout], [0, '3\n']);
  const slow = await run(['gex-host', 'slow'], { input: '["from stdin"]' });
  eq('gex-host with the JSON on stdin, answered later', [slow.code, slow.stdout], [0, '{"echoed":["from stdin"]}\n']);
  const fails = await run(['gex-host', 'fails', '{}']);
  eq('a throw is the program\'s error', [fails.code, fails.stdout, fails.stderr], [1, '', 'it failed\n']);
  const unknown = await run(['gex-host', 'nope', '{}']);
  check('an unknown name fails', unknown.code === 1 && /no host call nope/.test(unknown.stderr), JSON.stringify(unknown));
  const bad = await run(['gex-host', 'add', '{not json']);
  check('bad JSON fails', bad.code === 1 && /not JSON/.test(bad.stderr), JSON.stringify(bad));
  const many = await Promise.all([1, 2, 3, 4, 5, 6].map((i) => run(['gex-host', 'add', JSON.stringify({ a: i, b: 10 })])));
  eq('calls at once', many.map((r) => r.stdout), ['11\n', '12\n', '13\n', '14\n', '15\n', '16\n']);
  off();
  eq('unregistered', sys.hostCalls.names(), ['slow', 'fails']);

  await sys.shutdown();
});
