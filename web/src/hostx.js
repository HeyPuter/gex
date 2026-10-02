// A host-side X server: Xwayland as WebAssembly, and the compositor beside it.
//
// This is the machinery; `xwayland-worker.js` is the Worker that runs it in the
// page and `web/test/hostx.mjs` is the same thing under Node, which is where it
// is easiest to see the server come up. Both give it the same three things: the
// two module URLs, the global kernel, and the page's scene arena.
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
// `web/xwayland/gexos.c`), which is a `SharedArrayBuffer` because the module is
// built `-pthread`. So the page's compositor Worker uploads an X window to a
// texture straight out of the X server's heap: no copy on the way to the
// screen, exactly like a guest's own Wayland surface.
//
// The server blocks. `main()` never returns, and every wait goes through
// `gex_poll` -> `wait()` here -> `k_park`, the same bounded park a guest
// process uses. That is why the housekeeping -- pumping the window manager,
// publishing the scene, taking input -- happens inside `tick()`, which the
// server calls every turn of its loop: after `callMain` there is no JavaScript
// turn left to do it in.

import { WlProxy, outputSize, REC, EV, SLOTS } from '../../shims/browser/js/wl-bridge.mjs';

/** Events taken from the page in one turn. */
const MAX_INPUT = 64;
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
 * before the server has run a line). The specifier is a variable and marked
 * for the bundler to leave alone, so nothing tries to bundle 250 KB of glue.
 */
async function loadGlue(url) {
  return (await import(/* @vite-ignore */ url)).default;
}

/** What a relative module URL is relative to, in a page or under Node. */
function base() {
  if (typeof location !== 'undefined') return location.href;
  return new URL(`file://${process.cwd()}/`).href;
}

/** The bytes of a module, from a URL or a path. */
async function moduleBytes(url) {
  if (/^https?:/.test(url)) return (await fetch(url)).arrayBuffer();
  const { readFile } = await import('node:fs/promises');
  const { fileURLToPath } = await import('node:url');
  return (await readFile(url.startsWith('file:') ? fileURLToPath(url) : url)).buffer;
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
  /** What each descriptor means. */
  #fds = new Map();
  /** Pools of pixels, by descriptor: where they are in the server's memory. */
  #pools = new Map();
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

  async start(msg) {
    this.#channel = msg.wlChannel;
    this.#trace = msg.trace | 0;
    this.#token = msg.token;
    const size = outputSize(msg.wlChannel);
    this.#out = { w: size.w, h: size.h };

    // -- the X server ------------------------------------------------------
    const Module = await loadGlue(msg.xwaylandUrl);
    this.#x = await Module({
      // Where the server's own wasm is, said rather than inferred: the glue
      // works it out from `import.meta.url`, which is a blob when it is loaded
      // the way `loadGlue` has to load it in a page.
      locateFile: (f) => new URL(f, new URL(msg.xwaylandUrl, base())).href,
      // The server explains itself on stderr, and in a Worker that is the only
      // diagnostic there is.
      print: (s) => this.log(s),
      printErr: (s) => this.log(s),
      noInitialRun: true,
    });

    // The keymap the server will load. It cannot compile one -- that means
    // forking xkbcomp -- so `web/build-xwayland-wasm.sh` compiles it with the
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
        env: { memory: msg.kernel.memory, process: this.#x.wasmMemory },
      });
      // Every Worker in the kernel needs its own stack slice: wasm-ld gives
      // each instance the same `__stack_pointer`, so two Workers sharing one
      // would overwrite each other's locals mid-syscall. The page hands slices
      // out from the bottom (see host.mjs) and this takes the top one, which is
      // only contended by a page running 126 guest processes at once.
      const top = msg.kernel.stackTop ?? (inst.exports.k_stack_area
        ? (inst.exports.k_stack_area() >>> 0)
          + (inst.exports.k_stack_size() >>> 0) * ((inst.exports.k_stack_count() >>> 0) - 1)
        : 0);
      if (top) inst.exports.__stack_pointer.value = top | 0;
      this.#k = inst.exports;
    }
    if (!this.#k) throw new Error('the host X server needs the global kernel');

    // -- the arena ---------------------------------------------------------
    this.#proxy = new WlProxy(msg.wlChannel, () => this.emit({ kind: 'wl' }), () => this.#token);
    // The compositor Worker reads an X window's pixels out of this memory, so
    // it needs the memory itself -- the object, not a buffer, since a shared
    // memory that grows hands out a new one.
    this.emit({ kind: 'wl-memory', token: this.#token, memory: this.#x.wasmMemory });

    // -- the transport the server will use ---------------------------------
    this.#x.gexos = this.#gexos();

    const argv = this.#argv(msg);
    this.log(`[hostx] Xwayland ${argv.join(' ')}`);
    this.emit({ kind: 'ready' });
    // And from here the X server owns this thread: `main` does not return, and
    // everything else happens inside `tick`.
    this.#x.callMain(argv);
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
    // abstract name first and falls back to the path.
    const listen = ['\0/tmp/.X11-unix/X0', '/tmp/.X11-unix/X0']
      .map((name) => this.#listenOn(name))
      .filter((fd) => fd >= 0);
    if (!listen.length) throw new Error('the X display name is already taken');

    // The Wayland connection and the window manager's, both queues into the
    // module next door. libwayland takes the first from WAYLAND_SOCKET.
    const wl = this.#reserve({ kind: 'wl' });
    const wm = this.#reserve({ kind: 'wm' });
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
      '-rootless',
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
      '-wm', String(wm),
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
    return this.#reserve({ kind: 'listen', id });
  }

  /** `k_sock_bind` wants the name in the process's memory. */
  #kSockBind(name) {
    const bytes = new TextEncoder().encode(name);
    const p = this.#x._malloc(bytes.length);
    this.#x.HEAPU8.set(bytes, p);
    const id = this.#k.k_sock_bind(p, bytes.length);
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
    const pool = this.#pools.get(fd);
    this.#hwView();
    const w = this.#hwWords;
    if (!pool) {
      // Worth saying loudly: a pool the compositor cannot place is a window
      // whose pixels never reach the screen, and everything else about it looks
      // healthy -- the surface is there, the commits arrive, the scene is
      // published with an address of zero.
      this.#once(`pool:${fd}`, `[hostx] descriptor ${fd} is not a pool`
        + ` (pools: ${[...this.#pools.keys()].join(',') || 'none'})`);
      w[out >> 2] = 0;
      w[(out >> 2) + 1] = 0;
      return 0;
    }
    w[out >> 2] = pool.addr;
    w[(out >> 2) + 1] = pool.size;
    return 1;
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
        self.#fds.set(fd, { kind: 'listen', id });
        return 0;
      },

      connect(fd, path, len) {
        const id = self.#k.k_sock_connect(path, len);
        if (id < 0) return id;
        self.#fds.set(fd, { kind: 'kernel', id });
        return 0;
      },

      accept(fd) {
        const ep = self.#fds.get(fd);
        if (!ep || ep.kind !== 'listen') return -EBADF;
        const id = self.#k.k_sock_accept(ep.id);
        if (id < 0) return id;
        const client = self.#reserve({ kind: 'kernel', id });
        self.log(`[hostx] a guest connected to the X display (fd ${client})`);
        return client;
      },

      close(fd) {
        const ep = self.#fds.get(fd);
        if (!ep) return 0;
        if (ep.kind === 'kernel' || ep.kind === 'listen') self.#k.k_sock_close(ep.id);
        // A pool's descriptor is closed as soon as the server has handed it to
        // wl_shm, and the pixels have to outlive that: the compositor is still
        // reading them. So the allocation stays -- freeing it here is the one
        // way to make an X window's contents vanish mid-frame.
        self.#fds.delete(fd);
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

      send(fd, buf, len, fds, nfds) {
        const ep = self.#fds.get(fd);
        if (!ep) return -EBADF;
        if (ep.kind === 'kernel') return self.#k.k_sock_write(ep.id, buf, len);
        if (ep.kind === 'wl' || ep.kind === 'wm') {
          // Into the compositor module's memory, which is a different address
          // space: a protocol message, never a frame.
          const pad = (len + 3) & ~3;
          const p = self.#hw.hw_scratch(pad + nfds * 4);
          self.#hwView().set(self.#x.HEAPU8.subarray(buf, buf + len), p);
          for (let i = 0; i < nfds; i++) {
            self.#hwWords[((p + pad) >> 2) + i] = self.#x.HEAP32[(fds >> 2) + i];
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
          const n = self.#k.k_sock_read(ep.id, buf, len);
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
        self.#pools.set(fd, { addr, size });
        return fd;
      },

      shmAddr(fd) {
        return self.#pools.get(fd)?.addr ?? 0;
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
       * Wait, the way a guest process waits: on the global kernel's wake word.
       * A Worker may block, which is what makes this possible at all -- and
       * what makes the X server's loop look exactly like a native one.
       */
      wait(ms) {
        const seq = self.#k.k_wake_seq();
        self.#k.k_park(seq, BigInt(Math.max(1, ms)) * 1000000n);
        return 0;
      },
    };
    if (!this.#trace) return raw;
    // `?hostxtrace=1`: narrate the transport. A server that gives up without
    // saying why has usually asked for one thing and been told another, and
    // this is the only place both halves are visible.
    const wrapped = {};
    let left = this.#trace;
    for (const [name, fn] of Object.entries(raw)) {
      if (name === 'tick' || name === 'wait' || name === 'readable') {
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
    this.#pools.set(fd, { addr, size: n });
    this.#owed.push(fd);
    this.log(`[hostx] keymap event: ${n} bytes as fd ${fd}`);
  }

  /**
   * Between two turns of the server's loop: run the window manager, take the
   * page's input, publish the scene.
   */
  #tick() {
    // A hook for a harness: this is the only turn anything else gets once the
    // server's `main` has the thread. See web/test/hostx.mjs.
    globalThis.GEX_HOSTX_TICK?.();
    const hw = this.#hw;
    hw.hw_tick();
    this.#takeKeymap();

    // The output may have been resized under us; the arena carries the size.
    const size = outputSize(this.#channel);
    if (size.w !== this.#out.w || size.h !== this.#out.h) {
      this.#out = { w: size.w, h: size.h };
      hw.hw_set_output(size.w, size.h);
    }

    if (hw.hw_wants_input()) {
      const p = hw.hw_scratch(MAX_INPUT * EV * 4);
      const n = this.#proxy.pollInput(this.#hwMem, p, MAX_INPUT);
      if (n) hw.hw_input(p, n);
    }

    if (hw.hw_dirty()) {
      const p = hw.hw_scratch(SLOTS * REC * 4);
      const n = hw.hw_scene(p, SLOTS);
      this.#proxy.publish(this.#hwMem, p, n);
      // The first publish, and every hundredth: a scene that never leaves here
      // and a scene the page never draws look the same from the page.
      // The first few always, then one in a hundred: the interesting ones are
      // at the start (a window appearing) and a busy client publishes forever.
      if (this.#published++ < 5 || this.#published % 100 === 0) {
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

