// What the runtime API (page/runtime) and the single-thread build's system
// Worker share: the shim and kernel loaded, the process host the guests run
// under, and the channels of the page's devices (windows, GL, network, the X
// server).

import { ProcessHost } from '../../hosts/browser/js/host.mjs';
import { xcHash, xcRead } from '../../hosts/browser/js/xcache.mjs';

/** A saved translation cache's bytes by name (the runtime API's `xcache`). */
export { xcRead };
import { single, singleSupported, FiberWorker } from '../../hosts/browser/js/single.mjs';
import { SP_CODE, MP_CODE } from '../../hosts/browser/js/mode.mjs';
import { spareMemories } from '../../hosts/browser/js/lazy-fetch.mjs';
import { logNamed } from '../../hosts/browser/js/log.mjs';
import { startOptions } from '../../hosts/browser/js/options.mjs';
import { assetFiles, assetUrl, pageParams, pageSearch } from './page-env.js';
import { workerGraph, workerUrlOf } from './worker-graph.js';
import { startAudio } from './audio.js';

/** The guest Worker's modules: bytes fetched, bytes from the page's cache
 *  (worker-graph.js). */
export const workerGraphBytes = { net: 0, cached: 0 };

/** `u` made absolute against the page (unchanged where there is no page:
 *  Node). */
const absUrl = (u) => {
  try { return new URL(u, globalThis.location?.href).href; } catch { return u; }
};

/** A unit cache at a `file:` URL is read as a directory (Node: a Worker
 *  there has no synchronous request). */
const unitsAt = (c) => (c.url.startsWith('file:') ? { ...c, root: decodeURIComponent(new URL(c.url).pathname) } : c);

/** Where the built runtime lives, relative to the site root. */
// A memory import's `shared` flag is part of its type, so the shim, seed,
// kernel, closure and host-made memory must all be the shared build or nothing
// links.
//
// One set per guest architecture: the lifter is compiled into the shim, the
// seed's `enter` stub and the kernel's syscall ABI are per-arch too. The
// x86-64 set carries an `.x86` infix; `GEX_ARCH = 'x86'` selects it.
export function guestArch() {
  return globalThis.GEX_ARCH === 'x86' ? 'x86' : 'aarch64';
}
function archInfix() {
  return guestArch() === 'x86' ? '.x86' : '';
}
/** `/rootfs/x86/name` or `/rootfs/name`: where this architecture's assets live. */
export function archAsset(path) {
  return guestArch() === 'x86' ? path.replace(/^\/(rootfs|closure|fs)\//, '/$1/x86/') : path;
}
/** Whether this build has the scene compositor (compositor.js and its
 *  worker). gex-eject's feature selection (crates/gex-eject/README.md, "Features") builds
 *  the page with `__GEX_COMPOSITOR__` defined false, and everything behind
 *  this constant drops out of the bundle; anything else has it. */
const GEX_COMPOSITOR = typeof __GEX_COMPOSITOR__ === 'undefined' ? true : __GEX_COMPOSITOR__;

/** Debian's multiarch directory name for the guest. */
export function multiarch() {
  return guestArch() === 'x86' ? 'x86_64-linux-gnu' : 'aarch64-linux-gnu';
}
/** ELF `e_machine` of a guest binary. */
export function elfMachine() {
  return guestArch() === 'x86' ? 0x3e : 0xb7;
}
// `?mem64=1` (globalThis.GEX_MEM64): a 64-bit guest memory -- the wasm64
// shim and seed and the kernel that reaches a process's memory with i64 (`.m64`, x86 only).
const m64Infix = () => (globalThis.GEX_MEM64 && guestArch() === 'x86' ? '.m64' : '');
const SHIM_URL = () => assetUrl(`/gex/gex_browser${archInfix()}${m64Infix()}.shared.wasm`);
const SEED_URL = () => assetUrl(`/gex/seed${archInfix()}${m64Infix()}.shared.wasm`);
const KERNEL_URL = () => assetUrl(`/gex/gex_kernel${archInfix()}${m64Infix()}.wasm`);

let shimBytes = null;
let seedBytes = null;

/// Guard the wasm magic bytes: a dev server that cannot find a file answers
/// `index.html` with status 200, so `res.ok` proves nothing and the reader
/// would blame the build for a magic-word error that is really a bad request.
function checkWasm(url, res, bytes) {
  if (!url.endsWith('.wasm')) return bytes;
  const m = bytes.subarray(0, 4);
  if (m[0] === 0x00 && m[1] === 0x61 && m[2] === 0x73 && m[3] === 0x6d) return bytes;
  const head = [...m].map((b) => b.toString(16).padStart(2, '0')).join(' ');
  throw new Error(
    `${url} is not a wasm module: ${bytes.length} bytes of `
    + `${res.headers.get('content-type') || 'unknown type'} beginning ${head}`
    + (head === '3c 21 64 6f' ? ' -- that is an HTML page, so the server did not find it' : ''),
  );
}

/** `(url) => Promise<WebAssembly.Module>` for a compressed unit: fetched,
 *  decoded (`zstd`, the decoder module's URL) and compiled by one Worker
 *  (unit-worker.js), started at the first. */
function unitModuleWorker(zstd) {
  let w = null;
  let n = 0;
  const waiting = new Map();
  const fail = (error) => {
    for (const p of waiting.values()) p.reject(new Error(error));
    waiting.clear();
  };
  return (url) => {
    if (!w) {
      w = new Worker(new URL('./unit-worker.js', import.meta.url), { type: 'module', name: 'units' });
      w.onmessage = ({ data: { id, mod, error } }) => {
        const p = waiting.get(id);
        waiting.delete(id);
        if (mod) p?.resolve(mod);
        else p?.reject(new Error(error));
      };
      w.onerror = (e) => fail(`the units' Worker: ${e?.message ?? 'did not start'}`);
    }
    const id = ++n;
    return new Promise((resolve, reject) => {
      waiting.set(id, { resolve, reject });
      w.postMessage({ id, url, zstd });
    });
  };
}

/** `fetchUnit`, but a unit the fill brings (`late`: key to its Module's
 *  promise) from there: one download of it, not two. */
function lateFirst(late, fetchUnit) {
  if (!late?.size) return fetchUnit;
  return (url) => late.get(url.split('/').at(-2)) ?? fetchUnit(url);
}

async function fetchBytes(url, onProgress) {
  // Revalidated, never taken from cache on faith: the shim and seed have fixed
  // URLs with no content hash, so a browser would keep serving a broken build
  // across reloads. One conditional request, a 304 for the common case.
  const res = await fetch(url, { cache: 'no-cache' });
  if (!res.ok) throw new Error(`${url}: ${res.status} ${res.statusText}`);
  const total = Number(res.headers.get('content-length')) || 0;
  if (!onProgress || !res.body) {
    return checkWasm(url, res, new Uint8Array(await res.arrayBuffer()));
  }
  const reader = res.body.getReader();
  const chunks = [];
  let got = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    got += value.length;
    onProgress(got, total);
  }
  const out = new Uint8Array(got);
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return checkWasm(url, res, out);
}

/** The kernel's bytes until it is compiled. */
let kernelBytes = null;
/** What `?xcache=`'s fingerprint keeps of the shim and the kernel once
 *  their bytes are let go (`letGo`): a hash of each. */
const prints = { shim: null, kernel: null };
const xcacheOn = () => !!flag('xcache') && flag('xcache') !== '0';
/** The bytes of `which` ('shim', 'kernel') are no longer needed once
 *  compiled: their hash is kept for the fingerprint and the bytes dropped,
 *  not held for the page's life. A later `loadRuntime` fetches them again. */
function letGo(which, bytes) {
  if (xcacheOn() && bytes) prints[which] ??= xcHash(bytes);
  if (which === 'shim') shimBytes = null;
  else kernelBytes = null;
}

/** Whether the page still holds the shim's or the kernel's bytes (a test
 *  asks, after boot and a program). */
export function heldBytes() {
  return { shim: !!shimBytes, kernel: !!kernelBytes };
}

/**
 * `?xcache=1`: the name the translation cache is saved under, a fingerprint of everything a saved module depends on: the shim and
 * kernel this page loaded (an SP dist and an MP dist load different ones),
 * the dist's own flavour if it says, and the switches that change emission.
 * A different build never reads another's cache.
 */
export function xcacheName() {
  if (!xcacheOn()) return undefined;
  const shim = prints.shim ?? (shimBytes && xcHash(shimBytes));
  const kernel = prints.kernel ?? (kernelBytes && xcHash(kernelBytes));
  if (!shim || !kernel) return undefined;
  const switches = ['tso', 'coherentloads', 'arch', 'mem64', 'fjit', 'fjitlibs', 'dense', 'dual', 'cgen', 'providers', 'elfunits', 'provskip', 'pins', 'sp', 'inlinedispatch', 't1fold', 'liftmax', 'unitcache']
    .map((n) => `${n}=${flag(n) ?? ''}`)
    .join('&')
    // `tso=arm` names one switch value for two flavours: the host's is named too.
    + (flag('tso') === 'arm' && tso() ? '&tsohost=1' : '')
    // Named only when on: the cache of a page without it keeps its name.
    + (globalThis.GEX_NULL_GUARD ? '&nullguard=1' : '')
    + (globalThis.GEX_MXCSR ? '&mxcsr=1' : '');
  const flavour = globalThis.GEX_FLAVOUR ?? (typeof __GEX_FLAVOUR__ !== 'undefined' ? __GEX_FLAVOUR__ : '');
  // The page's path, which a system Worker is told (`pageGlobals`): its own
  // is the Worker's script, the same for every page. And the cache's own
  // name, when the page gave one (`?xcache=NAME`).
  const path = globalThis.GEX_PAGE_PATH ?? globalThis.location?.pathname ?? '';
  const named = flag('xcache') === '1' ? '' : `&xcache=${flag('xcache')}`;
  return `xc-${xcHash(shim, kernel, switches + named, flavour, path)}`;
}

let fetchOther = null;
const otherBytes = (url) => (fetchOther ?? fetchBytes)(url);

/** The page's copies, handed to the system Worker so the single build does
 *  not fetch the 5 MB shim a second time. */
export function primeRuntime(r) {
  // How the other memory's runtime is fetched when a process first needs it
  // (`other` below): an ejected site's page goes through its pack, where it
  // is compressed.
  if (r?.fetchOther) fetchOther = r.fetchOther;
  if (r?.shimBytes && !shimBytes) shimBytes = r.shimBytes;
  if (r?.seedBytes && !seedBytes) seedBytes = r.seedBytes;
  // An ejected site's page fetches the kernel through its pack too (app.js).
  if (r?.kernelBytes && !kernelBytes) kernelBytes = r.kernelBytes;
}

/** The two wasm modules every demo needs, fetched once for the page's life. */
export async function loadRuntime(onProgress) {
  if (!shimBytes) shimBytes = await fetchBytes(SHIM_URL(), onProgress);
  if (!seedBytes) seedBytes = await fetchBytes(SEED_URL());
  return { shimBytes, seedBytes };
}

/** The page's copies of the shim and seed, if it fetched them already
 *  (`primeRuntime` for the system Worker). */
export function loadedRuntime() {
  return shimBytes ? { shimBytes, seedBytes } : undefined;
}

export { fetchBytes };

/** Where the runtime's three modules are (an ejected site packs them). */
export function runtimeUrls() {
  return { shimBytes: SHIM_URL(), seedBytes: SEED_URL(), kernelBytes: KERNEL_URL() };
}

/**
 * `?tso=1`: x86 ordering on a weakly ordered host (an arm64 machine). Guest
 * loads and stores are emitted as atomic wasm accesses where aligned
 * (`?coherentloads=1`, the same thing by its older name), which V8 on arm64
 * makes `ldar`/`stlr`; plain ones there reorder, and x86 code relies on them
 * not doing so. Translated code only: tier 1 and the function JIT. Function-
 * tier units and closures, compiled ahead with plain accesses, are not used
 * (the shim refuses units too). Off by default. `tso=arm` turns it on only
 * when the host is arm64 ([`detectTso`]): a page whose program needs x86
 * ordering sets that, since on x86 an atomic store
 * is an `xchg` and the flavour costs every program that does not need it.
 */
function tso() {
  const v = flag('tso') ?? flag('coherentloads');
  if (v === 'arm') return tsoHost;
  return !!v && v !== '0';
}

/** Whether the host is arm64 (the browser's own word for it), for `tso=arm`.
 *  Set once by [`detectTso`]. */
let tsoHost = false;

/**
 * With `tso=arm`: ask the browser what CPU it runs on (`navigator.userAgentData`'s
 * high-entropy `architecture`, Chromium only; a Worker has it too) before the
 * first program starts. "arm" turns the TSO flavour on; no answer leaves it off.
 * Other values never ask. Returns the effective setting, for a page to report.
 */
export async function detectTso() {
  if (flag('tso') === 'arm') {
    try {
      const ua = globalThis.navigator?.userAgentData;
      const h = ua?.getHighEntropyValues ? await ua.getHighEntropyValues(['architecture']) : null;
      tsoHost = h?.architecture === 'arm';
    } catch {
      tsoHost = false;
    }
  }
  return tso();
}

/**
 * A `?name=` switch, from wherever this is running. `location` does not exist
 * under Node (where the terminal test drives this file), so the same switches
 * arrive there as `GEX_*` environment variables.
 */
/** `?audiolatency=S`: the AudioContext's latency hint in seconds, else
 *  'interactive' (page/src/audio.js). */
function audioLatency() {
  const v = flag('audiolatency');
  return v !== null && v !== '' && Number.isFinite(+v) ? +v : 'interactive';
}

function flag(name) {
  // The single-thread build's system Worker is handed the page's query: its
  // own `location` is the script's.
  const search = pageSearch();
  if (search) return new URLSearchParams(search).get(name);
  return globalThis.process?.env?.[`GEX_${name.toUpperCase()}`] ?? null;
}

/**
 * The global kernel, fetched once and instantiated per Worker.
 *
 * Everything guest processes share and no single process can own: the process
 * table, the filesystem, the pipes. Its memory is a `SharedArrayBuffer`, so the
 * page has to be cross-origin isolated (COOP/COEP headers in `vite.config.js`).
 * A page that is not gets no kernel and one process, which still runs anything
 * that does not fork.
 */
let kernelSpec = null;
/** Start the guest's OpenGL in whichever compositor is drawing.
 *
 * A failure here is not a failed session. The GL device is one part of a
 * desktop -- a device that will not start means GL clients have nowhere to
 * draw, and every other window, the X server and the shell are unaffected.
 * Throwing took the whole run down with it, which is a black page instead of
 * a session with one thing missing from it, and the reason was in a console
 * message rather than anywhere a person would look.
 *
 * The guest's `/dev/gex-gl` still exists and its ring is still there; the
 * ring is marked as having no device (gl-bridge.mjs `failGlChannel`), so a
 * GL client's request fails at once rather than at its timeout. That is the
 * same shape as a machine whose GPU has fallen over, and better than
 * pretending the session is broken.
 */
export async function startHostGl(compositor, glChannel, opts, term) {
  try {
    return await compositor.hostGl(glChannel, opts);
  } catch (e) {
    const why = e?.message || String(e);
    // Said where a person is looking. A GL device that will not start leaves
    // every GL window black and nothing else amiss, so a line in the console
    // is a line nobody reads: the demo's own terminal is where the rest of
    // the session's trouble is reported.
    console.error(`[gex] the guest's OpenGL did not start (${opts.backend || 'wgpu'}): ${why}`);
    term?.writeln?.(`gex: the guest's OpenGL did not start (${opts.backend || 'wgpu'}): ${why}`);
    term?.writeln?.('gex: GL windows will be blank; everything else still runs');
    globalThis.__gexGlError = why;
    if (glChannel?.control) (await import('@gex/gl-bridge.mjs')).failGlChannel(glChannel);
    return null;
  }
}

export async function loadKernel() {
  if (kernelSpec !== null) return kernelSpec || null;
  // Test for `SharedArrayBuffer` directly, not `crossOriginIsolated`: it is
  // what is actually needed, and asking for the constructor works under Node
  // too (which lets the terminal test drive this same file). The
  // single-thread build needs none: its kernel memory is plain, held by
  // every fiber.
  if (typeof SharedArrayBuffer !== 'function' && !(SP_CODE && single.on)) {
    kernelSpec = false;
    return null;
  }
  try {
    const bytes = kernelBytes ?? await fetchBytes(KERNEL_URL());
    kernelBytes = bytes;
    const module = await WebAssembly.compile(bytes);
    letGo('kernel', bytes);
    kernelSpec = {
      // The maximum must match `--max-memory` in `hosts/kernel/build.sh`
      // exactly: a shared memory carries its maximum in its type, so a host
      // offering a different one cannot instantiate the module. It reserves
      // address space, not pages -- the rootfs lives here and grows as files
      // arrive.
      module,
      memory: new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true }),
      // The empty process memories every Worker's instance imports for the
      // flavour it does not use, made once (lazy-fetch.mjs `spareMemories`).
      spare: spareMemories(!(SP_CODE && single.on)),
    };
  } catch {
    kernelSpec = false;
    return null;
  }
  return kernelSpec;
}

/**
 * How to make a Worker running one guest process. A page has `Worker`, Node has
 * `worker_threads` (the terminal test drives this file under Node). The browser
 * branch is spelled out inline because that is the shape vite bundles; the Node
 * path is kept out of a literal so the bundler leaves it alone.
 */
async function guestWorkerFactory() {
  // The single-thread build: a process is a fiber of this realm.
  if ((SP_CODE && single.on)) return () => new FiberWorker();
  if (!MP_CODE) throw new Error('the single-thread build runs its processes as fibers (single.mjs)');
  if (typeof Worker === 'function') {
    // A guest Worker is made for the pool before it has a process, so its
    // name (what a profiler or trace lists its thread by) is a serial; the
    // host records which processes and threads each one ran (`gexName`).
    let serial = 0;
    const named = (w, name) => ((w.gexName = name), w);
    const make = () => {
      const name = `gex-w${++serial}`;
      return named(new Worker(new URL('../../hosts/browser/js/worker.mjs', import.meta.url), {
        type: 'module',
        name,
      }), name);
    };
    // The module graph fetched once, each Worker started from it
    // (worker-graph.js); `?workergraph=0` loads each by URL.
    const url = pageParams().get('workergraph') === '0' ? null : workerUrlOf(make);
    const blob = url ? await workerGraph(absUrl(url), (net, cached) => {
      workerGraphBytes.net += net;
      workerGraphBytes.cached += cached;
    }) : null;
    if (!blob) return make;
    return () => {
      const name = `gex-w${++serial}`;
      return named(new Worker(blob, { type: 'module', name }), name);
    };
  }
  const { Worker: NodeWorker } = await import('node:worker_threads');
  const where = ['..', '..', 'hosts', 'browser', 'js', 'worker.mjs'].join('/');
  return () => new NodeWorker(new URL(where, import.meta.url));
}

/**
 * Run one program to completion. `files` is the guest's filesystem as a
 * path-to-bytes map, `args` its argv; returns the exit status. The caller owns
 * the terminal, so a demo can print its own banner first.
 *
 * The program runs in a Worker, and so does everything it forks -- what
 * replaced the snapshot switcher. Each process has its own memory; what they
 * share is in the global kernel, instantiated against one shared memory.
 */
/**
 * Whether this page runs the single-thread build:
 * `?single=1` asks for it, `?single=0` refuses it, and otherwise a page that
 * has no SharedArrayBuffer (not cross-origin isolated) gets it when the engine
 * has JSPI. A cross-origin isolated page keeps the Worker build.
 */
export function singleMode() {
  if ((SP_CODE && single.on)) return true;
  const want = flag('single');
  if (want === '0') return false;
  if (!singleSupported()) return false;
  return want === '1' || typeof SharedArrayBuffer !== 'function';
}

/** What of the page's state the system Worker needs: its query and every
 *  `GEX_*` switch the page set. */
export function pageGlobals() {
  const out = { GEX_PAGE_SEARCH: pageSearch(), GEX_PAGE_PATH: globalThis.GEX_PAGE_PATH ?? globalThis.location?.pathname };
  for (const k of Object.keys(globalThis)) {
    if (!k.startsWith('GEX_')) continue;
    const v = globalThis[k];
    if (v === null || ['string', 'number', 'boolean', 'undefined'].includes(typeof v)) out[k] = v;
  }
  // The runtime package's files (page-env.js assetUrl): the Worker's build
  // names none of its own.
  if (globalThis.GEX_ASSET_FILES) out.GEX_ASSET_FILES = globalThis.GEX_ASSET_FILES;
  return out;
}

/** `/dev/dsp`'s output, played from the first gesture on: a Worker's
 *  'audio' message (worker.mjs). */
export function audioFrom(m) {
  startAudio({ ...m, clock: flag('audioclock') !== '0', latency: audioLatency(), loop: flag('audioloop') === '1' });
}

/**
 * The X server in the page (`hostX`, page/src/hostx.js): a Worker holding
 * Xwayland and the compositor beside it, or a fiber of this realm in the
 * single-thread build. Resolves once it listens: on display :0's names, or
 * on `listen`, sockets of the global kernel bound already (a caller that
 * held :0 until a client came). `compositor()` is the page's compositor,
 * which takes the server's memory and pixels.
 */
export async function startXServer({ kernel, wlChannel, compositor, term, rootful = false, listen }) {
  let xServer;
  // The single-thread build: the server is a fiber of
  // this realm, the non-pthread Xwayland (page/public/gex/xwayland-st/) whose
  // waits suspend. The same messages, without a Worker between.
  const xRoot = (SP_CODE && single.on) ? '/gex/xwayland-st/' : '/gex/xwayland/';
  const xDir = assetUrl(xRoot);
  // Each file by itself: a bundler names them where it puts them.
  const xFile = (f) => new URL(assetFiles(xRoot)?.[f] ?? `${xDir}${f}`, location.href).href;
  if ((SP_CODE && single.on)) {
    const { HostX } = await import('./hostx.js');
    let deliver = null;
    const hx = new HostX((m) => deliver?.({ data: m }));
    xServer = {
      set onmessage(f) { deliver = f; },
      postMessage: (m) => hx.start(m).catch((err) => {
        hx.log(`[hostx] ${err?.stack || err}`);
        deliver?.({ data: { kind: 'failed', why: String(err?.message || err) } });
      }),
      terminate() {},
    };
  } else if (!MP_CODE) {
    throw new Error('the single-thread build runs its X server in its system Worker');
  } else xServer = new Worker(new URL('./xwayland-worker.js', import.meta.url), { type: 'module', name: 'xwayland' });
  // Waited for below: a guest that starts before the display is listening
  // gets "Can't open display" and dies, and the display here is a name in the
  // global kernel that only exists once the server has bound it.
  let listening = null;
  const up = new Promise((resolve) => (listening = resolve));
  const countX = !!flag('hostxcount');
  xServer.onmessage = (e) => {
    const m = e.data;
    if (m.kind === 'log') {
      term.writeln(m.text);
      // `?hostxcount=1`: to the console too, timed, for a harness.
      if (countX) console.log(`${(performance.now() / 1000).toFixed(2)} ${m.text}`);
    } else if (m.kind === 'win') console.log(`[win] ${m.text}`);
    else if (m.kind === 'wl-memory') compositor()?.addMemory(m.token, m.memory, m.kernel);
    else if (m.kind === 'wl-pixels') compositor()?.addPixels?.(m.token, m.id, m.buffer, m.replaces);
    else if (m.kind === 'x-gl') compositor()?.addXGl?.(m.ctl, m.pix);
    else if (m.kind === 'ready') listening(true);
    else if (m.kind === 'failed') {
      term.writeln(`gex: the X server failed: ${m.why}`);
      listening(false);
    }
  };
  xServer.postMessage({
    kind: 'start',
    xwaylandUrl: xFile('Xwayland.mjs'),
    hostwlUrl: xFile('gexhostwl.wasm'),
    keymapUrl: xFile('gex.xkm'),
    // What Xwayland.mjs asks its own directory for (its wasm), by name.
    xwaylandFiles: { 'Xwayland.wasm': xFile('Xwayland.wasm') },
    // Its memory is a wasm32 one, also under `?mem64=1`: the kernel module
    // serves both (gex-kmem, lazy-fetch.mjs `processMemories`).
    kernel: { module: kernel.module, memory: kernel.memory, spare: kernel.spare },
    wlChannel,
    // A name in the scene arena that no guest pid can collide with: pids
    // count up from 1, and this is 'xwl1' as ASCII.
    token: 0x78776c31,
    display: ':0',
    listen,
    // `hostX: 'rootful'`: one fullscreen X screen with no window manager of
    // the compositor's, for a session that runs its own (KWin, ...).
    rootful: rootful || undefined,
    // `?xverbose=N`: the X server's own log, which is the only thing that
    // says what it told a client.
    verbose: globalThis.GEX_HOSTX_VERBOSE || Number(pageParams().get('xverbose')) || undefined,
    trace: globalThis.GEX_HOSTX_TRACE || undefined,
    wlTrace: flag('wltrace') ? 1 : undefined,
    count: flag('hostxcount') ? 1 : undefined,
    // GEX_LOG=win: a line as a client's named window is first mapped.
    win: logNamed('win') ? 1 : undefined,
  });
  // Two modules and a kernel to instantiate; a slow machine takes a few
  // seconds. A run that goes ahead without the display is worse than one that
  // says so.
  const ok = await Promise.race([
    up,
    new Promise((r) => setTimeout(() => r('timeout'), 60000)),
  ]);
  if (ok === 'timeout') term.writeln('gex: the X server did not come up in 60s');
  return xServer;
}

/** The exec options every process takes from the page (`?strace=1`, the
 *  heartbeat, the link cache, recompilation), with its user. */
export function execOptions({ uid = 0, gid = uid } = {}) {
  return { ...startOptions('opts'), recompileObjects: defaultRecompile(), heartbeat: defaultHeartbeat(), link: defaultLink(), uid, gid };
}
const defaultRecompile = () => (typeof globalThis.GEX_RECOMPILE_OBJECTS === 'number' ? globalThis.GEX_RECOMPILE_OBJECTS : 0);
const defaultHeartbeat = () => globalThis.GEX_HEARTBEAT ?? 0;
const defaultLink = () => Number(globalThis.GEX_LINK ?? 0);

/** `start()` for the host's Mesa on `compositor` (a DOM desktop's window
 *  canvases when `dom`, else the scene compositor's), once however often it
 *  is called: resolves Mesa's description, or null if it would not start. */
export function glStarter(compositor, glChannel, { dom, backend = 'wgpu', source = null }, term) {
  const opts = dom ? { api: 'core', backend, source }
    : { api: 'core', backend, canvasBlit: pageParams().get('glcanvas') === '1', source };
  let started = null;
  return () => (started ??= startHostGl(compositor, glChannel, opts, term));
}

/**
 * The process host every guest process runs under (host.mjs): its Worker
 * pool, the runtime's modules, and the switches each Worker takes (`extra`).
 * `o` is what of `runProgram`'s options it needs, and the channels made for
 * the page's devices; `on` its callbacks (ProcessHost's `onOutput`,
 * `onExit`, `readStdin`, `onMessage`).
 */
export async function makeProcessHost({
  kernel, closure = null, lateUnits = null, heartbeat = defaultHeartbeat(), fill = null, lazyCache = null, pageFs = null,
  restBundle = false, identities = null, providerSources = null, stateful = null, recompileObjects = defaultRecompile(),
  closureBuildNumbered = false, link = defaultLink(), uid, preModules = null, glChannel = null, wlChannel = null,
  netChannel = null, network = null, winsize,
}, on) {
  const { shimBytes, seedBytes } = await loadRuntime();
  const makeGuestWorker = await guestWorkerFactory();
  const host = new ProcessHost({
  makeWorker: makeGuestWorker,
  // A compressed unit (`?cgenzst=1`) is fetched, decoded and compiled in a
  // Worker of its own, off this thread.
  unitModule: globalThis.GEX_CGEN && globalThis.GEX_CGEN_ZST && globalThis.GEX_CGEN_ZSTD
    ? lateFirst(lateUnits, unitModuleWorker(absUrl(globalThis.GEX_CGEN_ZSTD))) : null,
  shim: shimBytes,
  // The closure's image module in place of the seed: it holds the program's
  // own text, already lifted.
  guest: closure?.image ?? seedBytes,
  kernel,
  // `?mem64=1`: the 32-bit memory's runtime, fetched when a process first
  // needs it (host.mjs `#other`: 64-bit Wine's wineserver, a wasm32 module).
  // Otherwise, with the global kernel, the 64-bit memory's runtime, for a
  // program an exec asks one for (its GEX_MEM64=1, or its
  // image's user.gex.mem64 record); the page itself stays 32-bit. Fetched
  // only then, so a page that never asks loads nothing more.
  other: m64Infix() ? async () => ({
    shim: await otherBytes(assetUrl(`/gex/gex_browser${archInfix()}.shared.wasm`)),
    guest: await otherBytes(assetUrl(`/gex/seed${archInfix()}.shared.wasm`)),
    // the one kernel module serves both flavours (gex-kmem)
    kernel: kernel.module,
  }) : kernel && guestArch() === 'x86' ? async () => ({
    shim: await otherBytes(assetUrl(`/gex/gex_browser${archInfix()}.m64.shared.wasm`)),
    guest: await otherBytes(assetUrl(`/gex/seed${archInfix()}.m64.shared.wasm`)),
    kernel: kernel.module,
  }) : null,
  extra: {
    // The switches every page takes (hosts/browser/js/options.mjs).
    ...startOptions('extra'),
    // In `extra` so every Worker gets it, not only pid 1: the interesting
    // process is the X server and the client it forks, not the demo's shell.
    heartbeat: heartbeat || undefined,
    fill: fill || undefined,
    lazyCache: lazyCache || undefined,
    pageFs: pageFs || undefined,
    restBundle: restBundle || undefined,
    identities: identities || undefined,
    // The closure's libraries, in `extra` so a forked child gets them too: a
    // child adopts its parent's dispatch directory but its own tables start
    // empty, so it must instantiate the modules itself for the directory to
    // point at.
    closure: closure?.libs,
    // `?trace=1` turns on the shim's narration in every process.
    trace: flag('trace') ? 1 : undefined,
    // `?hot=1` asks every process to dump its hottest back edges when it ends.
    hot: flag('hot') ? 1 : undefined,
    // `?wholepage=1` fills an image page entirely on the first ask instead
    // of the 32-slot span around the miss. An experiment for #136: it says
    // whether the closure has the blocks an exec'd process lifts anyway.
    wholePage: flag('wholepage') ? 1 : undefined,
    // `?vforktrace=1`: each posix_spawn child run in its parent's Worker
    // (hosts/browser/src/vfork.rs), on the console.
    vforkTrace: flag('vforktrace') ? 1 : undefined,
    trapPc: flag('trappc') ? Number.parseInt(flag('trappc'), 16) : undefined,
    // Every process gets a memory two Workers can hold, so a `clone` asking
    // for a thread can be given one. `worker.mjs` passes this to the `Gex`
    // constructor as `shared`. A guest's threads are Workers -- the only
    // shape the shim has now.
    shared: true,
    // The GL ring's shared buffers, so a Worker that opens /dev/gex-gl can
    // reach the page's GlDevice.
    glChannel: glChannel ?? undefined,
    // The Wayland ring's shared buffers, so a Worker that binds the
    // compositor can carry its frames to the page's canvas. Like glChannel.
    wlChannel: wlChannel ?? undefined,
    // The page's network, served to every Worker over shared memory
    // (net-bridge.mjs).
    netChannel: netChannel ?? undefined,
    // The single-thread build's fibers share this realm, and so the
    // page's sockets themselves.
    pageNet: (SP_CODE && single.on) ? network ?? undefined : undefined,
    pageHosts: netChannel || (SP_CODE && single.on) ? undefined : network?.pageHosts,
    winsize,
    // Every guest write mirrored to the console and a crash narrated where it
    // happens, since a Worker that falls over takes its last messages with it.
    debugExit:
      !!globalThis.GEX_DEBUG_EXIT ||
      (typeof process !== 'undefined' && !!process.env?.GEX_DEBUG_EXIT),
    // `?dense=0`: classic per-slot dispatch tables (gex_emit::dense off).
    dense: globalThis.GEX_DENSE === 0 ? false : globalThis.GEX_DENSE === 2 ? 2 : undefined,
    // `?cgen=1|2`: function-tier units (1 where one exists, 2 required for
    // a program image), read from the server's `/gex-cgen/` (a unit
    // cache, GEX_CGEN_CACHE's layout) with synchronous requests.
    // `?cgenurl=` (an ejected project's build/web/cgen) instead.
    // Otherwise the export providers the dist carries (gex/cgen/providers/),
    // with no units; `?providers=0` runs glibc's
    // own code translated instead.
    // Absolute: a Worker started from a blob URL has no base to resolve a path against.
    cgen: globalThis.GEX_CGEN && !tso() ? unitsAt({ mode: globalThis.GEX_CGEN, url: absUrl(globalThis.GEX_CGEN_URL ?? '/gex-cgen'), zstd: globalThis.GEX_CGEN_ZSTD && absUrl(globalThis.GEX_CGEN_ZSTD), zst: globalThis.GEX_CGEN_ZST ? true : undefined })
      : flag('providers') === '0' ? undefined
      : { mode: 8, url: assetUrl('/gex/cgen'), files: assetFiles('/gex/cgen/') },
    // `?aot=1`: an ejected project's run, with no run-time translation (a
    // miss stops the run, saying where).
    jit: globalThis.GEX_NO_JIT ? false : undefined,
    aot: globalThis.GEX_AOT ? true : undefined,
    // `?mem64=1`: every Worker of the process builds or joins a 64-bit memory.
    mem64: globalThis.GEX_MEM64 ? 1 : undefined,
    // `?fjit=1`: the function JIT in every process.
    fjit: globalThis.GEX_FJIT || undefined,
    // `?xcache=1`: the translation cache across page loads.
    xcache: xcacheName(),
    // `?unitcache=1`: compiled translation units shared by every process.
    unitCache: globalThis.GEX_UNIT_CACHE || undefined,
    // `?elfunits=name,...`: shared objects that get function-tier units (cgen/libprov.rs).
    elfUnits: globalThis.GEX_ELF_UNITS || undefined,
    // Provider sources from outside gex (`name\ntext` entries, NUL
    // between): an ejected project's, as GEX_PROVIDER_DIRS natively.
    providerSources: providerSources || globalThis.GEX_PROVIDER_SOURCES || undefined,
    // `dll=file,...`: stateful providers replacing Windows libraries, their
    // modules under the unit cache (cgen.rs `set_stateful`).
    stateful: stateful || globalThis.GEX_STATEFUL || undefined,
    // `?dlchecked=1`: guard wasm32 modules' indirect calls (cgen/dylink.rs `checked`).
    dlChecked: flag('dlchecked') ? 1 : undefined,
    // `?jitdiag=1`: retire counts by page and call, and the watch on a page that keeps translating.
    jitDiag: flag('jitdiag') ? 1 : undefined,
    // `?sharetrace=1`: say how compiled modules are shared between a process's Workers.
    shareTrace: flag('sharetrace') ? 1 : undefined,
    // `?log=1` (or `?log=record,...`): gex's debug output, off by default
    // (hosts/browser/js/log.mjs); every Worker is handed it.
    log: (globalThis.GEX_LOG ??= flag('log') || undefined) || undefined,
    preModules: preModules ?? undefined,
    // Lightweight adopt-phase narration (no write-flood); see adoptProcess.
    adoptTrace:
      !!globalThis.GEX_ADOPT_TRACE ||
      (typeof process !== 'undefined' && !!process.env?.GEX_ADOPT_TRACE),
    // Thread-Worker attach/adopt narration: pinpoints a stranded thread whose
    // wasm instantiate never completes (threads-worker-per-thread residual).
    attachTrace:
      !!globalThis.GEX_ATTACH_TRACE ||
      (typeof process !== 'undefined' && !!process.env?.GEX_ATTACH_TRACE),
    // KiB of mapped text per object to statically recompile once hot. Eight
    // megabytes is the largest that works today (above it the graphics rootfs
    // meets #73) and covers every library these demos run except Mesa's
    // software rasteriser, which nothing here wants.
    recompileObjects,
    // `?nointrinsics=1` translates glibc's own memcpy/memcmp/memset instead
    // of substituting the shim's, to tell a substitution fault from a codegen
    // one. In `extra` so every process gets it.
    noIntrinsics: flag('nointrinsics') ? 1 : undefined,
    // `?pregrow=1`: grow guest memory to a large floor up front. Tests whether
    // a concurrent memory.grow is what stalls a spawned thread's wasm
    // instantiate (native-wayland-gtk-thread-stall). In `extra` so every
    // Worker sets it before running.
    pregrow: flag('pregrow') ? 1 : undefined,
    // `?checksp=1` checks `sp` at the top of every block, so a corrupt (e.g.
    // sign-extended) stack pointer names the block that ARRIVES with the bad
    // value rather than the later one that dereferences it and traps. In
    // `extra` so every Worker enables it.
    checkSp: flag('checksp') ? 1 : undefined,
    // `?filltime=1` (debugging): every Worker says when a dense module a
    // dispatch waited for took longer than any before it. In `extra` for
    // every Worker.
    fillTime: flag('filltime') ? 1 : undefined,
    // `?claimpages=0`: publish closure dir entries at the build's slot numbers
    // instead of per-process demand numbering — a diagnostic for the
    // many-object-closure wrong-dispatch crash. In `extra` for every Worker.
    claimPages:
      flag('claimpages') != null
        ? (flag('claimpages') === '0' ? 0 : 1)
        : (closureBuildNumbered ? 0 : undefined),
    // `?intrinsicmask=N` enables only some substitution families, to bisect
    // which one corrupts memory: bit0=memcpy, bit1=memset, bit2=memcmp.
    // In `extra` so every process gets it.
    intrinsicMask: flag('intrinsicmask') == null ? undefined : parseInt(flag('intrinsicmask'), 10) >>> 0,
    // `?copyloop=1` emits memcpy as a forward byte loop, to tell a
    // `memory.copy` fault from a bad-argument one. In `extra` for every process.
    copyLoop: flag('copyloop') ? 1 : undefined,
    // `?coherentloads=1`: guest loads and stores as atomic wasm accesses
    // where aligned, so V8 cannot move them across the guest's fences. In
    // `extra` for every Worker.
    coherentLoads: tso() ? 1 : undefined,
    // `?wltrace=1` turns on gex-wayland protocol tracing (buffers, commits,
    // "produced no frame") to debug why a client's window is not shown.
    wlTrace: flag('wltrace') ? 1 : undefined,
    // In `extra` so every forked command gets it, not just the shell that
    // starts the demo (which never sees `opts`).
    link: link || undefined,
    // Who the guest is, in `extra` so a forked child gets it.
    uid,
    gid: uid,
  },
    ...on,
  });
  // The host compiles the shim at its first process and keeps the Module.
  letGo('shim', shimBytes);
  return host;
}

/** The page's network served to every Worker (net-bridge.mjs), or null. A
 *  WebSocket in the guest's own Worker delivers on that Worker's event loop,
 *  which a wasm32 program waiting in a syscall never gives back. */
export async function openNet(network) {
  // shared memory: an isolated page's, or the Node test host's (which has
  // no crossOriginIsolated)
  if (!network || typeof SharedArrayBuffer !== 'function' || globalThis.crossOriginIsolated === false) return null;
  const { makeNetChannel, serveNet } = await import('../../hosts/browser/js/net-bridge.mjs');
  const channel = makeNetChannel();
  if (channel) serveNet(network, channel);
  return channel;
}

/** The GL ring the guest's /dev/gex-gl talks over (gl-bridge.mjs); `d3d`:
 *  /dev/gex-d3d too. */
export async function openGlChannel(d3d = false) {
  if (!globalThis.crossOriginIsolated) {
    throw new Error('the guest GL needs a cross-origin isolated page (SharedArrayBuffer bridge).');
  }
  const { makeGlChannel } = await import('@gex/gl-bridge.mjs');
  const channel = makeGlChannel();
  channel.d3d = !!d3d;
  return channel;
}

/** `start()` once the guest first opens its GL (gl-bridge.mjs STATE): a GL
 *  program started from a shell, not from the embedder, would otherwise
 *  make its first request into a ring nothing serves, wait thirty seconds
 *  for the answer, and die. */
export function glOnFirstRequest(glChannel, start) {
  if (!glChannel?.control || typeof Atomics.waitAsync !== 'function') return;
  const ctl = new Int32Array(glChannel.control);
  let started = false;
  (async () => {
    while (!started) {
      const st = Atomics.load(ctl, 0);
      if (st === 1) {
        console.warn('[gex] the guest opened its GL: starting Mesa');
        started = true;
        start(); // idempotent: the embedder may have started it already
        break;
      }
      const w = Atomics.waitAsync(ctl, 0, st, 5000);
      if (w.async) await w.value;
    }
  })();
}

/**
 * The Wayland compositor. The protocol state machine runs in each guest
 * process (the Rust shim, next to the client's memory, which is the only
 * place that can read a wl_shm pool); the scene, the output and the input
 * live in one worker that owns a WebGPU device. What crosses between them is
 * a shared arena of surface descriptors -- never a frame. `wl` a canvas (the
 * scene compositor) or an element (DomDesktop: windows as DOM elements,
 * decorations in the page's own HTML). `{ wlChannel, compositor, dom }`.
 */
export async function openWayland({ wl, wlInput = null, wlScale = 1, onDesktop = null, say = () => {} }) {
  if (!globalThis.crossOriginIsolated) {
    throw new Error('the Wayland compositor needs a cross-origin isolated page (SharedArrayBuffer bridge).');
  }
  const { makeWlChannel, setOutputSize } = await import('@gex/wl-bridge.mjs');
  const wlChannel = makeWlChannel();
  const element = typeof wl.getContext !== 'function' && wl.nodeType === 1 ? wl : null;
  if (element) {
    const { DomDesktop } = await import('./dom-compositor.js');
    const compositor = DomDesktop.create(element, wlChannel, { width: element.clientWidth, height: element.clientHeight });
    globalThis.__gexDesktop = compositor;
    // The page lays the windows out (bare-desktop.js, say) before any appears.
    onDesktop?.(compositor);
    // A GL device that dies mid-session leaves black windows and no other
    // sign; say it where the session's other trouble is reported.
    compositor.onglfailed = (why) => {
      say(`gex: the guest's OpenGL is gone: ${why}`);
      say('gex: GL windows will be blank; everything else still runs');
    };
    return { wlChannel, compositor, dom: true };
  }
  if (typeof wl.getContext !== 'function') return { wlChannel, compositor: null, dom: false };
  // A build without the scene compositor (gex-eject's `"compositor": false`)
  // has no compositor.js: the page passes an element instead (above).
  if (!GEX_COMPOSITOR) throw new Error('this build has no compositor: give `wl` an element (bare-desktop.js)');
  setOutputSize(wlChannel, wl.width, wl.height, Math.max(1, Math.round(wlScale)));
  const { Compositor } = await import('./compositor.js');
  const compositor = await Compositor.create(wl, wlChannel, wlInput || wl, { scale: wlScale });
  globalThis.__gexCompositor = compositor;
  return { wlChannel, compositor, dom: false };
}

export { loadKernel as globalKernel };

/**
 * The DOM desktop for a page whose guests are in the single-thread build's
 * system Worker: its channel a mirror of the Worker's arena (wl-bridge.mjs
 * `mirrorChannel`), its writes sent there by `post(fn, args)`; `apply` takes
 * the Worker's updates (`mirrorTake`).
 */
export async function openWaylandMirror({ wl, post, onDesktop = null, say = () => {} }) {
  const { mirrorChannel, mirrorApply } = await import('@gex/wl-bridge.mjs');
  const wlChannel = mirrorChannel(post);
  const { DomDesktop } = await import('./dom-compositor.js');
  const compositor = DomDesktop.create(wl, wlChannel, { width: wl.clientWidth, height: wl.clientHeight });
  globalThis.__gexDesktop = compositor;
  onDesktop?.(compositor);
  compositor.onglfailed = (why) => {
    say(`gex: the guest's OpenGL is gone: ${why}`);
    say('gex: GL windows will be blank; everything else still runs');
  };
  const pixelsTo = (token, id, buffer, replaces) => compositor.addPixels(token, id, buffer, replaces);
  return { wlChannel, compositor, dom: true, apply: (u) => mirrorApply(wlChannel, u, pixelsTo) };
}

