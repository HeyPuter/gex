// The single-thread build's system Worker (@gexemu/runtime-single): the global
// kernel and every guest process, in one realm with no SharedArrayBuffer.
// A process's threads are fibers of this Worker (single.mjs), each on its
// own JSPI stack, and every memory is a plain one that only this thread
// holds. The page's System reaches it as it reaches the default build's
// kernel-call Worker (kernel-worker.js), by message: the kernel's exports
// by name, the processes' host by the ops below.
//
// Here and not on the page's thread: a guest's lazy files and units are
// read with synchronous requests and its larger modules compiled
// synchronously, both of which a page's main thread refuses, and a guest's
// slice would stall the page's input.
//
// Messages in, `{ call, op, ... }`, answered `{ call, value }` or
// `{ call, error }`:
//   start { spec: { globals, arch, host, wl, gl } }  the page's switches (its GEX_*
//       globals, as `GEX_PAGE_SEARCH`), the guest's architecture, the
//       processes' host's options; answered with the kernel's export names
//   (and unanswered, before it: `{ prepare: { globals, arch } }`, the same
//   switches and architecture, so the runtime loads while the page lays out
//   its files)
//   calls { groups }       kernel.js `calls`; answered with the wait slot's
//       word too (`seq`), once the page has one
//   slot { word }          the page's wait slot, by word index
//   slotWait { seq }       when the slot's word leaves `seq`, or 30 s: its word
//   spawnExec { r }        host.mjs `spawnExec`; answered when the process ends
//   network { net }        the page's network: null for none, else
//       `{ datagram }`, whether it has `datagram`
//   startX { listen, rootful }  the X server, on the sockets the page held
//   useKernelStacks { stacks }, stop
// And out, unasked: `{ ev: 'output', fd, text }`, `{ ev: 'message', m }`
// (a process's message the page's parts take: audio, net events),
// `{ ev: 'census', pids, threads, memory }` as processes come and go, and
// `{ ev: 'comp-out', m }` and `{ ev: 'comp-stats', stats }` from the scene
// compositor (when `start`'s spec has `wl: { width, height, scale }`), whose
// page half posts `{ comp: m }` in; `{ ev: 'gl-wanted' }` at a program's
// first GL call (with `gl: { d3d }`), and beside a DOM desktop
// (`wl.dom`) `glStart { url, files, api }` starts Mesa here, `{ ev:
// 'gl-surfaces', list }` says its windows, and the page posts `{ glc }`
// with their canvases (gl-relay.js);
// `{ ev: 'net-dial', host, port, datagram, reply }` for each connection out
// (net.js `pageRelay`): the page's network stays on the page, and the
// guest's sockets are here, beside the fibers that wait on them. And
// `{ ev: 'page-fs', req, cap, reply }` for each call on a directory the page
// serves or to a host call (page-fs.mjs `pageFsSuspending`), answered
// `[result, bytes]` on the port `reply`.

import { enableSingle, sched } from '../../../hosts/browser/js/single.mjs';
import { runCallsAsync, readsFiles } from './kernel-calls.js';
import { lazyReady } from '../../../hosts/browser/js/lazy-fetch.mjs';
import { Sockets, pageRelay } from '../../src/net.js';
import { setPageFsAsk } from '../../../hosts/browser/js/page-fs.mjs';
import { blocksOpen, blocksVerify } from '../../../hosts/browser/js/blocks.mjs';

enableSingle();

const post = (m, transfer) => globalThis.postMessage(m, transfer ?? []);
// gex-eject's `"compositor": false` builds drop the scene compositor and the
// GL device beside a DOM desktop (gpu-device.js), as runtime.js does: the
// page's windows part asks for neither there.
const COMPOSITOR = typeof __GEX_COMPOSITOR__ === 'undefined' || __GEX_COMPOSITOR__;

let k = null;
let mem = null;
let kernel = null;
let host = null;
let slotWord = -1;
/** The scene compositor, when the page has a canvas for one
 *  (compositor-in-realm.js). */
let comp = null;
let compTimer;
/** Or the arena a page's DOM desktop mirrors. */
let mirror = null;
/** Beside that, the guests' GL (gl-in-realm.js). */
let glr = null;
/** The guest's sockets, on the page's network as it last said. */
const sockets = new Sockets(null);
const dial = (m, transfer) => post({ ev: 'net-dial', ...m }, transfer);
setPageFsAsk((req, cap) => new Promise((resolve) => {
  const { port1, port2 } = new MessageChannel();
  port1.onmessage = ({ data }) => {
    port1.close();
    resolve(data);
  };
  post({ ev: 'page-fs', req, cap, reply: port2 }, [req.buffer, port2]);
}));

const words = () => new Int32Array(kernel.memory.buffer);
let callsDone = Promise.resolve();
// The lazy files are in a rest bundle, its decoder not yet loaded (`calls`).
let restBundle = false;
let audioTimer = 0;

/** The audio rings at `at` (hosts/kernel/src/audio.rs), kept in the page's
 *  worklet's copy (page/src/audio.js) over `port`: each slot's new frames
 *  every 5 ms, from where a new generation starts; and what the worklet read
 *  and its clock, as it sends them, written back here for the device. */
function mirrorAudio(at, port) {
  const HDR = 32, SW = 16, OFF = (HDR + 8 * SW) * 4;
  const sent = [], gens = [];
  // A view each time: the kernel's memory may have grown since.
  const head = () => new Uint32Array(kernel.memory.buffer, at, HDR + 8 * SW);
  port.onmessage = ({ data: { head: h, r, u } }) => {
    const W = head();
    W[4] = h[0]; W[5] = h[1]; W[6] = h[2]; W[9] = h[3];
    // (Null for a slot the worklet has not played: the device's own stays.)
    for (let s = 0; s < r.length; s++) {
      if (r[s] === null) continue;
      W[HDR + s * SW + 4] = r[s];
      W[HDR + s * SW + 6] = u[s];
    }
  };
  return setInterval(() => {
    const W = head();
    const slots = W[2], cap = W[3];
    const list = [], transfer = [];
    for (let s = 0; s < slots; s++) {
      const b = HDR + s * SW;
      const gen = W[b + 2], w = W[b + 3];
      // The first time, every slot as it is, with none of its frames: the
      // worklet starts reading where the writer is.
      const first = sent[s] === undefined;
      if (first) sent[s] = w;
      let from = sent[s];
      if (!first && gen === gens[s] && from === w) continue;
      gens[s] = gen;
      let n = (w - from) >>> 0;
      if (n > cap) {
        from = (w - cap) >>> 0;
        n = cap;
      }
      const ring = new Float32Array(kernel.memory.buffer, at + OFF + s * cap * 8, cap * 2);
      const frames = new Float32Array(n * 2);
      for (let i = 0, p = from; i < n; i++, p = (p + 1) >>> 0) {
        const q = (p % cap) * 2;
        frames[2 * i] = ring[q];
        frames[2 * i + 1] = ring[q + 1];
      }
      list.push([s, W[b], W[b + 1], gen, from, w, frames]);
      transfer.push(frames.buffer);
      sent[s] = w;
    }
    if (list.length) port.postMessage(list, transfer);
  }, 5);
}
const promised = new Map();
/** The page's kernel call `f`, on a stack that may suspend. */
const invoke = (f, args) => {
  let fn = promised.get(f);
  if (!fn) promised.set(f, fn = WebAssembly.promising(k[f]));
  return fn(...args);
};
const seq = () => (slotWord < 0 ? undefined : Atomics.load(words(), slotWord));

function census() {
  post({ ev: 'census', pids: host?.pids ?? [], threads: host?.threads ?? 0, memory: kernel?.memory?.buffer?.byteLength ?? 0 });
}

/** The page's switches and the guest's architecture in, then the kernel,
 *  shim and seed fetched and the kernel's first instance made: what `start`
 *  needs before the page's devices and the processes' host. */
let loading = null;
let loadedArch = null;
async function load(globals, arch) {
  loadedArch = arch;
  // The page's switches, as the default build's Workers get them.
  Object.assign(globalThis, globals);
  await import('../../src/page-env.js');
  const q = new URLSearchParams(globalThis.GEX_PAGE_SEARCH ?? '');
  const { setGlobals } = await import('../../src/page-globals.js');
  setGlobals((name) => q.get(name), { arch });
  // A fiber runs worker.mjs's code: it registers itself (single.mjs
  // `registerWorkerMain`) as it loads.
  const [, rt] = await Promise.all([import('../../../hosts/browser/js/worker.mjs'), import('../../src/runtime.js')]);
  // The shim and seed (the processes' host's) are fetched while the
  // kernel is: one wait for the network, not two in turn.
  const runtime = rt.loadRuntime();
  runtime.catch(() => {});
  [globalThis.__gexTso, kernel] = await Promise.all([rt.detectTso(), rt.loadKernel()]);
  if (!kernel) throw new Error('the kernel would not load');
  await runtime;
  const { helperKernel } = await import('../../src/kernel-helper.js');
  ({ k, mem } = await helperKernel(kernel, 0, 32, { suspending: true }));
  return rt;
}

const ops = {
  async start({ spec: { globals, arch, host: hostOpts, wl, gl } }) {
    restBundle = !!hostOpts?.restBundle;
    // Begun by `prepare` when the page knew the architecture early, else now.
    if (loading && loadedArch !== arch) throw new Error(`the runtime was loaded for ${loadedArch}, and the system starts on ${arch}`);
    const rt = await (loading ??= load(globals, arch));
    // The page's switches as they are now: what the page set since.
    Object.assign(globalThis, globals);
    // The scene compositor (the page's windows part asked for one): here,
    // beside the memories it reads; the page's half relays (`comp`).
    if (wl?.dom) {
      // A DOM desktop on the page: the arena here, mirrored there (wl-bridge.mjs
      // `mirrorTake`), each frame that something changed.
      const bridge = await import('../../../hosts/browser/js/wl-bridge.mjs');
      const channel = bridge.makeWlChannel();
      bridge.setOutputSize(channel, wl.width, wl.height, wl.scale);
      const memories = new Map();
      const st = bridge.mirrorState();
      const memoryOf = (token) => {
        const m = memories.get(token);
        return m ? new Uint8Array(m.buffer) : null;
      };
      compTimer = setInterval(() => {
        const t = bridge.mirrorTake(channel, st, memoryOf);
        if (t) post({ ev: 'wl-mirror', update: t.update }, t.transfer);
      }, 16);
      mirror = { channel, call: (fn, args) => bridge.mirrorCall(channel, fn, args) };
      comp = {
        channel,
        compositor: {
          addMemory: (token, memory, k) => {
            if (memory) memories.set(token, memory);
            bridge.setKernelMemory(channel, k);
          },
          dropMemory: (token) => {
            memories.delete(token);
            bridge.releaseToken(channel, token);
            glr?.forget(token);
            post({ ev: 'wl-drop', token });
          },
          stop: () => {},
        },
        send: () => {},
      };
      if (gl && COMPOSITOR) glr = (await import('../../src/gl-in-realm.js')).openGlInRealm(post);
    } else if (wl && COMPOSITOR) {
      const { openCompositor } = await import('../../src/compositor-in-realm.js');
      comp = await openCompositor(wl.width, wl.height, wl.scale, (m, transfer) => post({ ev: 'comp-out', m }, transfer));
      compTimer = setInterval(() => post({ ev: 'comp-stats', stats: comp.stats() }), 500);
      // The guests' GL (MesaGl): no ring, each process's submit runs the
      // compositor's Mesa on its own memory (compositor-worker.js
      // `glInRealm`). The first says so, for the page to start Mesa.
      const inner = gl && globalThis.__gexGlInRealm;
      if (inner) {
        let said = false;
        globalThis.__gexGlInRealm = {
          submit(...a) {
            if (!said) post({ ev: 'gl-wanted' });
            said = true;
            return inner.submit(...a);
          },
        };
      }
    }
    const glChannel = gl && comp ? { inRealm: true, d3d: !!gl.d3d } : null;
    host = await rt.makeProcessHost({ kernel, uid: 0, ...hostOpts, network: sockets, wlChannel: comp?.channel ?? null, glChannel }, {
      onOutput: (fd, text) => post({ ev: 'output', fd, text: String(text) }),
      onExit: (m, pid) => {
        if (typeof pid === 'number') comp?.compositor.dropMemory(pid);
        // Once the host has let it go (it calls this just before): taken
        // now, the census still had it, and the last process to end stayed
        // in the page's list for good.
        queueMicrotask(census);
      },
      // No program reads the page's console: each has its own pipes or pty.
      readStdin: () => new Promise(() => {}),
      onMessage: (m) => {
        // A process's memory or pixels stay here (the compositor is this
        // realm's); what the page's parts take goes over.
        if (m?.kind === 'wl-memory') return void comp?.compositor.addMemory(m.token, m.memory, m.kernel);
        if (m?.kind !== 'audio' && m?.kind !== 'net') return;
        try {
          post({ ev: 'message', m });
        } catch {}
      },
    });
    return { value: Object.keys(k).filter((n) => typeof k[n] === 'function') };
  },
  // A call may suspend (a page directory's answer), on a stack of its own;
  // the page's instance has one stack, so one batch at a time.
  calls({ groups }) {
    const run = async () => {
      // A rest bundle's decoder before the page's first read of a file's
      // bytes, which may come before any process loaded it (gex.mjs).
      if (restBundle && readsFiles(groups)) {
        restBundle = false;
        globalThis.GEX_REST_BUNDLE = 1;
        await lazyReady();
      }
      const value = [];
      const transfer = [];
      for (const list of groups) {
        const r = await runCallsAsync(k, mem, list, invoke);
        value.push(r.value);
        transfer.push(...r.transfer);
      }
      // What the page's calls woke: a fiber parked on its wait slot looks.
      sched.checkSlots();
      return { value, transfer, seq: seq() };
    };
    const done = callsDone.then(run);
    callsDone = done.catch(() => {});
    return done;
  },
  slot({ word }) {
    slotWord = word;
    return { value: seq() };
  },
  async slotWait({ seq: at }) {
    if (slotWord >= 0) await sched.parkSlot(words, slotWord, at, 30e9);
    return { value: seq() };
  },
  spawnExec({ r }) {
    const done = host.spawnExec(r);
    census();
    return done.then(() => ({ value: null }));
  },
  network({ net }) {
    sockets.network = net ? pageRelay(dial, net) : null;
    return { value: null };
  },
  // The X server (the page's windows part, at the first X client): a fiber
  // here, beside the scene it draws into (runtime.js `startXServer`).
  async startX({ listen, rootful }) {
    if (!comp) throw new Error('no windows to put an X server in');
    const rt = await import('../../src/runtime.js');
    const term = { writeln: (t) => post({ ev: 'output', fd: 2, text: `${t}\n` }), write: (t) => post({ ev: 'output', fd: 2, text: String(t) }) };
    await rt.startXServer({ kernel, wlChannel: comp.channel, compositor: () => comp.compositor, term, listen, rootful });
    return { value: null };
  },
  // Mesa beside a DOM desktop, when the page's MesaGl starts it.
  glStart(opts) {
    if (!glr) throw new Error('no GL beside these windows');
    return glr.start(opts).then((value) => ({ value }));
  },
  // The game controller table (input.js `Gamepads`): its address and a copy,
  // which the page keeps and sends back changed (`{ joy }`).
  joyTable() {
    const at = k.k_joy_table() >>> 0;
    if (!at) return { value: { at: 0, words: null } };
    const all = words();
    const n = 16 + all[(at >>> 2) + 2] * all[(at >>> 2) + 3];
    return { value: { at, words: all.slice(at >>> 2, (at >>> 2) + n) } };
  },
  // The audio rings (audio.js `WebAudio`): where, and their sizes, for the
  // page's worklet to keep a copy of (`{ audio }`, `mirrorAudio`).
  audioRing() {
    const at = k.k_audio_ring() >>> 0;
    if (!at) return { value: { at: 0 } };
    const all = words();
    return { value: { at, slots: all[(at >>> 2) + 2], cap: all[(at >>> 2) + 3], ccap: all[(at >>> 2) + 8] } };
  },
  // An OPFS filesystem's store (system.js `#store`): its handle held here,
  // where the kernel and every process run, so their reads and writes of it
  // are calls of this realm's own (blocks.mjs).
  async blocksOpen({ name, verify }) {
    blocksVerify(verify);
    await blocksOpen(name);
    return { value: true };
  },
  // The translation cache a load saved (system.js `#xcacheRestore`): named
  // and read here, where the runtime that saves it was loaded, and laid into
  // the kernel's memory, which only this realm holds; in turn with the
  // page's calls. `{ name, size, n }`: no name without a cache to keep, no
  // size with nothing saved, `n` the modules restored or the kernel's
  // refusal (< 0).
  xcacheRestore() {
    const run = async () => {
      const rt = await loading;
      const name = rt.xcacheName();
      if (!name || typeof k.k_store_restore !== 'function') return { value: {} };
      await invoke('k_xcache_enable', []);
      const bytes = await rt.xcRead(name);
      if (!bytes) return { value: { name } };
      const at = (await invoke('k_store_restore_buf', [bytes.length])) >>> 0;
      new Uint8Array(kernel.memory.buffer).set(bytes, at);
      return { value: { name, size: bytes.length, n: await invoke('k_store_restore', []) } };
    };
    const done = callsDone.then(run);
    callsDone = done.catch(() => {});
    return done;
  },
  useKernelStacks({ stacks }) {
    host.useKernelStacks(stacks);
    return { value: null };
  },
  async stop() {
    clearInterval(compTimer);
    clearInterval(audioTimer);
    comp?.compositor.stop();
    glr?.stop();
    await host?.stop();
    return { value: null };
  },
};

globalThis.onmessage = async ({ data: m }) => {
  // The page's compositor half, as it would post to a compositor Worker,
  // or its DOM desktop's writes on the arena.
  if (m?.comp) return void comp?.send(m.comp);
  if (m?.wl) return void mirror?.call(m.wl.fn, m.wl.args);
  // The page's audio worklet's own port: its copy of the rings filled from here.
  if (m?.audio) {
    clearInterval(audioTimer);
    audioTimer = mirrorAudio(k.k_audio_ring() >>> 0, m.audio.port);
    return;
  }
  // The page's game controllers, written into the kernel's table, then
  // their readers woken (a kernel call, in turn with the page's).
  if (m?.joy) {
    words().set(m.joy.words, m.joy.at >>> 2);
    callsDone = callsDone.then(() => runCallsAsync(k, mem, [{ f: 'k_joy_notify' }], invoke)).catch(() => {});
    return;
  }
  // Its GL windows' canvases (gl-relay.js).
  if (m?.glc) return void (m.glc.stop ? glr?.stop() : glr?.canvas(m.glc));
  // The page knows the guest's architecture before its files are ready:
  // the runtime loads meanwhile (`start` then takes it).
  if (m?.prepare) {
    loading ??= load(m.prepare.globals, m.prepare.arch);
    loading.catch(() => {});
    return;
  }
  if (m?.call === undefined) return;
  try {
    const { value, transfer, seq: s } = await ops[m.op](m);
    post({ call: m.call, value, seq: s }, transfer);
  } catch (e) {
    post({ call: m.call, error: { message: e?.message ?? String(e), stack: e?.stack } });
  }
};
