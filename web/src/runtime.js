// A gex runtime bound to an xterm.js terminal.
//
// The seam is `Env`: gex hands the page every write and asks it for input.
// What this adds on top is the part a terminal needs and a pipe does not --
// a line discipline. Whether the page does the line editing or the guest does
// is the guest's choice, made by clearing ICANON in its termios, and the page
// has to follow it or every keystroke appears twice.

import { Env } from '../../shims/browser/js/gex.mjs';
import { ProcessHost } from '../../shims/browser/js/host.mjs';

const ICANON = 0x2;
const ECHO = 0x8;

/** Where the built runtime lives, relative to the site root. */
const SHIM_URL = '/gex/gex_browser.wasm';
const SEED_URL = '/gex/seed.wasm';
// The global kernel. `.plain` because each process's own memory is an
// ordinary `ArrayBuffer` here: a memory import's `shared` flag is part of its
// type, and a process whose threads are not yet Workers does not need one.
const KERNEL_URL = '/gex/gex_kernel.plain.wasm';

let shimBytes = null;
let seedBytes = null;

async function fetchBytes(url, onProgress) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status} ${res.statusText}`);
  const total = Number(res.headers.get('content-length')) || 0;
  if (!onProgress || !res.body) return new Uint8Array(await res.arrayBuffer());
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
  return out;
}

/** The two wasm modules every demo needs, fetched once for the page's life. */
export async function loadRuntime(onProgress) {
  if (!shimBytes) shimBytes = await fetchBytes(SHIM_URL, onProgress);
  if (!seedBytes) seedBytes = await fetchBytes(SEED_URL);
  return { shimBytes, seedBytes };
}

export { fetchBytes };

/**
 * An `Env` whose output goes to a terminal and whose input comes from one.
 *
 * The awkward part is that a guest read has to be able to wait. gex gives the
 * wasm stack back to JS when every thread is blocked on input, and calls
 * `readStdin` -- so this is where a promise sits until a key arrives.
 */
class TerminalEnv extends Env {
  constructor(term) {
    super();
    this.term = term;
    this.pending = [];
    this.line = [];
    this.waiter = null;
    this.closed = false;
    // The guest's terminal flags, as its Worker last reported them.
    //
    // Read rather than asked for, because the guest is in a Worker now and
    // `gex_tty_lflag` is a synchronous call into its shim. Every process
    // sends its flags with each write and each request for input, which is
    // when they change in practice: a shell sets its terminal up and then
    // prints a prompt.
    this.lflag = ICANON | ECHO;
  }

  write(fd, bytes) {
    // xterm wants \r\n; a Unix program writes \n. Nothing else in the byte
    // stream is touched, so colour and cursor control pass through.
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
   * The next thing typed, as bytes, or false at end of input.
   *
   * The bytes are *returned* rather than pushed into a shim: the guest is in
   * a Worker and this is the page, so what crosses is a message. Everything
   * waiting is joined into one, because a guest reading a line wants the
   * line.
   */
  async readStdin() {
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
 * Run one program to completion.
 *
 * `files` is the guest's filesystem as a path-to-bytes map, `args` its argv.
 * Returns the exit status. The caller owns the terminal; nothing here clears
 * it, so a demo can print its own banner first.
 */
/**
 * The global kernel, fetched once and instantiated per Worker.
 *
 * Everything guest processes share and no single process can own: the process
 * table, the filesystem, the pipes. Its memory is a `SharedArrayBuffer`, so
 * the page has to be cross-origin isolated -- see the COOP/COEP headers in
 * `vite.config.js`. A page that is not gets no kernel and one process, which
 * still runs anything that does not fork.
 */
let kernelSpec = null;
async function loadKernel() {
  if (kernelSpec !== null) return kernelSpec || null;
  // The direct question, rather than `crossOriginIsolated`: what is needed is
  // a `SharedArrayBuffer`, and in a page that flag is only how the browser
  // says whether there is one. Asking for the constructor works under Node
  // too, which is what lets the terminal test drive this same file.
  if (typeof SharedArrayBuffer !== 'function') {
    kernelSpec = false;
    return null;
  }
  try {
    const bytes = await fetchBytes(KERNEL_URL);
    kernelSpec = {
      module: await WebAssembly.compile(bytes),
      // Four gigabytes, and it has to match `--max-memory` in
      // `shims/kernel/build.sh` *exactly*: a shared memory carries its
      // maximum in its type, so a host that offers a different one cannot
      // instantiate the module at all. Raising one without the other is how
      // this page stopped booting.
      //
      // The rootfs lives in this memory, so the ceiling is the largest
      // machine a tab can boot. It reserves address space rather than pages;
      // the memory still starts at 128 MiB and grows as files arrive.
      memory: new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true }),
    };
  } catch {
    kernelSpec = false;
    return null;
  }
  return kernelSpec;
}

/**
 * How to make a Worker running one guest process.
 *
 * A page has `Worker`; Node has `worker_threads`. Both are here because the
 * terminal test drives this same file under Node -- the line discipline and
 * the promise that parks a guest read are page code, and running them in a
 * browser to test them would be a twenty-minute cycle instead of a two-minute
 * one.
 *
 * The browser branch is written out in full and inline because that is the
 * shape vite recognises and bundles; the Node one keeps its path out of a
 * literal so that the bundler leaves it alone.
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
 * Run one program to completion.
 *
 * `files` is the guest's filesystem as a path-to-bytes map, `args` its argv.
 * Returns the exit status. The caller owns the terminal; nothing here clears
 * it, so a demo can print its own banner first.
 *
 * The program runs in a Worker, and so does everything it forks. That is the
 * whole of what replaced the snapshot switcher: each process has a memory of
 * its own, nothing collides, nothing is written out and read back. What they
 * share is in the global kernel, which every one of them instantiates against
 * one shared memory.
 */
export async function runProgram(
  term,
  {
    elf,
    files = {},
    args,
    cwd = '/',
    tty = true,
    network = null,
    onNetEvent = null,
    gl = null,
    virgl = null,
    uid,
    // Every N safepoints, say where the guest is. A run that is merely slow
    // says nothing about *what* is slow, and this is the only instrument that
    // works in a page: the shim prints the guest pc, and which module that
    // address lands in is the answer.
    heartbeat = 0,
    // KiB of mapped text per object to statically recompile once it proves
    // hot; 0 turns it off and leaves the guest to arrive one dispatch miss at
    // a time. A parameter because it is the one guest-visible setting on which
    // the page and `run-node.mjs` disagree -- the page has always had it on
    // and the Node harness has it off by default -- so a fault that appears in
    // only one of the two hosts cannot be attributed until it can be switched.
    recompileObjects = 8192,
    // A closure built ahead of time by `gexc build --closure`: the image
    // module, every library module beside it, and the layout that says where
    // each one was placed. This is what the project is *for* -- the guest
    // arrives already translated and lifts almost nothing at runtime -- and
    // until now no page could be given one, so the browser had only ever been
    // measured in the configuration that matters least.
    closure = null,
  },
) {
  const { shimBytes, seedBytes } = await loadRuntime();
  const kernel = await loadKernel();
  const makeGuestWorker = await guestWorkerFactory();
  const tenv = new TerminalEnv(term);

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
      // In `extra`, so every Worker gets it and not only pid 1. The shell
      // that starts a demo is never the interesting process: the questions
      // are about the X server and the client it forks.
      heartbeat: heartbeat || undefined,
      // How many traps a forked child takes before it stops waiting to be an
      // `execve` and installs its parent's modules. In `extra`, so it
      // reaches the Worker that acts on it.
      adoptAfter: typeof globalThis.GEX_ADOPT_AFTER === "number"
        ? globalThis.GEX_ADOPT_AFTER
        : undefined,
      // The closure's libraries, in `extra` rather than `opts` so a forked
      // child gets them too: a child adopts its parent's memory and with it
      // the dispatch *directory*, but its dispatch tables belong to its own
      // guest instance and start empty, so it has to instantiate the modules
      // itself to have anything for the directory to point at.
      closure: closure?.libs,
      // `?trace=1` turns on the shim's own narration, in every process. The
      // Node harness has had this through an environment variable all along;
      // the page had no way to ask for it, so a question that needed it could
      // only be answered by editing the shim.
      trace: new URLSearchParams(location.search).get('trace') ? 1 : undefined,
      // `?hot=1` asks every process to dump its hottest back edges when it
      // ends. Only `run-node.mjs` could ask before, so the question "what is
      // the guest actually doing" could not be put to a browser at all --
      // and now that install and translation are 17 of a 57 second
      // `apt-get update`, the rest of it is the only thing left to ask about.
      hot: new URLSearchParams(location.search).get('hot') ? 1 : undefined,
      virgl: channel ?? undefined,
      wispUrl: network?.url ?? undefined,
      winsize: { rows: term.rows, cols: term.cols },
      // Every guest write mirrored to the console as well as posted, and a
      // crash narrated where it happens. A Worker that falls over takes its
      // last few messages with it, and those are the ones that say why.
      debugExit:
        !!globalThis.GEX_DEBUG_EXIT ||
        (typeof process !== 'undefined' && !!process.env?.GEX_DEBUG_EXIT),
      // Statically recompile each object the guest maps once it proves hot,
      // rather than meeting it one dispatch miss at a time. This is the
      // feature the project exists for, and the page was not using it: every
      // block on this site arrived through a miss, so glxgears spent 11.8 of
      // its first 34 seconds inside the translator and drew at a third of a
      // frame a second.
      //
      // A cap in KiB of mapped text per object. Eight megabytes is the
      // largest that works today -- above it the graphics rootfs meets #73 --
      // and it covers every library these demos run except Mesa's software
      // rasteriser, which nothing here wants because the renderer is on the
      // other side of the vtest socket.
      recompileObjects,
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
  // are echoed, and whether they are delivered a line at a time, is the
  // guest's choice, read out of its termios and reported by its Worker.
  const keys = term.onData((d) => tenv.onKey(d));
  // xterm only hears the keyboard through a hidden textarea, which has to
  // hold the focus. Nothing gives it that on its own, and the button that
  // starts a demo disables itself on the way -- which drops focus to the
  // body. So a program would start, print its prompt, and ignore every key
  // until the person thought to click on the terminal.
  term.focus?.();
  const resize = term.onResize(({ rows, cols }) => host.resize(rows, cols));
  try {
    const status = await host.start({
      elf,
      files,
      args,
      // The layout the closure was built against. Without it the placer hands
      // out an address inside a pinned object and the image lands somewhere
      // its own translated code does not expect.
      opts: { cwd, tty, uid, recompileObjects, heartbeat, pins: closure?.pins },
    });
    return { status, host, tenv };
  } finally {
    keys.dispose();
    resize.dispose();
    host.stop();
  }
}

export { TerminalEnv };
