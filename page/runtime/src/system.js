// `System`: the kernel, started with `boot()`, and the programs a page runs
// in it. Everything else a system has -- its files, windows, network,
// sound, GL -- is a class of its own that takes the system as
// its first argument (page/runtime/README.md).
//
// Built from the page runtime's pieces (page/src/runtime.js: the kernel, the
// processes' host, the devices), with no program of its own for pid 1
// unless `boot` is given one. Each program
// `spawn` asks for starts in a Worker of its own from an exec record
// (host.mjs `spawnExec`). The page reaches the kernel through its exports
// (kernel.js, over a kernel-call Worker): the programs' terminals and pipes
// (programs.js), the guest's files (kfs.js).

import { INTERNAL, bytesOf, envList, signalNumber } from './internal.js';
import { Process } from './process.js';
import { NetSwitch } from './netswitch.js';
import { Sockets, dialFor } from '../../src/net.js';
import { GuestConnection, UnixListener } from './connection.js';
import { PageFsHost } from './mount.js';
import { Kernel, errno } from './kernel.js';
import { Files } from './kfs.js';
import { Programs } from './programs.js';
import files from './runtime-files.js';
import { SP_CODE, MP_CODE } from '../../../hosts/browser/js/mode.mjs';
import { serveSlots, claimStore, storeBusy } from '../../../hosts/browser/js/blocks.mjs';

/** A unix socket name as `what` (listen, connect) takes it: an absolute
 *  path, made plain (`.`, `..` and repeated slashes resolved, as the
 *  kernel names a program's), or an abstract name (a leading NUL). At most
 *  107 bytes, what a program's `sockaddr_un` holds. */
function unixPath(what, path) {
  if (typeof path !== 'string' || !(path.startsWith('/') || path.startsWith('\0'))) throw new TypeError(`${what}: an absolute path, or an abstract name starting with \\0`);
  let p = path;
  if (path.startsWith('/')) {
    const parts = [];
    for (const c of path.split('/')) {
      if (c === '..') parts.pop();
      else if (c && c !== '.') parts.push(c);
    }
    p = `/${parts.join('/')}`;
    if (p === '/') throw new TypeError(`${what}: ${path} is the root`);
  }
  if (new TextEncoder().encode(p).length > 107) throw new TypeError(`${what}: ${path} is longer than a socket name can be (107 bytes)`);
  return p;
}

/** Whether this system is the single-thread build's (@gexemu/runtime-single:
 *  the kernel and every process in one Worker, kernel-single-worker.js): a
 *  package is one build or the other, and the source tree runs it where the
 *  page has no SharedArrayBuffer. */
const SINGLE = !MP_CODE || (SP_CODE && typeof SharedArrayBuffer !== 'function');

/** Where the runtime's files (gex/...) are: the tree's page/public in place.
 *  The package (page/runtime/build.sh defines `__GEX_RUNTIME_ASSETS__` null)
 *  lists its own files (`files`), and anything else is the page's site
 *  root's: no directory of the package's, which a bundler would take for a
 *  module to resolve. */
const ASSETS = typeof __GEX_RUNTIME_ASSETS__ === 'undefined' ? '../../public/' : __GEX_RUNTIME_ASSETS__;
const DEFAULT_BASE = () => (ASSETS === null ? new URL('/', globalThis.location?.href).href : new URL(ASSETS, import.meta.url).href);

/** Whether this page has made its system. */
let made = false;

/** The guest architectures a page may name (`arch`), as `describe()` says them. */
const ARCHES = { x86: 'x86', aarch64: 'aarch64' };

/** ELF's machine numbers, as the guest's programs are built for them. */
const EM = { 62: 'x86', 183: 'aarch64' };

/** The machine of the 64-bit ELF whose bytes start at `at` in `b`, or null
 *  (a gexcc program, wasm32, runs on either and says nothing). */
function machine(b, at = 0) {
  return b && b.length >= at + 20 && b[at] === 0x7f && b[at + 1] === 0x45 && b[at + 2] === 0x4c && b[at + 3] === 0x46 && b[at + 4] === 2
    ? EM[b[at + 18] | (b[at + 19] << 8)] ?? null
    : null;
}

/**
 * The guest architecture a tar's programs are built for: the machine of its
 * first native (non-wasm32) ELF, read from the headers up to that entry,
 * with no file's bytes but an ELF header read. Null for none: a wasm32 tree
 * whose native packages are still to come.
 */
function tarArch(tarEntries, b) {
  let found = null;
  tarEntries(b, ({ type, body }) => {
    if (type === '0' || type === '\0' || type === '7') found = machine(b, body);
    return !!found;
  });
  return found;
}

/** [`tarArch`] of single files (`{ path: bytes }`), in the order given. */
function filesArch(files = {}) {
  for (const d of Object.values(files)) if (machine(d)) return machine(d);
  return null;
}

/** The guest architecture of the layers laid at the root (fs.js), the last
 *  first, each one's read only as far as it takes. */
async function layersArch(layers) {
  const { tarEntries } = await import('../../src/tar.js');
  for (const l of [...layers].reverse()) {
    if (l.at && l.at !== '/') continue;
    const m = l.tar ? tarArch(tarEntries, l.tar) : filesArch(l.files);
    if (m) return m;
  }
  return null;
}

/** `spawn`'s and `boot`'s arguments, checked: `[argv, opts]`. One parser,
 *  so the two take the same. */
function programArgs(what, argv, opts = {}) {
  if (!Array.isArray(argv) || !argv.length || argv.some((a) => typeof a !== 'string')) {
    throw new TypeError(`${what}: argv is a non-empty array of strings`);
  }
  if (typeof opts !== 'object' || opts === null) throw new TypeError(`${what}: options is an object`);
  for (const k of Object.keys(opts)) {
    if (!['pty', 'env', 'cwd', 'uid', 'gid', 'detached'].includes(k)) throw new TypeError(`${what}: no option ${k}`);
  }
  return [argv, opts];
}

/** The Cache Storage that keeps the lazy files a system read (boot's owner
 *  of them, page/src/lazy-cache-worker.js), by URL and version. */
const LAZY_CACHE = 'gex-runtime-lazy-v1';

export class System extends EventTarget {
  #opts;
  #state = { phase: 'new', error: null, progress: null, console: '' };
  /** The bundle failed while the system ran (`#watchRest`). */
  #restFailed = false;
  #parts = new Map();
  #beforeBoot = [];
  #afterBoot = [];
  /** The kernel from the page (kernel.js), and what works through it. */
  #kernel = null;
  #files = null;
  #programs = null;
  /** The running system: { host, kernel, term, rt, compositor, wlChannel,
   *  spawnExec, onStop, stop }. */
  #sys = null;
  #devices = [];
  #procs = new Map(); // handle -> Process
  #byPid = new Map(); // pid -> Process
  #early = new Map(); // pid -> exit status heard before the pid was known
  /** Whether the Workers read a rest bundle (boot's layers had one). */
  #restBundle = false;
  /** The filesystem keeps its contents in a store (fs.js): sync has work. */
  #stored = false;
  /** This system's claim on its OPFS store, and the store Worker holding
   *  its handle (the default build). */
  #claim = null;
  #storeWorker = null;
  #nextHandle = 1;
  #booting = null;
  #arch = null;
  #net = new NetSwitch();
  #network = null;
  /** The guest's sockets (page/src/net.js), made with the first network. */
  #sockets = null;
  /** The single-thread build's system Worker (kernel.js `startSingle`). */
  #earlyWorker = null;
  /** And its kernel. */
  #singleKernel = null;
  #hostCalls = new Map();
  /** The page's unix socket listeners (`listen`): `{ path, l, bind,
   *  binding, closed }`, `l` the kernel's listener once bound. */
  #unix = new Set();
  /** Whether boot has bound the listeners made before it: one made after
   *  is bound at once. */
  #unixUp = false;
  #pageFs = null;
  /** The init (`boot(argv, opts)`), `[argv, opts]`, and its Process once
   *  it runs. */
  #init = null;
  #initProcess = null;
  /** What a preset over the system adds that the API does not offer
   *  (page/src/run-program.js): the processes' host's options (an ejected
   *  build's units, fill and lazy cache, a closure), each exec's (the
   *  recompile and link switches), the Workers' messages, and whether the
   *  runtime keeps its routine lines. */
  #preset = { host: {}, exec: {}, onMessage: [], loud: false };

  /**
   * `baseUrl`: where the runtime's files are (the shim, kernel, seed,
   * ...); default beside the package. `units`: a directory of compiled
   * function-tier units (`gexc cgen-image`'s cache), each used for the image
   * it was built for where one is mapped at its base; `{ url, require }`
   * with `require` makes a program image without one fail. `xcache`: keep
   * the translated code across loads (in the origin's private storage, a
   * directory under Node: GEX_XCACHE_DIR); a string names the cache, kept
   * apart from other names' on the same page. `debug`: the runtime's switches,
   * `{name: value}` as gex's test pages take `?name=value`; unstable, for
   * debugging only.
   */
  constructor({ baseUrl, units, xcache = false, arch, debug = {} } = {}) {
    super();
    for (const k of Object.keys(arguments[0] ?? {})) {
      if (!['baseUrl', 'units', 'xcache', 'arch', 'debug'].includes(k)) throw new TypeError(`System: no option ${k}`);
    }
    if (arch !== undefined && !ARCHES[arch]) throw new TypeError(`System: arch is one of ${Object.keys(ARCHES).join(', ')}`);
    // The page runtime keeps its state in the page's globals.
    if (made) throw new Error('System: a page runs one system; this page already made one');
    made = true;
    const u = typeof units === 'object' && units && !(units instanceof URL) ? units : units ? { url: units } : null;
    this.#opts = {
      baseUrl: String(baseUrl ?? DEFAULT_BASE()),
      // The package's own list of its files, unless the page names a place.
      files: baseUrl == null ? files : null,
      units: u && { url: new URL(String(u.url).replace(/\/*$/, ''), globalThis.location?.href).href, require: !!u.require },
      xcache: typeof xcache === 'string' && xcache ? xcache : !!xcache,
      arch: arch === undefined ? undefined : ARCHES[arch],
      debug: { ...debug },
    };
    // The page's mounts and host calls: over shared memory, or in the
    // single-thread build, which has none, by message (`page-fs`).
    this.#pageFs = new PageFsHost((name, json) => this.#hostCall(name, json), { shared: !SINGLE });
    const self = this;
    this[INTERNAL] = {
      system: this,
      /** One of each kind per system (a second throws). */
      attach: (kind, part, { once = true, beforeBoot = false } = {}) => {
        if (beforeBoot && this.#state.phase !== 'new') throw new Error(`${kind} must be made before the system boots`);
        if (once && this.#parts.has(kind)) throw new Error(`this system already has its ${kind}`);
        if (once) this.#parts.set(kind, part);
        else (this.#parts.get(kind) ?? this.#parts.set(kind, []).get(kind)).push(part);
      },
      part: (kind) => this.#parts.get(kind) ?? null,
      /** `fn(opts)` adds to the boot's options (the filesystem's layers)
       *  before the kernel is chosen (awaited). */
      beforeBoot: (fn) => this.#beforeBoot.push(fn),
      /** `fn(dev)` makes the part's device once the kernel is loaded and
       *  before any process (awaited): `dev` holds the runtime (`rt`,
       *  page/src/runtime.js), `kernel`, `term`, `say`, and what earlier
       *  parts made (`wlChannel`, `compositor`, `dom`, `glChannel`). Stage
       *  0 the windows, 1 what uses them (GL), whichever part was made first. */
      devices: (fn, stage = 0) => this.#devices.push({ fn, stage }),
      /** `fn(sys)` once the system runs (`#sys`); at once if it already
       *  does. */
      afterBoot: (fn) => {
        if (this.#state.phase === 'running') return fn(this.#sys);
        this.#afterBoot.push(fn);
      },
      /** `fn(m)` for each event of kind `ev` the system's programs raise
       *  (a part's own: 'watch', ...). */
      on: (ev, fn) => (this.#listeners.get(ev) ?? this.#listeners.set(ev, []).get(ev)).push(fn),
      /** The kernel's exports (kernel.js), the guest's files (kfs.js), and
       *  the programs, watches and X hold (programs.js), once the system runs. */
      kernel: () => this.#kernel,
      files: () => this.#files,
      programs: () => this.#programs,
      /** The guest's sockets from now on (page/src/net.js `Sockets`): a
       *  preset's, with its page-served hosts. `sys.network` sets theirs. */
      sockets: (sockets) => {
        sockets.network ??= this.#network;
        this.#net.set(this.#sockets = sockets);
      },
      sys: () => this.#sys,
      get phase() { return self.#state.phase; },
      opts: this.#opts,
      say: (text) => this.#say(text),
      /** The page-fs channel: the page's mounts and host calls. */
      pageFs: () => this.#pageFs,
      /** Whether this system's Workers read rest bundles (see boot). */
      restBundle: () => this.#restBundle,
      preset: this.#preset,
    };
  }

  /** `phase` ('new', 'booting', 'running', 'stopped', 'failed'), `error`,
   *  `console`: what the runtime said, and `init`: how the boot options'
   *  init ended (`{code, signal}`), which stops the system. */
  get state() {
    return { ...this.#state };
  }

  #say(text) {
    this.#state.console += text;
    if (this.#state.console.length > 65536) this.#state.console = this.#state.console.slice(-49152);
    this.dispatchEvent(Object.assign(new Event('console'), { text }));
    if (this.#opts.debug.console) console.log(`[gex] ${text.replace(/\n$/, '')}`);
  }

  /** Start the kernel. Subsystems that must exist before boot
   *  (the filesystem, windows) are made first. With
   *  arguments, as `spawn`'s, the program that is pid 1 (`argv[0]` its
   *  path), which then owns the processes: `spawn` throws, the page reaches
   *  programs through what the init starts (terminals, windows,
   *  /dev/gex-host), and when it ends the system stops. Without, pid 1 is
   *  the kernel's own and the page spawns. */
  boot(...args) {
    const init = args.length ? programArgs('boot', ...args) : null;
    if (this.#booting) return this.#booting;
    this.#init = init;
    this.#booting = this.#boot().catch((e) => {
      this.#earlyWorker?.terminate();
      this.#state.phase = 'failed';
      this.#state.error = String(e?.stack ?? e);
      throw e;
    });
    return this.#booting;
  }

  /** The rest of a bundle laid while the guest runs: its end marked; its
   *  failure (a fetch or decode error, a short archive) the system's: phase
   *  'failed', `error` and a 'failed' event. Its programs go on, and the
   *  files that never came are EIO to them (never ENOENT); what they wrote
   *  still goes to the store at shutdown. Under `?boottrace=1`, a file some
   *  use has waited 30 s for, named. */
  #watchRest(k, rest) {
    let tick = null;
    // Every wait for a file still to come, at the end: its ms, the bundle's
    // bytes laid when it began, its path.
    const waits = !!this.#opts.debug.boottrace && k.has('k_fs_pending_trace');
    if (waits) k.r('k_fs_pending_trace', 1).catch(() => {});
    const sayWaits = async () => {
      if (!waits) return;
      let all = '';
      for (;;) {
        const { r, out: [b] } = await k.call('k_fs_pending_waits', { out: 65536 }).catch(() => ({ r: 0, out: [] }));
        if (r <= 0) break;
        all += new TextDecoder().decode(b.subarray(0, r));
      }
      const rows = all.split('\n').filter(Boolean).map((l) => l.split('\t'));
      const ms = rows.reduce((n, [w]) => n + Number(w), 0);
      console.warn(`[boot] ${rows.length} waits for files still to come, ${ms} ms in all`);
      for (const [w, at, p] of rows) console.warn(`[boot] wait ${w} ms at ${(Number(at) / 1048576).toFixed(1)} MiB laid: ${p}`);
    };
    if (this.#opts.debug.boottrace && k.has('k_fs_pending_long')) {
      tick = setInterval(async () => {
        const { r, out: [b] } = await k.call('k_fs_pending_long', { out: 4096 }).catch(() => ({ r: 0, out: [] }));
        if (r > 0) console.warn(`[boot] waited 30 s for ${new TextDecoder().decode(b.subarray(0, r))}; ${await k.r('k_fs_pending_count').catch(() => '?')} files still to come`);
      }, 5000);
    }
    rest.then(() => { this.#mark('files laid'); return sayWaits(); }, (e) => {
      if (this.#state.phase === 'stopped' || this.#state.phase === 'failed') return;
      console.error(`gex: the bundle failed while the system ran: ${e?.message ?? e}`);
      this.#state.phase = 'failed';
      this.#state.error = String(e?.stack ?? e);
      this.#restFailed = true;
      this.dispatchEvent(Object.assign(new Event('failed'), { error: e }));
    }).finally(() => clearInterval(tick));
  }

  /** `?boottrace=1` (debug only, off by default): each step of the boot
   *  and the time since the page's start, on the console. */
  #mark(what) {
    if (this.#opts.debug.boottrace) globalThis.__gexBootTrace = true;
    if (this.#opts.debug.boottrace) console.warn(`[boot] ${what} (+${Math.round(performance.now())} ms)`);
  }

  async #boot() {
    if (this.#state.phase !== 'new') throw new Error('this system has already booted');
    this.#mark('boot');
    this.#state.phase = 'booting';
    if (!SINGLE && typeof SharedArrayBuffer !== 'function') {
      throw new Error(SP_CODE
        ? 'the gex runtime needs a cross-origin isolated page (Cross-Origin-Opener-Policy: same-origin, Cross-Origin-Embedder-Policy: require-corp), or JSPI for the single-thread build'
        : 'the gex runtime needs a cross-origin isolated page (Cross-Origin-Opener-Policy: same-origin, Cross-Origin-Embedder-Policy: require-corp); @gexemu/runtime-single runs without');
    }
    // The single-thread build's system Worker, now: its modules load while
    // the files below are got ready.
    if (SP_CODE && SINGLE && typeof Worker === 'function') this.#earlyWorker = Kernel.singleWorker();
    // The page runtime takes its switches and asset base from these
    // (page/src/page-env.js, page-globals.js); never the page's URL.
    const { arch: wantArch, ...switches } = this.#opts.debug;
    const q = new URLSearchParams();
    for (const [k, v] of Object.entries(switches)) if (v !== undefined && v !== false && k !== 'console') q.set(k, v === true ? '1' : String(v));
    // Quiet unless asked: a page embedding the runtime hears errors and rare
    // events, not gex's routine lines (each process started, each library
    // loaded). `log` names debug output (hosts/browser/js/log.mjs).
    const asked = this.#preset.loud || q.has('log') || globalThis.GEX_LOG !== undefined || !!globalThis.process?.env?.GEX_LOG;
    if (!asked) q.set('log', 'quiet');
    // The options the switches stand for.
    if (this.#opts.units) {
      q.set('cgen', this.#opts.units.require ? '2' : '1');
      q.set('cgenurl', this.#opts.units.url);
    }
    if (this.#opts.xcache) q.set('xcache', this.#opts.xcache === true ? '1' : this.#opts.xcache);
    globalThis.GEX_PAGE_SEARCH = `?${q}`;
    globalThis.GEX_ASSET_BASE = new URL(this.#opts.baseUrl, globalThis.location?.href).href;
    if (this.#opts.files) globalThis.GEX_ASSET_FILES = this.#opts.files;
    await import('../../src/page-env.js');
    this.#mark('page-env');

    // An architecture the page names (`arch`): the kernel, shim and seed load
    // while the files are got ready, not after the tree is read for one.
    const given = this.#opts.arch ?? wantArch;
    let kernelEarly = null;
    if (given) {
      const { setGlobals } = await import('../../src/page-globals.js');
      setGlobals((name) => q.get(name), { arch: given });
      const rt = await import('../../src/runtime.js');
      if (SP_CODE && SINGLE) this.#earlyWorker?.postMessage({ prepare: { globals: rt.pageGlobals(), arch: given } });
      // Not under Node, whose runtime is primed from files first (below).
      else if (!(globalThis.process?.versions?.node && !globalThis.document)) {
        kernelEarly = rt.loadKernel();
        kernelEarly.catch(() => {});
      }
    }

    // The filesystem's layers (fs.js).
    const opts = { layers: [] };
    for (const fn of this.#beforeBoot) await fn(opts);
    this.#mark('layers');

    // The translated architecture, beside which wasm32 programs run: the
    // global kernel is built for one (its syscall ABI, struct layouts, signal
    // frame), so it is chosen here, from the tree's first native program; a
    // wasm32 /bin/sh says nothing. A tree with none yet (wasm32 only, its
    // native packages to come) gets aarch64, Debian's arm64 being the usual
    // second architecture of a wasm32 tree, so apt can install them later.
    // A program of the other one does not run.
    const arch = given ?? (await layersArch(opts.layers)) ?? 'aarch64';
    const { setGlobals } = await import('../../src/page-globals.js');
    setGlobals((name) => q.get(name), { arch });
    const rt = await import('../../src/runtime.js');
    if (SP_CODE && SINGLE) return this.#bootSingle(rt, arch, opts);
    // Under Node (the runtime's tests) there is no fetch of a file URL.
    if (globalThis.process?.versions?.node && !globalThis.document) {
      const u = rt.runtimeUrls();
      const get = (x) => bytesOf(new URL(x, globalThis.GEX_ASSET_BASE).href);
      rt.primeRuntime({ shimBytes: await get(u.shimBytes), seedBytes: await get(u.seedBytes), kernelBytes: await get(u.kernelBytes), fetchOther: get });
    }
    this.#arch = arch;
    globalThis.__gexTso = await rt.detectTso();
    this.#mark('tso');
    const kernel = await (kernelEarly ?? rt.loadKernel());
    if (!kernel) throw new Error('the kernel would not load');
    this.#mark('kernel loaded');

    // What the runtime says (the X server, Mesa), on the console.
    const term = { write: (d) => this.#say(String(d)), writeln: (t) => this.#say(`${t}\n`) };
    // The page's devices, each made by the part that owns it (windows: the
    // compositor and its channel; MesaGl: the GL ring), before the processes'
    // host, which hands their channels to every Worker.
    const dev = { rt, kernel, term, say: (t) => this.#say(t), wlChannel: null, compositor: null, dom: false, glChannel: null };
    for (const { fn, stage } of this.#devices.sort((a, b) => a.stage - b.stage)) {
      await fn(dev);
      this.#mark(`device ${fn.name || 'at stage'} ${stage}`);
    }
    const { compositor } = dev;
    const netChannel = await rt.openNet(this.#net);
    this.#mark('network');
    // A lazy file in a rest bundle (`BUNDLE#OFFSET`: an ejected site's
    // lazy.json, site/eject-site.mjs) is read through zstd frames.
    this.#restBundle = opts.layers.some((l) => l.lazy?.some(([, , url]) => String(url).includes('#')));
    const stops = [];
    // One Worker owns the lazy files (page/src/lazy-cache-worker.js), as an
    // ejected page's does: every Worker's read asks it, and it keeps what it
    // fetched in Cache Storage for the next visit and decodes a rest
    // bundle's frames with the one decoder. Without it each Worker loaded a
    // decoder of its own, an engine memory reservation in each, and a
    // desktop's Workers ran the page out of them. Where it cannot run (no
    // shared memory, no Cache Storage), each Worker reads for itself.
    const lazyCache = await this.#lazyOwner(rt, kernel, opts.layers, stops);
    this.#mark('lazy owner');
    const host = await rt.makeProcessHost({
      // Every Worker gets the page-fs channel's control buffer: a mount made
      // after boot reaches the programs already running, and each reaches
      // /dev/gex-host. Every Worker reaches the network through the page;
      // which network is the page's to set, any time (net.js).
      uid: 0, ...this.#preset.host,
      kernel, pageFs: this.#pageFs.control, restBundle: this.#restBundle, lazyCache, glChannel: dev.glChannel, wlChannel: dev.wlChannel, netChannel, network: this.#net,
    }, {
      onOutput: (fd, text) => this.#say(text),
      // The compositor lets go of an exited process's memory.
      onExit: (m, pid) => {
        if (typeof pid === 'number') compositor?.dropMemory?.(pid);
      },
      // No program reads the page's console: each has its own pipes or pty.
      readStdin: () => new Promise(() => {}),
      onMessage: (m) => {
        if (m.kind === 'wl-memory') compositor?.addMemory(m.token, m.memory, m.kernel);
        else if (m.kind === 'wl-pixels') compositor?.addPixels?.(m.token, m.id, m.buffer, m.replaces);
        for (const fn of this.#preset.onMessage) fn(m);
      },
    });
    this.#mark('process host');
    this.#sys = {
      host, kernel, term, rt, compositor, wlChannel: dev.wlChannel, lazyCache,
      // Pid 1 is the kernel's: each process starts here, a child of it, from
      // its exec record (host.mjs `spawnExec`).
      spawnExec: ({ uid = 0, gid = uid, ...r }) => host.spawnExec({ ...r, opts: { ...rt.execOptions({ uid, gid }), ...this.#preset.exec } }),
      /** `fn()` when the system stops. */
      onStop: (fn) => stops.push(fn),
      stop: () => {
        for (const fn of stops.splice(0)) try { fn(); } catch {}
        host.stop();
      },
    };
    const fsi = this.#parts.get('filesystem')?.[INTERNAL];
    const { layers } = opts;
    opts.layers = null;
    try {
      await this.#start(this.#sys, layers, fsi);
    } catch (e) {
      this.#sys.stop();
      throw e;
    }
    return this.#begin();
  }

  /** Boot's end, in either build: the init, if any, and what waited for
   *  the system to run. */
  async #begin() {
    this.#mark('running');
    this.#state.phase = 'running';
    if (this.#init) {
      // Pid 1, before anything else runs. When it ends, so does the system,
      // as on Linux.
      // With no environment given, Linux's for an init: HOME and TERM.
      const [argv, o] = this.#init;
      const p = this.#initProcess = this.#spawn(argv, { env: ['HOME=/', 'TERM=linux'], ...o }, true);
      p.exited.then(async (st) => {
        if (this.#state.phase !== 'running') return;
        this.#state.init = st;
        // What it wrote last still comes: its output's end, unless a
        // program it left holds that open (a second at most).
        await Promise.race([p[INTERNAL].drained(), new Promise((r) => setTimeout(r, 1000))]);
        this.shutdown();
      }, (e) => {
        if (this.#state.phase !== 'running') return;
        this.#state.init = { error: String(e?.message ?? e) };
        this.shutdown();
      });
    }
    for (const fn of this.#afterBoot.splice(0)) await fn(this.#sys);
    this.dispatchEvent(new Event('running'));
    return this;
  }

  /** Boot's single-thread build (`SINGLE`): the kernel and the processes'
   *  host in the system Worker (kernel-single-worker.js), which the page
   *  reaches by message; the rest of boot as the default build's. Windows
   *  and GL are not in it yet. */
  async #bootSingle(rt, arch, opts) {
    // The page's parts: here, before the system Worker starts, they say
    // what it must make for them (`single.wl`: a canvas's compositor), and
    // once it runs they attach their page halves (`attach`).
    const single = { wl: null, listeners: new Set() };
    const dev = { rt, single, say: (t) => this.#say(t) };
    for (const { fn } of this.#devices.sort((a, b) => a.stage - b.stage)) await fn(dev);
    this.#arch = arch;
    const term = { write: (d) => this.#say(String(d)), writeln: (t) => this.#say(`${t}\n`) };
    let census = { pids: [], threads: 0, memory: 0 };
    const early = this.#earlyWorker;
    this.#earlyWorker = null;
    const wl = single.wl && { width: single.wl.width, height: single.wl.height, scale: single.wl.scale, dom: !!single.wl.dom };
    const gl = single.gl && { d3d: !!single.gl.d3d };
    // A rest bundle's lazy files (`BUNDLE#OFFSET`) are read through zstd
    // frames: the system Worker loads the decoder before its kernel starts.
    this.#restBundle = opts.layers.some((l) => l.lazy?.some(([, , url]) => String(url).includes('#')));
    const k = await Kernel.startSingle({ globals: rt.pageGlobals(), arch, host: { ...this.#preset.host, restBundle: this.#restBundle }, wl, gl }, (m) => {
      if (m.ev === 'output') this.#say(m.text);
      else if (m.ev === 'comp-out' || m.ev === 'comp-stats' || m.ev === 'gl-wanted' || m.ev === 'wl-mirror' || m.ev === 'wl-drop' || m.ev === 'gl-surfaces') for (const fn of single.listeners) fn(m);
      else if (m.ev === 'census') census = m;
      else if (m.ev === 'message') for (const fn of this.#preset.onMessage) fn(m.m);
      else if (m.ev === 'net-dial') dialFor(this.#network, m);
      else if (m.ev === 'page-fs') {
        this.#pageFs.request(m.req, m.cap).then((a) => {
          m.reply.postMessage(a);
          m.reply.close();
        });
      }
    }, early ?? undefined);
    this.#singleKernel = k;
    this.#networkToSingle();
    const stops = [];
    const host = {
      spawnExec: (r) => k.ask('spawnExec', { r }),
      useKernelStacks: (stacks) => void k.ask('useKernelStacks', { stacks }),
      stop: () => k.ask('stop').catch(() => {}),
      get pids() { return census.pids; },
      get threads() { return census.threads; },
    };
    this.#sys = {
      host, kernel: { memory: { buffer: { get byteLength() { return census.memory; } } } }, term, rt, compositor: null, wlChannel: null,
      // The X server, in the system Worker beside the scene (windows/x11.js).
      startX: single.wl ? (o) => k.ask('startX', o) : null,
      // The game controller table is the Worker's: a copy here, written back
      // (input.js `Gamepads`).
      joy: { table: () => k.ask('joyTable'), write: (at, words) => k.post({ joy: { at, words } }) },
      // So are the audio rings: the worklet keeps a copy (page/src/audio.js).
      audio: { ring: () => k.ask('audioRing'), connect: (port) => k.post({ audio: { port } }, [port]) },
      spawnExec: ({ uid = 0, gid = uid, ...r }) => host.spawnExec({ ...r, opts: { ...rt.execOptions({ uid, gid }), ...this.#preset.exec } }),
      onStop: (fn) => stops.push(fn),
      stop: () => {
        for (const fn of stops.splice(0)) try { fn(); } catch {}
        host.stop();
      },
    };
    const fsi = this.#parts.get('filesystem')?.[INTERNAL];
    const { layers } = opts;
    opts.layers = null;
    try {
      // The page's side of the scene: its compositor, and for a desktop of
      // windows its mirror of the arena (the clipboard's channel).
      if (single.wl) {
        const r = await single.wl.attach(k, single.listeners);
        this.#sys.compositor = r.compositor;
        this.#sys.wlChannel = r.wlChannel ?? null;
      }
      single.gl?.attach(this.#sys.compositor, single.listeners, k);
      await this.#start(this.#sys, layers, fsi, k);
    } catch (e) {
      this.#sys.stop();
      k.terminate();
      throw e;
    }
    return this.#begin();
  }

  /** The kernel's first instance (kernel.js), from the page: the
   *  filesystem's layers and what its store kept laid into the root before
   *  the first program, the page's mounts, host calls, pid 1; then the kernel's stacks handed to the processes' host (gex.mjs
   *  `kernelStacks`). */
  async #start(s, layers, fsi, started = null) {
    // Its reads of lazy files as a process's: by the page's owner of them,
    // or a rest bundle's with a decoder of its own (kernel-worker.js).
    const k = started ?? await Kernel.start({
      module: s.kernel.module, memory: s.kernel.memory, pageFs: this.#pageFs.control, restBundle: this.#restBundle, lazyCache: s.lazyCache ?? null,
    });
    this.#mark('kernel started');
    this.#kernel = k;
    this.#files = new Files(k);
    this.#programs = new Programs(k, this.#files, {
      data: (h, bytes) => this.#procs.get(h)?.[INTERNAL].data(bytes),
      end: (h) => this.#procs.get(h)?.[INTERNAL].data(null),
      exit: (m) => this.#exit(m),
      watch: (m) => this.#event('watch', m),
      xWanted: () => this.#event('x-wanted', {}),
    });
    // Where the files' contents go (fs.js), before any file is laid.
    await this.#store(s, k, fsi?.store?.());
    this.#mark('store');
    for (const l of layers) await (l.image ? this.#files.layImage(l) : this.#files.lay(l));
    // A bundle laid while the guest runs (kfs.js `#layEarly`): its index is
    // in, its files' bytes still coming.
    const rest = this.#files.background;
    if (rest) {
      this.#mark('index laid');
      this.#watchRest(k, rest);
    } else {
      this.#mark('files laid');
    }
    // What the store kept (gex_kernel::vfs::tmpfs::persist), over them.
    if (this.#stored) await this.#restore(k, fsi);
    // The steps below are not waited for one by one: the kernel's Worker
    // takes its batches in order, so what is asked in one turn goes in one
    // message, and each round trip to it at boot is one the first program
    // waits for. Only a step that needs an answer first waits.
    const steps = [];
    // Each one's failure is had at the end, not said as unhandled meanwhile.
    const step = (p) => { p.catch(() => {}); steps.push(p); };
    step(k.r('k_fs_populate_done'));
    if (this.#opts.xcache) {
      await Promise.all(steps);
      await this.#xcacheRestore(s, k);
    }
    for (const [path, id] of fsi?.mounts?.() ?? []) await this.#files.mount(path, id);
    // The unix socket names the page listens on (`listen`), bound before
    // pid 1, which may connect to them at once.
    this.#unixUp = true;
    await Promise.all([...this.#unix].map((u) => u.bind()));
    // The page answers host calls (`hostCalls`): every process gets a
    // /dev/gex-host.
    step(k.r('k_host_calls_serve'));
    step(this.#programs.start({ init: !!this.#init }));
    // Each process that crashes, to the page (`crash` events).
    if (k.has('k_crash_queue')) step(this.#watchCrashes(k));
    const opt = (f, ...a) => (k.has(f) ? [{ f, a }] : []);
    const list = [
      { f: 'k_stack_area' }, { f: 'k_stack_size' }, { f: 'k_stack_count' },
      ...opt('k_snap_word'), ...opt('k_ws_addr', 0), ...opt('k_ws_count'), ...opt('k_mono_held_at'), ...opt('k_snap_extents_at'),
    ];
    const r = Object.fromEntries((await k.calls(list)).map((x, i) => [list[i].f, x.r >>> 0]));
    await Promise.all(steps);
    this.#mark('kernel steps');
    s.host.useKernelStacks({
      area: r.k_stack_area, size: r.k_stack_size, count: r.k_stack_count,
      ...(k.has('k_snap_word') ? { snapWord: r.k_snap_word, wsAddr: r.k_ws_addr, wsCount: r.k_ws_count } : {}),
      ...(k.has('k_mono_held_at') ? { monoHeld: r.k_mono_held_at } : {}),
      ...(k.has('k_snap_extents_at') ? { extents: r.k_snap_extents_at } : {}),
    });
  }

  /** The translated code a load saved (`xcache`), into the kernel before
   *  the first process: each process's Worker names code by its bytes from
   *  then on, and checks a saved module's file when it first uses it. */
  async #xcacheRestore(s, k) {
    // The single-thread build's runtime and kernel memory are its system
    // Worker's (kernel-single-worker.js `xcacheRestore`).
    if (SP_CODE && SINGLE) {
      const { name, size, n } = await k.ask('xcacheRestore');
      if (name) this.#say(size === undefined ? `xcache ${name}: nothing saved\n` : `xcache ${name}: ${(size / 1048576).toFixed(1)} MiB, restored ${n} modules\n`);
      return;
    }
    const name = s.rt.xcacheName();
    if (!name || !k.has('k_store_restore')) return;
    await k.r('k_xcache_enable');
    const bytes = await s.rt.xcRead(name);
    if (!bytes) return void this.#say(`xcache ${name}: nothing saved\n`);
    const at = (await k.r('k_store_restore_buf', bytes.length)) >>> 0;
    new Uint8Array(s.kernel.memory.buffer).set(bytes, at);
    const n = await k.r('k_store_restore');
    this.#say(`xcache ${name}: ${(bytes.length / 1048576).toFixed(1)} MiB, restored ${n} modules\n`);
  }

  /** Every crash the kernel records (gex_kernel::proc::Crash: a program
   *  that a crash signal ended, or one that said it crashed, as Wine does
   *  for a Windows program's unhandled exception), as a `crash` event:
   *  `{ pid, name, reason, code, signal }`. */
  async #watchCrashes(k) {
    const queue = (await k.r('k_crash_queue')) >>> 0;
    if (!queue) return;
    await k.watch(queue);
    const dec = new TextDecoder();
    (async () => {
      for (;;) {
        const seq = k.seq();
        const { r, out } = await k.call('k_crash_take', { out: 16384 }).catch(() => ({ r: -1 }));
        if (r < 0) return;
        if (r === 0) {
          await k.changed(seq);
          continue;
        }
        for (const line of dec.decode(out[0].subarray(0, r)).split('\n')) {
          if (!line) continue;
          let c;
          try { c = JSON.parse(line); } catch { continue; }
          this.#say(`[gex] pid ${c.pid}${c.name ? ` (${c.name})` : ''} crashed: ${c.reason}\n`);
          this.dispatchEvent(Object.assign(new Event('crash'), c));
        }
      }
    })();
  }

  #exit(m) {
    const p = this.#byPid.get(m.pid);
    if (p) {
      this.#byPid.delete(m.pid);
      p[INTERNAL].exit(m);
    } else {
      this.#early.set(m.pid, m);
      if (this.#early.size > 4096) this.#early.delete(this.#early.keys().next().value);
    }
  }

  #event(ev, m) {
    for (const fn of this.#listeners.get(ev) ?? []) fn(m);
  }
  #listeners = new Map();

  /** A guest's call (/dev/gex-host, mount.js `#call1`): `{ ok, text }`. */
  async #hostCall(name, json) {
    try {
      const fn = this.#hostCalls.get(name);
      if (!fn) throw new Error(`no host call ${name}`);
      let arg;
      try {
        arg = json.trim() ? JSON.parse(json) : null;
      } catch {
        throw new Error(`${name}: not JSON: ${json.slice(0, 200)}`);
      }
      const v = await fn(arg);
      return { ok: true, text: v === undefined ? 'null' : JSON.stringify(v) };
    } catch (e) {
      return { ok: false, text: String(e?.message ?? e) };
    }
  }

  /**
   * The page's functions a guest program calls through /dev/gex-host: it
   * opens the device, writes `NAME\0JSON` (the JSON may be empty), and
   * reads the answer from the same descriptor: a status byte, 0 when
   * `fn(value)` returned (its value follows, as JSON) and 1 when it threw
   * (its message follows). Each open is one call.
   */
  get hostCalls() {
    const calls = this.#hostCalls;
    return {
      register(name, fn) {
        if (typeof name !== 'string' || !name || /[\0\s]/.test(name)) throw new TypeError('hostCalls.register: a name without spaces');
        if (typeof fn !== 'function') throw new TypeError('hostCalls.register: a function');
        calls.set(name, fn);
        return () => { if (calls.get(name) === fn) calls.delete(name); };
      },
      unregister(name) {
        calls.delete(name);
      },
      names: () => [...calls.keys()],
    };
  }

  /** Into the guest: `connectGuest(port)`, a connection to what a program
   *  listens on at 127.0.0.1:`port` (TCP), as `{ readable, writable,
   *  close() }`; it rejects with ECONNREFUSED when nothing listens. */
  get net() {
    return {
      connectGuest: (port, { host = '127.0.0.1' } = {}) => this.#connectGuest(port, host),
    };
  }

  /** The guest's connections out (README "Network"): any object with
   *  `connect(host, port)`, or null for none. Set any time; open
   *  connections stay where they are. */
  get network() {
    return this.#network;
  }
  set network(network) {
    if (network != null && typeof network.connect !== 'function') throw new TypeError('network: an object with connect(host, port), or null');
    this.#network = network ?? null;
    if (!this.#sockets) this.#net.set(this.#sockets = new Sockets(null));
    this.#sockets.network = this.#network;
    if (SP_CODE) this.#networkToSingle();
  }

  /** The single-thread build's system Worker told which network the page
   *  has (kernel-single-worker.js `network`): its sockets dial through it. */
  #networkToSingle() {
    const n = this.#network;
    this.#singleKernel?.ask('network', { net: n && { datagram: typeof n.datagram === 'function' } }).catch(() => {});
  }

  async #connectGuest(port, host) {
    this.#running('connectGuest');
    if (!['127.0.0.1', 'localhost', '0.0.0.0'].includes(host)) throw new Error(`connectGuest: only the guest's own 127.0.0.1, not ${host}`);
    if (!Number.isInteger(port) || port <= 0 || port > 65535) throw new TypeError(`connectGuest: no port ${port}`);
    return this.#connection((h) => this.#programs.connectGuest(h, port));
  }

  /** A connection of the page's, its ends made by `open(h)`. */
  async #connection(open) {
    const h = this.#nextHandle++;
    const pr = this.#programs;
    const c = new GuestConnection({
      read: () => pr.read(h),
      write: (bytes) => pr.write(h, bytes),
      closeStdin: () => pr.closeStdin(h),
      release: () => { pr.release(h).catch(() => {}); this.#procs.delete(h); },
    });
    this.#procs.set(h, c);
    try {
      await open(h);
    } catch (e) {
      this.#procs.delete(h);
      throw e;
    }
    return c;
  }

  /** Listen on the unix socket name `path` (an absolute path, or an
   *  abstract name with a leading NUL), as a program's AF_UNIX stream
   *  socket would: a program's `connect` to it is a connection the page
   *  takes from the listener (`for await (const conn of listener)`), each a
   *  `{ readable, writable, close() }`. Made before boot, the name is bound
   *  as the kernel comes up. The socket file appears at `path` while it
   *  listens. */
  listen(path) {
    const p = unixPath('listen', path);
    if (['stopped', 'failed'].includes(this.#state.phase)) throw new Error(`listen: the system is ${this.#state.phase}`);
    let ok, fail;
    const ready = new Promise((res, rej) => { ok = res; fail = rej; });
    const u = { path: p, l: null, binding: null, closed: false };
    u.bind = () => (u.binding ??= this.#programs.listenUnix(p).then((l) => { u.l = l; ok(); }, (e) => { this.#unix.delete(u); fail(e); }));
    this.#unix.add(u);
    const listener = new UnixListener(p, {
      ready,
      take: () => this.#takeUnix(u),
      close: async () => {
        u.closed = true;
        this.#unix.delete(u);
        await u.binding?.catch(() => {});
        if (u.l && this.#state.phase !== 'stopped') await this.#programs.unlistenUnix(u.l, p).catch(() => {});
        // One never bound (closed before boot) is not waited for.
        ok();
      },
    });
    if (this.#unixUp) u.bind();
    return listener;
  }

  /** The next connection to listener `u`, null once it is closed. */
  async #takeUnix(u) {
    const pr = this.#programs;
    const id = await pr.acceptUnix(u.l);
    if (id === null) return null;
    if (u.closed || this.#state.phase === 'stopped') {
      await this.#kernel.r('k_sock_close', id).catch(() => {});
      return null;
    }
    return this.#connection((h) => pr.adopt(h, id));
  }

  /** Connect to what a program listens on at the unix socket name `path`
   *  (an AF_UNIX stream socket's bind and listen): `{ readable, writable,
   *  close() }`. Rejects with ECONNREFUSED when nothing listens there, ENOENT
   *  when there is no such file either, as a program's connect fails. */
  async connect(path) {
    const p = unixPath('connect', path);
    this.#running('connect');
    return this.#connection((h) => this.#programs.connectUnix(h, p));
  }

  #running(what) {
    if (this.#state.phase !== 'running') throw new Error(`${what}: the system is ${this.#state.phase}, not running`);
  }

  /**
   * Start a program: `argv[0]` a path, or a name looked up in the PATH of
   * `env`. Options:
   *   pty: {cols, rows, encoding}  a terminal (else pipes)
   *   env: {K: V} or ['K=V']       the whole environment (nothing is added)
   *   cwd, uid, gid
   *   detached: true               no terminal and no pipes: /dev/null
   * Returns a Process at once; `p.started` resolves with its pid.
   */
  spawn(argv, opts = {}) {
    this.#running('spawn');
    if (this.#init) {
      throw new Error(`spawn: this system has an init (${this.#init[0][0]}), which owns the processes; the page reaches programs through what it starts`);
    }
    return this.#spawn(argv, opts, false, false);
  }

  /** The init (`boot(argv, opts)`) as a Process, null without one. */
  get init() {
    return this.#initProcess;
  }

  /** `untilExit`: its pipes end for the page once the program has exited
   *  and they hold nothing more (`run`); what a daemon it left writes to
   *  them later is read and dropped. */
  #spawn(argv, opts, init, untilExit) {
    [argv, opts] = programArgs(init ? 'boot' : 'spawn', argv, opts);
    const h = this.#nextHandle++;
    const stdio = opts.pty ? 'pty' : opts.detached ? 'null' : 'pipe';
    const pr = this.#programs;
    const quiet = (p) => p.catch((e) => this.#say(`gex: ${e?.message ?? e}\n`));
    const p = new Process({
      argv,
      stdio,
      encoding: opts.pty?.encoding,
      untilExit,
      read: (fd, done) => pr.read(h, fd, done),
      unread: (fd) => quiet(pr.unread(h, fd)),
      kick: () => pr.kick(),
      write: (bytes) => pr.write(h, bytes),
      closeStdin: () => pr.closeStdin(h),
      resize: (cols, rows) => quiet(pr.resize(h, cols, rows)),
      kill: (pid, sig) => quiet(pr.kill(pid, signalNumber(sig))),
      release: () => {
        this.#procs.delete(h);
        quiet(pr.release(h));
      },
      windows: (pid) => this.#parts.get('windows')?.[INTERNAL]?.of?.(pid) ?? [],
    });
    this.#procs.set(h, p);
    pr.spawn({
      h,
      argv,
      env: envList(opts.env),
      cwd: opts.cwd ?? '',
      stdio,
      cols: opts.pty?.cols ?? 80,
      rows: opts.pty?.rows ?? 24,
      init,
    }).then((r) => {
      if (r.error) {
        this.#procs.delete(h);
        p[INTERNAL].failed(r.error);
        return;
      }
      this.#byPid.set(r.pid, p);
      p[INTERNAL].started(r.pid);
      const early = this.#early.get(r.pid);
      if (early) {
        this.#early.delete(r.pid);
        this.#byPid.delete(r.pid);
        p[INTERNAL].exit(early);
      }
      const uid = opts.uid ?? 0;
      this.#sys.spawnExec({ pid: r.pid, exec: r.exec, fds: r.fds, cwd: r.cwd, wasmImage: r.wasmImage, uid, gid: opts.gid ?? opts.uid ?? 0 })
        .catch((e) => {
          this.#say(`gex: ${argv[0]} (pid ${r.pid}) did not start: ${e?.message ?? e}\n`);
          quiet(pr.abandon(r.pid));
        });
    }, (e) => {
      this.#procs.delete(h);
      p[INTERNAL].failed(e);
    });
    return p;
  }

  /** The store's handle and lock given up: the page is going away. */
  #letStoreGo() {
    this.#storeWorker?.terminate();
    this.#storeWorker = null;
    this.#claim?.release();
    this.#claim = null;
  }

  /** The root the store kept laid over the sources'. A generation that
   *  failed its checks is said, and the one before it is booted; a store
   *  that holds nothing whole boots the sources alone. */
  async #restore(k, fsi) {
    const exclude = fsi.exclude().join('\n');
    const r = await k.r('k_fs_restore', { in: exclude });
    if (r === 2) {
      const gen = await k.r('k_fs_restored_gen');
      console.error(`[gex] the filesystem's store: its last generation was torn or failed its checks; booted the one before it (${gen >>> 0})`);
    } else if (r === 3) {
      console.error("[gex] the filesystem's store: nothing it kept could be read back; booted the sources alone");
    }
  }

  /** The filesystem's store (fs.js `storeOf`) set up in the kernel: a
   *  memory root's limit, or an OPFS store's block store, answered by the
   *  page's store Worker (the default build) or the system Worker itself
   *  (the single-thread build). The debug switch `blockscheck` sums every
   *  block written and checks it read back (and reads every write back). */
  async #store(s, k, st) {
    // The origin's storage kept under pressure, not evicted with the cache
    // (granted or not as the browser decides; it asks nothing of the user
    // in Chromium).
    const persistent = () => navigator.storage?.persist?.().catch(() => {});
    if (!st) return;
    if (st.kind === 'memory') {
      if (st.max) await k.r('k_fs_limit', st.max % 0x100000000, Math.floor(st.max / 0x100000000));
      return;
    }
    const check = !!this.#opts.debug.blockscheck;
    if (st.kind !== 'page') {
      // One page per store: a second tab of the site is told so (EBUSY), at
      // once, not after the handle's retries (the page's start() asks
      // first, and may have offered a MemoryFilesystem instead).
      const claim = await claimStore(st.name);
      if (!claim) throw storeBusy(st.name);
      this.#sys.onStop(() => claim.release());
      this.#claim = claim;
    }
    const ctl = await k.r('k_blocks_config', 0, 0, 0, 0);
    if (st.kind === 'page') {
      // A page's own store (a Filesystem subclass), asked from here through
      // the slots. Not in the single-thread build: a request there must
      // suspend the calling stack, and the kernel reads files where it
      // cannot (a program's pages faulted in).
      if (SP_CODE && SINGLE) {
        throw new Error(`${st.fs.constructor.name}: a store of the page's own needs the default build; the single-thread build keeps files in a MemoryFilesystem or an OPFSFilesystem`);
      }
      try {
        await st.fs.open();
      } catch (e) {
        throw new Error(`${st.fs.constructor.name}: the store would not open: ${e?.message ?? e}`);
      }
      this.#sys.onStop(serveSlots(s.kernel.memory, ctl >>> 0, st.fs));
    } else if (SP_CODE && SINGLE) {
      persistent();
      await k.ask('blocksOpen', { name: st.name, verify: check });
    } else {
      persistent();
      const w = new Worker(new URL('../../src/blocks-worker.js', import.meta.url), { type: 'module', name: 'gex-blocks' });
      const ready = await new Promise((r) => {
        w.onmessage = ({ data: m }) => { if (m.kind === 'ready') r(m); };
        w.onerror = (e) => r({ ok: false, why: e.message ?? String(e) });
        w.postMessage({ memory: s.kernel.memory, ctl: ctl >>> 0, name: st.name, verify: check });
      });
      if (!ready.ok) {
        w.terminate();
        throw new Error(`OPFSFilesystem: the store ${st.name} would not open: ${ready.why}`);
      }
      this.#sys.onStop(() => w.terminate());
      this.#storeWorker = w;
    }
    await k.r('k_blocks_config', st.block, st.cache, st.inline, check ? 1 : 0);
    this.#stored = true;
    // The write-back: a block written reaches the store within `writeback`
    // ms (a tick every second at most, a block written back once it has
    // been dirty for `age` of them).
    const every = Math.min(1000, st.writeback);
    const age = Math.max(1, Math.round(st.writeback / every));
    let busy = false, said = 0;
    const timer = setInterval(() => {
      if (busy || this.#state.phase !== 'running') return;
      busy = true;
      k.r('k_blocks_tick', age).then((r) => {
        // A failure is said once per kind: the blocks stay dirty and are
        // tried again at the next tick.
        if (r < 0 && r !== said) console.error(`[gex] the filesystem's store failed a write-back: ${errno(r, '').code}; written data is kept in memory and tried again`);
        said = r < 0 ? r : 0;
      }, () => {}).finally(() => { busy = false; });
    }, every);
    // A page going away writes what it can first.
    const hide = () => { if (document.visibilityState === 'hidden') this.sync().catch(() => {}); };
    // A page left for good (not kept in the back/forward cache) lets the
    // store go once that is done, so the next load of the site (a reload)
    // opens it without waiting on this one's teardown.
    const leave = (e) => {
      const done = this.sync().catch(() => {});
      if (!e?.persisted) done.finally(() => this.#letStoreGo());
    };
    if (typeof document === 'object') {
      document.addEventListener('visibilitychange', hide);
      addEventListener('pagehide', leave);
    }
    this.#sys.onStop(() => {
      clearInterval(timer);
      if (typeof document === 'object') {
        document.removeEventListener('visibilitychange', hide);
        removeEventListener('pagehide', leave);
      }
    });
  }

  /** The lazy files' owner for boot's `layers` (see boot): its control
   *  buffer for every Worker (`{ ctl }`), or null where there is none. */
  async #lazyOwner(rt, kernel, layers, stops) {
    const lazy = layers.flatMap((l) => l.lazy ?? []);
    if (!MP_CODE || !lazy.length || typeof Worker !== 'function' || typeof caches !== 'object' || typeof SharedArrayBuffer !== 'function'
      || !(kernel.memory?.buffer instanceof SharedArrayBuffer)) return null;
    const { makeControl, lazyVersion } = await import('../../../hosts/browser/js/lazy-cache.mjs');
    const ctl = makeControl();
    const w = new Worker(new URL('../../src/lazy-cache-worker.js', import.meta.url), { type: 'module', name: 'lazy-cache' });
    const ready = await new Promise((r) => {
      w.onmessage = ({ data: m }) => { if (m.kind === 'ready') r(m); };
      w.onerror = (e) => r({ ok: false, why: e.message ?? String(e) });
      setTimeout(() => r({ ok: false, why: 'no answer in 5 s' }), 5000);
      w.postMessage({
        ctl, memory: kernel.memory, cache: LAZY_CACHE, zstd: this.#restBundle || null,
        versions: Object.fromEntries(lazy.map(([, size, url, , mtime]) => [url, lazyVersion(size, mtime)])),
      });
    });
    if (!ready.ok) {
      w.terminate();
      if (this.#restBundle) this.#say(`gex: lazy files read by each process: ${ready.why}\n`);
      return null;
    }
    stops.push(() => w.terminate());
    return { ctl };
  }

  /** Spawn with pipes, write `input` (if any), wait, collect:
   *  `{code, signal, stdout, stderr}`, text unless `encoding: null`. */
  async run(argv, { input, encoding = 'utf-8', ...opts } = {}) {
    this.#running('spawn');
    if (this.#init) {
      throw new Error(`run: this system has an init (${this.#init[0][0]}), which owns the processes; the page reaches programs through what it starts`);
    }
    // Its output is what it wrote before it ended: a daemon it started (a
    // server it leaves running) may hold its pipes for good.
    const p = this.#spawn(argv, { ...opts, detached: false, pty: undefined }, false, true);
    const take = async (s) => {
      const parts = [];
      for (const r = s.getReader(); ;) {
        const { value, done } = await r.read();
        if (done) break;
        parts.push(value);
      }
      const n = parts.reduce((a, b) => a + b.length, 0);
      const out = new Uint8Array(n);
      let at = 0;
      for (const b of parts) { out.set(b, at); at += b.length; }
      return encoding ? new TextDecoder(encoding).decode(out) : out;
    };
    const out = take(p.stdout);
    const err = take(p.stderr);
    const w = p.stdin.getWriter();
    if (input !== undefined) await w.write(typeof input === 'string' ? new TextEncoder().encode(input) : await bytesOf(input, 'input'));
    await w.close();
    const { code, signal } = await p.exited;
    return { code, signal, stdout: await out, stderr: await err };
  }

  /** `sync(2)`: what the programs wrote made durable in the filesystem's
   *  store (written back from the cache and the store flushed). Nothing to
   *  do for a MemoryFilesystem. Rejects with the error (`code` EIO, ENOSPC)
   *  when the store could not take it. */
  async sync() {
    if (this.#state.phase !== 'running' && !this.#restFailed) throw new Error(`sync: the system is ${this.#state.phase}, not running`);
    if (!this.#stored) return;
    const r = await this.#kernel.r('k_fs_sync_all');
    if (r < 0) throw errno(r, '/');
  }

  /** Stop every program and the kernel. */
  async shutdown() {
    if (this.#state.phase === 'stopped') return;
    // What was written goes to the store before the store goes.
    if ((this.#state.phase === 'running' || this.#restFailed) && this.#stored) await this.sync().catch((e) => console.error(`[gex] shutdown: the filesystem's store could not take what was written: ${e.code ?? e.message}`));
    this.#state.phase = 'stopped';
    // Listeners take nothing more.
    for (const u of this.#unix) {
      u.closed = true;
      if (u.l) u.l.closed = true;
    }
    this.#unix.clear();
    this.#programs?.kick();
    for (const p of this.#procs.values()) p[INTERNAL].exit({ code: 137, signal: 9, shutdown: true });
    this.#procs.clear();
    this.#byPid.clear();
    for (const parts of this.#parts.values()) for (const part of [parts].flat()) await part?.[INTERNAL]?.shutdown?.();
    this.#sys?.stop();
    this.#kernel?.terminate();
    this.dispatchEvent(new Event('stopped'));
  }

  /** What is running: the state, the programs started here, every guest
   *  process the runtime holds, and the kernel's memory. */
  describe() {
    const host = this.#sys?.host;
    return {
      phase: this.#state.phase,
      arch: this.#arch,
      processes: [...this.#procs.values()].map((p) => ({ pid: p.pid, argv: p.argv })),
      pids: host?.pids ?? [],
      threads: host?.threads ?? 0,
      memory: { kernel: this.#sys?.kernel?.memory?.buffer?.byteLength ?? 0 },
    };
  }
}
