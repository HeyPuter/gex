// The page's half of the guest's GPU.
//
// The guest runs Debian's own Mesa with its Gallium `virgl` driver, which
// speaks the vtest protocol over a UNIX socket. `shims/browser/src/virgl.rs`
// serves that socket and hands the bytes here; here hands them to
// virglrenderer, compiled to WebAssembly by `web/build-virgl-wasm.sh` and
// pointed at a WebGL 2 context.
//
// Two wasm modules, so the bytes cross through JavaScript. That is the whole
// of what this file is: a copy in, a copy out, and the canvas.
//
// # Why a second module rather than one
//
// virglrenderer is a hundred thousand lines of C that wants a libc, a GL
// binding and its own heap. The shim is Rust sharing the *guest's* memory --
// that is what makes a syscall a direct read of the guest's buffers. Putting
// both in one module would mean giving virglrenderer the guest's address
// space to allocate in, which is exactly what it must not have.

/** Load the renderer and point it at a canvas. */
/**
 * Load the renderer's emscripten glue, whatever the server thinks of it.
 *
 * It is a build artifact rather than source: it lives in `public/` next to
 * the wasm it loads, and both are copied as-is. Importing it directly works
 * on the built site and fails on the dev server, which answers
 *
 *   Failed to load url /gex/gexvirgl.mjs. This file is in /public and will be
 *   copied as-is during build without going through the plugin transforms,
 *   and therefore should not be imported from source code.
 *
 * -- a rule about *source*, which this is not. So it is fetched as text and
 * turned into a module here. The one thing that costs is the glue's idea of
 * where its wasm is, and `locateFile` says so explicitly.
 */
async function loadGlue(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status} ${res.statusText}`);
  const blob = URL.createObjectURL(
    new Blob([await res.text()], { type: 'text/javascript' }),
  );
  try {
    return (await import(/* @vite-ignore */ blob)).default;
  } finally {
    URL.revokeObjectURL(blob);
  }
}

export class VirglRenderer {
  #module = null;
  #canvas = null;
  #failed = null;

  /**
   * @param {HTMLCanvasElement} canvas the drawing surface
   * @param {string} url where gexvirgl.mjs was published
   */
  /**
   * @param {HTMLCanvasElement} canvas the drawing surface
   * @param {string} url where gexvirgl.mjs was published
   * @param {(s: string) => void} [onLog] where the renderer's own diagnostics
   *   go. The default is the console, which in a headless browser goes
   *   nowhere useful and is dropped under load -- a trace that arrives four
   *   lines out of twenty is worse than none, because it reads like the
   *   twentieth. A harness that has a channel of its own should pass it.
   */
  static async create(canvas, url = '/gex/gexvirgl.mjs', onLog = null) {
    const r = new VirglRenderer();
    r.#canvas = canvas;
    // virglrenderer looks the canvas up by selector, so it has to carry the
    // id the module asks for.
    canvas.id = 'gex-gl';
    try {
      const Module = await loadGlue(url);
      r.#module = await Module({
        // Where the renderer's own wasm is, said rather than inferred. The
        // glue works it out from `import.meta.url`, which is a blob when it
        // is loaded the way `loadGlue` has to load it.
        locateFile: (file) => new URL(file, new URL(url, location.href)).href,
        // virglrenderer explains itself on stderr and says nothing through a
        // return value beyond "no", so this is the only diagnostic there is.
        printErr: (s) => (onLog ? onLog(s) : console.warn('[virgl]', s)),
        print: (s) => (onLog ? onLog(s) : console.log('[virgl]', s)),
      });
      const rc = r.#module._gex_virgl_init(canvas.width, canvas.height);
      if (rc !== 0) {
        r.#failed = `the renderer would not start (${rc})`;
      }
    } catch (e) {
      r.#failed = e?.message || String(e);
    }
    return r;
  }

  get ok() {
    return this.#module !== null && this.#failed === null;
  }

  get error() {
    return this.#failed;
  }

  /** What the guest will be told it has. */
  describe() {
    return this.ok ? this.#module.UTF8ToString(this.#module._gex_virgl_describe()) : this.#failed;
  }

  /**
   * The renderer's GL state, and how many frames it has put on the canvas.
   *
   * `presented` is the one worth reading. vtest has no present -- the guest
   * reads its pixels back and never looks at a screen -- so a run can be
   * drawing perfectly with nothing visible, and this is what tells the two
   * apart.
   */
  glState() {
    return this.ok ? this.#module.UTF8ToString(this.#module._gex_virgl_gl_state()) : this.#failed;
  }

  /**
   * Every command the guest has sent, by name and count, and every reason a
   * readback did not become a picture.
   *
   * This is the one to read when the canvas is black. `presented` says only
   * that nothing arrived; this says whether the guest drew at all, whether
   * it ever read back, and if it did, what was wrong with what it read.
   */
  commands() {
    return this.ok ? this.#module.UTF8ToString(this.#module._gex_virgl_commands()) : this.#failed;
  }

  /**
   * Hand over `len` bytes of the guest's stream, at `ptr` in the *shim's*
   * memory, and run every command they complete.
   *
   * @returns {number} reply bytes waiting, or -1 with no renderer
   */
  submit(session, shimMemory, ptr, len) {
    if (!this.ok) return -1;
    const buf = this.#module._malloc(len);
    this.#module.HEAPU8.set(new Uint8Array(shimMemory.buffer, ptr, len), buf);
    this.#module._gex_virgl_feed(session, buf, len);
    this.#module._free(buf);
    const ran = this.#module._gex_virgl_pump(session);
    // A refusal is the end of the connection, not of this call: a stream
    // protocol cannot resynchronise after a command of unknown length, and
    // pretending otherwise turns one bad command into a stream of garbage.
    if (ran < 0) {
      this.#failed = 'the renderer refused the stream';
      return -1;
    }
    return this.#module._gex_virgl_pending(session);
  }

  /** Copy up to `max` reply bytes to `ptr` in the shim's memory. */
  take(session, shimMemory, ptr, max) {
    if (!this.ok) return 0;
    const have = this.#module._gex_virgl_pending(session);
    const n = Math.min(have, max);
    if (n > 0) {
      const from = this.#module._gex_virgl_replies(session);
      new Uint8Array(shimMemory.buffer, ptr, n).set(
        this.#module.HEAPU8.subarray(from, from + n),
      );
    }
    // All or nothing: the shim always asks for everything waiting, and a
    // partial drain would leave the rest to be re-sent on top of itself.
    this.#module._gex_virgl_drain(session);
    return n;
  }

  /**
   * Narrate every command as it starts.
   *
   * For the case where the renderer does not come back: it is called from
   * JavaScript, so a command that loops takes the page with it -- no timer
   * fires, the shim's counters stop, and the guest is not running. The last
   * line printed names the command it went into.
   */
  traceCommands(on) {
    if (this.ok) this.#module._gex_virgl_trace_commands(on ? 1 : 0);
  }

  /** A client has gone: drop its context and its buffers. */
  close(session) {
    if (this.ok) this.#module._gex_virgl_close(session);
  }
}

/**
 * The three imports `shims/browser/src/virgl.rs` declares.
 *
 * `memory` is the shim's, resolved lazily because the module that owns it is
 * being instantiated when these are handed over.
 */
export function virglImports(renderer, getMemory) {
  return {
    virgl_submit: (session, ptr, len) =>
      (renderer ? renderer.submit(session, getMemory(), ptr, len) : -1),
    virgl_take: (session, ptr, max) =>
      (renderer ? renderer.take(session, getMemory(), ptr, max) : 0),
    virgl_close: (session) => renderer?.close(session),
  };
}
