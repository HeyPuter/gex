// `System`: the kernel, started with `boot()`, and the programs a page runs
// in it. Everything else a system has -- its files, windows, network,
// persistence, sound, GL -- is a class of its own that takes the system as
// its first argument (web/runtime/README.md).
//
// Built on the page runtime (web/src/runtime.js `runProgram`): pid 1 is
// gex-init (web/runtime/init), which starts each program `spawn` asks for,
// and a Worker of the runtime's own (io-worker.js) holds the kernel's side of
// their terminals and pipes and reads and writes the guest's files.

import { INTERNAL, bytesOf, envList, signalNumber } from './internal.js';
import { Process } from './process.js';

/** Where pid 1's program is in the guest: under /run, which nothing
 *  persists. */
const INIT_PATH = '/run/gex/init';
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

/**
 * The guest architecture a tree's programs are built for: the machine of the
 * first native program found among the usual ones (a gexcc program, wasm32,
 * runs on either and says nothing), or null.
 */
export function treeArch({ files, symlinks = {}, hard = {} }) {
  const resolve = (p) => {
    for (let i = 0; i < 16; i++) {
      if (hard[p]) p = hard[p];
      const l = symlinks[p];
      if (l === undefined) return p;
      p = l.startsWith('/') ? l : `${p.slice(0, p.lastIndexOf('/'))}/${l}`;
      const out = [];
      for (const part of p.split('/')) {
        if (!part || part === '.') continue;
        if (part === '..') out.pop();
        else out.push(part);
      }
      p = `/${out.join('/')}`;
    }
    return p;
  };
  const machine = (b) => (b && b.length > 20 && b[0] === 0x7f && b[1] === 0x45 && b[2] === 0x4c && b[3] === 0x46 && b[4] === 2 ? EM[b[18] | (b[19] << 8)] ?? null : null);
  for (const p of ['/bin/sh', '/usr/bin/sh', '/bin/bash', '/usr/bin/env', '/sbin/init', '/bin/busybox', '/usr/bin/busybox']) {
    const m = machine(files[resolve(p)]);
    if (m) return m;
  }
  for (const b of Object.values(files)) {
    const m = machine(b);
    if (m) return m;
  }
  return null;
}

/** A Worker that answers calls, for the runtime's helpers. */
export class Rpc {
  #w; #next = 1; #calls = new Map();
  constructor(worker, onEvent) {
    this.#w = worker;
    const on = (m) => {
      if (m?.call === undefined) return onEvent?.(m);
      const c = this.#calls.get(m.call);
      if (!c) return;
      this.#calls.delete(m.call);
      if (m.error) c.reject(Object.assign(new Error(m.error.message), { code: m.error.code, errno: m.error.errno, workerStack: m.error.stack }));
      else c.resolve(m.value);
    };
    if (worker.addEventListener) worker.addEventListener('message', (e) => on(e.data));
    else worker.on('message', on);
    const fail = (e) => {
      const err = new Error(`the runtime's Worker failed: ${e?.message ?? e}`);
      for (const c of this.#calls.values()) c.reject(err);
      this.#calls.clear();
    };
    if (worker.addEventListener) worker.addEventListener('error', fail);
    else worker.on('error', fail);
  }
  call(op, args = {}, transfer = []) {
    const call = this.#next++;
    return new Promise((resolve, reject) => {
      this.#calls.set(call, { resolve, reject });
      this.#w.postMessage({ ...args, call, op }, transfer);
    });
  }
  /** A call nobody waits for (its failure is the Worker's to report). */
  send(op, args = {}, transfer = []) {
    this.call(op, args, transfer).catch(() => {});
  }
  terminate() {
    this.#w.terminate();
  }
}

async function ioWorker() {
  if (typeof Worker === 'function') return new Worker(new URL('./io-worker.js', import.meta.url), { type: 'module', name: 'gex-runtime' });
  const { Worker: NodeWorker } = await import('node:worker_threads');
  return new NodeWorker(new URL('./io-worker.js', import.meta.url));
}

export class System extends EventTarget {
  #opts;
  #state = { phase: 'new', error: null, progress: null, console: '' };
  #parts = new Map();
  #beforeBoot = [];
  #afterBoot = [];
  #io = null;
  #sys = null; // runProgram's onSystem: { host, kernel, tty, compositor, startGl }
  #run = null;
  #procs = new Map(); // handle -> Process
  #byPid = new Map(); // pid -> Process
  #early = new Map(); // pid -> exit status heard before the pid was known
  #nextHandle = 1;
  #booting = null;
  #arch = null;

  /**
   * `baseUrl`: where the runtime's files are (the shim, kernel, seed,
   * gex-init, ...); default beside the package. `debug`: the runtime's
   * switches, `{name: value}` as gex's test pages take `?name=value`;
   * unstable, for debugging only.
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
      /** `fn(m)` for each event of kind `ev` the runtime's Worker posts
       *  (a part's own: 'watch', ...). */
      on: (ev, fn) => (this.#listeners.get(ev) ?? this.#listeners.set(ev, []).get(ev)).push(fn),
      io: () => this.#io,
      sys: () => this.#sys,
      get phase() { return self.#state.phase; },
      opts: this.#opts,
      say: (text) => this.#say(text),
    };
  }

  /** `phase` ('new', 'booting', 'running', 'stopped', 'failed'), `error`,
   *  and `console`: what pid 1 and the runtime said. */
  get state() {
    return { ...this.#state };
  }

  #say(text) {
    this.#state.console += text;
    if (this.#state.console.length > 65536) this.#state.console = this.#state.console.slice(-49152);
    this.dispatchEvent(Object.assign(new Event('console'), { text }));
    if (this.#opts.debug.console) console.log(`[gex] ${text.replace(/\n$/, '')}`);
  }

  /** Start the kernel and pid 1. Subsystems that must exist before boot
   *  (the filesystem, windows, persistence) are made first. */
  boot() {
    this.#booting ??= this.#boot().catch((e) => {
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

    const init = await bytesOf(assetUrl('/gex/gex-init'), 'gex-init');
    const opts = {
      elf: init,
      // Its own file too: a fork's child finds its program by this path.
      args: [INIT_PATH],
      files: {},
      dirs: [],
      modes: {},
      hard: {},
      symlinks: {},
      lazy: [],
      env: [],
      cwd: '/',
      uid: 0,
      tty: true,
    };
    for (const fn of this.#beforeBoot) await fn(opts);
    opts.files[INIT_PATH] = init;
    opts.modes[INIT_PATH] = 0o755;

    // The translated architecture, beside which wasm32 programs (gex-init
    // among them) run: the global kernel is built for one (its syscall ABI,
    // struct layouts, signal frame), so it is chosen here, from the tree's
    // programs. A tree with none yet (wasm32 only, its native packages to
    // come) gets aarch64, Debian's arm64 being the usual second
    // architecture of a wasm32 tree. A program of the other one does not run.
    const arch = wantArch ?? treeArch(opts) ?? 'aarch64';
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
    const worker = await ioWorker();
    this.#io = new Rpc(worker, (m) => this.#event(m));
    opts.onSystem = (s) => {
      this.#sys = s;
      // A kernel stack of its own, which the host can hand out once pid 1's
      // Worker has said where the stacks are (before that its helper would
      // run on pid 1's). gex-init waits for the socket.
      const io = this.#io;
      const go = () => {
        if (this.#state.phase !== 'booting') return;
        const stackTop = s.host.takeKernelStack('runtime');
        if (stackTop === undefined) return void setTimeout(go, 10);
        io.call('start', { spec: { module: s.kernel.module, memory: s.kernel.memory }, stackTop })
          .then(() => io.call('ready'))
          .then(up.resolve, up.reject);
      };
      go();
    };
    this.#run = runProgram(term, opts);
    this.#run.then(({ status }) => {
      if (this.#state.phase === 'running' || this.#state.phase === 'booting') {
        this.#state.phase = 'stopped';
        this.#say(`gex: pid 1 ended (${status})\n`);
        up.reject(new Error(`pid 1 ended (${status}) before the system was up: ${this.#state.console.slice(-400)}`));
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
    for (const fn of this.#afterBoot.splice(0)) await fn(this.#sys);
    this.dispatchEvent(new Event('running'));
    return this;
  }

  #event(m) {
    if (m.ev === 'data' || m.ev === 'end') this.#procs.get(m.h)?.[INTERNAL].stream(m);
    else if (m.ev === 'exit') {
      const p = this.#byPid.get(m.pid);
      if (p) {
        this.#byPid.delete(m.pid);
        p[INTERNAL].exit(m);
      } else {
        this.#early.set(m.pid, m);
        if (this.#early.size > 4096) this.#early.delete(this.#early.keys().next().value);
      }
    } else if (m.ev === 'log') this.#say(`${m.text}\n`);
    else if (m.ev === 'init-gone') this.#say('gex: pid 1 closed the runtime\'s connection\n');
    else for (const fn of this.#listeners.get(m.ev) ?? []) fn(m);
  }
  #listeners = new Map();

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
    if (!Array.isArray(argv) || !argv.length || argv.some((a) => typeof a !== 'string')) {
      throw new TypeError('spawn: argv is a non-empty array of strings');
    }
    for (const k of Object.keys(opts)) {
      if (!['pty', 'env', 'cwd', 'uid', 'gid', 'detached'].includes(k)) throw new TypeError(`spawn: no option ${k}`);
    }
    const h = this.#nextHandle++;
    const stdio = opts.pty ? 'pty' : opts.detached ? 'null' : 'pipe';
    const io = this.#io;
    const p = new Process({
      argv,
      stdio,
      encoding: opts.pty?.encoding,
      write: (bytes) => io.send('write', { h, bytes }, [bytes.buffer]),
      closeStdin: () => io.send('closeStdin', { h }),
      resize: (cols, rows) => io.send('resize', { h, cols, rows }),
      kill: (pid, sig) => io.send('kill', { pid, signal: signalNumber(sig) }),
      release: () => {
        this.#procs.delete(h);
        io.send('release', { h });
      },
      windows: (pid) => this.#parts.get('windows')?.[INTERNAL]?.of?.(pid) ?? [],
    });
    this.#procs.set(h, p);
    io.call('spawn', {
      h,
      argv,
      env: envList(opts.env),
      cwd: opts.cwd ?? '',
      uid: opts.uid ?? -1,
      gid: opts.gid ?? opts.uid ?? -1,
      stdio,
      cols: opts.pty?.cols ?? 80,
      rows: opts.pty?.rows ?? 24,
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
    try { await this.#sys?.host?.stop(); } catch {}
    this.#sys?.compositor?.stop?.();
    this.#io?.terminate();
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
