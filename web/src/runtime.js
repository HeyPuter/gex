// A gex runtime bound to an xterm.js terminal.
//
// The seam is `Env`: gex hands the page every write and asks it for input. What
// this adds is a line discipline. Whether the page or the guest does the line
// editing is the guest's choice (ICANON in its termios), and the page has to
// follow it or every keystroke appears twice.

// Demo build: runtime .wasm as .wasm.zst (zfetch.js); a no-op elsewhere.
import './zfetch.js';
import { Env } from '../../shims/browser/js/gex.mjs';
import { ProcessHost } from '../../shims/browser/js/host.mjs';
import { PageTty } from '../../shims/browser/js/tty.mjs';
import { xcHash } from '../../shims/browser/js/xcache.mjs';
import { single, singleSupported, FiberWorker } from '../../shims/browser/js/single.mjs';
import { spareMemories } from '../../shims/browser/js/lazy-fetch.mjs';
import { assetUrl, pageParams, pageSearch } from './page-env.js';
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

const ICANON = 0x2;
const ECHO = 0x8;

/** Where the built runtime lives, relative to the site root. */
// A memory import's `shared` flag is part of its type, so the shim, seed,
// kernel, closure and host-made memory must all be the shared build or nothing
// links.
//
// One set per guest architecture: the lifter is compiled into the shim, the
// seed's `enter` stub and the kernel's syscall ABI are per-arch too. The
// x86-64 set carries an `.x86` infix; `?arch=x86` (see main.js) selects it.
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
 *  worker). gex-eject's feature selection (docs/eject.md, "Features") builds
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
// `?mem64=1` (globalThis.GEX_MEM64): a 64-bit guest memory (docs/mem64.md step 6) -- the wasm64
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

/** The kernel's bytes, for `?xcache=`'s fingerprint. */
let kernelBytes = null;

/**
 * `?xcache=1`: the name the translation cache is saved under (docs/gexcc.md
 * §8), a fingerprint of everything a saved module depends on: the shim and
 * kernel this page loaded (an SP dist and an MP dist load different ones),
 * the dist's own flavour if it says, and the switches that change emission.
 * A different build never reads another's cache.
 */
function xcacheName() {
  if (!flag('xcache') || flag('xcache') === '0' || !shimBytes || !kernelBytes) return undefined;
  const switches = ['tso', 'coherentloads', 'arch', 'mem64', 'fjit', 'fjitlibs', 'dense', 'dual', 'cgen', 'providers', 'elfunits', 'provskip', 'pins', 'sp', 'inlinedispatch', 't1fold', 'liftmax', 'unitcache']
    .map((n) => `${n}=${flag(n) ?? ''}`)
    .join('&')
    // `tso=arm` names one switch value for two flavours: the host's is named too.
    + (flag('tso') === 'arm' && tso() ? '&tsohost=1' : '')
    // Named only when on: the cache of a page without it keeps its name.
    + (globalThis.GEX_NULL_GUARD ? '&nullguard=1' : '')
    + (globalThis.GEX_MXCSR ? '&mxcsr=1' : '');
  const flavour = globalThis.GEX_FLAVOUR ?? (typeof __GEX_FLAVOUR__ !== 'undefined' ? __GEX_FLAVOUR__ : '');
  return `xc-${xcHash(shimBytes, kernelBytes, switches, flavour, globalThis.location?.pathname ?? '')}`;
}

/** The page's copies, handed to the system Worker so the single build does
 *  not fetch the 5 MB shim a second time. */
export function primeRuntime(r) {
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
 *  'interactive' (web/src/audio.js). */
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
 * An `Env` whose output goes to a terminal and whose input comes from one.
 *
 * A guest read has to be able to wait: gex gives the wasm stack back to JS when
 * every thread is blocked on input and calls `readStdin`, so this is where a
 * promise sits until a key arrives.
 */
class TerminalEnv extends Env {
  constructor(term, tty = null) {
    super();
    this.term = term;
    // The system's terminal, when a global kernel holds one: a keystroke goes
    // into the discipline in the kernel and `^C` becomes `SIGINT` to the
    // foreground group before this returns. Without it there is one process and
    // the line-editing fallback below runs instead.
    this.tty = tty;
    this.pending = [];
    this.line = [];
    this.waiter = null;
    this.closed = false;
    // The guest's terminal flags, as its Worker last reported them: read rather
    // than asked for, since the guest is in a Worker and every process sends its
    // flags with each write and read.
    this.lflag = ICANON | ECHO;
  }

  write(fd, bytes) {
    // Straight through, untouched: the kernel's tty has already done ONLCR, and
    // colour and cursor control pass unchanged.
    this.term.write(bytes.slice());
  }

  now() {
    return BigInt(Math.round(performance.now() * 1e6));
  }

  /** Whether the guest is doing its own line editing. */
  #raw() {
    return (this.lflag & ICANON) === 0;
  }

  #echoes() {
    return (this.lflag & ECHO) !== 0;
  }

  /** A keystroke from xterm. */
  onKey(data) {
    if (this.closed) return;
    if (this.tty) {
      // Straight into the discipline; what comes back as echo is what a terminal
      // shows, and nothing when the program has taken echo over. An answer
      // later, from the terminal's Worker (tty.mjs `RemoteTty`), or now from
      // the single-thread build's: chained, so keys stay in the order typed.
      this.keys = (this.keys ?? Promise.resolve()).then(async () => {
        const { echo, ready } = await this.tty.key(data);
        if (echo) this.term.write(echo);
        // A guest already parked waiting for input is woken inside the kernel;
        // one that gave the stack back to JavaScript is waiting on this.
        if (ready !== 0 && this.waiter) {
          const w = this.waiter;
          this.waiter = null;
          // `true` rather than the bytes: they are already in the guest's read
          // queue, and handing them over here would deliver them twice.
          w(ready < 0 ? false : true);
        }
      }).catch((e) => console.warn(`[gex] terminal: ${e?.message ?? e}`));
      return;
    }
    if (this.#raw()) {
      this.#deliver(data);
      return;
    }
    // Cooked mode: collect a line, echo as we go, deliver on Enter.
    for (const ch of data) {
      if (ch === '\r' || ch === '\n') {
        if (this.#echoes()) this.term.write('\r\n');
        this.line.push('\n');
        this.#deliver(this.line.join(''));
        this.line = [];
      } else if (ch === '\x7f' || ch === '\b') {
        if (this.line.length) {
          this.line.pop();
          if (this.#echoes()) this.term.write('\b \b');
        }
      } else if (ch === '\x03') {
        // Ctrl-C. No signal path from the page yet, so this abandons the line
        // the way a terminal does and says so.
        this.line = [];
        this.term.write('^C\r\n');
        this.#deliver('\n');
      } else if (ch === '\x04') {
        // Ctrl-D on an empty line is end of input.
        if (!this.line.length) {
          this.closed = true;
          this.#deliver(null);
        }
      } else if (ch >= ' ') {
        this.line.push(ch);
        if (this.#echoes()) this.term.write(ch);
      }
    }
  }

  #deliver(text) {
    if (text === null) {
      if (this.waiter) {
        const w = this.waiter;
        this.waiter = null;
        w(false);
      }
      return;
    }
    const bytes = new TextEncoder().encode(text);
    if (this.waiter) {
      const w = this.waiter;
      this.waiter = null;
      w(bytes);
    } else {
      this.pending.push(bytes);
    }
  }

  /**
   * The next thing typed, as bytes, or false at end of input. Returned rather
   * than pushed into a shim (the guest is in a Worker), and everything waiting
   * is joined into one.
   */
  async readStdin() {
    if (this.tty) {
      // Asked of the terminal's Worker, which answers in the order asked: a
      // key sent before this is counted in it, and one sent after finds the
      // waiter set below (its answer comes in a later task than this one).
      const ready = await this.tty.ready();
      if (ready < 0) return false;
      if (ready > 0) return true;
      return new Promise((resolve) => {
        this.waiter = resolve;
      });
    }
    if (this.closed) return false;
    if (this.pending.length) {
      const total = this.pending.reduce((n, b) => n + b.length, 0);
      const out = new Uint8Array(total);
      let at = 0;
      for (const b of this.pending) {
        out.set(b, at);
        at += b.length;
      }
      this.pending = [];
      return out;
    }
    return new Promise((resolve) => {
      this.waiter = resolve;
    });
  }
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
async function startHostGl(compositor, glChannel, opts, term) {
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

async function loadKernel() {
  if (kernelSpec !== null) return kernelSpec || null;
  // Test for `SharedArrayBuffer` directly, not `crossOriginIsolated`: it is
  // what is actually needed, and asking for the constructor works under Node
  // too (which lets the terminal test drive this same file). The
  // single-thread build needs none: its kernel memory is plain, held by
  // every fiber.
  if (typeof SharedArrayBuffer !== 'function' && !single.on) {
    kernelSpec = false;
    return null;
  }
  try {
    const bytes = kernelBytes ?? await fetchBytes(KERNEL_URL());
    kernelBytes = bytes;
    kernelSpec = {
      // The maximum must match `--max-memory` in `shims/kernel/build.sh`
      // exactly: a shared memory carries its maximum in its type, so a host
      // offering a different one cannot instantiate the module. It reserves
      // address space, not pages -- the rootfs lives here and grows as files
      // arrive.
      module: await WebAssembly.compile(bytes),
      memory: new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true }),
      // The empty process memories every Worker's instance imports for the
      // flavour it does not use, made once (lazy-fetch.mjs `spareMemories`).
      spare: spareMemories(!single.on),
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
  if (single.on) return () => new FiberWorker();
  if (typeof Worker === 'function') {
    // A guest Worker is made for the pool before it has a process, so its
    // name (what a profiler or trace lists its thread by) is a serial; the
    // host records which processes and threads each one ran (`gexName`).
    let serial = 0;
    const named = (w, name) => ((w.gexName = name), w);
    const make = () => {
      const name = `gex-w${++serial}`;
      return named(new Worker(new URL('../../shims/browser/js/worker.mjs', import.meta.url), {
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
  const where = ['..', '..', 'shims', 'browser', 'js', 'worker.mjs'].join('/');
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
 * Whether this page runs the single-thread build (docs/single-thread.md):
 * `?single=1` asks for it, `?single=0` refuses it, and otherwise a page that
 * has no SharedArrayBuffer (not cross-origin isolated) gets it when the engine
 * has JSPI. A cross-origin isolated page keeps the Worker build.
 */
export function singleMode() {
  if (single.on) return true;
  const want = flag('single');
  if (want === '0') return false;
  if (!singleSupported()) return false;
  return want === '1' || typeof SharedArrayBuffer !== 'function';
}

/** What of the page's state the system Worker needs: its query and every
 *  `GEX_*` switch a demo or `main.js` set. */
function pageGlobals() {
  const out = { GEX_PAGE_SEARCH: pageSearch() };
  for (const k of Object.keys(globalThis)) {
    if (!k.startsWith('GEX_')) continue;
    const v = globalThis[k];
    if (v === null || ['string', 'number', 'boolean', 'undefined'].includes(typeof v)) out[k] = v;
  }
  return out;
}

/**
 * `runProgram` for the single-thread build, from the page: the whole system
 * runs in one Worker (web/src/system-worker.js); this side holds the terminal
 * and forwards keys, sizes and output.
 */
async function runInSystemWorker(term, opts) {
  // `onFilesLoaded` and `onSystem` are functions, which cannot cross to the
  // Worker (nt.html passes `onSystem`: its every run failed to start here).
  const { network, onNetEvent, onFilesLoaded, onDesktop, onSystem, transfer, wl, wlInput, gl, hostX, preModules, closure, ...rest } = opts;
  // Said once, so a person (and tests/single/run.sh) can tell which build ran.
  term.writeln?.('gex: single-thread build (no SharedArrayBuffer): every guest thread in one Worker');
  // The compositor: a canvas only (not the DOM desktop), drawn by the system
  // Worker, which holds every guest memory it reads pixels out of.
  const canvas = typeof wl?.getContext === 'function' && typeof wl.transferControlToOffscreen === 'function' ? wl : null;
  // GL: the compositor's own (`gl: 'compositor'`) runs in the system Worker
  // beside it, the non-pthread Mesa (web/public/gex/st/); a page-side device
  // does not exist there.
  const glHere = gl === 'compositor' && !!canvas;
  // The X server too: in the system Worker, beside the compositor.
  const xHere = !!hostX && !!canvas;
  if ((wl && !canvas) || (gl && !glHere) || (hostX && !xHere)) {
    term.writeln?.(`gex: the single-thread build has no ${[wl && !canvas && 'DOM desktop', gl && !glHere && 'GL', hostX && !xHere && 'X server'].filter(Boolean).join(', ')} yet${canvas ? '' : '; running without the screen'}`);
  }
  const w = new Worker(new URL('./system-worker.js', import.meta.url), { type: 'module', name: 'system' });
  globalThis.__gexSystemWorker = w;
  const ready = new Promise((resolve, reject) => {
    w.addEventListener('message', (e) => { if (e.data?.kind === 'ready') resolve(); });
    w.addEventListener('error', (e) => reject(new Error(e.message || 'the system Worker failed to start')));
  });
  await ready;
  // The compositor's page side (listeners, canvas size) talks to its worker
  // half through the system Worker: `comp` messages in, `comp-out` back.
  const compListeners = new Set();
  let compStats = null;
  const compPort = {
    postMessage(m, transfer = []) {
      if (m?.kind === 'init' && m.stats) compStats = new Int32Array(m.stats);
      w.postMessage({ kind: 'comp', m }, transfer);
    },
    addEventListener(type, f) { if (type === 'message') compListeners.add(f); },
    removeEventListener(type, f) { compListeners.delete(f); },
    terminate() {},
  };
  w.addEventListener('message', (e) => {
    const m = e.data;
    if (m?.kind === 'comp-out') for (const f of [...compListeners]) f({ data: m.m });
    else if (m?.kind === 'comp-stats' && compStats) compStats.set(m.stats.subarray(0, compStats.length));
  });
  let compositor = null;
  if (canvas && GEX_COMPOSITOR) {
    const { Compositor } = await import('./compositor.js');
    // Before `run`: the Worker takes the canvas first, then starts the guest.
    w.postMessage({ kind: 'comp-open', width: canvas.width, height: canvas.height });
    compositor = await Compositor.create(canvas, null, wlInput || canvas, { makeWorker: () => compPort });
    globalThis.__gexCompositor = compositor;
  }
  const keys = term.onData((data) => w.postMessage({ kind: 'key', data }));
  const resize = term.onResize(({ rows, cols }) => w.postMessage({ kind: 'resize', rows, cols }));
  term.focus?.();
  try {
    return await new Promise((resolve, reject) => {
      w.addEventListener('message', (e) => {
        const m = e.data;
        if (m.kind === 'write') term.write(m.data);
        else if (m.kind === 'net') onNetEvent?.(m.event);
        else if (m.kind === 'done') resolve({ status: m.status, host: null, tenv: null });
        else if (m.kind === 'failed') reject(Object.assign(new Error(m.message), { workerStack: m.stack }));
      });
      w.postMessage({
        kind: 'run',
        globals: pageGlobals(),
        // Already fetched by the page (main.js loadRuntime): copied, not
        // fetched again by the Worker.
        runtime: shimBytes ? { shimBytes, seedBytes } : undefined,
        rows: term.rows,
        cols: term.cols,
        opts: {
          ...rest,
          networkUrl: network?.url ?? null,
          wl: canvas ? { inRealm: true } : undefined,
          gl: glHere ? 'compositor' : undefined,
          hostX: xHere ? hostX : undefined,
        },
      });
    });
  } finally {
    keys.dispose();
    resize.dispose();
    compositor?.stop();
    w.terminate();
  }
}

export async function runProgram(term, opts) {
  if (singleMode() && !single.on) return runInSystemWorker(term, opts);
  return runProgramHere(term, opts);
}

async function runProgramHere(
  term,
  {
    elf,
    files = {},
    // Directories the rootfs contains that hold no files. `gex_add_file`
    // makes the ones on the way to a file, so only the empty ones need
    // saying -- and they are exactly the ones apt looks for.
    dirs = [],
    // Which bundled files are executable; see `untar`.
    modes = {},
    // Hard links, path to target (`untar`'s `hard`): made as links, so the
    // two names are one inode as on the host.
    hard = {},
    // Symbolic links, path to target text (`untar`'s `links`), made as links.
    symlinks = {},
    // Files fetched as they are read, [path, size, url] (`?lazy=`).
    lazy = [],
    // File times, path to seconds (`untar`'s `mtimes`), when they matter.
    mtimes = undefined,
    // Extended attributes, path to { name: bytes } (`untar`'s `xattrs`).
    xattrs = undefined,
    /// `KEY=VALUE` strings for pid 1. Empty takes the shim's default.
    env = [],
    args,
    cwd = '/',
    tty = true,
    network = null,
    onNetEvent = null,
    // Called once pid 1 has copied `files` into the kernel's filesystem: the
    // caller can drop its own references to the bytes (a bundle's buffer).
    onFilesLoaded = null,
    // Buffers `files` views that the caller gives up to pid 1's Worker
    // (moved, not copied; freed there once the kernel has the files).
    transfer = [],
    // Called with the DOM desktop when `wl` is an element, before any window.
    onDesktop = null,
    gl = null,
    // Which Gallium driver the host's Mesa runs when `gl` is 'compositor':
    // gex_wgpu, straight onto WebGPU.
    glBackend = 'wgpu',
    // The compositor's output canvas. Its control is transferred to a worker
    // that owns a WebGPU device and draws the whole scene there: every
    // window, its decorations, its menus, and the cursor.
    wl = null,
    // Kept for callers that used to route input to a different canvas from the
    // one wl_shm frames landed on. There is one output now, so this is only a
    // fallback for where to attach the listeners.
    wlInput = null,
    // The page's devicePixelRatio when `wl`'s backing store is in device
    // pixels: the compositor scales its frames by it and guest Wayland
    // clients are told the rounded value as wl_output.scale.
    wlScale = 1,
    // Run the X server in the page instead of in a guest process: a Worker
    // holding `Xwayland.wasm` and the compositor beside it (see
    // `web/src/hostx.js`). Guest X clients reach it through the global
    // kernel's unix socket, so from their side nothing changes -- but the
    // server's own drawing runs at wasm speed rather than translated AArch64.
    hostX = false,
    uid,
    // Every N safepoints, print the guest pc -- the only instrument for
    // locating what is slow in a page.
    heartbeat = globalThis.GEX_HEARTBEAT ?? 0,
    // Lazy files the page fills in the background (app.js `fill`), handed to
    // every Worker (lazy-fetch.mjs `setFill`).
    fill = null,
    // The page's cache of lazy files (app.js, lazy-cache.mjs): its control
    // buffer, handed to every Worker.
    lazyCache = null,
    // Units' keys and identities (`base:key:identity:path;...`), so a Worker
    // takes them instead of hashing each image it maps (cgen.rs `known`).
    identities = null,
    // Stateful providers (`dll=file,...`, files under the unit cache: cgen.rs
    // `set_stateful`), an ejected project's.
    stateful = null,
    // Provider sources from outside gex (`name\ntext`, NUL between).
    providerSources = null,
    // KiB of mapped text per object to statically recompile once hot; 0 leaves
    // the guest to arrive one dispatch miss at a time. A parameter because the
    // page and `run-node.mjs` used to disagree, so a host-specific fault could
    // not be attributed until it could be switched.
    //
    // Off now, which is what the shim has always documented as the default and
    // what `run-node.mjs` has always passed. The page said 8192, and on a run of
    // more than a handful of processes that was worse on every axis measured --
    // a 25-process `$(echo)` loop, per process:
    //
    //           dispatch slots   runtime heap   memory   lifetime
    //     on          107k          48 MiB      219 MiB    345ms
    //     off            0          16 MiB      187 MiB    298ms
    //
    // Zero slots, because with the image named (see `program` below) a process
    // reuses what its predecessors translated instead of claiming its own --
    // and claiming its own is what ran the shared slot space out and stopped
    // `apt-get update` dead at its 77th process. Bulk recompilation only ever
    // helped a run that is long *and* library-bound, and it is one query
    // parameter away for such a run: `?recompile=8192`.
    recompileObjects = typeof globalThis.GEX_RECOMPILE_OBJECTS === 'number'
      ? globalThis.GEX_RECOMPILE_OBJECTS
      : 0,
    // A closure built ahead of time by `gexc build --closure`: the image
    // module, every library beside it, and the layout they were placed at.
    closure = null,
    // Number this closure's dispatch slots at the build's numbers, not the
    // per-process demand allocator. The per-process path keeps tables small
    // across the dozens of forks an `apt-get update` makes, but its streaming
    // interaction corrupts dispatch for a profile-guided closure whose pages are
    // demand-filled while pid 1 is already running (the graphics-rootfs wayland
    // closures trap at libc+0x84710). A single-client demo makes at most a
    // couple of processes, so the build-numbered table growth is negligible and
    // the numbering is the complete, correct one. Off by default so the
    // many-process closure demos (debian) keep the small-table optimisation.
    closureBuildNumbered = false,
    // Keep the address space the guest's dynamic linker produced, so the next
    // process of the same program starts at its own entry with every
    // relocation already applied.
    link = Number(globalThis.GEX_LINK ?? 0),
    // Function-tier modules compiled ahead of the run (an ejected project's
    // page compiles its units while the bundle downloads), as [name, Module]
    // pairs every Worker takes instead of fetching and compiling its own.
    preModules = null,
    // Called once the system is up and before pid 1 starts, with what an
    // embedder needs to act on the running system from outside the guest:
    // the process host (kernel stack slices), the global kernel, the shared
    // terminal and the compositor. The demo desktop's launcher adds files
    // with it (web/src/overlay-worker.js) and types commands to pid 1.
    onSystem = null,
    // With `gl: 'compositor'`: start the host's Mesa only when `startGl`
    // (handed to `onSystem`) is called -- a desktop whose first windows
    // draw no GL does not download it.
    glLazy = false,
    // Where the page compositor's Mesa comes from (DomDesktop and the scene
    // compositor, whose worker gets the bytes): `(name)
    // => Promise<Uint8Array|null>` for 'gex-wgpu-replay.js', '.wasm' and
    // '.deferred.wasm', null for "fetch it as a file". app.js answers from
    // the site's packed files and the page's cache (mesa-gl.js `source`).
    glSource = null,
  },
) {
  // `tso=arm`: the TSO flavour if the host is arm64. What was chosen is
  // `__gexTso`, for a page or a recorder to read.
  globalThis.__gexTso = await detectTso();
  // A closure's code was translated ahead with plain accesses (`tso`).
  if (tso() && closure) {
    console.warn('[gex] tso: the closure is translated with plain accesses; translating instead');
    closure = null;
  }
  const { shimBytes, seedBytes } = await loadRuntime();
  const kernel = await loadKernel();
  const makeGuestWorker = await guestWorkerFactory();
  // The system's terminal, instantiated here so a keystroke reaches the line
  // discipline by a call rather than a message to a Worker. Null without a
  // shared memory, and `TerminalEnv` then does its own line editing.
  const sharedTty = tty ? await PageTty.open(kernel) : null;
  const tenv = new TerminalEnv(term, sharedTty);
  // The terminal, reachable from a console or a test (see `gexTerminalText`):
  // `gexTty.fg()` tells whether a lost `^C` was a wrong foreground group. Null
  // when there is no shared terminal.
  globalThis.gexTty = sharedTty;
  // Before pid 1 runs, so the first `TIOCGWINSZ` is the truth: a shell asks
  // once at startup and then only on `SIGWINCH`.
  sharedTty?.resize(term.rows, term.cols);

  // Which GPU rings this run wired up. A demo that passes `gl` but whose
  // worker never mounts /dev/gex-gl is otherwise indistinguishable from one
  // that never asked, and the guest's only symptom is an empty GL_VERSION
  // much later.
  console.warn(`[gex] runtime rings: gl=${!!gl} wl=${!!wl}`);
  // The GPU: a `MesaGlDevice` (web/src/mesa-gl.js) the guest drives through
  // libGLESv2/libEGL marshallers over a command ring. It needs a
  // SharedArrayBuffer bridge -- `gl_submit` is synchronous inside an
  // ioctl and the canvas is here, not in the Worker. Without this the `gl`
  // option was accepted and dropped, so `/dev/gex-gl` was never mounted (it is
  // gated on `have_gl()`) and gles-gears died at `eglInitialize`/`ring_open`.
  let glChannel = null;
  let glServe = null;
  let startGl = null;
  let glStarted = null;
  // `glLazy`, and the guest asks first: a GL program started from a shell,
  // not from the embedder's launcher, makes its first request into a ring
  // nothing serves yet -- and waits thirty seconds for the answer, then
  // dies. Its first request starts Mesa (gl-bridge.mjs STATE).
  const startGlOnFirstRequest = () => {
    if (!glChannel?.control || typeof Atomics.waitAsync !== 'function') return;
    const ctl = new Int32Array(glChannel.control);
    (async () => {
      while (!glStarted) {
        const st = Atomics.load(ctl, 0);
        if (st === 1) {
          console.warn('[gex] the guest opened its GL: starting Mesa');
          startGl();
          break;
        }
        const w = Atomics.waitAsync(ctl, 0, st, 5000);
        if (w.async) await w.value;
      }
    })();
  };
  if (gl && single.on) {
    // The single-thread build: no ring. Each process's `gl_submit` calls the
    // compositor's Mesa in this realm on its own memory (compositor-worker.js
    // `glInRealm`), started by the system Worker.
    glChannel = { inRealm: true };
  } else if (gl) {
    if (!globalThis.crossOriginIsolated) {
      throw new Error('the GL demo needs a cross-origin isolated page (SharedArrayBuffer bridge).');
    }
    const { makeGlChannel, serveGl } = await import('@gex/gl-bridge.mjs');
    glChannel = makeGlChannel();
    // `gl: 'compositor'` means the device is not here at all: the compositor's
    // worker hosts it, so its frames are textures in the context that draws
    // the scene. Nothing on this side serves the ring in that case -- see
    // `hostGl` below, once the compositor exists.
    if (gl !== 'compositor') {
      // An OffscreenCanvas renderer consumes the shared ring in its own Worker;
      // the returned function is then only the cheap wake-up fallback. A
      // main-thread renderer retains the original page-side service path.
      glServe = gl.attachChannel?.(glChannel) || serveGl(gl, glChannel);
    }
  }

  // The network: the page's Wisp connection, which every Worker calls across
  // shared memory. A WebSocket in the guest's own Worker delivers on that
  // Worker's event loop, which a wasm32 program waiting in a syscall never
  // gives back.
  let netChannel = null;
  if (network && globalThis.crossOriginIsolated) {
    const { makeNetChannel, serveNet } = await import('@gex/net-bridge.mjs');
    netChannel = makeNetChannel();
    if (netChannel) serveNet(network, netChannel);
  }

  // The Wayland compositor. The protocol state machine runs in each guest
  // process (the Rust shim, next to the client's memory, which is the only
  // place that can read a wl_shm pool); the scene, the output and the input
  // live in one worker that owns a WebGPU device. What crosses between them is
  // a shared arena of surface descriptors -- never a frame.
  let wlChannel = null;
  let compositor = null;
  if (wl?.inRealm) {
    // The single-thread build: the compositor is in this realm, set up by the
    // system Worker before this run (system-worker.js).
    ({ channel: wlChannel, compositor } = wl.inRealm);
  } else if (wl) {
    if (!globalThis.crossOriginIsolated) {
      throw new Error('the Wayland demo needs a cross-origin isolated page (SharedArrayBuffer bridge).');
    }
    const { makeWlChannel, setOutputSize } = await import('@gex/wl-bridge.mjs');
    wlChannel = makeWlChannel();
    // An element that is not a canvas asks for the other compositor: windows
    // as DOM elements, decorations in the page's own HTML. Same arena, same
    // input ring, same protocol -- only the drawing is different.
    const element = wl && typeof wl.getContext !== 'function' && wl.nodeType === 1 ? wl : null;
    if (element) {
      const { DomDesktop } = await import('./dom-compositor.js');
      compositor = DomDesktop.create(element, wlChannel, { width: element.clientWidth, height: element.clientHeight });
      globalThis.__gexDesktop = compositor;
      // The page lays the windows out (bare-desktop.js, say) before any appears.
      onDesktop?.(compositor);
      // A GL device that dies mid-session leaves black windows and no other
      // sign; say it where the session's other trouble is reported.
      compositor.onglfailed = (why) => {
        term.writeln(`gex: the guest's OpenGL is gone: ${why}`);
        term.writeln('gex: GL windows will be blank; everything else still runs');
      };
      // The GL goes onto the window's own canvas here rather than into a
      // scene: see `DomDesktop.hostGl`.
      if (gl === 'compositor' && glChannel) {
        const opts = { api: 'core', backend: glBackend, source: glSource };
        startGl = () => (glStarted ??= startHostGl(compositor, glChannel, opts, term));
        if (!glLazy) await startGl();
        else startGlOnFirstRequest();
      }
    }
    const canvas = typeof wl?.getContext === 'function' ? wl : null;
    // A build without the scene compositor (gex-eject's `"compositor": false`)
    // has no compositor.js: the page passes an element instead (above).
    if (canvas && !GEX_COMPOSITOR) throw new Error('this build has no compositor: give `wl` an element (bare-desktop.js)');
    if (canvas && GEX_COMPOSITOR) {
      setOutputSize(wlChannel, canvas.width, canvas.height, Math.max(1, Math.round(wlScale)));
      const { Compositor } = await import('./compositor.js');
      compositor = await Compositor.create(canvas, wlChannel, wlInput || canvas, { scale: wlScale });
      globalThis.__gexCompositor = compositor;
      if (gl === 'compositor' && glChannel) {
        const opts = {
          api: 'core',
          backend: glBackend,
          canvasBlit: pageParams().get('glcanvas') === '1',
          source: glSource,
        };
        // `glLazy`: Mesa (about 20 MB before compression) is fetched when
        // the embedder first asks (`onSystem`'s `startGl`), not at boot.
        startGl = () => (glStarted ??= startHostGl(compositor, glChannel, opts, term));
        if (!glLazy) await startGl();
        else startGlOnFirstRequest();
      }
    }
  }

  // The X server, in the page. Started before the guest so the display is
  // already listening when the first X client connects: the socket lives in the
  // global kernel, so "listening" is a name in its table rather than a file.
  let xServer = null;
  if (hostX && wlChannel) {
    if (!kernel) throw new Error('the host X server needs the global kernel');
    // The single-thread build (docs/single-thread.md): the server is a fiber of
    // this realm, the non-pthread Xwayland (web/public/gex/xwayland-st/) whose
    // waits suspend. The same messages, without a Worker between.
    const xDir = assetUrl(single.on ? '/gex/xwayland-st/' : '/gex/xwayland/');
    if (single.on) {
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
      } else if (m.kind === 'wl-memory') compositor?.addMemory(m.token, m.memory, m.kernel);
      else if (m.kind === 'ready') listening(true);
      else if (m.kind === 'failed') {
        term.writeln(`gex: the X server failed: ${m.why}`);
        listening(false);
      }
    };
    xServer.postMessage({
      kind: 'start',
      xwaylandUrl: new URL(`${xDir}Xwayland.mjs`, location.href).href,
      hostwlUrl: new URL(`${xDir}gexhostwl.wasm`, location.href).href,
      keymapUrl: new URL(`${xDir}gex.xkm`, location.href).href,
      // Its memory is a wasm32 one, also under `?mem64=1`: the kernel module
      // serves both (gex-kmem, lazy-fetch.mjs `processMemories`).
      kernel: { module: kernel.module, memory: kernel.memory, spare: kernel.spare },
      wlChannel,
      // A name in the scene arena that no guest pid can collide with: pids
      // count up from 1, and this is 'xwl1' as ASCII.
      token: 0x78776c31,
      display: ':0',
      // `hostX: 'rootful'`: one fullscreen X screen with no window manager of
      // the compositor's, for a session that runs its own (KWin, ...).
      rootful: hostX === 'rootful' || undefined,
      // `?xverbose=N`: the X server's own log, which is the only thing that
      // says what it told a client.
      verbose: globalThis.GEX_HOSTX_VERBOSE || Number(pageParams().get('xverbose')) || undefined,
      trace: globalThis.GEX_HOSTX_TRACE || undefined,
      wlTrace: flag('wltrace') ? 1 : undefined,
      count: flag('hostxcount') ? 1 : undefined,
    });
    // Two modules and a kernel to instantiate; a slow machine takes a few
    // seconds. A run that goes ahead without the display is worse than one that
    // says so.
    const ok = await Promise.race([
      up,
      new Promise((r) => setTimeout(() => r('timeout'), 60000)),
    ]);
    if (ok === 'timeout') term.writeln('gex: the X server did not come up in 60s');
  }

  const host = new ProcessHost({
    makeWorker: makeGuestWorker,
    shim: shimBytes,
    // The closure's image module in place of the seed: it holds the program's
    // own text, already lifted.
    guest: closure?.image ?? seedBytes,
    kernel,
    // `?mem64=1`: the 32-bit memory's runtime, fetched when a process first
    // needs it (host.mjs `#other`: W64's wineserver, a wasm32 module).
    // Otherwise, with the global kernel, the 64-bit memory's runtime, for a
    // program an exec asks one for (docs/mem64.md §5: its GEX_MEM64=1, or its
    // image's user.gex.mem64 record); the page itself stays 32-bit. Fetched
    // only then, so a page that never asks loads nothing more.
    other: m64Infix() ? async () => ({
      shim: await fetchBytes(assetUrl(`/gex/gex_browser${archInfix()}.shared.wasm`)),
      guest: await fetchBytes(assetUrl(`/gex/seed${archInfix()}.shared.wasm`)),
      // the one kernel module serves both flavours (gex-kmem)
      kernel: kernel.module,
    }) : kernel && guestArch() === 'x86' ? async () => ({
      shim: await fetchBytes(assetUrl(`/gex/gex_browser${archInfix()}.m64.shared.wasm`)),
      guest: await fetchBytes(assetUrl(`/gex/seed${archInfix()}.m64.shared.wasm`)),
      kernel: kernel.module,
    }) : null,
    extra: {
      // In `extra` so every Worker gets it, not only pid 1: the interesting
      // process is the X server and the client it forks, not the demo's shell.
      heartbeat: heartbeat || undefined,
      fill: fill || undefined,
      lazyCache: lazyCache || undefined,
      identities: identities || undefined,
      // How many traps a forked child takes before it stops waiting to be an
      // `execve` and installs its parent's modules.
      adoptAfter: typeof globalThis.GEX_ADOPT_AFTER === "number"
        ? globalThis.GEX_ADOPT_AFTER
        : undefined,
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
      // (shims/browser/src/vfork.rs), on the console.
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
      // (net-bridge.mjs); `wispUrl` only where the page cannot serve it.
      netChannel: netChannel ?? undefined,
      // The single-thread build's fibers share this realm, and so the one
      // Wisp connection itself.
      wispNet: single.on ? network ?? undefined : undefined,
      wispUrl: netChannel || single.on ? undefined : network?.url ?? undefined,
      // Only when there is no shared terminal to hold the size. With one this
      // must not be sent: it is captured at `runProgram` time and every Worker
      // writes it on startup, so a process forked after a resize would reset the
      // terminal to its boot size.
      winsize: sharedTty ? undefined : { rows: term.rows, cols: term.cols },
      // Every guest write mirrored to the console and a crash narrated where it
      // happens, since a Worker that falls over takes its last messages with it.
      debugExit:
        !!globalThis.GEX_DEBUG_EXIT ||
        (typeof process !== 'undefined' && !!process.env?.GEX_DEBUG_EXIT),
      // Label process/thread Worker exits without `debugExit`'s expensive
      // mirroring of every guest write.
      exitTrace: !!globalThis.GEX_EXIT_TRACE,
      // The engine's wasm-frame stack for a trap, in every process. In `extra`
      // because a trap that matters happens in a Worker, and a global set in
      // the page is not visible there.
      trapStack: globalThis.GEX_TRAP_STACK ? 1 : undefined,
      // `?doorstats=1`: wineserver door call counts every 10 s (docs/wineserver-kernel.md).
      // (a number past 1: the line's period in ms)
      doorStats: globalThis.GEX_DOOR_STATS || undefined,
      fcacheStats: globalThis.GEX_FCACHE_STATS || undefined,
      // `?timescale=N&fixedtime=S` (benchmarks only): the monotonic clocks
      // N times real speed, the wall clock starting at Unix time S, as
      // pewasm's PEWASM_TIME_SCALE and PEWASM_FIXED_TIME.
      timeScale: globalThis.GEX_TIME_SCALE || undefined,
      fixedTime: globalThis.GEX_FIXED_TIME || undefined,
      calloutTrace: globalThis.GEX_CALLOUT_TRACE || undefined,
      // `?straytrap=1`: native Wine's pad past open call-outs stays a breakpoint (cgen.rs STRAY_TRAP).
      strayTrap: globalThis.GEX_STRAY_TRAP || undefined,
      // `?w3split=1`: W3's address space split at 2 GiB (nt::w3_space), in every process.
      w3Split: globalThis.GEX_W3_SPLIT || undefined,
      // `?nullguard=1`: the null guard, a debugging opt-in (gex_abi::null_guard).
      nullGuard: globalThis.GEX_NULL_GUARD || undefined,
      // `?mxcsr=1`: SSE under MXCSR, a per-program opt-in (gex_ir::mxcsr_exact).
      mxcsr: globalThis.GEX_MXCSR || undefined,
      // `?pinregion=BYTES`: an ejected project's libraries' region (gex_abi::set_pin_region).
      pinRegion: globalThis.GEX_PIN_REGION || undefined,
      // `?sehreport=1`: crash reports name the SEH chain (a debugging opt-in).
      sehReport: globalThis.GEX_SEH_REPORT || undefined,
      // `?dense=0`: classic per-slot dispatch tables (gex_emit::dense off).
      dense: globalThis.GEX_DENSE === 0 ? false : globalThis.GEX_DENSE === 2 ? 2 : undefined,
      // `?cgen=1|2`: function-tier units (1 where one exists, 2 required for
      // a program image), read from the server's `/gex-cgen/` (a unit
      // cache, GEX_CGEN_CACHE's layout) with synchronous requests.
      // `?cgenurl=` (an ejected project's build/web/cgen) instead.
      // Otherwise the export providers the dist carries (gex/cgen/providers/,
      // docs/gexcc.md §7.9), with no units; `?providers=0` runs glibc's
      // own code translated instead.
      // Absolute: a Worker started from a blob URL has no base to resolve a path against.
      cgen: globalThis.GEX_CGEN && !tso() ? { mode: globalThis.GEX_CGEN, url: absUrl(globalThis.GEX_CGEN_URL ?? '/gex-cgen'), zstd: globalThis.GEX_CGEN_ZSTD && absUrl(globalThis.GEX_CGEN_ZSTD) }
        : flag('providers') === '0' ? undefined
        : { mode: 8, url: assetUrl('/gex/cgen') },
      // `?aot=1`: an ejected project's run, with no run-time translation (a
      // miss stops the run, saying where).
      jit: globalThis.GEX_NO_JIT ? false : undefined,
      aot: globalThis.GEX_AOT ? true : undefined,
      denseDebug: !!globalThis.GEX_DENSE_DEBUG,
      // `?mem64=1`: every Worker of the process builds or joins a 64-bit memory.
      mem64: globalThis.GEX_MEM64 ? 1 : undefined,
      // `?fjit=1`: the function JIT in every process.
      fjit: globalThis.GEX_FJIT || undefined,
      // `?xcache=1`: the translation cache across page loads (docs/gexcc.md §8).
      xcache: xcacheName(),
      // `?liftmax=N`: blocks one dispatch miss may lift (default 2048).
      liftMax: globalThis.GEX_LIFT_MAX || undefined,
      // `?inlinedispatch=1`: dispatch inline in every block, as before.
      inlineDispatch: globalThis.GEX_INLINE_DISPATCH || undefined,
      // `?xlatereport=N`: a translator progress line every N translations.
      xlateReport: globalThis.GEX_XLATE_REPORT || undefined,
      // `?t1fold=1`: tier-1's tee folding (gex-emit `set_t1_fold`).
      t1Fold: globalThis.GEX_T1_FOLD || undefined,
      // `?unitcache=1`: compiled translation units shared by every process.
      unitCache: globalThis.GEX_UNIT_CACHE || undefined,
      // `?wasmdlls=soname=path,...`: libraries replaced by wasm64 providers (cgen/libprov.rs).
      wasmDlls: globalThis.GEX_WASM_DLLS || undefined,
      // `?elfunits=name,...`: shared objects that get function-tier units (cgen/libprov.rs).
      elfUnits: globalThis.GEX_ELF_UNITS || undefined,
      // `?provskip=export,...`: exports no provider binds (gex-cgen `set_skip`).
      providerSkip: globalThis.GEX_PROVIDER_SKIP || undefined,
      // Provider sources from outside gex (`name\ntext` entries, NUL
      // between): an ejected project's, as GEX_PROVIDER_DIRS natively.
      providerSources: providerSources || globalThis.GEX_PROVIDER_SOURCES || undefined,
      // `dll=file,...`: stateful providers replacing Windows libraries, their
      // modules under the unit cache (cgen.rs `set_stateful`).
      stateful: stateful || globalThis.GEX_STATEFUL || undefined,
      // `?fjitlibs=+name,-name` (fjit.rs `gex_fjit_libs`).
      fjitLibs: globalThis.GEX_FJIT_LIBS || undefined,
      traceMisses: !!globalThis.GEX_TRACE_MISSES,
      // `?jitdiag=1`: retire counts by page and call, and the watch on a page that keeps translating.
      jitDiag: flag('jitdiag') ? 1 : undefined,
      // `?sharetrace=1`: say how compiled modules are shared between a process's Workers.
      shareTrace: flag('sharetrace') ? 1 : undefined,
      // `?log=1` (or `?log=record,...`): gex's debug output, off by default
      // (shims/browser/js/log.mjs); every Worker is handed it.
      log: (globalThis.GEX_LOG ??= flag('log') || undefined) || undefined,
      preModules: preModules ?? undefined,
      // `?forkskip=N`, in `extra` because the process that acts on it is a
      // forked child, not the page's own.
      forkSkip: typeof globalThis.GEX_FORK_SKIP_MIB === 'number'
        ? globalThis.GEX_FORK_SKIP_MIB
        : undefined,
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
      // `?eagerblocks=1` (debugging): every Worker instantiates every dense
      // module of its process as it catches up, instead of each one when a
      // dispatch first wants a block of it. In `extra` for every Worker.
      eagerBlocks: flag('eagerblocks') ? 1 : undefined,
      // `?filltime=1` (debugging): every Worker says when a dense module a
      // dispatch waited for took longer than any before it. In `extra` for
      // every Worker.
      fillTime: flag('filltime') ? 1 : undefined,
      // `?relayall=1` (debugging): every module a Worker compiles is handed to
      // its siblings compiled, small ones too. In `extra` for every Worker.
      relayAll: flag('relayall') ? 1 : undefined,
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
      // Whether a fork hands the child its parent's compiled modules. In `extra`
      // (spread into every Worker message), not on `globalThis`: the list is
      // built in the forking Worker, so a global set here is set in the page.
      // See task 145.
      forkMods:
        globalThis.GEX_FORK_MODULES === undefined
          ? undefined
          : Number(globalThis.GEX_FORK_MODULES),
      // In `extra` so every forked command gets it, not just the shell that
      // starts the demo (which never sees `opts`).
      link: link || undefined,
      // Who the guest is, in `extra` so a forked child gets it.
      uid,
      gid: uid,
    },
    onOutput: (fd, text, pid, m) => {
      if (m?.lflag !== undefined) tenv.lflag = m.lflag;
      term.write(text);
    },
    // A process has exited (a thread's end is keyed "pid:tid" and is not
    // one): the compositor lets go of its memory, and the host's Mesa of its
    // GL context and windows. Nothing said so before, so every Wayland or GL
    // client's whole memory stayed held by the compositor for the life of
    // the page, and a GL program's windows stayed in Mesa's list: a second
    // glxgears, given the first one's X window id, matched both.
    onExit: (m, pid) => {
      if (typeof pid === 'number') compositor?.dropMemory?.(pid);
    },
    readStdin: (lflag) => {
      if (lflag !== undefined) tenv.lflag = lflag;
      return tenv.readStdin();
    },
    onMessage: (m) => {
      if (m.kind === 'gl') glServe?.();
      // A process that has a compositor block hands the compositor its memory:
      // that is where the pixels of every one of its surfaces are.
      else if (m.kind === 'wl-memory') compositor?.addMemory(m.token, m.memory, m.kernel);
      else if (m.kind === 'wl') {
        /* a publish happened; the compositor draws on its own clock */
      }
      else if (m.kind === 'net') onNetEvent?.(m.event);
      // Pid 1 has copied `files` into the kernel's filesystem.
      else if (m.kind === 'files-loaded') onFilesLoaded?.();
      // Where `/dev/dsp`'s output is: played from the first gesture on.
      else if (m.kind === 'audio') startAudio({ ...m, clock: flag('audioclock') !== '0', latency: audioLatency(), loop: flag('audioloop') === '1' });
    },
  });

  // Keystrokes go to the environment, not straight to the guest: whether they
  // are echoed and line-buffered is the guest's choice, read from its termios.
  const keys = term.onData((d) => tenv.onKey(d));
  // xterm hears the keyboard only through a hidden textarea that must hold
  // focus; the Run button disables itself and drops focus to the body, so
  // without this a program would ignore every key until the terminal was
  // clicked.
  term.focus?.();
  // A resize is `SIGWINCH` to the foreground group and one write to the shared
  // terminal, not a message to each process.
  const resize = term.onResize(({ rows, cols }) => {
    // A promise from the terminal's Worker; nothing to wait for.
    if (sharedTty) Promise.resolve(sharedTty.resize(rows, cols)).catch(() => {});
    else host.resize(rows, cols);
  });
  onSystem?.({ host, kernel, tty: sharedTty, compositor, startGl });
  try {
    const status = await host.start({
      elf,
      files,
      transfer,
      dirs,
      modes,
      env,
      args,
      // The layout the closure was built against. Without it the placer hands
      // out an address inside a pinned object and the image lands somewhere
      // its own translated code does not expect.
      opts: {
        cwd, tty, uid, recompileObjects, heartbeat, link, dirs, modes, hard, symlinks, lazy, mtimes, xattrs, env, pins: pinsFrom(closure?.pins, globalThis.GEX_PINS),
        // `?strace=1`, and every child inherits it: see `#opts` in gex.mjs.
        strace: globalThis.GEX_STRACE ? 1 : undefined,
        // `?stracepid=N`: narrate that one process, which is affordable.
        stracePid: globalThis.GEX_STRACE_PID || undefined,
        // `?straceexe=NAME`: the process that execs NAME, whatever its pid.
        straceExe: globalThis.GEX_STRACE_EXE || undefined,
        // Which program pid 1 is, so its own text gets a code identity and a
        // second process can reuse what the first translated of it.
        //
        // This came only from a closure manifest, so a run without a closure --
        // every demo here -- left the main image unnamed, and an unnamed page
        // can never be looked up in the shared slot map. Every process
        // therefore re-lifted the whole image: 14,807 blocks each, ~160ms each,
        // and a fresh module stored for every one of them. The store grew
        // without bound, so each new process was served an ever-larger batch of
        // its predecessors' near-duplicates -- which is where the tab's memory
        // went, and why a `$(…)` loop ran out of dispatch slots at all.
        //
        // `args[0]` is that program's path, which is exactly what `do_exec`
        // already uses to name an image it loads itself.
        program: closure?.program ?? args?.[0],
        // `?permiss=N`: how many stored units one dispatch miss may take.
        perMiss: typeof globalThis.GEX_PER_MISS === 'number' ? globalThis.GEX_PER_MISS : undefined,
      },
    });
    return { status, host, tenv };
  } finally {
    keys.dispose();
    resize.dispose();
    host.stop();
    // The compositor outlives no run: its worker holds a GL context and every
    // process's memory, and a page that starts a second demo must not keep
    // either.
    compositor?.stop();
    // The X server owns its Worker's thread for good -- `main` never returns --
    // so ending the run means ending the Worker.
    xServer?.terminate();
    glServe?.stop?.();
  }
}

export { TerminalEnv };
export { loadKernel as globalKernel };

/** A closure's pins plus the page's `?pins=path@base+span,...` (hex; the query wins). */
function pinsFrom(closurePins, spec) {
  if (!spec) return closurePins;
  const out = { ...(closurePins ?? {}) };
  for (const w of spec.split(',').map((x) => x.trim()).filter(Boolean)) {
    const m = /^(.+)@(?:0x)?([0-9a-f]+)(?:\+(?:0x)?([0-9a-f]+))?$/i.exec(w);
    if (!m) { console.error(`[gex] ?pins: cannot read ${w}`); continue; }
    out[m[1]] = { base: Number.parseInt(m[2], 16), span: m[3] ? Number.parseInt(m[3], 16) : 0 };
  }
  return out;
}
