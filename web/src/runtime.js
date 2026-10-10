// A gex runtime bound to an xterm.js terminal.
//
// The seam is `Env`: gex hands the page every write and asks it for input. What
// this adds is a line discipline. Whether the page or the guest does the line
// editing is the guest's choice (ICANON in its termios), and the page has to
// follow it or every keystroke appears twice.

import { Env } from '../../shims/browser/js/gex.mjs';
import { ProcessHost } from '../../shims/browser/js/host.mjs';
import { PageTty } from '../../shims/browser/js/tty.mjs';

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
const SHIM_URL = () => `/gex/gex_browser${archInfix()}${m64Infix()}.shared.wasm`;
const SEED_URL = () => `/gex/seed${archInfix()}${m64Infix()}.shared.wasm`;
const KERNEL_URL = () => `/gex/gex_kernel${archInfix()}${m64Infix()}.wasm`;

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

/** The two wasm modules every demo needs, fetched once for the page's life. */
export async function loadRuntime(onProgress) {
  if (!shimBytes) shimBytes = await fetchBytes(SHIM_URL(), onProgress);
  if (!seedBytes) seedBytes = await fetchBytes(SEED_URL());
  return { shimBytes, seedBytes };
}

export { fetchBytes };

/**
 * A `?name=` switch, from wherever this is running. `location` does not exist
 * under Node (where the terminal test drives this file), so the same switches
 * arrive there as `GEX_*` environment variables.
 */
function flag(name) {
  const search = globalThis.location?.search;
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
      // shows, and nothing when the program has taken echo over.
      if (this.tty.input(data)) {
        const echo = this.tty.drainEcho();
        if (echo) this.term.write(echo);
      }
      // A guest already parked waiting for input is woken inside the kernel;
      // one that gave the stack back to JavaScript is waiting on this.
      const ready = this.tty.ready();
      if (ready !== 0 && this.waiter) {
        const w = this.waiter;
        this.waiter = null;
        // `true` rather than the bytes: they are already in the guest's read
        // queue, and handing them over here would deliver them twice.
        w(ready < 0 ? false : true);
      }
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
      const ready = this.tty.ready();
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
 * The guest is told nothing: its `/dev/gex-gl` still exists and its ring is
 * still there, so a GL client blocks on a device that never answers rather
 * than failing at `eglInitialize`. That is the same shape as a machine whose
 * GPU has fallen over, and better than pretending the session is broken.
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
    return null;
  }
}

async function loadKernel() {
  if (kernelSpec !== null) return kernelSpec || null;
  // Test for `SharedArrayBuffer` directly, not `crossOriginIsolated`: it is
  // what is actually needed, and asking for the constructor works under Node
  // too (which lets the terminal test drive this same file).
  if (typeof SharedArrayBuffer !== 'function') {
    kernelSpec = false;
    return null;
  }
  try {
    const bytes = await fetchBytes(KERNEL_URL());
    kernelSpec = {
      // The maximum must match `--max-memory` in `shims/kernel/build.sh`
      // exactly: a shared memory carries its maximum in its type, so a host
      // offering a different one cannot instantiate the module. It reserves
      // address space, not pages -- the rootfs lives here and grows as files
      // arrive.
      module: await WebAssembly.compile(bytes),
      memory: new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true }),
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
  if (typeof Worker === 'function') {
    return () =>
      new Worker(new URL('../../shims/browser/js/worker.mjs', import.meta.url), {
        type: 'module',
      });
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
export async function runProgram(
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
    /// `KEY=VALUE` strings for pid 1. Empty takes the shim's default.
    env = [],
    args,
    cwd = '/',
    tty = true,
    network = null,
    onNetEvent = null,
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
  },
) {
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
  if (gl) {
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
  if (wl) {
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
      compositor = DomDesktop.create(element, wlChannel);
      globalThis.__gexDesktop = compositor;
      // A GL device that dies mid-session leaves black windows and no other
      // sign; say it where the session's other trouble is reported.
      compositor.onglfailed = (why) => {
        term.writeln(`gex: the guest's OpenGL is gone: ${why}`);
        term.writeln('gex: GL windows will be blank; everything else still runs');
      };
      // The GL goes onto the window's own canvas here rather than into a
      // scene: see `DomDesktop.hostGl`.
      if (gl === 'compositor' && glChannel) {
        await startHostGl(compositor, glChannel, { api: 'core', backend: glBackend }, term);
      }
    }
    const canvas = typeof wl?.getContext === 'function' ? wl : null;
    if (canvas) {
      setOutputSize(wlChannel, canvas.width, canvas.height);
      const { Compositor } = await import('./compositor.js');
      compositor = await Compositor.create(canvas, wlChannel, wlInput || canvas);
      globalThis.__gexCompositor = compositor;
      if (gl === 'compositor' && glChannel) {
        await startHostGl(compositor, glChannel, {
          api: 'core',
          backend: glBackend,
          canvasBlit: new URLSearchParams(location.search).get('glcanvas') === '1',
        }, term);
      }
    }
  }

  // The X server, in the page. Started before the guest so the display is
  // already listening when the first X client connects: the socket lives in the
  // global kernel, so "listening" is a name in its table rather than a file.
  let xServer = null;
  if (hostX && wlChannel) {
    if (!kernel) throw new Error('the host X server needs the global kernel');
    xServer = new Worker(new URL('./xwayland-worker.js', import.meta.url), { type: 'module' });
    // Waited for below: a guest that starts before the display is listening
    // gets "Can't open display" and dies, and the display here is a name in the
    // global kernel that only exists once the server has bound it.
    let listening = null;
    const up = new Promise((resolve) => (listening = resolve));
    xServer.onmessage = (e) => {
      const m = e.data;
      if (m.kind === 'log') term.writeln(m.text);
      else if (m.kind === 'wl-memory') compositor?.addMemory(m.token, m.memory);
      else if (m.kind === 'ready') listening(true);
      else if (m.kind === 'failed') {
        term.writeln(`gex: the X server failed: ${m.why}`);
        listening(false);
      }
    };
    xServer.postMessage({
      kind: 'start',
      xwaylandUrl: new URL('/gex/xwayland/Xwayland.mjs', location.href).href,
      hostwlUrl: new URL('/gex/xwayland/gexhostwl.wasm', location.href).href,
      keymapUrl: new URL('/gex/xwayland/gex.xkm', location.href).href,
      // The X server's own memory is a wasm32 one, so with `?mem64=1` it takes the
      // wasm32 build of the kernel (same kernel memory, other process-memory type).
      kernel: { module: m64Infix() ? await WebAssembly.compile(await fetchBytes(`/gex/gex_kernel${archInfix()}.wasm`)) : kernel.module, memory: kernel.memory },
      wlChannel,
      // A name in the scene arena that no guest pid can collide with: pids
      // count up from 1, and this is 'xwl1' as ASCII.
      token: 0x78776c31,
      display: ':0',
      // `?xverbose=N`: the X server's own log, which is the only thing that
      // says what it told a client.
      verbose: globalThis.GEX_HOSTX_VERBOSE || Number(new URLSearchParams(location.search).get('xverbose')) || undefined,
      trace: globalThis.GEX_HOSTX_TRACE || undefined,
      wlTrace: flag('wltrace') ? 1 : undefined,
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
    extra: {
      // In `extra` so every Worker gets it, not only pid 1: the interesting
      // process is the X server and the client it forks, not the demo's shell.
      heartbeat: heartbeat || undefined,
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
      wispUrl: netChannel ? undefined : network?.url ?? undefined,
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
      // `?dense=0`: classic per-slot dispatch tables (gex_emit::dense off).
      dense: globalThis.GEX_DENSE === 0 ? false : globalThis.GEX_DENSE === 2 ? 2 : undefined,
      // `?cgen=1|2`: function-tier units (1 where one exists, 2 required for
      // a program image), read from the server's `/gex-cgen/` (a unit
      // cache, GEX_CGEN_CACHE's layout) with synchronous requests.
      // `?cgenurl=` (an ejected project's build/web/cgen) instead.
      // Otherwise the export providers the dist carries (gex/cgen/providers/,
      // docs/gexcc.md §7.9), with no units; `?providers=0` runs glibc's
      // own code translated instead.
      cgen: globalThis.GEX_CGEN ? { mode: globalThis.GEX_CGEN, url: globalThis.GEX_CGEN_URL ?? '/gex-cgen' }
        : flag('providers') === '0' ? undefined
        : { mode: 8, url: '/gex/cgen' },
      // `?aot=1`: an ejected project's run, with no run-time translation (a
      // miss stops the run, saying where).
      jit: globalThis.GEX_NO_JIT ? false : undefined,
      aot: globalThis.GEX_AOT ? true : undefined,
      denseDebug: !!globalThis.GEX_DENSE_DEBUG,
      // `?mem64=1`: every Worker of the process builds or joins a 64-bit memory.
      mem64: globalThis.GEX_MEM64 ? 1 : undefined,
      // `?fjit=1`: the function JIT in every process.
      fjit: globalThis.GEX_FJIT || undefined,
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
      // `?fjitlibs=+name,-name` (fjit.rs `gex_fjit_libs`).
      fjitLibs: globalThis.GEX_FJIT_LIBS || undefined,
      traceMisses: !!globalThis.GEX_TRACE_MISSES,
      // `?sharetrace=1`: say how compiled modules are shared between a process's Workers.
      shareTrace: flag('sharetrace') ? 1 : undefined,
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
      // `?lazythread=1`: a spawned thread's Worker instantiates the process's
      // modules when a branch reaches them, not all up front -- a memory saver
      // for a threaded program with many modules (x86-64's denser slots make
      // each Worker's tables and modules dearer). In `extra` for every Worker.
      lazyThread: flag('lazythread') ? 1 : undefined,
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
      coherentLoads: flag('coherentloads') ? 1 : undefined,
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
    readStdin: (lflag) => {
      if (lflag !== undefined) tenv.lflag = lflag;
      return tenv.readStdin();
    },
    onMessage: (m) => {
      if (m.kind === 'gl') glServe?.();
      // A process that has a compositor block hands the compositor its memory:
      // that is where the pixels of every one of its surfaces are.
      else if (m.kind === 'wl-memory') compositor?.addMemory(m.token, m.memory);
      else if (m.kind === 'wl') {
        /* a publish happened; the compositor draws on its own clock */
      }
      else if (m.kind === 'net') onNetEvent?.(m.event);
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
    if (sharedTty) sharedTty.resize(rows, cols);
    else host.resize(rows, cols);
  });
  try {
    const status = await host.start({
      elf,
      files,
      dirs,
      modes,
      env,
      args,
      // The layout the closure was built against. Without it the placer hands
      // out an address inside a pinned object and the image lands somewhere
      // its own translated code does not expect.
      opts: {
        cwd, tty, uid, recompileObjects, heartbeat, link, dirs, modes, hard, symlinks, lazy, env, pins: pinsFrom(closure?.pins, globalThis.GEX_PINS),
        // `?strace=1`, and every child inherits it: see `#opts` in gex.mjs.
        strace: globalThis.GEX_STRACE ? 1 : undefined,
        // `?stracepid=N`: narrate that one process, which is affordable.
        stracePid: globalThis.GEX_STRACE_PID || undefined,
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
