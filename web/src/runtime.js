// A gex runtime bound to an xterm.js terminal.
//
// The seam is `Env`: gex hands the page every write and asks it for input.
// What this adds on top is the part a terminal needs and a pipe does not --
// a line discipline. Whether the page does the line editing or the guest does
// is the guest's choice, made by clearing ICANON in its termios, and the page
// has to follow it or every keystroke appears twice.

import { Env } from '../../shims/browser/js/gex.mjs';
import { ProcessHost } from '../../shims/browser/js/host.mjs';
import { PageTty } from '../../shims/browser/js/tty.mjs';

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

/// Four bytes that say a response is a wasm module and not a web page.
///
/// A dev server that cannot find a file answers with `index.html` and status
/// *200*, so `res.ok` proves nothing. Handing that to `WebAssembly.compile`
/// reports a magic-word error against the module, which sends the reader
/// looking at the build rather than at the request -- and the build is fine.
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
  // Revalidated, never taken from cache on faith.
  //
  // `/gex/gex_browser.wasm` and the seed have fixed URLs with no content
  // hash, so a browser will happily keep serving a build it fetched hours
  // ago. That is fine until a broken one is published for a few minutes, and
  // then a reload does not fix it: the page keeps running the broken shim and
  // the guest traps instantly -- 133, SIGTRAP, from a dispatch table that no
  // longer matches the closure beside it. Cheap insurance: one conditional
  // request, and a 304 for the common case.
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
 * A `?name=` switch, from wherever this is running.
 *
 * `location` does not exist under Node, and this file is deliberately
 * runnable there -- the terminal test drives it, because the alternative is a
 * twenty-minute cycle in a browser for a change to the line discipline. Three
 * bare `location.search` reads had crept back in, and they threw before the
 * guest was started, so the test reported `ReferenceError: location is not
 * defined` and nothing else.
 */
function flag(name) {
  const search = globalThis.location?.search;
  if (search) return new URLSearchParams(search).get(name);
  // Under Node the same switches arrive as environment variables, spelled
  // the way the rest of the harness spells them: `GEX_TRACE`, `GEX_HOT`.
  return globalThis.process?.env?.[`GEX_${name.toUpperCase()}`] ?? null;
}

/**
 * An `Env` whose output goes to a terminal and whose input comes from one.
 *
 * The awkward part is that a guest read has to be able to wait. gex gives the
 * wasm stack back to JS when every thread is blocked on input, and calls
 * `readStdin` -- so this is where a promise sits until a key arrives.
 */
class TerminalEnv extends Env {
  constructor(term, tty = null) {
    super();
    this.term = term;
    // The system's terminal, when there is a global kernel to hold one.
    //
    // With it, none of the line editing below runs: a keystroke goes into the
    // discipline in the kernel and comes back as echo, the bytes a program
    // reads are already in the queue that program reads from, and `^C` has
    // become `SIGINT` to the foreground group before this returns. Without
    // it there is one process and no process table, and the fallback below is
    // exactly right for that.
    this.tty = tty;
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
    if (this.tty) {
      // Straight into the discipline. What comes back is what a terminal
      // shows: the character in cooked mode, `^C` and a fresh line for an
      // interrupt, backspace-space-backspace for an erase -- and nothing at
      // all when the program has taken echo over.
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
        // `true` rather than the bytes: they are already in the queue the
        // guest reads from, and handing them over here would deliver them
        // twice.
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
   * The next thing typed, as bytes, or false at end of input.
   *
   * The bytes are *returned* rather than pushed into a shim: the guest is in
   * a Worker and this is the page, so what crosses is a message. Everything
   * waiting is joined into one, because a guest reading a line wants the
   * line.
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
    // Every N safepoints, say where the guest is. A run that is merely slow
    // says nothing about *what* is slow, and this is the only instrument that
    // works in a page: the shim prints the guest pc, and which module that
    // address lands in is the answer.
    heartbeat = globalThis.GEX_HEARTBEAT ?? 0,
    // KiB of mapped text per object to statically recompile once it proves
    // hot; 0 turns it off and leaves the guest to arrive one dispatch miss at
    // a time. A parameter because it is the one guest-visible setting on which
    // the page and `run-node.mjs` disagree -- the page has always had it on
    // and the Node harness has it off by default -- so a fault that appears in
    // only one of the two hosts cannot be attributed until it can be switched.
    recompileObjects = typeof globalThis.GEX_RECOMPILE_OBJECTS === 'number'
      ? globalThis.GEX_RECOMPILE_OBJECTS
      : 8192,
    // A closure built ahead of time by `gexc build --closure`: the image
    // module, every library module beside it, and the layout that says where
    // each one was placed. This is what the project is *for* -- the guest
    // arrives already translated and lifts almost nothing at runtime -- and
    // until now no page could be given one, so the browser had only ever been
    // measured in the configuration that matters least.
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
  // The system's terminal, instantiated here so that a keystroke reaches the
  // line discipline and the process table by a call rather than by a message
  // to a Worker. Null without a shared memory, and `TerminalEnv` then does
  // its own line editing -- see its constructor.
  const sharedTty = tty ? await PageTty.open(kernel) : null;
  const tenv = new TerminalEnv(term, sharedTty);
  // Before pid 1 runs, so the first `TIOCGWINSZ` is the truth. A shell asks
  // once at startup and then only on `SIGWINCH`, so a wrong answer here is
  // one that lasts.
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
      trace: flag('trace') ? 1 : undefined,
      // `?hot=1` asks every process to dump its hottest back edges when it
      // ends. Only `run-node.mjs` could ask before, so the question "what is
      // the guest actually doing" could not be put to a browser at all --
      // and now that install and translation are 17 of a 57 second
      // `apt-get update`, the rest of it is the only thing left to ask about.
      hot: flag('hot') ? 1 : undefined,
      // `?inlinethreads=1` keeps a guest's threads on the process's own
      // Worker instead of giving each one a Worker of its own.
      //
      // Both shapes exist already -- a memory that is not shared cannot have
      // Worker threads and has always run them inline -- and which one a run
      // gets was decided by the memory rather than by anyone choosing. That
      // makes the one real threaded program in the Debian rootfs impossible
      // to compare: liblzma's decoder spawns a worker, and `dpkg-deb` dies of
      // `free(): invalid pointer` in the browser while the same archive comes
      // out whole under the native adapter three times for three.
      inlineThreads: flag('inlinethreads') ? 1 : undefined,
      virgl: channel ?? undefined,
      wispUrl: network?.url ?? undefined,
      // The size, but only when there is no terminal to hold it.
      //
      // With a shared terminal this must not be sent. It is captured when
      // `runProgram` is called and every Worker writes it on startup -- so a
      // process forked *after* a resize set the system's terminal back to the
      // size it had at boot. `stty size` in a command run from a resized
      // window reported the old size, and the window that reported it was the
      // one that had just been resized.
      winsize: sharedTty ? undefined : { rows: term.rows, cols: term.cols },
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
      // In `extra` rather than `opts`: the shell that starts a demo is not
      // the process this is for. What pays for the dynamic linker is the
      // forty commands it runs, and each of those is a Worker of its own
      // that never sees `opts`.
      link: link || undefined,
      // Who the guest is, in `extra` so a forked child gets it: identity is
      // fixed when a kernel is built and a child builds its own.
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
  // are echoed, and whether they are delivered a line at a time, is the
  // guest's choice, read out of its termios and reported by its Worker.
  const keys = term.onData((d) => tenv.onKey(d));
  // xterm only hears the keyboard through a hidden textarea, which has to
  // hold the focus. Nothing gives it that on its own, and the button that
  // starts a demo disables itself on the way -- which drops focus to the
  // body. So a program would start, print its prompt, and ignore every key
  // until the person thought to click on the terminal.
  term.focus?.();
  // A resize is `SIGWINCH` to the foreground group and one write to the
  // terminal every process shares -- not a message to each of them, which is
  // what it was when each process had a window size of its own.
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
