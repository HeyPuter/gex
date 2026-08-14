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
const SHIM_URL = '/gex/gex_browser.shared.wasm';
const SEED_URL = '/gex/seed.shared.wasm';
const KERNEL_URL = '/gex/gex_kernel.wasm';

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
  if (!shimBytes) shimBytes = await fetchBytes(SHIM_URL, onProgress);
  if (!seedBytes) seedBytes = await fetchBytes(SEED_URL);
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
    const bytes = await fetchBytes(KERNEL_URL);
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
    /// `KEY=VALUE` strings for pid 1. Empty takes the shim's default.
    env = [],
    args,
    cwd = '/',
    tty = true,
    network = null,
    onNetEvent = null,
    gl = null,
    virgl = null,
    uid,
    // Every N safepoints, print the guest pc -- the only instrument for
    // locating what is slow in a page.
    heartbeat = globalThis.GEX_HEARTBEAT ?? 0,
    // KiB of mapped text per object to statically recompile once hot; 0 leaves
    // the guest to arrive one dispatch miss at a time. A parameter because the
    // page (on) and `run-node.mjs` (off) disagree, so a host-specific fault
    // cannot be attributed until it can be switched.
    recompileObjects = typeof globalThis.GEX_RECOMPILE_OBJECTS === 'number'
      ? globalThis.GEX_RECOMPILE_OBJECTS
      : 8192,
    // A closure built ahead of time by `gexc build --closure`: the image
    // module, every library beside it, and the layout they were placed at.
    closure = null,
    // Keep the address space the guest's dynamic linker produced, so the next
    // process of the same program starts at its own entry with every
    // relocation already applied.
    link = Number(globalThis.GEX_LINK ?? 0),
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

  // The GPU, if this demo has one. The renderer stays here, with the canvas;
  // the guest reaches it across a `SharedArrayBuffer` and blocks on
  // `Atomics.wait`, because `virgl_submit` is called from inside a syscall
  // and there is nothing to await with. See `virgl-bridge.js`.
  let channel = null;
  let serve = null;
  if (virgl) {
    if (!globalThis.crossOriginIsolated) {
      throw new Error(
        'the graphics demo needs a cross-origin isolated page: the renderer is ' +
          'here and the guest is in a Worker, and what carries the command ' +
          'stream between them is a SharedArrayBuffer.',
      );
    }
    const { makeChannel, serveVirgl } = await import('@gex/virgl-bridge.mjs');
    channel = makeChannel();
    serve = serveVirgl(virgl, channel);
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
      // Every process gets a memory two Workers can hold, so a `clone` asking
      // for a thread can be given one. `worker.mjs` passes this to the `Gex`
      // constructor as `shared`. A guest's threads are Workers -- the only
      // shape the shim has now.
      shared: true,
      virgl: channel ?? undefined,
      wispUrl: network?.url ?? undefined,
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
      // KiB of mapped text per object to statically recompile once hot. Eight
      // megabytes is the largest that works today (above it the graphics rootfs
      // meets #73) and covers every library these demos run except Mesa's
      // software rasteriser, which nothing here wants.
      recompileObjects,
      // `?nointrinsics=1` translates glibc's own memcpy/memcmp/memset instead
      // of substituting the shim's, to tell a substitution fault from a codegen
      // one. In `extra` so every process gets it.
      noIntrinsics: flag('nointrinsics') ? 1 : undefined,
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
      if (m.kind === 'virgl') serve?.();
      else if (m.kind === 'net') onNetEvent?.(m.event);
    },
  });

  // Keystrokes go to the environment, not straight to the guest: whether they
  // are echoed and line-buffered is the guest's choice, read from its termios.
  const keys = term.onData((d) => tenv.onKey(d));
  // xterm hears the keyboard only through a hidden textarea that must hold
  // focus; the Boot button disables itself and drops focus to the body, so
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
        cwd, tty, uid, recompileObjects, heartbeat, link, dirs, modes, env, pins: closure?.pins,
        program: closure?.program,
        // `?permiss=N`: how many stored units one dispatch miss may take.
        perMiss: typeof globalThis.GEX_PER_MISS === 'number' ? globalThis.GEX_PER_MISS : undefined,
      },
    });
    return { status, host, tenv };
  } finally {
    keys.dispose();
    resize.dispose();
    host.stop();
  }
}

export { TerminalEnv };
