// Bring up the host-side X server under Node.
//
//   node web/test/hostx.mjs [--build <dir>] [--seconds N]
//
// The same `HostX` the page's Worker runs (web/src/hostx.js), with the same
// global kernel and the same scene arena -- so everything that can go wrong in
// the transport goes wrong here too, in a terminal, in two seconds, with a
// stack trace. What it cannot do is draw: there is no WebGL in Node, so the
// scene is reported as records rather than pixels.
//
// It says what it saw: the server's own log, the compositor's protocol trace,
// the window manager's notes, and every surface published.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { HostX } from '../src/hostx.js';
import { makeWlChannel, setOutputSize, readScene } from '../../shims/browser/js/wl-bridge.mjs';

const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : dflt;
};
const here = fileURLToPath(new URL('.', import.meta.url));
const build = opt('--build', '/tmp/gex-xwl2');
const seconds = Number(opt('--seconds', '6'));
const kernelPath = opt('--kernel', `${here}../public/gex/gex_kernel.wasm`);

const stamp = () => new Date().toISOString().slice(11, 23);
const say = (...a) => console.log(stamp(), ...a);

// The kernel's memory maximum must match `--max-memory` in
// shims/kernel/build.sh: a shared memory carries its maximum in its type.
const kernel = {
  module: await WebAssembly.compile(readFileSync(kernelPath)),
  memory: new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true }),
};

const channel = makeWlChannel();
setOutputSize(channel, 1280, 760);

// What the page's compositor Worker would do with the scene, printed instead.
let published = 0;
const watch = setInterval(() => {
  const scene = readScene(channel);
  if (!scene.length) return;
  const now = scene
    .map((s) => `${s.token}:${s.client}:${s.surface} role=${s.role} ${s.width}x${s.height}`
      + `@${s.relX},${s.relY} addr=0x${(s.addr >>> 0).toString(16)} "${s.title}"`)
    .join(' | ');
  if (now !== published) {
    say(`scene: ${now}`);
    published = now;
  }
}, 250);

const host = new HostX((m) => {
  if (m.kind === 'log') say(m.text);
  else if (m.kind === 'wl-memory') say(`memory: token ${m.token}, ${m.memory.buffer.byteLength >> 20} MiB`);
  else say(`message: ${m.kind}${m.why ? ` (${m.why})` : ''}`);
});

// `start` never returns -- the X server owns this thread once `main` runs -- so
// the deadline is a timer inside the server's own loop: see `tick` below.
const deadline = Date.now() + seconds * 1000;
const origExit = process.exit.bind(process);
setTimeout(() => {
  say('the deadline passed without the server blocking, which is unexpected');
  clearInterval(watch);
  origExit(0);
}, (seconds + 4) * 1000).unref?.();

// A client, the way a guest process is one: another instance of the same global
// kernel module against the same kernel memory, connecting to the display name.
//
// This is the whole X socket path minus the guest -- bind in the server's
// Worker, connect from somewhere else, accept, read, write -- and it runs
// inside the server's own poll, which is the only place a test gets a turn.
let probe = null;
if (args.includes('--probe')) {
  const mem = new WebAssembly.Memory({ initial: 2, maximum: 4, shared: true });
  const inst = await WebAssembly.instantiate(kernel.module, {
    env: { memory: kernel.memory, process: mem, lazy_fetch: () => -1, process64: new WebAssembly.Memory({ address: 'i64', initial: 0n, maximum: 0n, shared: true }) },
  });
  const k = inst.exports;
  const bytes = new Uint8Array(mem.buffer);
  const put = (at, data) => {
    bytes.set(data, at);
    return at;
  };
  probe = { k, mem, bytes, put, state: 'idle', at: 0, id: -1, tries: 0 };
}

function pumpProbe() {
  if (!probe) return;
  const { k, put } = probe;
  if (probe.state === 'idle') {
    if (probe.tries++ < 8) return;      // let the server settle first
    for (const name of ['\0/tmp/.X11-unix/X0', '/tmp/.X11-unix/X0']) {
      const raw = new TextEncoder().encode(name);
      put(0, raw);
      const id = k.k_sock_connect(0n, raw.length);
      say(`probe: connect ${JSON.stringify(name)} -> ${id}`);
      if (id >= 0) {
        probe.id = id;
        break;
      }
    }
    if (probe.id < 0) {
      probe.state = 'done';
      return;
    }
    // The X11 setup request: little-endian, protocol 11.0, no authorisation.
    const setup = new Uint8Array([0x6c, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    put(256, setup);
    const n = k.k_sock_write(probe.id, 256n, setup.length);
    say(`probe: wrote ${n} bytes of setup`);
    probe.state = 'waiting';
    return;
  }
  if (probe.state === 'waiting') {
    const n = k.k_sock_read(probe.id, 512n, 4096);
    if (n <= 0) return;
    const reply = probe.bytes.subarray(512, 512 + n);
    say(`probe: the server answered ${n} bytes, first byte ${reply[0]}`
      + ` (1 = success, 0 = refused)`);
    probe.state = 'done';
  }
}

// A hook the harness can hang a deadline on: `HostX` calls `tick` from inside
// the server's poll, and this is the only place a test gets a turn.
globalThis.GEX_HOSTX_TICK = () => {
  pumpProbe();
  if (Date.now() < deadline) return;
  clearInterval(watch);
  say('done');
  origExit(0);
};

await host.start({
  xwaylandUrl: `${build}/build/xserver/hw/xwayland/Xwayland.js`,
  hostwlUrl: fileURLToPath(new URL('../../target/wasm32-unknown-unknown/release/gex_hostwl.wasm', import.meta.url)),
  kernel,
  wlChannel: channel,
  token: 0x78776c31,
  keymapUrl: `${build}/xkb/gex.xkm`,
  display: ':0',
  verbose: Number(opt('--verbose', '0')) || undefined,
  trace: Number(opt('--trace', '0')) || undefined,
});
