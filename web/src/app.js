// gex's app library: run a program on gex's browser runtime inside any page.
//
// An ejected program's web build (docs/eject.md, "The web build") is this
// module, built as `app.mjs`, plus its files. The page around it is the
// user's: it imports `start` and gives it an element.
//
//   import { start } from './app.mjs';
//   const app = await start({ container: document.body });
//   const status = await app.exited;
//
// What the build decided (which runtime pieces are in it, the units, the
// bundle and how it is split) comes from the build's launch file
// (`project/build/web/nt.json` beside app.mjs). Everything else is an option
// here, defaulting to what the build recorded (user/eject.json "start"): the
// arguments, the environment, the working directory, the size, the traces.
// Asking for a piece the build left out fails, naming the build option.
//
// No URL parsing: the page's URL is the user's. gex's own test page
// (nt.html, nt-page.js) maps its query onto these options.

import { untar } from './tar.js';
import { runProgram, globalKernel, singleMode } from './runtime.js';
import { openedFiles } from './fs-opened.js';
import { restore, persist } from './persist.js';

/** Whether this build has the scene compositor: see runtime.js. */
const GEX_COMPOSITOR = typeof __GEX_COMPOSITOR__ === 'undefined' ? true : __GEX_COMPOSITOR__;

/** What the user's eject.json says for each piece a start option may ask for. */
const PIECES = {
  compositor: '"features": {"compositor": true}',
  gl: '"features": {"gl": true}',
  translation: '"features": {"translation": true} (a build that translates what its units do not cover)',
  hostx: 'an X server in the build (hostx)',
  mem64: 'the 64-bit guest memory runtime (mem64)',
};

/**
 * Start the program. Resolves once it is running, with a handle:
 *   exited    a promise of the exit status (rejects if the run fails)
 *   kill()    stop it now (`exited` resolves 137)
 *   resize(size)  the desktop's new size: 'fit' (the container's) or {w, h}
 *   state     what a driver reads (phase, out, error, session, progress, ...)
 *   describe()    the desktop's windows, titles and pixels
 *   ready()   the start-up is over (with `ready: 'manual'`): progress 'ready'
 * Rejects when the run cannot start (no cross-origin isolation, no WebGPU
 * for a build that needs it, a download that failed, an option for a piece
 * the build left out).
 *
 * Options:
 *   container  the element the desktop fills (a canvas or a box is made in it)
 *   canvas     or: the canvas to draw into (the scene compositor's output)
 *   size       'fit' (the container's size), {w, h} or 'WxH'; default the build's
 *   args       the program's arguments (an array), instead of the recorded ones
 *   env        more environment, ['K=V', ...] or {K: V}; a name given replaces the build's
 *   cwd, exe   the working directory and program (a path in the bundle)
 *   mtimes     give the guest the bundle's file times
 *   persist    false, or {dir, db}: a guest directory kept in IndexedDB
 *   strace     true (every process) or a pid: narrate system calls
 *   trapstack  a trap's wasm frames
 *   debug      more runtime switches, {name: value}, as gex's test page's
 *              ?name=value (docs/eject.md lists them)
 *   compositor, gl, translation, hostx, mem64
 *              true asks for a runtime piece; an error if the build left it out.
 *              compositor: false draws windows straight into the page.
 *   baseUrl    where app.mjs's files are (default: beside app.mjs)
 *   launch     the build's launch file, relative to baseUrl
 *   build      the launch, already read (gex's test page; see `fromLaunch`)
 *   onLog(text)     the program's terminal output
 *   onExit(status)  called when it exits
 *   onProgress(p)   the start-up's progress, for a loading screen: called
 *              when it moves (a few times a second at most while bytes
 *              arrive; at once on a phase change, when a file is whole and
 *              when a fetch of the running program starts or ends) with `state.progress`'s
 *              copy (`progressMeter` says what is in it)
 *   ready      'frame' (the default): progress is 'ready' at the first frame
 *              shown; 'manual': the page says when, with the handle's ready()
 *              (a program whose first frame is not yet what the page waits for)
 */
export async function start(opts = {}) {
  const base = new URL(opts.baseUrl ?? new URL('.', import.meta.url), globalThis.location?.href);
  const at = (p) => (p ? new URL(String(p).replace(/^\//, ''), base).href : p);
  const state = { out: '', status: null, error: null, phase: 'loading' };
  globalThis.__nt = state;
  const meter = progressMeter(state, opts.onProgress);
  try {
    return await begin(opts, base, at, state, meter);
  } catch (e) {
    state.error = String(e?.stack ?? e);
    state.phase = 'failed';
    meter.end('failed', state.error);
    throw e;
  }
}

/**
 * The start-up's progress: `state.progress`, and `onProgress(copy)` when it
 * moves. Its phases, in order (a phase never goes back):
 *   download     the bundle and the units' files arrive: `download` is
 *                {done, total} bytes (as sent, compressed) and {filesDone,
 *                files}; a file the site did not pack adds its size when
 *                its response says it
 *   compile      the downloads are in, the units still compiling: `compile`
 *                is {done, total} units (they compile as they arrive, so
 *                `compile` moves during 'download' too)
 *   boot         the program's process has started; nothing is shown yet
 *   first-frame  a window's first pixels are in the page
 *   ready        the start-up is over: the first frame, or the page's
 *                handle.ready() with `ready: 'manual'`
 * and then 'exited' or 'failed' (with `error`). `at` holds each phase's
 * start, and `elapsed` the time at the call, in ms since start().
 */
function progressMeter(state, onProgress) {
  const t0 = performance.now();
  const order = ['download', 'compile', 'boot', 'first-frame', 'ready'];
  const p = {
    phase: 'download',
    download: { done: 0, total: 0, filesDone: 0, files: 0 },
    compile: { done: 0, total: 0 },
    at: { download: 0 },
    elapsed: 0,
    error: null,
    fetch: null,
  };
  state.progress = p;
  let timer = null;
  let last = -1e9;
  const emit = () => {
    if (timer) clearTimeout(timer);
    timer = null;
    last = performance.now();
    p.elapsed = Math.round(last - t0);
    if (!onProgress) return;
    try {
      onProgress({ ...p, download: { ...p.download }, compile: { ...p.compile }, at: { ...p.at }, fetch: p.fetch && { ...p.fetch } });
    } catch (e) {
      console.warn(`[app] onProgress: ${e?.stack ?? e}`);
    }
  };
  return {
    p,
    /** A count moved: tell the page soon (every 100 ms at most). */
    tick() {
      if (!timer) timer = setTimeout(emit, Math.max(0, 100 - (performance.now() - last)));
    },
    /** The program's own fetches while it runs (shims/browser/js/fetch-note.mjs):
     *  `p.fetch` is the latest in flight, {kind, name, done, total,
     *  inflight}, or null when none is: kind 'file' (a lazy file: name its
     *  file name, done/total its bytes read so far and its size) or 'code'
     *  (a unit of translated code: name the module it is for, '' if not
     *  known; done the bytes in, total 0). */
    watchFetches(lazyNames, unitNames) {
      if (typeof BroadcastChannel !== 'function') return;
      const ch = new BroadcastChannel('gex-fetch');
      const live = new Map(); // id -> { name, key }
      const files = new Map(); // key -> { name, done, total }
      const show = () => {
        let last = null;
        for (const f of live.values()) last = f;
        p.fetch = last ? { ...files.get(last.key), inflight: live.size } : null;
        // At once: a fetch shorter than a tick was otherwise never seen, and
        // one that ended was still shown until the next.
        emit();
      };
      ch.onmessage = ({ data: m }) => {
        if (!m || typeof m.what !== 'string') return;
        let key = m.what;
        let name;
        let kind = 'file';
        let total = 0;
        if (m.what.startsWith('unit:')) {
          const unit = m.what.slice('unit:'.length).split('/')[0];
          kind = 'code';
          name = unitNames[unit] || '';
        } else {
          try { key = decodeURIComponent(new URL(m.what, globalThis.location?.href).pathname); } catch {}
          const l = lazyNames.get(key);
          name = l?.name ?? key.split('/').pop();
          total = l?.size ?? 0;
        }
        const f = files.get(key) ?? { kind, name, done: 0, total };
        files.set(key, f);
        if (m.phase === 'start') {
          live.set(m.id, { key });
        } else {
          live.delete(m.id);
          f.done += m.bytes || 0;
          if (!f.total || f.done > f.total) f.total = Math.max(f.total, f.done);
        }
        show();
      };
      state.stopFetches = () => ch.close();
    },
    /** Bytes in, a file whole, a size learned late, a unit compiled. */
    bytes(n) { p.download.done += n; this.tick(); },
    // A file whole is said at once, so the count shown is never a tick
    // behind the bytes (a phase change is, too: see `phase`).
    fileDone() { p.download.filesDone++; emit(); },
    more(n) { p.download.total += n; },
    compiled() { p.compile.done++; this.tick(); },
    /** On to `name`, if it is further along than where it is. */
    phase(name) {
      if (!order.includes(name) || order.indexOf(name) <= order.indexOf(p.phase)) return;
      p.phase = name;
      p.at[name] = Math.round(performance.now() - t0);
      emit();
    },
    /** The run is over: 'exited' or 'failed'. */
    end(name, error = null) {
      if (p.phase === 'failed' || p.phase === 'exited') return;
      p.phase = name;
      p.at[name] = Math.round(performance.now() - t0);
      if (error) p.error = error;
      emit();
    },
  };
}

async function begin(opts, base, at, state, meter) {
  const mark = (what) => console.warn(`[app] ${what} at ${Math.round(performance.timeOrigin + performance.now()) % 1e7} (+${Math.round(performance.now())} ms)`);
  const say = (s) => {
    state.out += s;
    opts.onLog?.(s);
  };
  const b = opts.build ?? fromLaunch(await fetchJson(at(opts.launch ?? 'project/build/web/nt.json'), 'the build\'s launch file'), at);

  // What the build left out cannot be asked for.
  const has = {
    compositor: GEX_COMPOSITOR && !b.featuresOff.includes('compositor'),
    gl: !b.featuresOff.includes('gl'),
    translation: b.aot !== '1' || b.jitFallback,
    hostx: !!b.hostx,
    mem64: !!b.mem64,
  };
  for (const k of Object.keys(PIECES)) {
    if (opts[k] === true && !has[k]) {
      throw new Error(`start({ ${k}: true }): this build has no ${k === 'gl' ? 'OpenGL' : k}; it needs ${PIECES[k]} in user/eject.json, and a rebuild`);
    }
  }
  for (const r of [opts.ready, b.ready]) {
    if (r !== undefined && r !== null && r !== 'frame' && r !== 'manual') throw new Error(`start({ ready: ${JSON.stringify(r)} }): 'frame' or 'manual'`);
  }
  const compositor = has.compositor && opts.compositor !== false && b.compositor !== false;
  const gl = b.gl && opts.gl !== false;

  // The runtime's switches: the build's, then the options'. The deep readers
  // (runtime.js, compositor.js, mesa-gl.js) see these, never the page's URL.
  const sw = { ...b.switches };
  if (opts.translation === true && b.aot === '1') delete sw.aot;
  if (opts.strace) {
    sw.strace = '1';
    if (typeof opts.strace === 'number') sw.stracepid = String(opts.strace);
  }
  if (opts.trapstack) sw.trapstack = '1';
  for (const [k, v] of Object.entries(opts.debug ?? {})) {
    if (v === null || v === undefined || v === false) delete sw[k];
    else sw[k] = v === true ? '1' : String(v);
  }
  if (!compositor) sw.compositor = '0';
  const sq = new URLSearchParams(sw);
  globalThis.GEX_PAGE_SEARCH = `?${sq}`;
  globalThis.GEX_ASSET_BASE = base.href;
  setGlobals((k) => sq.get(k), b);

  if (!globalThis.crossOriginIsolated && !singleMode()) {
    throw new Error('This page needs cross-origin isolation for its threads: serve it with the headers Cross-Origin-Opener-Policy: same-origin and Cross-Origin-Embedder-Policy: require-corp (the build\'s serve.mjs and _headers do).');
  }
  if ((compositor || gl) && !navigator.gpu) {
    throw new Error('This browser has no WebGPU, which this build draws with. Use a recent desktop Chrome or Edge with hardware acceleration on.');
  }

  // The desktop: a canvas for the scene compositor, a box for the bare one.
  const container = opts.container ?? opts.canvas?.parentElement ?? document.body;
  const fitSize = () => [Math.max(640, container.clientWidth || innerWidth), Math.max(480, container.clientHeight || innerHeight)];
  const sizeOf = (s) => {
    if (s === 'fit' || s === undefined || s === null) return fitSize();
    if (typeof s === 'string') return s.split('x').map(Number);
    return [Number(s.w ?? s.width), Number(s.h ?? s.height)];
  };
  let [w, h] = sizeOf(opts.size ?? b.size);
  let canvas = opts.canvas ?? null;
  let desk = null;
  let bareDesktop = null;
  if (compositor) {
    if (!canvas) {
      canvas = document.createElement('canvas');
      canvas.style.display = 'block';
      container.append(canvas);
    }
    canvas.width = w;
    canvas.height = h;
    canvas.style.width = `${w}px`;
    canvas.style.height = `${h}px`;
  } else {
    desk = document.createElement('div');
    desk.className = 'gex-desktop';
    desk.tabIndex = -1;
    desk.style.cssText = `position: relative; overflow: hidden; width: ${w}px; height: ${h}px; background: #3a6ea5; outline: none;`;
    if (canvas) canvas.replaceWith(desk);
    else container.append(desk);
    canvas = null;
    ({ bareDesktop } = await import('./bare-desktop.js'));
  }

  state.phase = 'fetching';
  mark('fetching');
  const zstd = zstdLoader(at(b.zstd ?? 'vendor/zstddec-stream.mjs'));
  const pack = b.pack ? fetchJson(at(b.pack)).catch(() => ({})) : Promise.resolve({});
  const fetcher = { pack, zstd, base, state, meter };
  // What is to come, for the progress: the bundle's files and the units'.
  // The two small lists are read before the bundle starts, so the totals
  // are whole from the first count.
  const preList = b.preload ? await fetch(at(b.preload)).then((r) => (r.ok ? r.json() : null)).catch(() => null) : null;
  {
    const d = meter.p.download;
    for (const href of [...b.tar.map(at), ...(preList ?? []).map(([, path]) => at(path))]) {
      d.files++;
      d.total += (await packedEntry(href, fetcher))?.zsize ?? 0;
    }
    meter.p.compile.total = (preList ?? []).filter(([name]) => !name.startsWith('file:')).length;
    meter.tick();
  }
  const preloading = preList ? preloadUnits(preList, fetcher, at) : Promise.resolve(null);
  const trees = [];
  const buffers = [];
  for (const url of b.tar) {
    const tar = await fetchShared(at(url), fetcher);
    buffers.push(tar.buffer);
    mark(`fetched ${url} (${(tar.length / 1048576).toFixed(0)} MiB)`);
    // The buffer, not the array: `untar` views a buffer, copies an array.
    trees.push(untar(tar.buffer));
    mark('untarred');
  }
  const tree = trees.reduce((a, t) => ({
    files: Object.assign(a.files, t.files),
    dirs: [...a.dirs, ...t.dirs],
    modes: Object.assign(a.modes ?? {}, t.modes),
    hard: Object.assign(a.hard ?? {}, t.hard),
    links: Object.assign(a.links ?? {}, t.links),
    mtimes: Object.assign(a.mtimes ?? {}, t.mtimes),
  }));
  const files = { ...tree.files };
  // A guest directory kept in IndexedDB: what the program left there last
  // time, over the bundle's.
  const keep = opts.persist === false ? null : (opts.persist ?? b.persist ?? null);
  if (keep) {
    const saved = await restore(keep.db, keep.dir);
    for (const [p, d] of saved) files[p] = d;
    state.restored = saved.length;
    if (saved.length) say(`[app] restored ${saved.length} file(s) under ${keep.dir}\n`);
  }
  // Hard links go to the guest as links, not copies: glibc knows a loaded
  // library by its inode.
  const hard = Object.fromEntries(Object.entries(tree.hard ?? {}).filter(([, t]) => files[t]));
  // Lazy files: read from the host when the guest first reads them (a bundle
  // split's, docs/eject.md "The bundle split").
  let lazy = [];
  const lazyNames = new Map(); // a lazy file's URL path -> { name, size }
  const lazyMtimes = {};
  if (b.lazy) {
    const m = await fetchJson(at(b.lazy), 'the lazy files\' list');
    // A path on this origin, not a URL: the kernel keeps a lazy file's
    // source as a path (k_fs_add_lazy), and fetches it from the page's origin.
    const lazyBase = new URL(at(m.base)).pathname;
    for (const f of m.files) if (f[3] !== undefined) lazyMtimes[f[0]] = f[3];
    lazy = m.files.map(([path, size, rel]) => [path, size, lazyBase + rel.split('/').map(encodeURIComponent).join('/')]);
    for (const [path, size, url] of lazy) lazyNames.set(decodeURIComponent(url), { name: path.split('/').pop(), size });
    mark(`lazy: ${lazy.length} files, ${(lazy.reduce((a, f) => a + f[1], 0) / 1048576).toFixed(0)} MiB`);
  }
  const exe = opts.exe ?? b.exe;
  // A copy, not a view: a view would keep the whole bundle alive.
  const exeBytes = (files[exe] ?? files[hard[exe]])?.slice();
  if (!exeBytes) throw new Error(`${exe} is not in the bundle`);
  const args = [exe, ...(opts.args ?? b.argv ?? [])];
  const env = mergeEnv(b.env, opts.env);
  if (!env.some((e) => e.startsWith('GEX_WINE_PREFIX='))) env.push('GEX_WINE_PREFIX=/wine');
  if (!env.some((e) => e.startsWith('GEX_NT_STRICT='))) env.push('GEX_NT_STRICT=1');
  const cwd = opts.cwd ?? b.cwd ?? (exe.replace(/\/[^/]*$/, '') || '/');
  const mtimes = opts.mtimes ?? b.mtimes;
  // How it was started (gex-eject's W3 recorder keeps this).
  state.session = { exe, argv: args.slice(1), env, cwd, mtimes: !!mtimes, size: `${w}x${h}`, tar: b.tar, params: Object.fromEntries(sq) };
  // The files the run has opened so far: the recorder's read set.
  globalThis.__ntOpened = async () => openedFiles(await globalKernel());
  if (meter.p.compile.done < meter.p.compile.total) meter.phase('compile');
  const preModules = await preloading;
  if (preModules) mark(`compiled ${preModules.filter(([k]) => !k.startsWith('file:')).length} unit modules ahead, ${preModules.filter(([k]) => k.startsWith('file:')).length} files`);
  state.phase = 'running';
  mark('running');
  if (keep) {
    const p = persist(keep.db, keep.dir, await globalKernel(), {
      every: Number(sw.persistms || 30000),
      onSync: (m) => { state.persisted = m; },
    });
    globalThis.__ntPersist = () => p?.sync();
  }

  const term = {
    rows: 40,
    cols: 120,
    write(d) { say(typeof d === 'string' ? d : new TextDecoder().decode(d)); },
    writeln(s) { say(`${s}\n`); },
    onData() { return { dispose() {} }; },
    onResize() { return { dispose() {} }; },
    focus() {},
  };
  let sys = null;
  let desktop = null;
  let settle;
  meter.phase('boot');
  meter.watchFetches(lazyNames, b.unitNames ?? {});
  const killed = new Promise((r) => { settle = r; });
  const run = runProgram(term, {
    elf: exeBytes,
    files,
    dirs: tree.dirs,
    modes: tree.modes,
    hard,
    lazy,
    // A Wine prefix's `dosdevices/c:`, which Wine opens by that name.
    symlinks: tree.links ?? {},
    mtimes: mtimes ? { ...lazyMtimes, ...tree.mtimes } : undefined,
    env,
    args,
    cwd,
    tty: false,
    gl: gl ? 'compositor' : null,
    glBackend: b.glBackend ?? 'wgpu',
    wl: desk ?? canvas,
    wlInput: desk ?? canvas,
    onDesktop: desk ? (d) => { desktop = d; globalThis.__gexCompositor = bareDesktop(d, desk); } : null,
    onSystem: (s) => { sys = s; },
    hostX: !!b.hostx && opts.hostx !== false,
    preModules,
    transfer: buffers,
    // Once the kernel has its own copy of every file, nothing here may view
    // the bundle's buffer, or it stays resident beside that copy.
    onFilesLoaded: () => {
      for (const o of [files, tree.files, ...trees.map((t) => t.files)]) for (const k of Object.keys(o)) delete o[k];
      trees.length = 0;
      buffers.length = 0;
      mark('bundle released');
    },
  });
  const exited = Promise.race([run.then(({ status }) => status), killed]).then((status) => {
    state.status = status;
    state.phase = 'done';
    meter.end('exited');
    opts.onExit?.(status);
    return status;
  }, (e) => {
    state.error = String(e?.stack ?? e);
    state.phase = 'failed';
    meter.end('failed', state.error);
    throw e;
  });
  // The first frame: a window's first pixels in the page. The bare desktop
  // counts a window's paints; the scene compositor its frames drawn since
  // the first window was mapped.
  const manual = (opts.ready ?? b.ready ?? 'frame') === 'manual';
  let framesAtMap = null;
  const shown = () => {
    if (desktop) return desktop.presented?.() ?? false;
    const c = sys?.compositor?.counters?.();
    if (!c || !c.windows) return false;
    framesAtMap ??= c.frames;
    return c.frames > framesAtMap;
  };
  const watch = setInterval(() => {
    if (state.phase === 'done' || state.phase === 'failed') {
      clearInterval(watch);
    } else if (shown()) {
      clearInterval(watch);
      meter.phase('first-frame');
      if (!manual) meter.phase('ready');
    }
  }, 50);
  return {
    exited,
    state,
    kill() {
      if (state.phase === 'done' || state.phase === 'failed') return;
      sys?.host?.stop();
      sys?.compositor?.stop?.();
      globalThis.__gexSystemWorker?.terminate?.();
      settle(137);
    },
    resize(size = 'fit') {
      [w, h] = sizeOf(size);
      if (desk) {
        desk.style.width = `${w}px`;
        desk.style.height = `${h}px`;
        desktop?.resizeOutput?.(w, h);
      } else {
        sys?.compositor?.resize?.(w, h);
      }
      return { w, h };
    },
    describe: () => globalThis.__gexCompositor?.describe?.() ?? null,
    ready() {
      meter.phase('first-frame');
      meter.phase('ready');
    },
  };
}

/** `KEY=VALUE` strings, `more`'s names replacing `base`'s. */
function mergeEnv(base = [], more = null) {
  const list = Array.isArray(more) ? more : Object.entries(more ?? {}).map(([k, v]) => `${k}=${v}`);
  const names = new Set(list.map((e) => e.split('=')[0]));
  return [...base.filter((e) => !names.has(e.split('=')[0])), ...list];
}

/**
 * The build's launch file (gex-eject: a W3 page's, kind "w3", or an NT-layer
 * program's) as the options `start` reads. Site paths in it (`/project/...`)
 * are under `at`'s base.
 */
export function fromLaunch(m, at = (p) => p) {
  return withStart(launchBuild(m, at), m.start);
}

/** user/eject.json "start": the build's defaults for start()'s options. */
function withStart(b, st) {
  if (!st) return b;
  const out = { ...b, switches: { ...b.switches } };
  if (st.args) out.argv = st.args;
  if (st.env) out.env = mergeEnv(b.env, st.env);
  if (st.cwd) out.cwd = st.cwd;
  if (st.size) out.size = st.size;
  if (st.mtimes !== undefined) out.mtimes = !!st.mtimes;
  if (st.strace) {
    out.switches.strace = '1';
    if (typeof st.strace === 'number') out.switches.stracepid = String(st.strace);
  }
  if (st.trapstack) out.switches.trapstack = '1';
  if (st.persist !== undefined) out.persist = st.persist || null;
  if (st.ready) out.ready = st.ready;
  return out;
}

function launchBuild(m, at) {
  const pack = `pack.json${m.build ? `?v=${m.build}` : ''}`;
  if (m.kind === 'w3') {
    const p = { ...(m.params ?? {}) };
    const lazy = p.lazy ?? null;
    delete p.lazy;
    return {
      tar: ['project/build/web/rootfs.tar'],
      exe: m.exe,
      argv: m.argv ?? [],
      env: m.env ?? [],
      cwd: m.cwd ?? '/',
      size: m.size ?? 'fit',
      mtimes: !!m.mtimes,
      featuresOff: m.features_off ?? [],
      aot: p.aot ?? null,
      gl: false,
      pack,
      preload: 'project/build/web/units/preload.json',
      lazy,
      // A unit's module, by key: what the page names while one is fetched.
      unitNames: Object.fromEntries((m.units ?? []).map((u) => [u.key, String(u.path ?? '').split(/[\\/]/).pop()])),
      switches: { cgen: '1', cgenurl: at('project/build/web/units'), ...p },
    };
  }
  if (m.unsupported) throw new Error(`gex-eject: this project has no web build: ${m.unsupported}`);
  const switches = {
    cgen: '2',
    cgenurl: at(`project/${m.cgen ?? 'build/web/cgen'}`),
    aot: '1',
    unguarded: m.unguarded ? '1' : '0',
  };
  if (m.direct) switches.direct = '1';
  return {
    tar: ['project/build/web/rootfs.tar'],
    exe: m.exe,
    argv: m.args ?? [],
    env: [`GEX_WINE_PREFIX=${m.prefix}`, ...(m.settings ?? []), ...(m.env ?? [])],
    cwd: m.cwd && m.cwd !== '/' ? m.cwd : undefined,
    size: m.size ?? 'fit',
    mtimes: false,
    featuresOff: m.features_off ?? [],
    aot: '1',
    gl: !!m.gl,
    glBackend: 'wgpu',
    pack,
    preload: `project/${m.cgen ?? 'build/web/cgen'}/preload.json`,
    persist: m.persist ? { dir: m.persist, db: `gex-eject:${m.exe}` } : null,
    switches,
  };
}

/**
 * The runtime's globals from its switches (`get(name)`): what gex.mjs,
 * runtime.js and the Workers read. One list, for the library and the test page.
 */
export function setGlobals(get, b = {}) {
  globalThis.GEX_ARCH = 'x86';
  if (get('trapstack')) globalThis.GEX_TRAP_STACK = 1;
  if (get('doorstats')) globalThis.GEX_DOOR_STATS = Number(get('doorstats')) || 1;
  if (get('exittrace')) globalThis.GEX_EXIT_TRACE = 1;
  if (get('callouttrace')) globalThis.GEX_CALLOUT_TRACE = 1;
  if (get('heartbeat')) globalThis.GEX_HEARTBEAT = Number(get('heartbeat'));
  if (get('mem64') === '1' || b.mem64) globalThis.GEX_MEM64 = 1;
  // The function JIT: off by default (it doubles a W3 program's start-up;
  // nt-page.js said why).
  const fjit = get('fjit');
  if (fjit !== null && fjit !== '0') globalThis.GEX_FJIT = Number(fjit) || 1;
  if (get('wasmdlls')) globalThis.GEX_WASM_DLLS = get('wasmdlls');
  if (get('elfunits')) globalThis.GEX_ELF_UNITS = get('elfunits');
  if (get('fjitlibs')) globalThis.GEX_FJIT_LIBS = get('fjitlibs');
  if (get('pins')) globalThis.GEX_PINS = get('pins');
  if (get('dense') === '0') globalThis.GEX_DENSE = 0;
  if (get('dense') === '2') globalThis.GEX_DENSE = 2;
  // cgen=1|2 (prefer|require units), the unguarded flavour unless unguarded=0.
  if (get('cgen')) globalThis.GEX_CGEN = Number(get('cgen')) | (get('unguarded') === '0' ? 0 : 4);
  if (get('cgenurl')) globalThis.GEX_CGEN_URL = get('cgenurl');
  if (get('aot') === '1') globalThis.GEX_NO_JIT = 1;
  if (get('aot')) globalThis.GEX_AOT = 1;
  if (get('densedebug')) globalThis.GEX_DENSE_DEBUG = 1;
  if (get('tracemisses')) globalThis.GEX_TRACE_MISSES = 1;
  if (get('attachtrace')) globalThis.GEX_ATTACH_TRACE = 1;
  if (get('strace')) globalThis.GEX_STRACE = 1;
  if (get('stracepid')) globalThis.GEX_STRACE_PID = Number(get('stracepid'));
  if (get('liftmax')) globalThis.GEX_LIFT_MAX = Number(get('liftmax'));
  globalThis.GEX_UNIT_CACHE = get('unitcache') !== null ? Number(get('unitcache')) : 1;
  if (get('t1fold')) globalThis.GEX_T1_FOLD = Number(get('t1fold'));
  if (get('xlatereport')) globalThis.GEX_XLATE_REPORT = Number(get('xlatereport'));
  if (get('inlinedispatch')) globalThis.GEX_INLINE_DISPATCH = Number(get('inlinedispatch'));
}

async function fetchJson(url, what = url) {
  const r = await fetch(url, { cache: 'no-cache' });
  if (!r.ok) throw new Error(`${what} (${url}): ${r.status} ${r.statusText}`);
  return r.json();
}

/*
 * The site's packed files (eject-site.mjs's pack.json: site path ->
 * {size, zsize}): FILE.zst on the server, plain bytes, fetched and decoded
 * here straight into the one buffer that is then moved on. No service worker.
 */
async function packedEntry(url, { pack, base }) {
  const { files = {}, build = '' } = await pack;
  const href = new URL(url, base).href;
  const rel = href.startsWith(base.href) ? `/${href.slice(base.href.length).split('?')[0]}` : new URL(href).pathname;
  const e = files[rel];
  return e ? { ...e, build } : null;
}

/** The decoder (zstddec's streaming build), loaded only when something is
 *  packed. Fed a network chunk at a time: its memory never shrinks. */
function zstdLoader(src) {
  let p = null;
  return () => (p ??= (async () => {
    const { ZSTDDecoder } = await import(/* @vite-ignore */ src);
    const d = new ZSTDDecoder();
    await d.init();
    return d;
  })());
}

/**
 * A file in one ArrayBuffer, which is moved (not cloned) to pid 1's Worker:
 * a clone would copy it, and a SharedArrayBuffer stays resident for the
 * session. The Worker frees it once loaded.
 */
async function fetchShared(url, f, { shared = false } = {}) {
  const z = await packedEntry(url, f);
  if (z) return fetchPacked(url, z, shared, f);
  const res = await fetch(url, { cache: 'no-cache' });
  if (!res.ok) throw new Error(`${url}: ${res.status} ${res.statusText}`);
  const total = Number(res.headers.get('content-length'));
  if (!total) throw new Error(`${url}: no content-length`);
  // Not packed: its size was not in the totals.
  f.meter?.more(total);
  const out = new Uint8Array(shared ? new SharedArrayBuffer(total) : new ArrayBuffer(total));
  const reader = res.body.getReader();
  let at = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    out.set(value, at);
    at += value.length;
    f.meter?.bytes(value.length);
  }
  f.meter?.fileDone();
  if (at !== total) throw new Error(`${url}: ${at} of ${total} bytes`);
  return out;
}

async function fetchPacked(url, { size, zsize, build }, shared, f) {
  const u = new URL(url);
  u.search = build ? `v=${build}` : '';
  u.pathname += '.zst';
  const res = await fetch(u.href);
  if (!res.ok) throw new Error(`${u.href}: ${res.status} ${res.statusText}`);
  const parts = [];
  let at = 0;
  const reader = res.body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    parts.push(value);
    at += value.length;
    f.state.downloaded = (f.state.downloaded ?? 0) + value.length;
    f.meter?.bytes(value.length);
  }
  f.meter?.fileDone();
  if (at !== zsize) throw new Error(`${u.href}: ${at} of ${zsize} bytes`);
  const d = await f.zstd();
  const out = new Uint8Array(shared ? new SharedArrayBuffer(size) : new ArrayBuffer(size));
  let n = 0;
  // Synchronous to the end: one decoder serves every fetch in flight.
  for (const part of d.decodeStreaming(parts)) {
    if (n + part.length > size) throw new Error(`${url}: longer than its ${size} bytes`);
    out.set(part, n);
    n += part.length;
  }
  if (n !== size) throw new Error(`${url}: decoded ${n} of ${size} bytes`);
  return out;
}

/** Compile the units a preload list (`[[name, path], ...]`) names while the
 *  bundle downloads, a few at a time. */
async function preloadUnits(list, f, at) {
  const out = [];
  let i = 0;
  const one = async () => {
    while (i < list.length) {
      const [name, path] = list[i++];
      const href = at(path);
      try {
        if (name.startsWith('file:')) {
          // A small file of the unit cache, which every Worker would
          // otherwise fetch synchronously: shared, so each gets it uncopied.
          if (await packedEntry(href, f)) {
            out.push([name, await fetchShared(href, f, { shared: true })]);
            continue;
          }
          const b = new Uint8Array(await (await fetch(href)).arrayBuffer());
          f.meter?.fileDone();
          const s = new Uint8Array(new SharedArrayBuffer(b.byteLength));
          s.set(b);
          out.push([name, s]);
          continue;
        }
        if (await packedEntry(href, f)) {
          out.push([name, await WebAssembly.compile(await fetchShared(href, f))]);
        } else {
          const res = fetch(href);
          res.then(() => f.meter?.fileDone(), () => {});
          out.push([name, await WebAssembly.compileStreaming(res)]);
        }
        f.meter?.compiled();
      } catch (e) {
        console.warn(`[app] preload ${href}: ${e?.message ?? e}`);
      }
    }
  };
  await Promise.all([one(), one(), one(), one()]);
  return out;
}
