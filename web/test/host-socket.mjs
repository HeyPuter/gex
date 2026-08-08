// The host connecting into a socket the guest serves.
//
//   node web/test/host-socket.mjs <socket-server-elf>
//
// The other direction of the seam the virtio-gpu renderer uses. A guest that
// *connects* to a host-served socket has worked for a while; a guest that
// *serves* one had no way to be reached from outside, and that is the
// direction a Worker per process needs -- an X server in one Worker and its
// client in another have to have a socket between them, and a relay in
// JavaScript can only be that if it can reach both ends.
//
// `SocketNames::connect` already did the work. It only needed a host-owned
// socket to do it with, which is what `gex_host_connect` supplies.
import { readFile, access } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
const SHIM = join(ROOT, 'target/wasm32-unknown-unknown/release/gex_browser.wasm');
const SEED = join(ROOT, 'web/public/gex/seed.wasm');
const PATH = '/tmp/echo';
const MESSAGE = 'hello from the host';

const exists = async (p) => access(p).then(() => true, () => false);
const skip = (why) => {
  console.log(`skip: ${why}`);
  process.exit(0);
};

const elfPath = process.argv[2];
if (!elfPath || !(await exists(elfPath))) skip('give it the socket-server program');
if (!(await exists(SHIM))) skip('no browser shim (shims/browser/build.sh)');
if (!(await exists(SEED))) skip('no seed module (web/build-assets.sh)');

const { Gex, Env } = await import(join(ROOT, 'shims/browser/js/gex.mjs'));

let output = '';
class Collect extends Env {
  write(fd, bytes) {
    output += new TextDecoder().decode(bytes);
  }
  now() {
    return BigInt(Date.now()) * 1000000n;
  }
}

const gex = new Gex(new Collect());
const entry = await gex.load(
  new Uint8Array(await readFile(SHIM)),
  new Uint8Array(await readFile(SEED)),
  new Uint8Array(await readFile(elfPath)),
  {},
  [elfPath],
  {},
);
const e = gex.shim.exports;
const u8 = () => new Uint8Array(gex.memory.buffer);
const enc = new TextEncoder();

/** Put bytes somewhere the shim can see them, and say where. */
const stage = (bytes) => {
  const p = e.gex_alloc(bytes.length) >>> 0;
  u8().set(bytes, p);
  return p;
};

// The guest has to reach `listen` before anything can connect, and it then
// blocks in `accept` -- so the connection cannot come from inside the run
// loop. Start the guest, then keep trying until it takes.
const running = gex.run(entry);
const pathAt = stage(enc.encode(PATH));
let handle = -1;
for (let i = 0; i < 300 && handle < 0; i++) {
  await new Promise((r) => setTimeout(r, 10));
  handle = e.gex_host_connect(pathAt, PATH.length);
}
if (handle < 0) {
  console.log(`FAIL host-socket: nothing was bound at ${PATH}`);
  console.log(output.split('\n').map((l) => `     ${l}`).join('\n'));
  process.exit(1);
}

e.gex_host_send(handle, stage(enc.encode(MESSAGE)), MESSAGE.length);

const back = e.gex_alloc(64) >>> 0;
let reply = '';
for (let i = 0; i < 300 && !reply; i++) {
  await new Promise((r) => setTimeout(r, 10));
  const n = e.gex_host_recv(handle, back, 64);
  if (n > 0) reply = new TextDecoder().decode(u8().subarray(back, back + n));
}
await running;

const want = MESSAGE.toUpperCase();
if (reply === want) {
  console.log(`ok   host-socket (round-tripped through the guest: "${reply}")`);
  process.exit(0);
}
console.log(`FAIL host-socket: wanted ${JSON.stringify(want)}, got ${JSON.stringify(reply)}`);
console.log(output.split('\n').map((l) => `     ${l}`).join('\n'));
process.exit(1);
