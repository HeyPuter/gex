// Two guest processes, in two Workers, talking to each other.
//
//   node web/test/two-workers.mjs <socket-server-elf> <socket-client-elf>
//
// The seam the Worker-per-process design rests on, and the first thing to cross
// it. Each Worker is its own guest -- own memory, kernel, filesystem -- which
// is what stopped two processes colliding on library addresses, and also means
// a client in one Worker cannot see a socket bound in the other.
//
// What crosses is the stream, in two halves built separately:
//   * `gex_host_connect` reaches into the server's Worker, to a socket the
//     guest bound (already existed),
//   * `gex_host_bind` serves a path inside the client's Worker, so a guest
//     connecting out is answered by the host (new).
//
// The relay between them is dumb JavaScript moving bytes: if this works, the
// hard part is the kernel state two processes share, not the transport.
import { Worker } from 'node:worker_threads';
import { readFile, access } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
const SHIM = join(ROOT, 'target/wasm32-unknown-unknown/release/gex_browser.wasm');
const SEED = join(ROOT, 'web/public/gex/seed.wasm');
const GEX = join(ROOT, 'shims/browser/js/gex.mjs');
const PATH = '/tmp/relay';

const exists = async (p) => access(p).then(() => true, () => false);
const skip = (why) => {
  console.log(`skip: ${why}`);
  process.exit(0);
};

const [serverElf, clientElf] = process.argv.slice(2);
if (!serverElf || !clientElf) skip('give it the socket-server and socket-client programs');
for (const p of [serverElf, clientElf, SHIM, SEED]) {
  if (!(await exists(p))) skip(`missing ${p}`);
}

const shim = new Uint8Array(await readFile(SHIM));
const seed = new Uint8Array(await readFile(SEED));

/**
 * A guest in its own Worker, driven far enough to be talked to.
 *
 * The runtime is loaded here rather than in `worker.mjs` because the relay
 * needs the shim's exports, and they only exist on the side that owns the
 * memory.
 */
function spawnGuest(elfBytes, argv) {
  return new Promise((resolve) => {
    const w = new Worker(
      `
      import { parentPort } from 'node:worker_threads';
      const { Gex, Env } = await import(${JSON.stringify(GEX)});
      let out = '';
      class E extends Env {
        write(fd, bytes) {
          out += new TextDecoder().decode(bytes.slice());
          parentPort.postMessage({ kind: 'out', text: out });
        }
        now() { return BigInt(Date.now()) * 1000000n; }
      }
      parentPort.on('message', async (m) => {
        if (m.kind !== 'run') return;
        const gex = new Gex(new E());
        const entry = await gex.load(m.shim, m.seed, m.elf, {}, m.argv, {});
        const e = gex.shim.exports;
        const u8 = () => new Uint8Array(gex.memory.buffer);
        const enc = new TextEncoder();
        const stage = (s) => {
          const b = enc.encode(s);
          const p = e.gex_alloc(b.length) >>> 0;
          u8().set(b, p);
          return [p, b.length];
        };
        // Whatever the parent asks for, on the guest's own memory. The relay
        // cannot reach across a Worker boundary, so each side does its own
        // half and the parent moves the bytes as messages.
        parentPort.on('message', (q) => {
          if (q.kind === 'bind') {
            const [p, n] = stage(q.path);
            parentPort.postMessage({ kind: 'bound', id: e.gex_host_bind(p, n) });
          } else if (q.kind === 'accept') {
            parentPort.postMessage({ kind: 'accepted', h: e.gex_host_accept(q.id) });
          } else if (q.kind === 'connect') {
            const [p, n] = stage(q.path);
            parentPort.postMessage({ kind: 'connected', h: e.gex_host_connect(p, n) });
          } else if (q.kind === 'send') {
            const [p, n] = stage(q.text);
            parentPort.postMessage({ kind: 'sent', n: e.gex_host_send(q.h, p, n) });
          } else if (q.kind === 'recv') {
            const p = e.gex_alloc(256) >>> 0;
            const n = e.gex_host_recv(q.h, p, 256);
            parentPort.postMessage({
              kind: 'got',
              text: n > 0 ? new TextDecoder().decode(u8().subarray(p, p + n)) : '',
            });
          }
        });
        parentPort.postMessage({ kind: 'ready' });
        const status = await gex.run(entry);
        parentPort.postMessage({ kind: 'exit', status, text: out });
      });
      `,
      { eval: true },
    );
    const pending = [];
    const guest = {
      worker: w,
      out: '',
      exited: null,
      ask(msg, want) {
        return new Promise((r) => {
          pending.push({ want, r });
          w.postMessage(msg);
        });
      },
    };
    w.on('message', (m) => {
      if (m.kind === 'out') guest.out = m.text;
      else if (m.kind === 'exit') guest.exited = m;
      else if (m.kind === 'ready') resolve(guest);
      else {
        const i = pending.findIndex((p) => p.want === m.kind);
        if (i >= 0) pending.splice(i, 1)[0].r(m);
      }
    });
    w.postMessage({ kind: 'run', shim, seed, elf: elfBytes, argv });
  });
}

const server = await spawnGuest(new Uint8Array(await readFile(serverElf)), [serverElf]);
const client = await spawnGuest(new Uint8Array(await readFile(clientElf)), [clientElf]);

// The client's Worker serves the path its guest will connect to.
const bound = await client.ask({ kind: 'bind', path: PATH }, 'bound');
if (bound.id < 0) {
  console.log('FAIL two-workers: could not bind the relay path in the client Worker');
  process.exit(1);
}

// The server's guest has to reach `listen` before anything can connect, and
// then blocks in `accept`, so this cannot happen inside either run loop.
const until = async (what, tries = 400) => {
  for (let i = 0; i < tries; i++) {
    const got = await what();
    if (got !== null && got !== undefined) return got;
    await new Promise((r) => setTimeout(r, 10));
  }
  return null;
};

const toServer = await until(async () => {
  const r = await server.ask({ kind: 'connect', path: '/tmp/echo' }, 'connected');
  return r.h >= 0 ? r.h : null;
});
const fromClient = await until(async () => {
  const r = await client.ask({ kind: 'accept', id: bound.id }, 'accepted');
  return r.h >= 0 ? r.h : null;
});

if (toServer === null || fromClient === null) {
  console.log(
    `FAIL two-workers: no route (server handle ${toServer}, client handle ${fromClient})`,
  );
  console.log(`     server said: ${server.out.trim()}`);
  console.log(`     client said: ${client.out.trim()}`);
  process.exit(1);
}

// The relay. Client -> server, then server -> client, which is the whole of
// what a socket between two Workers has to do.
const asked = await until(async () => {
  const r = await client.ask({ kind: 'recv', h: fromClient }, 'got');
  return r.text || null;
});
await server.ask({ kind: 'send', h: toServer, text: asked ?? '' }, 'sent');
const answered = await until(async () => {
  const r = await server.ask({ kind: 'recv', h: toServer }, 'got');
  return r.text || null;
});
await client.ask({ kind: 'send', h: fromClient, text: answered ?? '' }, 'sent');

const done = await until(async () => (client.exited ? client.exited : null));
server.worker.terminate();
client.worker.terminate();

const said = (done?.text || client.out).trim();
if (asked === 'over here' && said.includes('OVER HERE')) {
  console.log(`ok   two-workers (a guest in one Worker was answered by a guest in another)`);
  console.log(`     client sent "${asked}", got back "${said.split(': ').pop()}"`);
  process.exit(0);
}
console.log('FAIL two-workers: the round trip did not complete');
console.log(`     client sent: ${JSON.stringify(asked)}`);
console.log(`     server answered: ${JSON.stringify(answered)}`);
console.log(`     client said: ${JSON.stringify(said)}`);
process.exit(1);
