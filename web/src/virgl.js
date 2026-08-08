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
export class VirglRenderer {
  #module = null;
  #canvas = null;
  #failed = null;

  /**
   * @param {HTMLCanvasElement} canvas the drawing surface
   * @param {string} url where gexvirgl.mjs was published
   */
  static async create(canvas, url = '/gex/gexvirgl.mjs') {
    const r = new VirglRenderer();
    r.#canvas = canvas;
    // virglrenderer looks the canvas up by selector, so it has to carry the
    // id the module asks for.
    canvas.id = 'gex-gl';
    try {
      const Module = (await import(/* @vite-ignore */ url)).default;
      r.#module = await Module({
        // virglrenderer explains itself on stderr and says nothing through a
        // return value beyond "no", so this is the only diagnostic there is.
        printErr: (s) => console.warn('[virgl]', s),
        print: (s) => console.log('[virgl]', s),
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
   * Hand over `len` bytes of the guest's stream, at `ptr` in the *shim's*
   * memory, and run every command they complete.
   *
   * @returns {number} reply bytes waiting, or -1 with no renderer
   */
  submit(shimMemory, ptr, len) {
    if (!this.ok) return -1;
    const buf = this.#module._malloc(len);
    this.#module.HEAPU8.set(new Uint8Array(shimMemory.buffer, ptr, len), buf);
    this.#module._gex_virgl_feed(buf, len);
    this.#module._free(buf);
    const ran = this.#module._gex_virgl_pump();
    // A refusal is the end of the connection, not of this call: a stream
    // protocol cannot resynchronise after a command of unknown length, and
    // pretending otherwise turns one bad command into a stream of garbage.
    if (ran < 0) {
      this.#failed = 'the renderer refused the stream';
      return -1;
    }
    return this.#module._gex_virgl_pending();
  }

  /** Copy up to `max` reply bytes to `ptr` in the shim's memory. */
  take(shimMemory, ptr, max) {
    if (!this.ok) return 0;
    const have = this.#module._gex_virgl_pending();
    const n = Math.min(have, max);
    if (n > 0) {
      const from = this.#module._gex_virgl_replies();
      new Uint8Array(shimMemory.buffer, ptr, n).set(
        this.#module.HEAPU8.subarray(from, from + n),
      );
    }
    // All or nothing: the shim always asks for everything waiting, and a
    // partial drain would leave the rest to be re-sent on top of itself.
    this.#module._gex_virgl_drain();
    return n;
  }
}

/**
 * The two imports `shims/browser/src/virgl.rs` declares.
 *
 * `memory` is the shim's, resolved lazily because the module that owns it is
 * being instantiated when these are handed over.
 */
export function virglImports(renderer, getMemory) {
  return {
    virgl_submit: (ptr, len) => (renderer ? renderer.submit(getMemory(), ptr, len) : -1),
    virgl_take: (ptr, max) => (renderer ? renderer.take(getMemory(), ptr, max) : 0),
  };
}
