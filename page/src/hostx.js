// A host-side X server: Xwayland as WebAssembly, and the compositor beside it.
//
// This is the machinery; `xwayland-worker.js` is the Worker that runs it in the
// page, and it runs the same way under Node, which is where it is easiest to
// see the server come up. Either host gives it the same three things: the two
// module URLs, the global kernel, and the page's scene arena.
//
// Nothing between the X server and the compositor is a socket. The server's
// Wayland connection is a queue into the compositor module next door, and its
// window-manager connection (`-wm`) is another, so a frame's worth of protocol
// never leaves this thread. The one real socket is the X display itself --
// `/tmp/.X11-unix/X0` in the global kernel -- which is where guest processes
// connect and where this accepts them with `k_sock_accept`.
//
// The pixels never move. An X window's contents are a `wl_shm` pool, and a pool
// here is a plain allocation in the server's own memory (see
// `ports/xwayland/gexos.c`), which is a `SharedArrayBuffer` because the module is
// built `-pthread`. So the page's compositor Worker uploads an X window to a
// texture straight out of the X server's heap: no copy on the way to the
// screen, exactly like a guest's own Wayland surface.
//
// The server blocks. `main()` never returns, and every wait goes through
// `gex_poll` -> `wait()` here -> `k_ws_park`, the same park a guest
// process uses. That is why the housekeeping -- pumping the window manager,
// publishing the scene, taking input -- happens inside `tick()`, which the
// server calls every turn of its loop: after `callMain` there is no JavaScript
// turn left to do it in.

import { WlProxy, outputSize, REC, EV, SLOTS } from '../../hosts/browser/js/wl-bridge.mjs';
import { isZstd, unzstd } from './zfetch.js';
import { xglBuffers, xglServer } from './xgl.js';
import { helperStackTop, kernelThread, lazyFetch, processMemories } from '../../hosts/browser/js/lazy-fetch.mjs';
import { blockIo } from '../../hosts/browser/js/blocks.mjs';
import { pageFs } from '../../hosts/browser/js/page-fs.mjs';
import { single, sched } from '../../hosts/browser/js/single.mjs';
import { SP_CODE } from '../../hosts/browser/js/mode.mjs';

/** Events taken from the page in one turn. */
const MAX_INPUT = 64;
/** The most clipboard text that crosses to and from X programs. */
const X_CLIP = 256 << 10;
/** errno values the transport answers with. */
const EAGAIN = 11;
const EBADF = 9;

/**
 * Import emscripten's glue.
 *
 * Directly, not through a blob: this module is built
 * `-pthread`, and emscripten's thread support works out where to load its
 * Worker from `import.meta.url` -- which is not a URL at all when the glue was
 * imported from a blob ("TypeError: Invalid URL", from `allocateUnusedWorker`,
 * before the server has run a line): a page's kept copy (below) has its
 * `import.meta.url` written as the glue's real URL. The specifier is a variable and marked
 * for the bundler to leave alone, so nothing tries to bundle 250 KB of glue.
 */
async function loadGlue(url) {
  // A page that fetched the glue up front (with the rest of what its session
  // loads, before the tree: the KDE site's index.html) names its copy in
  // `GEX_URL_MAP` (URL -> blob URL); a module is never fetched by import().
  const kept = globalThis.GEX_URL_MAP?.[String(url).split('?')[0]];
  return (await import(/* @vite-ignore */ kept ?? url)).default;
}

/** What a relative module URL is relative to, in a page or under Node. */
function base() {
  if (typeof location !== 'undefined') return location.href;
  return new URL(`file://${process.cwd()}/`).href;
}

/**
 * The bytes of a module, from a URL or a path. Over HTTP a `.wasm` is asked for
 * as `NAME.wasm.zst` first (the compositor bridge is 15 MB as it is, under 3
 * zstd'd), decoded with the page's zstd decoder; a site that ships only the
 * plain file answers that with a 404 (or its index page), and the plain file
 * is fetched.
 */
export async function moduleBytes(url) {
  if (/^https?:/.test(url)) {
    if (/\.wasm$/.test(new URL(url).pathname)) {
      try {
        const res = await fetch(`${url}.zst`);
        if (res.ok) {
          const z = new Uint8Array(await res.arrayBuffer());
          if (isZstd(z)) return unzstd(z);
        }
      } catch {
        // No .zst: the plain file below.
      }
    }
    return (await fetch(url)).arrayBuffer();
  }
  const { readFile } = await import('node:fs/promises');
  const { fileURLToPath } = await import('node:url');
  return (await readFile(url.startsWith('file:') ? fileURLToPath(url) : url)).buffer;
}

/** gexgl.c's names for xgl.js's server half. */
function xglHooks(s) {
  return { xglWatch: s.watch, xglViewable: s.viewable, xglTake: s.take, xglDone: s.done };
}

/**
 * One host X server: the two modules, the descriptor table, and the loop's
 * housekeeping.
 */
export class HostX {
  /**
   * @param {(m: object, transfer?: object[]) => void} emit where notes and
   *   events go: `postMessage` in a Worker, a print in a test.
   */
  constructor(emit) {
    this.emit = emit ?? (() => {});
  }

  /** A line for whoever is watching. */
  log(s) {
    this.emit({ kind: 'log', text: s });
  }

  /** The emscripten module (the X server). */
  #x = null;
  /** The compositor module's exports and memory. */
  #hw = null;
  #hwMem = null;
  #hwBytes = null;
  #hwWords = null;
  /** The global kernel, instantiated against the X server's memory. */
  #k = null;
  /** Its memory. */
  #kmem = null;
  /** What each descriptor means. */
  #fds = new Map();
  /**
   * Pools of pixels, by descriptor: where they are in the server's memory.
   *
   * A pool is freed when nothing refers to it: `refs` counts the server's
   * descriptor, its mappings (`gex_shm_map`/`gex_shm_unmap` in gexos.c) and
   * the compositor's handles (`#held`, given back by `host_release`). Then
   * it waits in `#dying` until the page has drawn a scene published after
   * that, so a frame already on its way is never read out of freed memory.
   * Xwayland makes a new pool for a window that has not drawn for a second
   * (about 0.9 MB for an xterm), and before this every one of them stayed.
   */
  #pools = new Map();
  /** Mapped pools, by address: what `gex_shm_unmap` names. */
  #mapped = new Map();
  /** Pools nothing refers to, with the scene generation they died after. */
  #dying = [];
  /** Pools made and not yet freed, for `?hostxcount=1`. */
  #live = 0;
  /**
   * The pools the compositor holds, by its own handle for each: what a
   * descriptor sent on the Wayland connection became when it arrived.
   *
   * A descriptor is the server's name for a pool only until the server closes
   * it, and Xwayland closes a pool's descriptor as soon as it has sent it --
   * then the next pool it makes gets the same number. Handing the compositor
   * the number made every buffer of the old pool resolve, at its next commit,
   * to whichever pool had the number now. Typing into the xterm reuses its
   * two buffers, and with both pools on one number both showed the second's
   * memory: the window was a frame behind what it drew until something made
   * a fresh pool (clicking off it and back on did). A cursor pool on the
   * number was too small to hold the frame, and the commit was dropped. Over
   * a socket `SCM_RIGHTS` gives the receiver its own descriptor for the same
   * memory, and this is that: a handle per descriptor sent, never reused.
   */
  #held = new Map();
  #nextHeld = 1;
  /**
   * Descriptors owed to the server on its Wayland connection.
   *
   * Some events carry one: `wl_keyboard.keymap` is a length and a descriptor,
   * and libwayland refuses the message outright if the descriptor is not there
   * ("file descriptor expected, object (12), message keymap(uhu)"). The
   * compositor state machine cannot make one, so the payload becomes a pool of
   * bytes in the server's own memory and its descriptor is queued here to ride
   * the next read.
   */
  #owed = [];
  /** The page's scene arena. */
  #proxy = null;
  #channel = null;
  #out = { w: 0, h: 0 };
  #token = 0;
  /** Reported once each, so a broken transport is loud but not a flood. */
  #said = new Set();
  /** How many transport calls left to narrate; 0 for none. */
  #trace = 0;
  /** Publishes so far, for the note in `#tick`. */
  #published = 0;
  /**
   * `?hostxcount=1`: once a second, what moved through the server -- bytes in
   * from and out to each X client, bytes to the compositor, scenes published.
   * Where a picture stops between a client drawing and the page composing it.
   */
  #count = null;
  /** `win` (GEX_LOG=win): each guest connection's requests, read for the
   *  first MapWindow of a window its client named (WM_CLASS), by fd. */
  #maps = null;

  async start(msg) {
    this.#channel = msg.wlChannel;
    this.#trace = msg.trace | 0;
    if (msg.win) this.#maps = new Map();
    if (msg.count) this.#count = { at: Date.now(), n: new Map(), polls: 0 };
    this.#token = msg.token;
    const size = outputSize(msg.wlChannel);
    this.#out = { w: size.w, h: size.h };

    // -- the X server ------------------------------------------------------
    const Module = await loadGlue(msg.xwaylandUrl);
    this.#x = await Module({
      // Where the server's own wasm is, said rather than inferred: the glue
      // works it out from `import.meta.url`, which is a blob when it is loaded
      // the way `loadGlue` has to load it in a page.
      locateFile: (f) => msg.xwaylandFiles?.[f] ?? new URL(f, new URL(msg.xwaylandUrl, base())).href,
      // The server explains itself on stderr, and in a Worker that is the only
      // diagnostic there is.
      print: (s) => this.log(s),
      printErr: (s) => this.log(s),
      noInitialRun: true,
    });

    // The keymap the server will load. It cannot compile one -- that means
    // forking xkbcomp -- so `ports/xwayland/build.sh` compiles it with the
    // host's xkbcomp and patch.py points the server's loader at this path.
    if (msg.keymapUrl) {
      const km = new Uint8Array(await moduleBytes(msg.keymapUrl));
      try {
        this.#x.FS.mkdir('/xkb');
      } catch {
        // Already there.
      }
      this.#x.FS.writeFile('/xkb/gex.xkm', km);
      this.log(`[hostx] keymap: ${km.length} bytes at /xkb/gex.xkm`);
    }

    // -- the compositor beside it ------------------------------------------
    const bytes = await moduleBytes(msg.hostwlUrl);
    const hw = await WebAssembly.instantiate(await WebAssembly.compile(bytes), {
      gex_host: {
        host_locate: (fd, out) => this.#locate(fd, out),
        host_release: (fd) => {
          const pool = this.#held.get(fd);
          if (!pool) return;
          this.#held.delete(fd);
          this.#drop(pool);
        },
        host_trace: (ptr, len) => this.log(this.#hwText(ptr, len).replace(/\n$/, '')),
      },
    });
    this.#hw = hw.exports;
    this.#hwMem = hw.exports.memory;
    // `?wltrace=1` on the page turns this on for the guest compositors; it
    // means the same thing here.
    if (msg.wlTrace) this.#hw.hw_set_trace?.(1);
    this.#hw.hw_init(this.#out.w, this.#out.h);
    if (msg.wlTrace) this.#hw.hw_set_trace?.(1);

    // -- the global kernel, against the server's memory --------------------
    //
    // The same module every guest process instantiates, with the same kernel
    // memory (so this Worker sees the one process table, filesystem and socket
    // namespace) and this module's memory as the process memory. That second
    // import is what makes `k_sock_read` land bytes directly in the server's
    // own buffer.
    if (msg.kernel) {
      const mod = msg.kernel.module instanceof WebAssembly.Module
        ? msg.kernel.module
        : await WebAssembly.compile(msg.kernel.module);
      const inst = await WebAssembly.instantiate(mod, {
        env: { memory: msg.kernel.memory, ...processMemories(this.#x.wasmMemory, false, msg.kernel.spare), lazy_fetch: lazyFetch(msg.kernel.memory), block_io: blockIo(msg.kernel.memory), page_fs: pageFs(msg.kernel.memory) },
      });
      // Every Worker in the kernel needs its own stack slice: wasm-ld gives
      // each instance the same `__stack_pointer`, so two Workers sharing one
      // would overwrite each other's locals mid-syscall. The page hands slices
      // out from the bottom (see host.mjs) and this takes its own helper slot
      // at the top (`HELPER_STACKS`), which the host never hands out.
      const top = msg.kernel.stackTop ?? (inst.exports.k_stack_area ? helperStackTop(inst.exports, 'hostx') : 0);
      // And thread-locals of its own at the top of it (`kernelThread`).
      if (top) kernelThread(inst.exports, top);
      this.#k = inst.exports;
      this.#kmem = msg.kernel.memory;
    }
    if (!this.#k) throw new Error('the host X server needs the global kernel');
    this.#waitInit();

    // -- the arena ---------------------------------------------------------
    this.#proxy = new WlProxy(msg.wlChannel, () => this.emit({ kind: 'wl' }), () => this.#token);
    // The server waits on its wait slot (`wait` below), which the compositor
    // notifies through the server's own queue when it puts input in this
    // block's ring.
    this.#proxy.onPixels = (p) => this.emit({ kind: 'wl-pixels', ...p });
    this.#proxy.wakeAddr = () => ({ addr: this.#k.k_wq_addr(this.#queue) >>> 0, slots: this.#k.k_ws_addr(0) >>> 0 });
    // The compositor Worker reads an X window's pixels out of this memory
    // only without pixel buffers in the channel (wl-bridge.mjs) -- the
    // object, not a buffer, since a shared memory that grows hands out a new
    // one. The global kernel's goes once a page, for the wake.
    {
      const kernel = this.#proxy.firstForKernel() ? msg.kernel.memory : undefined;
      const pixels = !!msg.wlChannel.pixels;
      if (!pixels || kernel) this.emit({ kind: 'wl-memory', token: this.#token, memory: pixels ? undefined : this.#x.wasmMemory, kernel });
    }

    // -- the transport the server will use ---------------------------------
    this.#x.gexos = this.#gexos();

    // Rootful, a GL client's frames go into its window's own pixels (xgl.js):
    // the buffers they come through, for the compositor.
    if (msg.rootful) {
      const b = xglBuffers();
      if (b) {
        Object.assign(this.#x.gexos, xglHooks(xglServer(b, this.#x, () => this.#proxy.block)));
        this.emit({ kind: 'x-gl', ...b });
      }
    }

    const argv = this.#argv(msg);
    this.log(`[hostx] Xwayland ${argv.join(' ')}`);
    this.emit({ kind: 'ready' });
    // And from here the X server owns this thread: `main` does not return, and
    // everything else happens inside `tick`.
    const ran = this.#x.callMain(argv);
    // Built with JSPI (the single-thread build), `main` is a promise that
    // suspends at every wait and this thread is free between them.
    if (ran instanceof Promise) {
      ran.then(
        () => this.log('[hostx] the X server returned, which it should not'),
        (e) => this.log(`[hostx] the X server failed: ${e?.stack || e}`),
      );
      return;
    }
    this.log('[hostx] the X server returned, which it should not');
  }

  /** One environment variable, as the C side sees it. */
  #setenv(name, value) {
    this.#x.ccall('setenv', 'number', ['string', 'string', 'number'], [name, value, 1]);
    if (this.#x.ENV) this.#x.ENV[name] = value;
  }

  /** What the C side sees for one variable, which is the only thing that
   * counts: emscripten's ENV object is only what the runtime started with. */
  #getenv(name) {
    return this.#x.ccall('getenv', 'string', ['string'], [name]);
  }

  /** The server's arguments, with the descriptors this Worker made. */
  #argv(msg) {
    // Where guest X clients connect. Both spellings, because libxcb tries the
    // abstract name first and falls back to the path. Or the sockets the
    // starter bound already (`listen`, global kernel socket ids), where a
    // client may be waiting.
    const listen = (msg.listen
      ? msg.listen.map((id) => { this.#watch(id); return this.#reserve({ kind: 'listen', id }); })
      : ['\0/tmp/.X11-unix/X0', '/tmp/.X11-unix/X0'].map((name) => this.#listenOn(name)))
      .filter((fd) => fd >= 0);
    if (!listen.length) throw new Error('the X display name is already taken');

    // The Wayland connection and the window manager's, both queues into the
    // module next door. libwayland takes the first from WAYLAND_SOCKET.
    const wl = this.#reserve({ kind: 'wl' });
    // Rootful: no window manager connection. The X screen is one window, and
    // the session in it runs its own window manager (KWin, ...), which draws
    // the frames; the compositor shows the screen fullscreen and frameless.
    const wm = msg.rootful ? -1 : this.#reserve({ kind: 'wm' });
    // Through `setenv` rather than emscripten's ENV object: ENV is what the
    // runtime *starts* with, and by the time this runs the environment has
    // already been materialised for the C side. libwayland reads
    // WAYLAND_SOCKET and connects to that descriptor instead of a name.
    this.#setenv('WAYLAND_SOCKET', String(wl));
    this.#setenv('XDG_RUNTIME_DIR', '/tmp');
    this.#setenv('DISPLAY', msg.display ?? ':0');

    this.log(`[hostx] wayland fd ${wl}, wm fd ${wm}, listen ${listen.join(',')}, `
      + `WAYLAND_SOCKET=${this.#getenv('WAYLAND_SOCKET')}`);
    const argv = [
      msg.display ?? ':0',
      // Rootful: one toplevel for the whole screen that asks to be fullscreen,
      // which the compositor shows frameless over the whole output.
      // The screen is the output's size, so nothing scales it.
      ...(msg.rootful
        ? ['-fullscreen', '-geometry', `${Math.max(1, this.#out?.w | 0)}x${Math.max(1, this.#out?.h | 0)}`]
        : ['-rootless']),
      // No lock file: `link()` on emscripten's in-memory filesystem cannot
      // make one, and there is no second X server in this tab to lock against.
      '-nolock',
      '-noreset',
      // The clients are guest processes in the same sandbox; there is no
      // authority to check and no .Xauthority to check it against.
      '-ac',
      // The keymap is the one compiled into the module (see patch.py): the
      // server compiles keymaps by forking xkbcomp, and there is no fork here.
      '-nokeymap',
      '-shm',
      ...(msg.rootful ? [] : ['-wm', String(wm)]),
    ];
    for (const fd of listen) argv.push('-listenfd', String(fd));
    if (msg.verbose) argv.push('-verbose', String(msg.verbose));
    return argv;
  }

  // -- descriptors ---------------------------------------------------------

  /**
   * A descriptor for one of ours.
   *
   * Really opened, on /dev/null, so the number can never collide with one
   * emscripten's filesystem hands out for a font or the keymap. What it *means*
   * is in `#fds`, and every read and write of it goes through the calls in
   * gexos.h rather than the filesystem.
   */
  #reserve(what) {
    const stream = this.#x.FS.open('/dev/null', 'r+');
    this.#fds.set(stream.fd, what);
    return stream.fd;
  }

  /** Bind a name in the global kernel and return a descriptor for it. */
  #listenOn(name) {
    const id = this.#kSockBind(name);
    if (id < 0) {
      this.log(`[hostx] cannot bind ${JSON.stringify(name)}: errno ${-id}`);
      return -1;
    }
    this.#watch(id);
    return this.#reserve({ kind: 'listen', id });
  }

  // -- waiting -------------------------
  //
  // The server waits as a guest thread does: on a wait slot of its own in the
  // global kernel's memory, registered on the wait queue of every socket it
  // holds and on a queue of its own the compositor notifies for input. A
  // guest's write to one of its sockets notifies it, and nothing else does.

  /** The server's wait slot, its own queue, and the slot's value when the
   *  last wait returned (a notify since then ends the next wait at once). */
  #slot = -1;
  #queue = 0;
  #seq = 0;

  #waitInit() {
    const k = this.#k;
    this.#slot = k.k_ws_alloc();
    if (this.#slot < 0) throw new Error('the host X server found no free wait slot');
    this.#queue = k.k_wq_new();
    k.k_wq_add(this.#queue, this.#slot);
    this.#seq = k.k_ws_seq(this.#slot);
  }

  /** A guest connection's request bytes `got` (fd `fd`), for `#maps`: whole
   *  requests are read, a partial one waits for the rest. */
  #readMaps(fd, got) {
    const c = this.#maps.get(fd);
    if (!c) return;
    const b = new Uint8Array(c.buf.length + got.length);
    b.set(c.buf);
    b.set(got, c.buf.length);
    const v = new DataView(b.buffer);
    let at = 0;
    for (;;) {
      const left = b.length - at;
      if (c.setup) {
        if (left < 12) break;
        c.le = b[at] === 0x6c;
        const n = v.getUint16(at + 6, c.le), d = v.getUint16(at + 8, c.le);
        const size = 12 + ((n + 3) & ~3) + ((d + 3) & ~3);
        if (left < size) break;
        at += size;
        c.setup = false;
        continue;
      }
      if (left < 4) break;
      let words = v.getUint16(at + 2, c.le), head = 4;
      if (words === 0) {
        // BIG-REQUESTS: the length is the next word.
        if (left < 8) break;
        words = v.getUint32(at + 4, c.le);
        head = 8;
      }
      const size = words * 4;
      if (size < head || left < size) break;
      const op = b[at];
      if (op === 18 && size >= head + 20) {
        // ChangeProperty of WM_CLASS (atom 67): the instance name.
        const win = v.getUint32(at + head, c.le), prop = v.getUint32(at + head + 4, c.le);
        const units = v.getUint32(at + head + 16, c.le), fmt = b[at + head + 12];
        if (prop === 67 && fmt === 8) {
          const data = b.subarray(at + head + 20, Math.min(at + size, at + head + 20 + units));
          const nul = data.indexOf(0);
          c.named.set(win, new TextDecoder().decode(nul < 0 ? data : data.subarray(0, nul)));
        }
      } else if (op === 8 && size >= head + 4) {
        // MapWindow
        const win = v.getUint32(at + head, c.le);
        if (c.named.has(win) && !c.mapped.has(win)) {
          c.mapped.add(win);
          this.emit({ kind: 'win', text: `map 0x${win.toString(16)} "${c.named.get(win)}"` });
        }
      }
      at += size;
    }
    c.buf = b.slice(at);
  }

  /** Register on kernel socket `id`'s queue, for as long as it is held. */
  #watch(id) {
    this.#k.k_wq_add(this.#k.k_sock_queue(id), this.#slot);
  }

  #unwatch(id) {
    this.#k.k_wq_del(this.#k.k_sock_queue(id), this.#slot);
  }

  /** `k_sock_bind` wants the name in the process's memory. */
  #kSockBind(name) {
    const bytes = new TextEncoder().encode(name);
    const p = this.#x._malloc(bytes.length);
    this.#x.HEAPU8.set(bytes, p);
    const id = this.#k.k_sock_bind(BigInt(p), bytes.length);
    this.#x._free(p);
    return id;
  }

  // -- the compositor's memory ---------------------------------------------

  #hwView() {
    if (!this.#hwBytes || this.#hwBytes.buffer !== this.#hwMem.buffer) {
      this.#hwBytes = new Uint8Array(this.#hwMem.buffer);
      this.#hwWords = new Int32Array(this.#hwMem.buffer);
    }
    return this.#hwBytes;
  }

  #hwText(ptr, len) {
    return new TextDecoder().decode(this.#hwView().subarray(ptr, ptr + len));
  }

  /** Where the pool this descriptor names lives, for the compositor. */
  #locate(fd, out) {
    const pool = this.#held.get(fd);
    this.#hwView();
    const w = this.#hwWords;
    if (!pool) {
      // Worth saying loudly: a pool the compositor cannot place is a window
      // whose pixels never reach the screen, and everything else about it looks
      // healthy -- the surface is there, the commits arrive, the scene is
      // published with an address of zero.
      this.#once(`pool:${fd}`, `[hostx] descriptor ${fd} is not a pool`
        + ` (held: ${this.#held.size})`);
      w[out >> 2] = 0;
      w[(out >> 2) + 1] = 0;
      return 0;
    }
    w[out >> 2] = pool.addr;
    w[(out >> 2) + 1] = pool.size;
    return 1;
  }

  /** One reference to `pool` fewer; with none left, free it once it is safe. */
  #drop(pool) {
    if (--pool.refs > 0) return;
    if (this.#mapped.get(pool.addr) === pool) this.#mapped.delete(pool.addr);
    this.#dying.push({ pool, gen: this.#proxy?.ok ? this.#proxy.generation : -1 });
  }

  /** Free the pools that died before a scene the page has drawn since. */
  #reap() {
    if (!this.#dying.length) return;
    const drawn = this.#proxy?.ok ? this.#proxy.drawnGeneration : Infinity;
    const keep = [];
    for (const d of this.#dying) {
      if (drawn > d.gen) {
        this.#x._free(d.pool.addr);
        this.#live--;
        if (this.#count) this.#bump('freed', d.pool.size);
      } else {
        keep.push(d);
      }
    }
    this.#dying = keep;
  }

  #once(key, text) {
    if (this.#said.has(key)) return;
    this.#said.add(key);
    this.log(text);
  }

  // -- what the server calls ----------------------------------------------

  #gexos() {
    const self = this;
    const raw = {
      socket: () => self.#reserve({ kind: 'idle' }),

      bind(fd, path, len) {
        const name = new TextDecoder().decode(self.#x.HEAPU8.subarray(path, path + len));
        const id = self.#kSockBind(name);
        if (id < 0) return id;
        self.#watch(id);
        self.#fds.set(fd, { kind: 'listen', id });
        return 0;
      },

      connect(fd, path, len) {
        const id = self.#k.k_sock_connect(BigInt(path), len);
        if (id < 0) return id;
        self.#watch(id);
        self.#fds.set(fd, { kind: 'kernel', id });
        return 0;
      },

      accept(fd) {
        const ep = self.#fds.get(fd);
        if (!ep || ep.kind !== 'listen') return -EBADF;
        const id = self.#k.k_sock_accept(ep.id);
        if (id < 0) return id;
        self.#watch(id);
        const client = self.#reserve({ kind: 'kernel', id });
        self.log(`[hostx] a guest connected to the X display (fd ${client})`);
        self.#maps?.set(client, { setup: true, big: false, buf: new Uint8Array(0), named: new Map(), mapped: new Set() });
        return client;
      },

      close(fd) {
        const ep = self.#fds.get(fd);
        if (!ep) return 0;
        if (ep.kind === 'kernel' || ep.kind === 'listen') {
          self.#unwatch(ep.id);
          self.#k.k_sock_close(ep.id);
        }
        // A pool's descriptor is closed as soon as the server has handed it to
        // wl_shm, and the pixels have to outlive that: the server draws
        // through its mapping and the compositor is still reading them. So
        // closing it is one reference fewer, not a free (see `#pools`).
        self.#fds.delete(fd);
        self.#maps?.delete(fd);
        const pool = self.#pools.get(fd);
        if (pool) {
          self.#pools.delete(fd);
          self.#drop(pool);
        }
        try {
          self.#x.FS.close(self.#x.FS.getStream(fd));
        } catch {
          // Already gone: nothing to do.
        }
        return 0;
      },

      readable(fd) {
        const ep = self.#fds.get(fd);
        if (self.#trace) {
          // Which descriptors the server actually waits on, once each: a
          // listener the server never polls is a display nothing can connect
          // to, and it looks exactly like a listener nobody connected to.
          self.#once(`poll:${fd}`, `[hostx] the server polls fd ${fd} (${ep?.kind ?? 'unknown'})`);
        }
        if (!ep) return -1;
        switch (ep.kind) {
        case 'listen':
          return self.#k.k_sock_pending(ep.id) ? 1 : 0;
        case 'kernel':
          return self.#k.k_sock_readable(ep.id) ? 1 : 0;
        case 'wl':
          return self.#hw.hw_wayland_pending() > 0 ? 1 : 0;
        case 'wm':
          return self.#hw.hw_x_pending() > 0 ? 1 : 0;
        default:
          return 0;
        }
      },

      /** Room to write: a guest's socket in the kernel fills (256 KiB); the
       *  compositor's queues never do. */
      writable(fd) {
        const ep = self.#fds.get(fd);
        if (!ep) return -1;
        if (ep.kind === 'kernel') return self.#k.k_sock_writable(ep.id) ? 1 : 0;
        return 1;
      },

      send(fd, buf, len, fds, nfds) {
        const ep = self.#fds.get(fd);
        if (!ep) return -EBADF;
        if (ep.kind === 'kernel') {
          const r = self.#k.k_sock_write(ep.id, BigInt(buf), len);
          if (self.#count && r > 0) self.#bump(`out${fd}`, r);
          return r;
        }
        if (self.#count) self.#bump(ep.kind, len);
        if (ep.kind === 'wl' || ep.kind === 'wm') {
          // Into the compositor module's memory, which is a different address
          // space: a protocol message, never a frame.
          const pad = (len + 3) & ~3;
          const p = self.#hw.hw_scratch(pad + nfds * 4);
          self.#hwView().set(self.#x.HEAPU8.subarray(buf, buf + len), p);
          for (let i = 0; i < nfds; i++) {
            // The compositor's own reference to the pool (see `#held`), not
            // the server's descriptor number, which the server is about to
            // close and reuse.
            const sent = self.#x.HEAP32[(fds >> 2) + i];
            const pool = ep.kind === 'wl' ? self.#pools.get(sent) : null;
            let word = sent;
            if (pool) {
              word = self.#nextHeld++;
              self.#held.set(word, pool);
              pool.refs++;
            }
            self.#hwWords[((p + pad) >> 2) + i] = word;
          }
          if (nfds && self.#trace) {
            const got = [];
            for (let i = 0; i < nfds; i++) got.push(self.#hwWords[((p + pad) >> 2) + i]);
            self.#once(`fds:${got.join(',')}`,
              `[hostx] the server sent descriptor(s) ${got.join(',')} with ${len} bytes`);
          }
          if (ep.kind === 'wl') self.#hw.hw_wayland_receive(p, len, p + pad, nfds);
          else self.#hw.hw_x_receive(p, len);
          return len;
        }
        self.#once(`send:${ep.kind}`, `[hostx] a write to a ${ep.kind} descriptor went nowhere`);
        return len;
      },

      recv(fd, buf, len, fds, maxfds, nfds) {
        const ep = self.#fds.get(fd);
        if (!ep) return -EBADF;
        if (nfds) self.#x.HEAP32[nfds >> 2] = 0;
        if (ep.kind === 'kernel') {
          const n = self.#k.k_sock_read(ep.id, BigInt(buf), len);
          if (self.#count && n > 0) self.#bump(`in${fd}`, n);
          if (self.#maps && n > 0) self.#readMaps(fd, self.#x.HEAPU8.subarray(buf, buf + n));
          return n;
        }
        if (ep.kind === 'wl' || ep.kind === 'wm') {
          const p = self.#hw.hw_scratch(len);
          const n = ep.kind === 'wl'
            ? self.#hw.hw_wayland_take(p, len)
            : self.#hw.hw_x_take(p, len);
          if (!n) return -EAGAIN;
          self.#x.HEAPU8.set(self.#hwView().subarray(p, p + n), buf);
          // Any descriptor the compositor owes rides along: libwayland queues
          // them and takes one when a message's `h` argument wants it.
          if (ep.kind === 'wl' && self.#owed.length && nfds && maxfds > 0) {
            const give = self.#owed.splice(0, maxfds);
            for (let i = 0; i < give.length; i++) self.#x.HEAP32[(fds >> 2) + i] = give[i];
            self.#x.HEAP32[nfds >> 2] = give.length;
          }
          return n;
        }
        return -EAGAIN;
      },

      /** A pool of pixels the compositor will read where it lies. */
      shm(addr, size) {
        const fd = self.#reserve({ kind: 'pool' });
        self.#pools.set(fd, { addr, size, refs: 1, maps: 0 });
        self.#live++;
        return fd;
      },

      /** `mmap` of a pool: its address, and a reference until `shmUnmap`. */
      shmAddr(fd) {
        const pool = self.#pools.get(fd);
        if (!pool) return 0;
        pool.refs++;
        pool.maps++;
        self.#mapped.set(pool.addr, pool);
        return pool.addr;
      },

      /** `munmap` of a pool the server mapped. */
      shmUnmap(addr) {
        const pool = self.#mapped.get(addr);
        if (!pool || !pool.maps) return 0;
        if (--pool.maps === 0) self.#mapped.delete(addr);
        self.#drop(pool);
        return 0;
      },

      /**
       * Note that this catches: a throw here unwinds the server's whole stack
       * (it is a JavaScript exception crossing wasm frames) and the run ends
       * with no explanation from either side. One report, then keep going.
       */
      tick() {
        try {
          self.#tick();
        } catch (e) {
          self.#once('tick', `[hostx] the compositor turn failed: ${e?.stack || e}`);
        }
      },

      /**
       * Wait, the way a guest process waits: on its wait slot, until one of
       * its sockets (or its compositor input) is notified, or `ms` -- the
       * server's own slice (gexos.c `gex_poll`). A Worker may block, which is
       * what makes this possible at all -- and what makes the X server's loop
       * look exactly like a native one.
       */
      wait(ms) {
        const k = self.#k;
        const slot = self.#slot;
        const seq = self.#seq;
        const ns = BigInt(Math.max(1, ms)) * 1000000n;
        const done = () => {
          self.#seq = k.k_ws_seq(slot);
          return 0;
        };
        // The single-thread build: the server is a
        // fiber of the system Worker, so it parks on the scheduler, and the
        // wait is a promise its stack suspends on (gexos.c GEX_XWL_JSPI).
        if ((SP_CODE && single.on)) {
          const words = () => new Int32Array(self.#kmem.buffer);
          const r = sched.parkSlot(words, (k.k_ws_addr(slot) >>> 0) >>> 2, seq, ns);
          return r === 1 ? done() : r.then(done);
        }
        k.k_ws_park(slot, seq, ns);
        return done();
      },
    };
    // And what it does to a guest's socket notifies that socket's waiters,
    // whose words the guests' fibers do not `Atomics.wait` on in the
    // single-thread build: they look.
    if ((SP_CODE && single.on)) {
      for (const name of ['connect', 'accept', 'close', 'send', 'recv']) {
        const fn = raw[name];
        raw[name] = (...a) => {
          const r = fn(...a);
          sched.checkSlots();
          return r;
        };
      }
    }
    if (!this.#trace) return raw;
    // `?hostxtrace=1`: narrate the transport. A server that gives up without
    // saying why has usually asked for one thing and been told another, and
    // this is the only place both halves are visible.
    const wrapped = {};
    let left = this.#trace;
    for (const [name, fn] of Object.entries(raw)) {
      if (name === 'tick' || name === 'wait' || name === 'readable' || name === 'writable') {
        wrapped[name] = fn;
        continue;
      }
      wrapped[name] = (...a) => {
        const r = fn(...a);
        if (left-- > 0) this.log(`[hostx] ${name}(${a.join(', ')}) = ${r}`);
        return r;
      };
    }
    return wrapped;
  }

  /**
   * A keymap the compositor has queued an event for: into a pool of the
   * server's own memory, with its descriptor owed on the next read.
   */
  #takeKeymap() {
    const max = 1 << 16;
    const p = this.#hw.hw_scratch(max);
    const n = this.#hw.hw_keymap_take(p, max);
    if (!n) return;
    const addr = this.#x._malloc(n);
    this.#x.HEAPU8.set(this.#hwView().subarray(p, p + n), addr);
    const fd = this.#reserve({ kind: 'pool' });
    // Never freed: one per keymap, and the server does not map it through
    // `shmAddr`, so nothing would count its last reader.
    this.#pools.set(fd, { addr, size: n, refs: Infinity, maps: 0 });
    this.#owed.push(fd);
    this.log(`[hostx] keymap event: ${n} bytes as fd ${fd}`);
  }

  /** `?hostxcount=1`: `n` more bytes under `key` this second. */
  #bump(key, n) {
    const m = this.#count.n;
    m.set(key, (m.get(key) ?? 0) + n);
  }

  /** The clipboard's number this server last saw or set. */
  #clipSeen = 0;

  /**
   * The clipboard between the page and X programs, through the window
   * manager: an X program's copy onto the page's clipboard, and the page's
   * new text to the manager, which owns the X clipboard with it.
   */
  #clip() {
    const hw = this.#hw;
    const px = this.#proxy;
    if (!hw.hw_clip_take || !px) return;
    let p = hw.hw_scratch(X_CLIP);
    const n = hw.hw_clip_take(p, X_CLIP);
    if (n >= 0) this.#clipSeen = px.clipSet(this.#hwMem, p, n);
    const serial = px.clipSerial();
    if (serial && serial !== this.#clipSeen) {
      this.#clipSeen = serial;
      p = hw.hw_scratch(X_CLIP);
      const len = px.clipGet(this.#hwMem, p, X_CLIP);
      hw.hw_clip_set(p, Math.min(len, X_CLIP));
    }
  }

  /**
   * Between two turns of the server's loop: run the window manager, take the
   * page's input, publish the scene.
   */
  #tick() {
    if (this.#count) {
      const c = this.#count;
      c.polls++;
      if (Date.now() - c.at >= 1000) {
        const parts = [...c.n].map(([k, v]) => `${k}=${v}`).join(' ');
        this.log(`[hostx-count] turns=${c.polls} pub=${this.#published} pools=${this.#live} ${parts}`);
        c.n.clear();
        c.polls = 0;
        c.at = Date.now();
      }
    }
    // A hook for a harness: this is the only turn anything else gets once the
    // server's `main` has the thread.
    globalThis.GEX_HOSTX_TICK?.();
    const hw = this.#hw;
    this.#reap();
    hw.hw_tick();
    this.#takeKeymap();

    // The output may have been resized under us; the arena carries the size.
    const size = outputSize(this.#channel);
    if (size.w !== this.#out.w || size.h !== this.#out.h) {
      this.#out = { w: size.w, h: size.h };
      hw.hw_set_output(size.w, size.h);
    }

    this.#clip();

    if (hw.hw_wants_input()) {
      const p = hw.hw_scratch(MAX_INPUT * EV * 4);
      const n = this.#proxy.pollInput(this.#hwMem, p, MAX_INPUT);
      if (n) hw.hw_input(p, n);
    }

    if (hw.hw_dirty()) {
      const p = hw.hw_scratch(SLOTS * REC * 4);
      const n = hw.hw_scene(p, SLOTS);
      // The scene is in the Wayland server's memory, the windows' pixels in
      // the X server's.
      this.#proxy.publish(this.#hwMem, p, n, this.#x.wasmMemory);
      // The first few, then at 10, 100, 1000...: a scene that never leaves
      // here and a scene the page never draws look the same from the page, and
      // the interesting ones are at the start (a window appearing). A game
      // publishes forever, and one line in a hundred was a line every three
      // seconds in its terminal.
      const k = this.#published++;
      if (k < 5 || (k >= 10 && /^10*$/.test(String(k)))) {
        this.log(`[hostx] published ${n} surface(s) as token ${this.#proxy.token}`
          + `${this.#proxy.ok ? '' : ' (no block in the arena!)'}`);
      }
    }

    const why = hw.hw_failed(hw.hw_scratch(256), 256);
    if (why) {
      this.#once('failed', `[hostx] the compositor dropped the X server: ${
        this.#hwText(hw.hw_scratch(256), why)}`);
    }
  }
}

