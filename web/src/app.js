// gex's app library: run a program on gex's browser runtime inside any page.
//
// An ejected program's web build (crates/gex-eject/README.md, "The web build") is this
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
import { makeControl as lazyCacheControl, lazyVersion } from '../../shims/browser/js/lazy-cache.mjs';
import { runProgram, globalKernel, singleMode, primeRuntime, runtimeUrls, workerGraphBytes } from './runtime.js';
import { openedFiles } from './fs-opened.js';
import { restore, persist } from './persist.js';
import { logOn } from '../../shims/browser/js/log.mjs';

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
 *   uid        the guest's user and group id (default the build's, else 1000)
 *   mtimes     give the guest the bundle's file times
 *   persist    false, or {dir, db}: a guest directory kept in IndexedDB
 *   wisp       a Wisp proxy's URL (ws:// or wss://, ending in a slash): the
 *              guest's network (its TCP and DNS go over it); none by default
 *   strace     true (every process) or a pid: narrate system calls
 *   trapstack  a trap's wasm frames
 *   debug      more runtime switches, {name: value}, as gex's test page's
 *              ?name=value (crates/gex-eject/README.md lists them)
 *   compositor, gl, translation, hostx, mem64
 *              true asks for a runtime piece; an error if the build left it out.
 *              compositor: false draws windows straight into the page.
 *   glLazy     start Mesa at the first GL request instead of at boot (a 32-bit Wine
 *              build's default)
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
  // The guest Worker's modules: fetched, or from the page's cache (worker-graph.js).
  state.workerGraph = workerGraphBytes;
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
 *
 * From 'boot' on, what the program has reached (a 32-bit Wine build's units, as each
 * process installs them: shims/browser/js/fetch-note.mjs `noteStatus`):
 *   boot   {units: {done, total}, programs: {done, total}, program, image}:
 *          the build's units installed so far (distinct images) of all it
 *          ships, the .exe ones among them (each a process), the latest
 *          program started and the latest image loaded (file names; a
 *          native wasm32 module, a Wine builtin, counts there but is no
 *          unit); a build without units has totals of 0
 *   log    the latest events, oldest first, at most 50: {t (ms since
 *          start()), kind ('phase', 'program', 'image', 'fetch'), text}
 */
function progressMeter(state, onProgress) {
  const t0 = performance.now();
  const order = ['download', 'compile', 'boot', 'first-frame', 'ready'];
  const p = {
    phase: 'download',
    download: { done: 0, total: 0, filesDone: 0, files: 0, cached: 0 },
    compile: { done: 0, total: 0 },
    at: { download: 0 },
    elapsed: 0,
    error: null,
    fetch: null,
    boot: { units: { done: 0, total: 0 }, programs: { done: 0, total: 0 }, program: null, image: null },
    log: [],
  };
  state.progress = p;
  const log = (kind, text) => {
    p.log.push({ t: Math.round(performance.now() - t0), kind, text });
    if (p.log.length > 50) p.log.splice(0, p.log.length - 50);
  };
  let timer = null;
  let last = -1e9;
  const emit = () => {
    if (timer) clearTimeout(timer);
    timer = null;
    last = performance.now();
    p.elapsed = Math.round(last - t0);
    if (!onProgress) return;
    try {
      const b = p.boot;
      onProgress({
        ...p, download: { ...p.download }, compile: { ...p.compile }, at: { ...p.at }, fetch: p.fetch && { ...p.fetch },
        boot: { ...b, units: { ...b.units }, programs: { ...b.programs } }, log: p.log.map((l) => ({ ...l })),
      });
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
     *  known; done the bytes in, total 0); `owner` the .exe of the process
     *  that fetched it, '' if not known yet. */
    watchFetches(lazyNames, unitNames) {
      if (typeof BroadcastChannel !== 'function') return;
      const ch = new BroadcastChannel('gex-fetch');
      const live = new Map(); // id -> { name, key }
      const files = new Map(); // key -> { name, done, total }
      // The build's units, and which of them a process has installed.
      const base = (path) => String(path ?? '').split(/[\\/]/).pop();
      // A unit fetch as said to the user: the module, and the program it is for.
      const codeFor = (f) => {
        if (f.name && f.owner && f.name.toLowerCase() !== f.owner.toLowerCase()) return `code for ${f.name} (${f.owner})`;
        return `code for ${f.name || f.owner || 'the program'}`;
      };
      const shipped = new Map(Object.entries(unitNames));
      const isExe = (name) => /\.exe$/i.test(name);
      p.boot.units.total = shipped.size;
      p.boot.programs.total = [...shipped.values()].filter(isExe).length;
      const seen = new Set();
      const modules = new Set();
      // Each thread of a process installs its modules: a program is said once
      // per command line.
      const started = new Set();
      const start = (name, st) => {
        const k = `${st.path} ${st.program}`;
        if (started.has(k)) return;
        started.add(k);
        p.boot.program = name;
        log('program', `Started ${name}`);
      };
      const status = (st) => {
        if (st?.kind === 'module') {
          // A native module (a Wine builtin): a program when it is an .exe,
          // else an image, said once; not one of the build's units.
          const name = base(st.path);
          if (st.program !== undefined) start(name, st);
          else if (!modules.has(st.path)) {
            p.boot.image = name;
            log('image', `Loaded ${name}`);
          }
          modules.add(st.path);
          this.tick();
          return;
        }
        if (st?.kind !== 'unit') return;
        const name = base(st.path);
        const first = !seen.has(st.key);
        seen.add(st.key);
        if (first && shipped.has(st.key)) {
          p.boot.units.done++;
          if (isExe(name)) p.boot.programs.done++;
        }
        if (st.program !== undefined) start(name, st);
        else if (first) {
          p.boot.image = name;
          log('image', st.hit ? `Loaded ${name}` : `Loaded ${name} (no unit: translated as it runs)`);
        }
        this.tick();
      };
      const show = () => {
        let last = null;
        for (const f of live.values()) last = f;
        p.fetch = last ? { ...files.get(last.key), inflight: live.size } : null;
        // At once: a fetch shorter than a tick was otherwise never seen, and
        // one that ended was still shown until the next.
        emit();
      };
      ch.onmessage = ({ data: m }) => {
        if (m?.status) { status(m.status); return; }
        if (!m || typeof m.what !== 'string') return;
        // A unit's indirect-call side file: a probe the runtime makes, not
        // code the program waits for.
        if (m.what.startsWith('unit:icall/')) return;
        let key = m.what;
        let name;
        let kind = 'file';
        let total = 0;
        if (m.what.startsWith('unit:')) {
          const rel = m.what.slice('unit:'.length);
          const unit = rel.split('/')[0];
          kind = 'code';
          // A native module (a Wine builtin) is `providers/<name>-<fingerprint>.wasm`.
          name = unit === 'providers' ? rel.slice('providers/'.length).replace(/-[0-9a-f]+\.wasm$/i, '') : unitNames[unit] || '';
        } else {
          try { key = decodeURIComponent(new URL(m.what, globalThis.location?.href).pathname); } catch {}
          const l = lazyNames.get(key);
          name = l?.name ?? key.split('/').pop();
          total = l?.size ?? 0;
        }
        // The fetching process's .exe (fetch-note.mjs), '' before its main image is in.
        const owner = typeof m.owner === 'string' ? m.owner : '';
        const f = files.get(key) ?? { kind, name, owner, done: 0, total };
        if (!f.owner && owner) f.owner = owner;
        if (!files.has(key)) log('fetch', kind === 'code' ? `Fetching ${codeFor(f)}` : `Fetching ${name}`);
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
    /** Bytes the page had kept (a piece in its cache): counted as done, and
     *  as `cached`, so the page can say they came from there. */
    cached(n) { p.download.done += n; p.download.cached += n; this.tick(); },
    // A file whole is said at once, so the count shown is never a tick
    // behind the bytes (a phase change is, too: see `phase`).
    fileDone() { p.download.filesDone++; emit(); },
    more(n) { p.download.total += n; },
    compiled() { p.compile.done++; this.tick(); },
    /** On to `name`, if it is further along than where it is. */
    phase(name) {
      if (!order.includes(name) || order.indexOf(name) <= order.indexOf(p.phase)) return;
      const from = p.phase;
      p.phase = name;
      p.at[name] = Math.round(performance.now() - t0);
      const secs = (ms) => `${(ms / 1000).toFixed(1)} s`;
      if (from === 'download') {
        const c = p.download.cached;
        log('phase', `Downloaded ${((p.download.done - c) / 1e6).toFixed(1)} MB${c ? `, ${(c / 1e6).toFixed(1)} MB from cache,` : ''} in ${secs(p.at[name])}`);
      }
      if (name === 'boot') log('phase', p.compile.total ? `Compiled ${p.compile.done} units; starting the program` : 'Starting the program');
      if (name === 'first-frame') log('phase', `First window after ${secs(p.at[name])}`);
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
  // Start-up's steps, as debug output (`?log=1`). `always`: the fill's end and
  // its late units, which are said by default (tests/eject/wine32-page.mjs reads them).
  const mark = (what, always = false) => {
    if (always || logOn('debug')) console.warn(`[app] ${what} at ${Math.round(performance.timeOrigin + performance.now()) % 1e7} (+${Math.round(performance.now())} ms)`);
  };
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
    if ((opts[k] === true || (k === 'hostx' && opts[k] === 'rootful')) && !has[k]) {
      throw new Error(`start({ ${k}: true }): this build has no ${k === 'gl' ? 'OpenGL' : k}; it needs ${PIECES[k]} in user/eject.json, and a rebuild`);
    }
  }
  for (const r of [opts.ready, b.ready]) {
    if (r !== undefined && r !== null && r !== 'frame' && r !== 'manual') throw new Error(`start({ ready: ${JSON.stringify(r)} }): 'frame' or 'manual'`);
  }
  const compositor = has.compositor && opts.compositor !== false && b.compositor !== false;
  const gl = b.gl && opts.gl !== false;
  // Mesa started by the first GL request rather than at boot.
  const glLazy = gl && (opts.glLazy ?? b.glLazy ?? false);

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
  // A unit split by function (gex_cgen::split): its Workers decode the cold
  // clusters they fetch with the same zstd decoder.
  // A unit served compressed (cgenzst=1) is decoded with it too.
  if (sq.get('cold') || sq.get('cgenzst') === '1') {
    const src = at(b.zstd ?? 'vendor/zstddec-stream.mjs');
    globalThis.GEX_CGEN_ZSTD = src;
    // With compressed units, fetched once, here: every Worker imports it
    // from memory, not with a request of its own at its start.
    if (sq.get('cgenzst') === '1') try {
      const r = await fetch(src);
      if (r.ok) globalThis.GEX_CGEN_ZSTD = URL.createObjectURL(new Blob([await r.text()], { type: 'text/javascript' }));
    } catch { /* the URL, then */ }
  }

  if (!globalThis.crossOriginIsolated && !singleMode()) {
    throw new Error('This page needs cross-origin isolation for its threads: serve it with the headers Cross-Origin-Opener-Policy: same-origin and Cross-Origin-Embedder-Policy: require-corp (the build\'s serve.mjs and _headers do).');
  }
  // A lazy GL needs WebGPU only once a program asks for it, and fails then
  // as a GL device that will not start (runtime.js `startHostGl`).
  if ((compositor || (gl && !glLazy)) && !navigator.gpu) {
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
  const fetcher = { pack, zstd, base, state, meter, pieces: pieceStore(), files: fileStore() };
  // The lazy files' list, beside the bundle rather than after it. Packed
  // (pack.json names it, with its bytes' hash) it comes compressed and is
  // kept in the page's cache like the bundle, so a visit never fetches it
  // again, whatever the HTTP cache keeps.
  const lazyList = b.lazy ? (async () => {
    const href = at(b.lazy);
    if (!(await packedEntry(href, fetcher))?.h) return fetchJson(href, 'the lazy files\' list');
    const bytes = await fetchShared(href, fetcher);
    return JSON.parse(new TextDecoder().decode(bytes));
  })() : null;
  lazyList?.catch(() => {});
  // The background fill's list (eject-site.mjs --late): lazy files the page
  // downloads once the boot's files are in, while the guest runs.
  const fillList = b.fill ? fetchJson(at(b.fill), 'the fill list').catch((e) => { say(`[app] no fill: ${e}\n`); return null; }) : null;
  // The page compositor's Mesa, packed and kept (`mesaSource`); fetched in
  // the background once the boot's and the fill's downloads are done.
  // Not before the start-up is over (\`ready\`) and the boot's downloads,
  // the fill's included, are done, and then after a pause: the program's
  // first seconds after its first frame (the shell's taskbar, its tray)
  // still read lazy files, which the download would share the link with.
  // The scene compositor's Mesa too: compositor.js resolves the source on
  // the page and hands its bytes to the compositor's worker, so Mesa comes
  // from the site's packed files and the page's cache, never as plain files.
  const mesa = gl ? mesaSource(fetcher, at) : null;
  const mesaWait = { ready: false, fetched: false, started: false };
  const prefetchMesa = (why) => {
    mesaWait[why] = true;
    if (!mesa || !glLazy || sq.get('glprefetch') === '0') return;
    if (!mesaWait.ready || !mesaWait.fetched || mesaWait.started) return;
    mesaWait.started = true;
    setTimeout(() => {
      const t0 = performance.now();
      mesa.prefetch().then((n) => n && mark(`Mesa prefetched: ${n} files in ${Math.round(performance.now() - t0)} ms`));
    }, MESA_PREFETCH_PAUSE_MS);
  };
  // The bundle's file bytes in pieces, kept in the page's cache.
  const pieceList = b.pieces ? await fetchJson(at(b.pieces), 'the pieces\' list') : null;
  // What is to come, for the progress: the bundle's files and the units'.
  // The two small lists are read before the bundle starts, so the totals
  // are whole from the first count.
  const preList = b.preload ? await fetch(at(b.preload)).then((r) => (r.ok ? r.json() : null)).catch(() => null) : null;
  {
    const d = meter.p.download;
    for (const href of [...b.tar.map(at), ...(pieceList?.pieces ?? []).map(([url]) => at(url)), ...(preList ?? []).map(([, path]) => at(path))]) {
      d.files++;
      d.total += (await packedEntry(href, fetcher))?.zsize ?? 0;
    }
    // The runtime's modules and the lazy files' list, the ones the site
    // packed (the rest load as they always did, outside these counts).
    for (const href of [...Object.values(runtimeUrls()), ...(b.lazy ? [at(b.lazy)] : [])]) {
      const e = await packedEntry(href, fetcher);
      if (e) { d.files++; d.total += e.zsize ?? 0; }
    }
    // A units archive (`tar:N`) holds N modules.
    meter.p.compile.total = (preList ?? []).filter(([name]) => !name.startsWith('file:'))
      .reduce((n, [name]) => n + (name.startsWith('tar:') ? Number(name.slice(4)) || 0 : 1), 0);
    meter.tick();
  }
  const preloading = preList ? preloadUnits(preList, fetcher, at) : Promise.resolve(null);
  // The runtime's wasm, compressed in the pack when the site packed it
  // (plain, the shim alone was 6.6 MB of Paint's download), and fetched now,
  // first, when it did not: a module left for pid 1 to fetch (the 2 KB seed,
  // which packing does not shrink) waited 90 ms behind the bundle and the fill.
  const runtime = Promise.all(Object.entries(runtimeUrls()).map(async ([k, url]) =>
    [k, await fetchShared(url, fetcher, { priority: 'high' })]))
    .then((kv) => primeRuntime(Object.fromEntries(kv)));
  const trees = [];
  const buffers = [];
  // The pieces, four at a time, beside the bundle's archive.
  const pieceBufs = pieceList ? (async () => {
    const out = new Array(pieceList.pieces.length);
    let next = 0;
    const one = async () => {
      while (next < out.length) {
        const i = next++;
        out[i] = await fetchShared(at(pieceList.pieces[i][0]), fetcher);
      }
    };
    await Promise.all([one(), one(), one(), one()]);
    return out;
  })() : null;
  pieceBufs?.catch(() => {});
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
    xattrs: Object.assign(a.xattrs ?? {}, t.xattrs),
  }));
  if (pieceBufs) {
    // The archive holds each such file empty (its mode, its time); its bytes
    // are a piece's, viewed, not copied.
    const bufs = await pieceBufs;
    for (const u of bufs) buffers.push(u.buffer);
    for (const [path, i, off, n] of pieceList.files) {
      // A hard link's name is its target's inode (below), not a copy: a copy
      // has no time of its own (the archive gives a link none), so Wine's
      // wineboot saw /wine32/share/wine/wine.inf from 1970, took the prefix for
      // stale and ran wine.inf's whole install on every boot of the page.
      if (tree.hard?.[path]) continue;
      const u = bufs[i];
      tree.files[path] = new Uint8Array(u.buffer, u.byteOffset + off, n);
    }
    mark(`pieces in (${pieceList.pieces.length}, ${pieceList.files.length} files)`);
  }
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
  // split's, crates/gex-eject/README.md "The bundle split").
  let lazy = [];
  const lazyNames = new Map(); // a lazy file's URL path -> { name, size }
  const lazyMtimes = {};
  if (b.lazy) {
    const m = await lazyList;
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
  if (!env.some((e) => e.startsWith('GEX_WIN_STRICT='))) env.push('GEX_WIN_STRICT=1');
  // The browser's time zone, unless the caller names one: without TZ the
  // guest's libc is on UTC (no /etc/localtime), and Wine's clock with it.
  // A zoneinfo name (':' = a file under /usr/share/zoneinfo, which Wine also
  // maps to its Windows zone).
  if (!env.some((e) => e.startsWith('TZ='))) {
    let zone = null;
    try { zone = Intl.DateTimeFormat().resolvedOptions().timeZone; } catch { /* none */ }
    if (zone) env.push(`TZ=:${zone}`);
  }
  const cwd = opts.cwd ?? b.cwd ?? (exe.replace(/\/[^/]*$/, '') || '/');
  const mtimes = opts.mtimes ?? b.mtimes;
  // How it was started (gex-eject's 32-bit Wine recorder keeps this).
  state.session = { exe, argv: args.slice(1), env, cwd, mtimes: !!mtimes, size: `${w}x${h}`, tar: b.tar, params: Object.fromEntries(sq) };
  // The files the run has opened so far: the recorder's read set.
  globalThis.__ntOpened = async () => openedFiles(await globalKernel());
  if (meter.p.compile.done < meter.p.compile.total) meter.phase('compile');
  const preModules = await preloading;
  await runtime;
  if (preModules) mark(`compiled ${preModules.filter(([k]) => k.startsWith('cgen:')).length} unit modules ahead, ${preModules.filter(([k]) => k.startsWith('file:')).length} files`);
  // The fill: its buffer and counter made now, every Worker given them; its
  // download started now, after the boot's, so it does not share their link.
  let fill = null;
  const lateUnits = new Map();
  const fl = fillList ? await fillList : null;
  if (fl && typeof SharedArrayBuffer === 'function') {
    const buf = new SharedArrayBuffer(fl.size);
    const counter = new Int32Array(new SharedArrayBuffer(4));
    fill = { buf, at: counter, files: fl.files };
    const t0 = performance.now();
    // The late units (eject-site.mjs --late: `unit:KEY/FILE` entries), each
    // compiled once its bytes are in and added to the modules every Worker
    // started from then on is given (`preModules`, in the host's `extra`); a
    // Worker started before takes the bytes from the fill itself
    // (worker.mjs `loadFile`).
    const late = fl.files.filter(([p]) => p.startsWith('unit:'));
    let lateN = 0;
    // Each late unit's Module, promised now: what the host hands a process
    // that maps one before it is in (runtime.js `lateFirst`).
    for (const [p] of late) {
      const m = /^unit:([^/]+)\/c\.wasm$/.exec(p);
      if (!m || (fl.demand ?? []).includes(m[1])) continue;
      let resolve, reject;
      const pr = new Promise((y, n) => { resolve = y; reject = n; });
      pr.catch(() => {});
      lateUnits.set(m[1], Object.assign(pr, { resolve, reject }));
    }
    const add = (entry) => {
      const k = entry[0].startsWith('cgen:') ? entry[0].slice(5) : null;
      if (k) lateUnits.get(k)?.resolve(entry[1]);
      if (!preModules) return;
      // Before `closed`, which a Worker reads as the end of the list.
      const c = preModules.findIndex(([k]) => k === 'closed');
      preModules.splice(c < 0 ? preModules.length : c, 0, entry);
    };
    // Downloaded, decoded into `buf` and its late units compiled in a Worker
    // (fill-worker.js): on this thread the decode held pid 1's start-up,
    // whose every await waited behind a chunk (running to pid 1 ready took
    // 110-160 ms beside it, 53-69 ms before the fill).
    const z = await packedEntry(at(fl.url), fetcher);
    const fw = new Worker(new URL('./fill-worker.js', import.meta.url), { type: 'module', name: 'fill' });
    const failed = (e) => {
      for (const p of lateUnits.values()) p.reject(new Error(`the fill failed: ${e}`));
      Atomics.store(counter, 0, -1);
      Atomics.notify(counter, 0);
      say(`[app] the fill failed: ${e}\n`);
      fw.terminate();
    };
    fw.onerror = (e) => failed(e.message ?? e);
    fw.onmessage = ({ data: m }) => {
      if (m.kind === 'bytes') {
        if (m.cached) state.cached = (state.cached ?? 0) + m.n;
        else state.downloaded = (state.downloaded ?? 0) + m.n;
      } else if (m.kind === 'late') {
        if (m.error) say(`[app] a late unit failed: ${m.error}\n`);
        else add(m.entry);
        if (++lateN === late.length) mark(`late units: ${late.length} files from the fill in ${Math.round(performance.now() - t0)} ms`, true);
      } else if (m.kind === 'done') {
        mark(`fill: ${fl.files.length} files, ${(fl.size / 1048576).toFixed(1)} MiB in ${Math.round(performance.now() - t0)} ms`, true);
        prefetchMesa('fetched');
        // Its late units may still be compiling: it ends with the page.
      } else if (m.kind === 'failed') {
        failed(m.error);
      }
    };
    if (!z) failed(`${fl.url} is not in the pack`);
    else {
      const u = new URL(at(fl.url));
      u.search = z.build ? `v=${z.build}` : '';
      u.pathname += '.zst';
      fw.postMessage({ url: u.href, zsize: z.zsize, size: z.size, buf, counter: counter.buffer, zstd: at(b.zstd ?? 'vendor/zstddec-stream.mjs'), late, demand: fl.demand ?? [], ...(/\/pieces\//.test(fl.url) ? { cache: PIECE_CACHE } : {}) });
    }
  }
  // The lazy files kept in the page's cache (lazy-cache.mjs): one Worker
  // owns it and every Worker's read of a lazy file asks it first. Needs
  // shared memory (the SP build reads lazy files from the network) and a
  // Cache Storage that holds (`?lazycache=0` turns it off).
  let lazyCache = null;
  const lazyVersions = Object.fromEntries(lazy.map(([path, size, url]) => [url, lazyVersion(size, lazyMtimes[path])]));
  if (lazy.length && typeof SharedArrayBuffer === 'function' && sw.lazycache !== '0') {
    const k = await globalKernel();
    if (k?.memory) {
      const ctl = lazyCacheControl();
      const w = new Worker(new URL('./lazy-cache-worker.js', import.meta.url), { type: 'module', name: 'lazy-cache' });
      const lc = (state.lazyCache = { on: false, net: 0, cached: 0 });
      const ready = await new Promise((r) => {
        w.onmessage = ({ data: m }) => {
          if (m.kind === 'ready') r(m);
          else if (m.kind === 'bytes') {
            lc.net += m.net;
            lc.cached += m.cached;
            state.downloaded = (state.downloaded ?? 0) + m.net;
            state.cached = (state.cached ?? 0) + m.cached;
          }
        };
        w.onerror = (e) => r({ ok: false, why: e.message ?? String(e) });
        setTimeout(() => r({ ok: false, why: 'no answer in 5 s' }), 5000);
        w.postMessage({ ctl, memory: k.memory, cache: PIECE_CACHE, versions: lazyVersions });
      });
      if (ready.ok) {
        lc.on = true;
        lazyCache = { ctl };
      } else {
        w.terminate();
        lc.why = ready.why;
        say(`[app] lazy files not cached: ${ready.why}\n`);
      }
    }
  }
  state.phase = 'running';
  mark('running');
  // What this build no longer names goes from the cache: its pieces, its
  // units' archives, its lazy files.
  prunePieces(fetcher, [
    ...(pieceList?.pieces ?? []).map(([u]) => u),
    ...(preList ?? []).map(([, u]) => u),
    ...lazy.map(([, , u]) => u),
    ...(fl ? [fl.url] : []),
  ], lazyVersions).catch(() => {});
  pruneFiles(fetcher).catch(() => {});
  // No fill to wait for: the boot's downloads are done.
  if (!fl) prefetchMesa('fetched');
  if (keep) {
    const p = persist(keep.db, keep.dir, await globalKernel(), {
      every: Number(sw.persistms || 30000),
      // `persistedWrote`: the files every snapshot of this visit stored,
      // the ones it took of its own after a save included.
      onSync: (m) => { state.persisted = m; state.persistedWrote = (state.persistedWrote ?? 0) + (m.wrote ?? 0); },
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
  // The guest's network: a Wisp proxy the page names (runtime.js serves it
  // to every Worker). With none, the guest's sockets reach nothing.
  let network = null;
  if (opts.wisp) {
    const { WispNetwork } = await import('./net.js');
    network = new WispNetwork(String(opts.wisp), (e) => say(`[net] ${e}\n`));
    // Opened and waited for, as worker.mjs does: constructing one only
    // remembers the address, and until the proxy has said hello `open`
    // answers zero, which the shim turns into ENETUNREACH (every guest
    // connect failed). A proxy that cannot be reached does not stop the run:
    // the guest gets a network that fails, and the page says so.
    try {
      await network.start();
    } catch (e) {
      say(`[net] ${e?.message ?? e}\n`);
    }
  }
  const run = runProgram(term, {
    network,
    elf: exeBytes,
    files,
    dirs: tree.dirs,
    modes: tree.modes,
    hard,
    lazy,
    // A Wine prefix's `dosdevices/c:`, which Wine opens by that name.
    symlinks: tree.links ?? {},
    mtimes: mtimes ? { ...lazyMtimes, ...tree.mtimes } : undefined,
    // Wine's DOS attributes on the prefix (`tar --xattrs`).
    xattrs: tree.xattrs,
    env,
    args,
    cwd,
    uid: opts.uid ?? b.uid,
    tty: false,
    gl: gl ? 'compositor' : null,
    glLazy,
    glSource: mesa,
    glBackend: b.glBackend ?? 'wgpu',
    d3d: !!b.d3d,
    wl: desk ?? canvas,
    wlInput: desk ?? canvas,
    onDesktop: desk ? (d) => { desktop = d; globalThis.__gexCompositor = bareDesktop(d, desk); } : null,
    onSystem: (s) => { sys = s; },
    // `hostx: 'rootful'` (or the build's default): one X screen, fullscreen,
    // under the X session's own window manager (see hostx.js).
    hostX: !!b.hostx && opts.hostx !== false
      && ((opts.hostx === 'rootful' || (b.hostxRootful && opts.hostx === undefined)) ? 'rootful' : true),
    preModules,
    lateUnits,
    fill,
    lazyCache,
    identities: b.identities ?? null,
    stateful: b.stateful ?? null,
    providerSources: b.providerSources ?? null,
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
      if (!manual) {
        meter.phase('ready');
        prefetchMesa('ready');
      }
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
      prefetchMesa('ready');
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
 * The build's launch file (gex-eject: a 32-bit Wine page's, kind "wine32") as
 * the options `start` reads. Site paths in it (`/project/...`)
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
  // "rootful": the X server is one fullscreen window the X session's own
  // window manager runs in, rather than a window per X window.
  if (st.hostx === 'rootful') out.hostxRootful = true;
  return out;
}

function launchBuild(m, at) {
  const pack = `pack.json${m.build ? `?v=${m.build}` : ''}`;
  // ("w3": a build made before the rename.)
  if (m.kind === 'wine32' || m.kind === 'w3') {
    const p = { ...(m.params ?? {}) };
    const lazy = p.lazy ?? null;
    delete p.lazy;
    const fill = p.fill ?? null;
    delete p.fill;
    const identities = p.identities ?? null;
    delete p.identities;
    // Stateful providers (`dll=file,...` under the units).
    const stateful = p.stateful ?? null;
    delete p.stateful;
    // The bundle's file bytes in pieces (eject-site.mjs --pieces).
    const pieces = p.pieces ?? null;
    delete p.pieces;
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
      // Wine's opengl32 reaches Mesa through /dev/gex-gl. Started by the
      // first process that asks, so a visit whose programs draw no GL does
      // not download it (`glLazy`; nt.json `gl_lazy: false` starts it at
      // boot, to compare).
      gl: !(m.features_off ?? []).includes('gl'),
      glLazy: m.gl_lazy ?? true,
      pack,
      preload: 'project/build/web/units/preload.json',
      lazy,
      fill,
      identities,
      stateful,
      pieces,
      // A unit's module, by key: what the page names while one is fetched.
      unitNames: Object.fromEntries((m.units ?? []).map((u) => [u.key, String(u.path ?? '').split(/[\\/]/).pop()])),
      switches: { cgen: '1', cgenurl: at('project/build/web/units'), ...p },
    };
  }
  if (m.unsupported) throw new Error(`gex-eject: this project has no web build: ${m.unsupported}`);
  throw new Error(`gex-eject: the build's launch file is of kind ${JSON.stringify(m.kind)}, not a 32-bit Wine page's`);
}

/**
 * The runtime's globals from its switches (`get(name)`): what gex.mjs,
 * runtime.js and the Workers read. One list, for the library and the test page.
 */
export function setGlobals(get, b = {}) {
  // The guest's architecture: x86 unless the build says aarch64 (a Linux
  // tree of Debian's arm64 packages), which picks the aarch64 shim, seed and
  // kernel (runtime.js).
  globalThis.GEX_ARCH = b.arch === 'aarch64' ? 'aarch64' : 'x86';
  if (get('trapstack')) globalThis.GEX_TRAP_STACK = 1;
  if (get('doorstats')) globalThis.GEX_DOOR_STATS = Number(get('doorstats')) || 1;
  // fcachestats=1: the function probe cache's misses, every 10 s (cgen.rs `fcache_stat`).
  if (get('fcachestats')) globalThis.GEX_FCACHE_STATS = 1;
  if (get('exittrace')) globalThis.GEX_EXIT_TRACE = 1;
  if (get('callouttrace')) globalThis.GEX_CALLOUT_TRACE = 1;
  // straytrap=1: a return to native Wine's pad past open call-outs stays a breakpoint (cgen.rs STRAY_TRAP).
  if (get('straytrap')) globalThis.GEX_STRAY_TRAP = 1;
  // winesplit=1: 32-bit Wine processes take the 2 GiB split (the shim's heap above
  // 2 GiB), not the compact space: for a game that needs all of Windows' 2 GiB.
  if (get('winesplit')) globalThis.GEX_WINE32_SPLIT = 1;
  // nullguard=1: guest accesses below 64 KiB fault, as a null pointer's do
  // natively (gex_abi::null_guard). A debugging opt-in: off, tier 1 and the
  // units are as without it; on, 2-8% slower in tier 1.
  if (get('nullguard') === '1') globalThis.GEX_NULL_GUARD = 1;
  // mxcsr=1: SSE honours MXCSR's rounding control in every tier
  // (gex_ir::mxcsr_exact), for a program that needs it. Off, SSE rounds to
  // nearest, and tier 1 and the units are as without it.
  if (get('mxcsr') === '1') globalThis.GEX_MXCSR = 1;
  // pinregion=BYTES: address space above the placement window for the
  // project's libraries, each at its build's base (gex_abi::set_pin_region).
  if (get('pinregion')) globalThis.GEX_PIN_REGION = Number(get('pinregion'));
  // sehreport=1: a crash report names the SEH chain and a longjmp's jmp_buf.
  if (get('sehreport') === '1') globalThis.GEX_SEH_REPORT = 1;
  if (get('heartbeat')) globalThis.GEX_HEARTBEAT = Number(get('heartbeat'));
  // Benchmarks: game clocks faster than real time, a fixed start date (runtime.js).
  if (get('timescale')) globalThis.GEX_TIME_SCALE = Number(get('timescale'));
  if (get('fixedtime')) globalThis.GEX_FIXED_TIME = Number(get('fixedtime'));
  if (get('mem64') === '1' || b.mem64) globalThis.GEX_MEM64 = 1;
  // The function JIT: off by default (it doubles a 32-bit Wine program's start-up;
  // nt-page.js said why).
  const fjit = get('fjit');
  if (fjit !== null && fjit !== '0') globalThis.GEX_FJIT = Number(fjit) || 1;
  if (get('wasmdlls')) globalThis.GEX_WASM_DLLS = get('wasmdlls');
  if (get('elfunits') && get('cgen') !== '0') globalThis.GEX_ELF_UNITS = get('elfunits');
  // provskip=export,...: exports no provider binds, as GEX_PROVIDER_SKIP natively (units built with the same skip).
  if (get('provskip')) globalThis.GEX_PROVIDER_SKIP = get('provskip');
  if (get('fjitlibs')) globalThis.GEX_FJIT_LIBS = get('fjitlibs');
  if (get('pins')) globalThis.GEX_PINS = get('pins');
  if (get('dense') === '0') globalThis.GEX_DENSE = 0;
  if (get('dense') === '2') globalThis.GEX_DENSE = 2;
  // cgen=1|2 (prefer|require units), the unguarded flavour unless unguarded=0.
  // icallcount=1: the counting flavour of units (16), whose run prints the
  // indirect-call profile (cgen.rs gex_icall_dump; GEX_CGEN_ICALL_COUNT=1 units).
  // cgen=0 is off: no unit of any kind, the ELF objects' among them.
  const cgen = get('cgen');
  if (cgen && cgen !== '0') globalThis.GEX_CGEN = Number(cgen) | (get('unguarded') === '0' ? 0 : 4) | (get('icallcount') === '1' ? 16 : 0);
  if (get('cgenurl')) globalThis.GEX_CGEN_URL = get('cgenurl');
  // cgenzst=1: each unit's module is served as KEY/c.wasm.zst, decoded before it is compiled.
  if (get('cgenzst') === '1') globalThis.GEX_CGEN_ZST = 1;
  if (get('aot') === '1') globalThis.GEX_NO_JIT = 1;
  if (get('aot')) globalThis.GEX_AOT = 1;
  if (get('densedebug')) globalThis.GEX_DENSE_DEBUG = 1;
  if (get('tracemisses')) globalThis.GEX_TRACE_MISSES = 1;
  if (get('attachtrace')) globalThis.GEX_ATTACH_TRACE = 1;
  if (get('strace')) globalThis.GEX_STRACE = 1;
  if (get('stracepid')) globalThis.GEX_STRACE_PID = Number(get('stracepid'));
  if (get('straceexe')) globalThis.GEX_STRACE_EXE = get('straceexe');
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

/*
 * The site's pieces (eject-site.mjs --pieces: project/build/web/pieces/ID,
 * the bundle's file bytes and the units' archives, each named by its
 * content) kept in the page's Cache Storage under their URL without the
 * build's query: a visit after a rebuild takes every piece that did not
 * change from there and fetches the rest. No service worker; the page reads
 * and writes the cache itself. Where there is no Cache Storage (an insecure
 * origin, a storage error) pieces are fetched as any file.
 */
const PIECE_CACHE = 'gex-pieces-v1';
const isPiece = (href) => /\/pieces\/[^/]+$/.test(new URL(href).pathname.replace(/\.zst$/, ''));
function pieceStore() {
  let p = null;
  return () => (p ??= (async () => {
    try { return typeof caches === 'object' && caches ? await caches.open(PIECE_CACHE) : null; } catch { return null; }
  })());
}
/** A piece's response, from the cache or the network (then kept); `cached`
 *  says which. */
async function pieceFetch(href, f, init) {
  const u = new URL(href);
  u.search = '';
  const cache = await f.pieces?.();
  if (cache) {
    try {
      const r = await cache.match(u.href);
      if (r?.ok) return { res: r, cached: true, key: u.href, cache };
    } catch { /* fetched */ }
  }
  const res = await fetch(u.href, init);
  if (res.ok && cache) cache.put(u.href, res.clone()).catch(() => {});
  return { res, cached: false, key: u.href, cache };
}
/** Pieces of older builds: whatever the cache holds that this build does not
 *  name (`keep`, URL paths) goes once the program runs, and so do lazy file
 *  chunks of a version other than `versions` names for their path. */
async function prunePieces(f, keep, versions = {}) {
  const cache = await f.pieces?.();
  if (!cache) return;
  try {
    const want = new Set(keep.map((k) => new URL(k, location.href).pathname));
    for (const r of await cache.keys()) {
      const u = new URL(r.url);
      const path = u.pathname;
      // A lazy file's chunks of another version than the list's (the file
      // changed under its URL) go too.
      const stale = u.searchParams.has('c') && versions[path] !== undefined && u.searchParams.get('v') !== versions[path];
      if (stale || (!want.has(path) && !want.has(path.replace(/\.zst$/, '')))) await cache.delete(r);
    }
  } catch { /* kept */ }
}

/*
 * The site's other packed files that carry their content's hash (pack.json
 * `h`: the runtime's wasm, Mesa) kept in the page's Cache Storage under their
 * URL with the hash for its query: a rebuild that did not change one is not
 * a download of it. The pieces' cache is not the place: it is pruned to the
 * pieces' list.
 */
const FILE_CACHE = 'gex-files-v1';
function fileStore() {
  let p = null;
  return () => (p ??= (async () => {
    try { return typeof caches === 'object' && caches ? await caches.open(FILE_CACHE) : null; } catch { return null; }
  })());
}
/** `href` (FILE.zst?v=BUILD) from the cache by its hash, or the network (then
 *  kept; `put` settles once it is). */
async function fileFetch(href, h, f, init) {
  const u = new URL(href);
  u.search = `h=${h}`;
  const cache = await f.files?.();
  if (cache) {
    try {
      const r = await cache.match(u.href);
      if (r?.ok) return { res: r, cached: true, key: u.href, cache };
    } catch { /* fetched */ }
  }
  const res = await fetch(href, init);
  const put = res.ok && cache ? cache.put(u.href, res.clone()).catch(() => {}) : Promise.resolve();
  return { res, cached: false, key: u.href, cache, put };
}
/** Kept files this build does not have (another hash, or gone). */
async function pruneFiles(f) {
  const cache = await f.files?.();
  if (!cache) return;
  const { files = {} } = await f.pack;
  const want = new Set();
  for (const [path, e] of Object.entries(files)) if (e.h) want.add(`${new URL(`${path.replace(/^\//, '')}.zst`, f.base).pathname}?h=${e.h}`);
  for (const r of await cache.keys()) {
    const u = new URL(r.url);
    if (!want.has(`${u.pathname}${u.search}`)) await cache.delete(r);
  }
}

/** Mesa's three files (mesa-gl.js `loadFromSource`) from the site's pack and
 *  the page's cache: null for one that is not packed, which Mesa then
 *  fetches as a file. `prefetch()` puts them in the cache without decoding,
 *  and a request for one already on its way waits for that download. */
const MESA_PREFETCH_PAUSE_MS = 20000;
const MESA_FILES = ['gex-wgpu-replay.js', 'gex-wgpu-replay.wasm', 'gex-wgpu-replay.deferred.wasm'];
function mesaSource(f, at) {
  const busy = new Map();
  const source = async (name) => {
    await busy.get(name);
    const href = at(`gex/${name}`);
    const z = await packedEntry(href, f);
    return z ? fetchPacked(href, z, false, f) : null;
  };
  source.prefetch = async () => {
    let n = 0;
    await Promise.all(MESA_FILES.map((name) => {
      const p = (async () => {
        const href = at(`gex/${name}`);
        const z = await packedEntry(href, f);
        if (!z?.h) return;
        const u = new URL(href);
        u.search = z.build ? `v=${z.build}` : '';
        u.pathname += '.zst';
        const got = await fileFetch(u.href, z.h, f, { priority: 'low' });
        if (got.cached) return;
        const bytes = (await got.res.arrayBuffer()).byteLength;
        f.state.downloaded = (f.state.downloaded ?? 0) + bytes;
        await got.put;
        n++;
      })().catch(() => {});
      busy.set(name, p);
      return p;
    }));
    return n;
  };
  return source;
}

/**
 * A file in one ArrayBuffer, which is moved (not cloned) to pid 1's Worker:
 * a clone would copy it, and a SharedArrayBuffer stays resident for the
 * session. The Worker frees it once loaded.
 */
async function fetchShared(url, f, { shared = false, priority = 'auto' } = {}) {
  const z = await packedEntry(url, f);
  if (z) return fetchPacked(url, z, shared, f, { priority });
  const { res, cached } = isPiece(url) ? await pieceFetch(url, f, { priority }) : { res: await fetch(url, { cache: 'no-cache', priority }), cached: false };
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
    if (cached) f.meter?.cached(value.length);
    else f.meter?.bytes(value.length);
  }
  f.meter?.fileDone();
  if (at !== total) throw new Error(`${url}: ${at} of ${total} bytes`);
  return out;
}

async function fetchPacked(url, z, shared, f, opt = {}) {
  try {
    return await fetchPackedOnce(url, z, shared, f, opt);
  } catch (e) {
    // A kept piece that does not decode (a cache cut short): dropped, and
    // fetched again.
    if (!e?.fromCache) throw e;
    await e.fromCache.cache.delete(e.fromCache.key).catch(() => {});
    console.warn(`[app] ${url}: the cached copy is bad (${e.message}); fetching it`);
    return fetchPackedOnce(url, z, shared, f, { ...opt, noCache: true });
  }
}

async function fetchPackedOnce(url, { size, zsize, build, h }, shared, f, { into = null, progress = null, priority = 'auto', noCache = false } = {}) {
  const u = new URL(url);
  u.search = build ? `v=${build}` : '';
  u.pathname += '.zst';
  const piece = isPiece(u.href);
  const got = piece ? await pieceFetch(u.href, noCache ? { ...f, pieces: null } : f, { priority })
    : h ? await fileFetch(u.href, h, noCache ? { ...f, files: null } : f, { priority })
    : { res: await fetch(u.href, { priority }), cached: false };
  const res = got.res;
  if (!res.ok) throw new Error(`${u.href}: ${res.status} ${res.statusText}`);
  const bad = (msg) => Object.assign(new Error(msg), got.cached ? { fromCache: got } : {});
  // Decoded as it arrives: the decode is done when the download is.
  const d = await f.zstd();
  const out = new Uint8Array(into ?? (shared ? new SharedArrayBuffer(size) : new ArrayBuffer(size)));
  let n = 0;
  const emit = (part) => {
    if (n + part.length > size) throw new Error(`${url}: longer than its ${size} bytes`);
    out.set(part, n);
    n += part.length;
  };
  const z = d.stream();
  let at = 0;
  const reader = res.body.getReader();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      z.push(value, emit);
      progress?.(n);
      at += value.length;
      if (got.cached) {
        f.state.cached = (f.state.cached ?? 0) + value.length;
        f.meter?.cached(value.length);
      } else {
        f.state.downloaded = (f.state.downloaded ?? 0) + value.length;
        f.meter?.bytes(value.length);
      }
    }
  } catch (e) {
    throw got.cached ? bad(e?.message ?? String(e)) : e;
  } finally {
    z.end();
  }
  f.meter?.fileDone();
  if (at !== zsize) throw bad(`${u.href}: ${at} of ${zsize} bytes`);
  if (n !== size) throw bad(`${url}: decoded ${n} of ${size} bytes`);
  return out;
}

/** Compile the units a preload list (`[[name, path, {demand}?], ...]`) names while the
 *  bundle downloads, a few at a time. */
async function preloadUnits(list, f, at) {
  const out = [];
  let closed = false;
  let i = 0;
  const one = async () => {
    while (i < list.length) {
      const [name, path, opt] = list[i++];
      const href = at(path);
      try {
        if (name.startsWith('tar:')) {
          // Every unit in one archive (web/eject-site.mjs): its modules
          // compiled, its other files shared, and `closed`: the Workers
          // ask the host for no unit file the archive does not hold.
          // Its `demand` units, the ones the recording first used after its
          // window, are shared as bytes like its other files: the first
          // Worker to map one compiles it (gex.mjs `#cgenModule`, before
          // the image runs) and shares the Module. Compiled here, each held
          // 2.8 times its size of the renderer's code space even if nothing
          // mapped it.
          const demand = new Set(opt?.demand ?? []);
          // An open archive's files stay views of it, in one shared buffer:
          // a start message then carries one buffer, not one per file. One
          // of modules only (`modules`) is let go once they are compiled.
          const keep = !!opt?.open && !opt?.modules;
          const tar = (await fetchShared(href, f, { shared: keep })).buffer;
          const t = untar(tar);
          const mods = [];
          for (const [p, bytes] of Object.entries(t.files)) {
            const rel = p.replace(/^\/+/, '');
            if (rel.endsWith('/c.wasm') && !demand.has(rel.slice(0, -'/c.wasm'.length))) {
              mods.push(WebAssembly.compile(bytes).then((m) => {
                f.meter?.compiled();
                return [`cgen:${rel.slice(0, -'/c.wasm'.length)}`, m];
              }));
              continue;
            }
            if (keep) {
              out.push([`file:${rel}`, bytes]);
              continue;
            }
            // Shared where there is a SharedArrayBuffer (the SP build has none).
            const s = new Uint8Array(typeof SharedArrayBuffer === 'function' ? new SharedArrayBuffer(bytes.byteLength) : new ArrayBuffer(bytes.byteLength));
            s.set(bytes);
            out.push([`file:${rel}`, s]);
          }
          out.push(...await Promise.all(mods));
          // `open`: the archive holds some of the units' files (their entries,
          // say), and a Worker asks the server for the rest.
          if (!opt?.open) closed = true;
          // Everything in it is compiled (V8 keeps its own copy of each
          // module's bytes) or copied out: the archive itself goes now. Left
          // to the collector it stayed until the next full collection, the
          // shell's 361 MiB for minutes.
          if (!keep && !(typeof SharedArrayBuffer === 'function' && tar instanceof SharedArrayBuffer)) tar.transfer?.(0);
          continue;
        }
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
  // After every archive (one, or the pieces': eject-site.mjs --pieces).
  if (closed) out.push(['closed', true]);
  return out;
}
