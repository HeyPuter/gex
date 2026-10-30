// `System`: the kernel, started with `boot()`, and the programs a page runs
// in it. Everything else a system has -- its files, windows, network,
// persistence, sound, GL -- is a class of its own that takes the system as
// its first argument (web/runtime/README.md).
//
// Built on the page runtime (web/src/runtime.js `runProgram`), with no
// program of its own for pid 1 unless `boot` is given one. Each program
// `spawn` asks for starts in a Worker of its own from an exec record
// (host.mjs `spawnExec`). The page reaches the kernel through its exports
// (kernel.js, over a kernel-call Worker): the programs' terminals and pipes
// (programs.js), the guest's files (kfs.js), persistence (kpersist.js).

import { INTERNAL, bytesOf, envList, signalNumber } from './internal.js';
import { Process } from './process.js';
import { NetSwitch } from './netswitch.js';
import { GuestConnection } from './connection.js';
import { PageFsHost } from './mount.js';
import { Kernel } from './kernel.js';
import { Files } from './kfs.js';
import { Programs } from './programs.js';
import { Changes } from './kpersist.js';

/** Where the runtime's files (gex/...) are: beside the package's modules
 *  in a build (web/runtime/build.sh defines `__GEX_RUNTIME_ASSETS__`), the
 *  tree's web/public in place. Not a literal, so a bundler does not take
 *  the directory for an asset of its own. */
const ASSETS = typeof __GEX_RUNTIME_ASSETS__ === 'string' ? __GEX_RUNTIME_ASSETS__ : '../../public/';
const DEFAULT_BASE = () => new URL(ASSETS, import.meta.url).href;

/** Whether this page has made its system. */
let made = false;

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
 * The guest architecture a tar's programs are built for: the machine of
 * what `/bin/sh` is, its links followed, read from the headers up to the
 * entry that decides it, with no file's bytes but an ELF header read; else
 * of the first native program in it. Null for none.
 */
function tarArch(tarEntries, b) {
  const links = new Map(); // path -> [target, hard]
  const files = new Map(); // path -> where its bytes start
  const resolve = (p) => {
    let todo = p.split('/');
    let cur = [];
    for (let hops = 0; todo.length;) {
      const x = todo.shift();
      if (!x || x === '.') continue;
      if (x === '..') {
        cur.pop();
        continue;
      }
      cur.push(x);
      const l = links.get(`/${cur.join('/')}`);
      if (!l) continue;
      if (++hops > 40) return null;
      const [t, hard] = l;
      cur.pop();
      todo = [...(t.startsWith('/') || hard ? [] : cur), ...t.split('/'), ...todo];
      cur = [];
    }
    return `/${cur.join('/')}`;
  };
  let want = '/bin/sh';
  let found = null;
  let first = null;
  tarEntries(b, ({ name, type, body, link }) => {
    const p = `/${name.split("/").filter((x) => x && x !== ".").join("/")}`;
    if (type === '2' || type === '1') {
      links.set(p, [link, type === '1']);
      want = resolve('/bin/sh');
    } else if (type === '0' || type === '\0' || type === '7') {
      files.set(p, body);
      first ??= machine(b, body);
    }
    if (want && files.has(want)) {
      found = machine(b, files.get(want));
      return true;
    }
    return false;
  });
  return found ?? first;
}

/** [`tarArch`] of single files (`{ path: bytes }`). */
function filesArch(files = {}) {
  for (const p of ['/bin/sh', '/usr/bin/sh']) if (machine(files[p])) return machine(files[p]);
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

export class System extends EventTarget {
  #opts;
  #state = { phase: 'new', error: null, progress: null, console: '' };
  #parts = new Map();
  #beforeBoot = [];
  #afterBoot = [];
  /** The kernel from the page (kernel.js), and what works through it. */
  #kernel = null;
  #files = null;
  #programs = null;
  #changes = null;
  #sys = null; // runProgram's onSystem: { host, kernel, tty, compositor, startGl }
  #run = null;
  #procs = new Map(); // handle -> Process
  #byPid = new Map(); // pid -> Process
  #early = new Map(); // pid -> exit status heard before the pid was known
  #nextHandle = 1;
  #booting = null;
  #arch = null;
  #net = new NetSwitch();
  #hostCalls = new Map();
  #pageFs = null;
  /** The init (`boot(argv, opts)`), `[argv, opts]`, and its Process once
   *  it runs. */
  #init = null;
  #initProcess = null;

  /**
   * `baseUrl`: where the runtime's files are (the shim, kernel, seed,
   * ...); default beside the package. `debug`: the runtime's switches, `{name: value}`
   * as gex's test pages take `?name=value`; unstable, for debugging only.
   */
  constructor({ baseUrl, debug = {} } = {}) {
    super();
    for (const k of Object.keys(arguments[0] ?? {})) {
      if (!['baseUrl', 'debug'].includes(k)) throw new TypeError(`System: no option ${k}`);
    }
    // The page runtime keeps its state in the page's globals.
    if (made) throw new Error('System: a page runs one system; this page already made one');
    made = true;
    this.#opts = { baseUrl: String(baseUrl ?? DEFAULT_BASE()), debug: { ...debug } };
    this.#pageFs = new PageFsHost((name, json) => this.#hostCall(name, json));
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
      /** `fn(opts)` adds to runProgram's options before boot (awaited). */
      beforeBoot: (fn) => this.#beforeBoot.push(fn),
      /** `fn(sys)` once the system runs, with runProgram's `onSystem` view;
       *  at once if it already does. */
      afterBoot: (fn) => {
        if (this.#state.phase === 'running') return fn(this.#sys);
        this.#afterBoot.push(fn);
      },
      /** `fn(m)` for each event of kind `ev` the system's programs raise
       *  (a part's own: 'watch', ...). */
      on: (ev, fn) => (this.#listeners.get(ev) ?? this.#listeners.set(ev, []).get(ev)).push(fn),
      /** The kernel's exports (kernel.js), the guest's files (kfs.js), the
       *  programs, watches and X hold (programs.js), and persistence's
       *  changes (kpersist.js), once the system runs. */
      kernel: () => this.#kernel,
      files: () => this.#files,
      programs: () => this.#programs,
      changes: () => this.#changes,
      /** The guest's network from now on (net.js). */
      network: (backend) => this.#net.set(backend),
      sys: () => this.#sys,
      get phase() { return self.#state.phase; },
      opts: this.#opts,
      say: (text) => this.#say(text),
      /** The page-fs channel: the page's mounts and host calls. */
      pageFs: () => this.#pageFs,
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
   *  (the filesystem, windows, persistence) are made first. With
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
      this.#state.phase = 'failed';
      this.#state.error = String(e?.stack ?? e);
      throw e;
    });
    return this.#booting;
  }

  async #boot() {
    if (this.#state.phase !== 'new') throw new Error('this system has already booted');
    this.#state.phase = 'booting';
    if (typeof SharedArrayBuffer !== 'function') {
      throw new Error('the gex runtime needs a cross-origin isolated page (Cross-Origin-Opener-Policy: same-origin, Cross-Origin-Embedder-Policy: require-corp)');
    }
    // The page runtime takes its switches and asset base from these
    // (web/src/page-env.js, page-globals.js); never the page's URL.
    const { arch: wantArch, ...switches } = this.#opts.debug;
    const q = new URLSearchParams();
    for (const [k, v] of Object.entries(switches)) if (v !== undefined && v !== false && k !== 'console') q.set(k, v === true ? '1' : String(v));
    globalThis.GEX_PAGE_SEARCH = `?${q}`;
    globalThis.GEX_ASSET_BASE = new URL(this.#opts.baseUrl, globalThis.location?.href).href;
    const { assetUrl } = await import('../../src/page-env.js');

    const opts = {
      // No program: pid 1 is the kernel's.
      elf: null,
      // The filesystem's (fs.js).
      layers: [],
      records: [],
      env: [],
      uid: 0,
      // Every Worker reaches the network through the page; which network
      // is the page's to set, any time (net.js).
      network: this.#net,
      // Every Worker gets the page-fs channel's control buffer: a mount
      // made after boot reaches the programs already running, and each
      // reaches /dev/gex-host.
      pageFs: this.#pageFs.control,
      // `/dev/dsp` plays once the page makes a WebAudio (audio.js).
      playAudio: false,
    };
    for (const fn of this.#beforeBoot) await fn(opts);

    // The translated architecture, beside which wasm32 programs run: the global kernel is built for one (its syscall ABI,
    // struct layouts, signal frame), so it is chosen here, from the tree's
    // programs. A tree with none yet (wasm32 only, its native packages to
    // come) gets aarch64, Debian's arm64 being the usual second
    // architecture of a wasm32 tree. A program of the other one does not run.
    const arch = wantArch ?? (await layersArch(opts.layers)) ?? 'aarch64';
    const { setGlobals } = await import('../../src/page-globals.js');
    setGlobals((name) => q.get(name), { arch });
    const { runProgram, primeRuntime, runtimeUrls } = await import('../../src/runtime.js');
    // Under Node (the runtime's tests) there is no fetch of a file URL.
    if (globalThis.process?.versions?.node && !globalThis.document) {
      const u = runtimeUrls();
      const get = (x) => bytesOf(new URL(x, globalThis.GEX_ASSET_BASE).href);
      primeRuntime({ shimBytes: await get(u.shimBytes), seedBytes: await get(u.seedBytes), kernelBytes: await get(u.kernelBytes), fetchOther: get });
    }
    this.#arch = arch;

    const term = {
      rows: 24,
      cols: 80,
      write: (d) => this.#say(typeof d === 'string' ? d : new TextDecoder().decode(d)),
      writeln: (s) => this.#say(`${s}\n`),
      onData: () => ({ dispose() {} }),
      onResize: () => ({ dispose() {} }),
      focus() {},
    };
    let up;
    const started = new Promise((resolve, reject) => { up = { resolve, reject }; });
    opts.onSystem = (s) => {
      this.#sys = s;
      const fsi = this.#parts.get('filesystem')?.[INTERNAL];
      const { layers, records } = opts;
      opts.layers = opts.records = null;
      this.#start(s, layers, records, fsi).then(up.resolve, up.reject);
    };
    this.#run = runProgram(term, opts);
    this.#run.then(() => {
      if (this.#state.phase === 'running' || this.#state.phase === 'booting') {
        this.#state.phase = 'stopped';
        up.reject(new Error(`the system stopped before it was up: ${this.#state.console.slice(-400)}`));
        this.dispatchEvent(new Event('stopped'));
      }
    }, (e) => {
      this.#state.phase = 'failed';
      this.#state.error = String(e?.stack ?? e);
      up.reject(e);
      this.dispatchEvent(new Event('stopped'));
    });
    await started;
    this.#state.phase = 'running';
    if (this.#init) {
      // Pid 1, before anything else runs. When it ends, so does the system,
      // as on Linux.
      // With no environment given, Linux's for an init: HOME and TERM.
      const [argv, o] = this.#init;
      const p = this.#initProcess = this.#spawn(argv, { env: ['HOME=/', 'TERM=linux'], ...o }, true);
      p.exited.then((st) => {
        if (this.#state.phase !== 'running') return;
        this.#state.init = st;
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

  /** The kernel's first instance (kernel.js), from the page: the
   *  filesystem's layers and what persistence kept laid into the root before
   *  the first program, the page's mounts, host calls, persistence's seed,
   *  pid 1; then the kernel's stacks handed to the processes' host (gex.mjs
   *  `kernelStacks`). */
  async #start(s, layers, records, fsi) {
    const k = await Kernel.start({ module: s.kernel.module, memory: s.kernel.memory, pageFs: this.#pageFs.control });
    this.#kernel = k;
    this.#files = new Files(k);
    this.#changes = new Changes(k, this.#files);
    this.#programs = new Programs(k, this.#files, {
      data: (h, bytes) => this.#procs.get(h)?.[INTERNAL].data(bytes),
      end: (h) => this.#procs.get(h)?.[INTERNAL].data(null),
      exit: (m) => this.#exit(m),
      watch: (m) => this.#event('watch', m),
      xWanted: () => this.#event('x-wanted', {}),
    });
    for (const l of layers) await this.#files.lay(l);
    if (records.length) await this.#files.restore(records);
    await k.r('k_fs_populate_done');
    for (const [path, id] of fsi?.mounts?.() ?? []) await this.#files.mount(path, id);
    // The page answers host calls (`hostCalls`): every process gets a
    // /dev/gex-host.
    await k.r('k_host_calls_serve');
    const keep = fsi?.persistArgs?.();
    if (keep) await this.#changes.start(keep);
    await this.#programs.start({ init: !!this.#init });
    const opt = (f, ...a) => (k.has(f) ? [{ f, a }] : []);
    const list = [
      { f: 'k_stack_area' }, { f: 'k_stack_size' }, { f: 'k_stack_count' },
      ...opt('k_snap_word'), ...opt('k_ws_addr', 0), ...opt('k_ws_count'), ...opt('k_mono_held_at'), ...opt('k_snap_extents_at'),
    ];
    const r = Object.fromEntries((await k.calls(list)).map((x, i) => [list[i].f, x.r >>> 0]));
    s.host.useKernelStacks({
      area: r.k_stack_area, size: r.k_stack_size, count: r.k_stack_count,
      ...(k.has('k_snap_word') ? { snapWord: r.k_snap_word, wsAddr: r.k_ws_addr, wsCount: r.k_ws_count } : {}),
      ...(k.has('k_mono_held_at') ? { monoHeld: r.k_mono_held_at } : {}),
      ...(k.has('k_snap_extents_at') ? { extents: r.k_snap_extents_at } : {}),
    });
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

  async #connectGuest(port, host) {
    this.#running('connectGuest');
    if (!['127.0.0.1', 'localhost', '0.0.0.0'].includes(host)) throw new Error(`connectGuest: only the guest's own 127.0.0.1, not ${host}`);
    if (!Number.isInteger(port) || port <= 0 || port > 65535) throw new TypeError(`connectGuest: no port ${port}`);
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
      await pr.connectGuest(h, port);
    } catch (e) {
      this.#procs.delete(h);
      throw e;
    }
    return c;
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
    return this.#spawn(argv, opts, false);
  }

  /** The init (`boot(argv, opts)`) as a Process, null without one. */
  get init() {
    return this.#initProcess;
  }

  #spawn(argv, opts, init) {
    [argv, opts] = programArgs(init ? 'boot' : 'spawn', argv, opts);
    const h = this.#nextHandle++;
    const stdio = opts.pty ? 'pty' : opts.detached ? 'null' : 'pipe';
    const pr = this.#programs;
    const quiet = (p) => p.catch((e) => this.#say(`gex: ${e?.message ?? e}\n`));
    const p = new Process({
      argv,
      stdio,
      encoding: opts.pty?.encoding,
      read: (fd) => pr.read(h, fd),
      unread: (fd) => quiet(pr.unread(h, fd)),
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

  /** Spawn with pipes, write `input` (if any), wait, collect:
   *  `{code, signal, stdout, stderr}`, text unless `encoding: null`. */
  async run(argv, { input, encoding = 'utf-8', ...opts } = {}) {
    const p = this.spawn(argv, { ...opts, detached: false, pty: undefined });
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

  /** Stop every program and the kernel. */
  async shutdown() {
    if (this.#state.phase === 'stopped') return;
    this.#state.phase = 'stopped';
    for (const p of this.#procs.values()) p[INTERNAL].exit({ code: 137, signal: 9, shutdown: true });
    this.#procs.clear();
    this.#byPid.clear();
    for (const parts of this.#parts.values()) for (const part of [parts].flat()) await part?.[INTERNAL]?.shutdown?.();
    this.#sys?.halt?.();
    try { await this.#run; } catch {}
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
