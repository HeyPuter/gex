// A gex runtime bound to an xterm.js terminal.
//
// The seam is `Env`: gex hands the page every write and asks it for input.
// What this adds on top is the part a terminal needs and a pipe does not --
// a line discipline. Whether the page does the line editing or the guest does
// is the guest's choice, made by clearing ICANON in its termios, and the page
// has to follow it or every keystroke appears twice.

import { Gex, Env } from '../../shims/browser/js/gex.mjs';

const ICANON = 0x2;
const ECHO = 0x8;

/** Where the built runtime lives, relative to the site root. */
const SHIM_URL = '/gex/gex_browser.wasm';
const SEED_URL = '/gex/seed.wasm';

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
  constructor(term, gexRef) {
    super();
    this.term = term;
    this.gexRef = gexRef;
    this.pending = [];
    this.line = [];
    this.waiter = null;
    this.closed = false;
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
    const shim = this.gexRef.current?.shim;
    if (!shim?.exports?.gex_tty_lflag) return false;
    return (shim.exports.gex_tty_lflag() & ICANON) === 0;
  }

  #echoes() {
    const shim = this.gexRef.current?.shim;
    if (!shim?.exports?.gex_tty_lflag) return true;
    return (shim.exports.gex_tty_lflag() & ECHO) !== 0;
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
      this.gexRef.current.pushStdin(bytes);
      w(true);
    } else {
      this.pending.push(bytes);
    }
  }

  async readStdin() {
    if (this.closed) return false;
    if (this.pending.length) {
      for (const b of this.pending) this.gexRef.current.pushStdin(b);
      this.pending = [];
      return true;
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
export async function runProgram(
  term,
  { elf, files = {}, args, cwd = '/', tty = true, network = null, gl = null, virgl = null, uid },
) {
  const { shimBytes, seedBytes } = await loadRuntime();
  const ref = { current: null };
  const tenv = new TerminalEnv(term, ref);
  const gex = new Gex(tenv);
  ref.current = gex;

  // Before `load`: the graphics device node is created while the guest's
  // filesystem is, and a guest that opens /dev/gex-gl and does not find it
  // concludes the machine has no graphics.
  if (gl) gex.useGl(gl);
  // Likewise, and for a sharper reason: the vtest socket is bound while the
  // guest's namespace is built, and Mesa connecting to a socket nobody serves
  // does not fail -- it writes into the dead descriptor and waits forever.
  if (virgl) gex.useVirgl(virgl);
  const entry = await gex.load(shimBytes, seedBytes, elf, files, args, { cwd, tty, uid });
  if (!entry) throw new Error('the program could not be loaded');
  if (gex.shim.exports.gex_set_winsize) {
    gex.shim.exports.gex_set_winsize(term.rows, term.cols);
  }
  if (network) gex.useNetwork(network);
  // Statically recompile each shared object the guest maps, rather than
  // meeting it one dispatch miss at a time. This is the whole feature the
  // project exists for, and the page was not using it: every block on this
  // site arrived through a miss, so glxgears spent 11.8 of its first 34
  // seconds inside the translator and drew at a third of a frame a second.
  //
  // The value is a cap in KiB of mapped text per object. Eight megabytes is
  // the largest that works today -- above it the graphics rootfs meets #73 --
  // and it covers every library the demos actually run except Mesa's software
  // rasteriser, which nothing here wants because the renderer is on the other
  // side of the vtest socket.
  gex.shim.exports.gex_set_recompile_objects?.(8192);
  // Keystrokes go to the environment, not straight to the guest: whether
  // they are echoed, and whether they are delivered a line at a time, is the
  // guest's choice, read out of its termios.
  const keys = term.onData((d) => tenv.onKey(d));
  // xterm only hears the keyboard through a hidden textarea, which has to
  // hold the focus. Nothing gives it that on its own, and the button that
  // starts a demo disables itself on the way -- which drops focus to the
  // body. So a program would start, print its prompt, and ignore every key
  // until the person thought to click on the terminal.
  term.focus?.();
  const resize = term.onResize(({ rows, cols }) => {
    gex.shim.exports.gex_set_winsize?.(rows, cols);
  });
  try {
    const status = await gex.run(entry);
    return { status, gex, tenv };
  } finally {
    keys.dispose();
    resize.dispose();
  }
}

export { TerminalEnv };
