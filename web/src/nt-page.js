// A Windows program on gex's NT layer in a page (docs/nt-layer.md §10.2):
// the bundle is a tar (a staged Wine prefix under /wine plus the program),
// the program's windows are toplevels in the page's compositor, and its GL
// (opengl32 → /dev/gex-gl) runs on Mesa in the compositor's worker.
//
//   /nt.html?tar=/gex/nt/bundle.tar&exe=/t/window.exe&args=30&size=1024x768
//            [&gl=1&glbackend=wgpu][&env=K=V,K=V][&cwd=/t][&hostx=1]
//            [&cgen=2&cgenurl=/project/build/web/cgen&aot=1]   (an ejected project)
//            [&persist=/guest/dir&persistdb=NAME]   (that directory in IndexedDB)
//
// The exe may also be a Linux ELF (an i386 or x86-64 program with its
// libraries in the bundle); `hostx=1` gives it the page's X server
// (Xwayland as wasm, web/build-xwayland-wasm.sh) at DISPLAY=:0.
//
// What a driver reads: `window.__nt` = { out, status, compositor }.

import { untar } from './tar.js';
import { runProgram, globalKernel } from './runtime.js';
import { restore, persist } from './persist.js';

globalThis.GEX_ARCH = 'x86';
const q = new URLSearchParams(location.search);
const logEl = document.getElementById('log');
const state = { out: '', status: null, error: null, phase: 'loading' };
globalThis.__nt = state;
// Start-up's phases, as wall-clock marks (ms mod 1e7, as the Workers' [unit] lines).
const mark = (what) => console.warn(`[nt-page] ${what} at ${Math.round(performance.timeOrigin + performance.now()) % 1e7} (+${Math.round(performance.now())} ms)`);
const say = (s) => {
  state.out += s;
  logEl.textContent = state.out.slice(-4000);
};

const term = {
  rows: 40,
  cols: 120,
  write(d) {
    say(typeof d === 'string' ? d : new TextDecoder().decode(d));
  },
  writeln(s) {
    say(`${s}\n`);
  },
  onData() {
    return { dispose() {} };
  },
  onResize() {
    return { dispose() {} };
  },
  focus() {},
};

/**
 * The bundle, in a SharedArrayBuffer. Every file is a view of it, and the
 * views go to the process's Worker in its start message: a structured clone
 * shares a SharedArrayBuffer where it would copy an ArrayBuffer, and copying
 * Noita's 1.8 GB fails ("Data cannot be cloned, out of memory").
 */
async function fetchShared(url) {
  const res = await fetch(url, { cache: 'no-cache' });
  if (!res.ok) throw new Error(`${url}: ${res.status} ${res.statusText}`);
  const total = Number(res.headers.get('content-length'));
  if (!total) throw new Error(`${url}: no content-length`);
  const out = new Uint8Array(new SharedArrayBuffer(total));
  const reader = res.body.getReader();
  let at = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    out.set(value, at);
    at += value.length;
  }
  if (at !== total) throw new Error(`${url}: ${at} of ${total} bytes`);
  return out;
}

/** Compile the units a preload list names, a few at a time, streaming from
 *  the network (or the service worker's cache); null if the list is gone. */
async function preloadUnits(url) {
  let list;
  try {
    list = await (await fetch(url)).json();
  } catch {
    return null;
  }
  const out = [];
  let i = 0;
  const one = async () => {
    while (i < list.length) {
      const [name, href] = list[i++];
      try {
        if (name.startsWith('file:')) {
          // A small file of the unit cache (entries, a provider, a profile),
          // which every Worker would otherwise fetch with a synchronous
          // request: in a SharedArrayBuffer, so each Worker gets it uncopied.
          const b = new Uint8Array(await (await fetch(href)).arrayBuffer());
          const shared = new Uint8Array(new SharedArrayBuffer(b.byteLength));
          shared.set(b);
          out.push([name, shared]);
          continue;
        }
        out.push([name, await WebAssembly.compileStreaming(fetch(href))]);
      } catch (e) {
        console.warn(`[nt-page] preload ${href}: ${e?.message ?? e}`);
      }
    }
  };
  await Promise.all([one(), one(), one(), one()]);
  return out;
}

async function main() {
  // `?trapstack=1`: a trap's wasm frames, which name the shim function it
  // was in (runtime.js passes the switch to every Worker).
  if (q.get('trapstack')) globalThis.GEX_TRAP_STACK = 1;
  if (q.get('doorstats')) globalThis.GEX_DOOR_STATS = Number(q.get('doorstats')) || 1;
  // `?exittrace=1`: each Worker's exit status (and pc), from the host.
  if (q.get('exittrace')) globalThis.GEX_EXIT_TRACE = 1;
  if (q.get('callouttrace')) globalThis.GEX_CALLOUT_TRACE = 1;
  // `heartbeat=N`: say where the guest is every N safepoints (a stuck run from a slow one).
  if (q.get('heartbeat')) globalThis.GEX_HEARTBEAT = Number(q.get('heartbeat'));
  // `?mem64=1`: a 64-bit guest memory (the wasm64 shim; docs/mem64.md step 6).
  if (q.get('mem64') === '1') globalThis.GEX_MEM64 = 1;
  // `?fjit=1`: the function JIT (gexc run --fjit's browser half, fjit.rs);
  // `?fjit=2` traces it, `?fjit=0` turns it off. Off by default: it serves
  // every cold tier-1 miss, which doubles a W3 program's start-up (Undertale's
  // first frame 28.6 s with `?fjit=1`, 16.4 s without; worldmachine,
  // units-ut3) for about 3% of steady fps there (18.2 vs 17.9). It comes back
  // as the default once it waits for a block to be hot.
  const fjitQ = q.get('fjit');
  if (fjitQ !== null && fjitQ !== '0') globalThis.GEX_FJIT = Number(fjitQ) || 1;
  // `?wasmdlls=soname=path,...`: shared objects replaced by wasm64 providers (cgen/libprov.rs).
  if (q.get('wasmdlls')) globalThis.GEX_WASM_DLLS = q.get('wasmdlls');
  // `?elfunits=name,...`: shared objects that get function-tier units (with `?cgen=1`).
  if (q.get('elfunits')) globalThis.GEX_ELF_UNITS = q.get('elfunits');
  // `?fjitlibs=+name,-name`: the function JIT only for (+) or not for (-) those objects (a bisection aid).
  if (q.get('fjitlibs')) globalThis.GEX_FJIT_LIBS = q.get('fjitlibs');
  // `?pins=path@base+span,...` (hex): place those shared objects at fixed bases, as a
  // closure's pins do, so a unit built for one base (`?elfunits=`) is used whatever
  // else (the function JIT's heap, say) moved the arena first.
  if (q.get('pins')) globalThis.GEX_PINS = q.get('pins');
  if (q.get('dense') === '0') globalThis.GEX_DENSE = 0;
  if (q.get('dense') === '2') globalThis.GEX_DENSE = 2;
  // cgen=1|2 (prefer|require units), in the unguarded flavor of i386 units
  // (the native one, mode bit 4: built without GEX_CGEN_GUARD=1), which ran
  // Noita 14% faster than the guarded one (docs/noita-perf.md, n1);
  // `unguarded=0` picks the guarded, browser flavor (GEX_CGEN_GUARD=1).
  if (q.get('cgen')) globalThis.GEX_CGEN = Number(q.get('cgen')) | (q.get('unguarded') === '0' ? 0 : 4);
  // An ejected project's page (gexc eject): its units, and no translation.
  if (q.get('cgenurl')) globalThis.GEX_CGEN_URL = q.get('cgenurl');
  if (q.get('aot') === '1') globalThis.GEX_NO_JIT = 1;
  // `aot=record`: its training run (translation on, the rest as aot=1).
  if (q.get('aot')) globalThis.GEX_AOT = 1;
  if (q.get('densedebug')) globalThis.GEX_DENSE_DEBUG = 1;
  if (q.get('tracemisses')) globalThis.GEX_TRACE_MISSES = 1;
  // `?attachtrace=1`: time each thread Worker from the guest's clone to its
  // start (host.mjs, gex.mjs `#catchUp`), as main.js does for the demos.
  if (q.get('attachtrace')) globalThis.GEX_ATTACH_TRACE = 1;
  // `?strace=1`: narrate every syscall (`gex_set_trace`), as main.js does.
  if (q.get('strace')) globalThis.GEX_STRACE = 1;
  if (q.get('stracepid')) globalThis.GEX_STRACE_PID = Number(q.get('stracepid'));
  // The translator's switches, as main.js reads them (docs/jit-guest.md).
  if (q.get('liftmax')) globalThis.GEX_LIFT_MAX = Number(q.get('liftmax'));
  // The page-wide unit cache is on unless `?unitcache=0`.
  globalThis.GEX_UNIT_CACHE = q.get('unitcache') !== null ? Number(q.get('unitcache')) : 1;
  if (q.get('t1fold')) globalThis.GEX_T1_FOLD = Number(q.get('t1fold'));
  if (q.get('xlatereport')) globalThis.GEX_XLATE_REPORT = Number(q.get('xlatereport'));
  if (q.get('inlinedispatch')) globalThis.GEX_INLINE_DISPATCH = Number(q.get('inlinedispatch'));
  const [w, h] = (q.get('size') || '1024x768').split('x').map(Number);
  const canvas = document.getElementById('out');
  canvas.width = w;
  canvas.height = h;
  canvas.style.width = `${w}px`;
  canvas.style.height = `${h}px`;
  state.phase = 'fetching';
  mark('fetching');
  // `preload=URL`: the units to compile while the bundle downloads (a JSON
  // list of [module name, url]); every Worker then starts with them.
  const preloading = q.get('preload') ? preloadUnits(q.get('preload')) : Promise.resolve(null);
  // `tar=a.tar,b.tar`: bundles laid over one another, later ones winning --
  // a small test program over a large cached bundle of its libraries.
  const trees = [];
  for (const url of (q.get('tar') || '/gex/nt/bundle.tar').split(',')) {
    const tar = await fetchShared(url);
    // The buffer, not the array: `untar` wraps what it is given in a new
    // Uint8Array, which copies an array but views a buffer.
    mark(`fetched ${url} (${(tar.length / 1048576).toFixed(0)} MiB)`);
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
  // `persist=DIR`: what the program left in DIR last time (saves, settings),
  // over the bundle's, and DIR kept in IndexedDB from now on (persist.js).
  const persistDir = q.get('persist');
  const persistDb = q.get('persistdb') || `gex-persist:${location.pathname}`;
  if (persistDir) {
    const saved = await restore(persistDb, persistDir);
    for (const [p, d] of saved) files[p] = d;
    state.restored = saved.length;
    if (saved.length) say(`[nt-page] restored ${saved.length} file(s) under ${persistDir}\n`);
  }
  // Hard links name a file by its path from the archive's root (tar -h
  // writes one for every second path to an inode it has stored). They go to
  // the guest as links, not copies: glibc knows a loaded library by its
  // inode, and `libGL.so.1` and `libEGL.so.1` copies of one file load as two
  // libraries with two sets of GL state. Symlinks are not followed here: a
  // bundle is made with -h.
  const hard = Object.fromEntries(Object.entries(tree.hard ?? {}).filter(([, t]) => files[t]));
  // `lazy=URL`: a manifest, {"base": URL prefix, "files": [[guest path, size,
  // path under base], ...]}, of files the guest reads from the server as it
  // reads them instead of from the bundle (a game's data).
  let lazy = [];
  if (q.get('lazy')) {
    const m = await (await fetch(q.get('lazy'))).json();
    lazy = m.files.map(([path, size, rel]) => [path, size, m.base + rel.split('/').map(encodeURIComponent).join('/')]);
    mark(`lazy: ${lazy.length} files, ${(lazy.reduce((a, f) => a + f[1], 0) / 1048576).toFixed(0)} MiB`);
  }
  const exe = q.get('exe') || '/t/window.exe';
  const exeBytes = files[exe] ?? files[hard[exe]];
  if (!exeBytes) throw new Error(`${exe} is not in the bundle`);
  // `argv=` (a JSON array) keeps arguments with spaces whole; `args=` splits.
  const args = [exe, ...(q.get('argv') ? JSON.parse(q.get('argv')) : q.get('args') ? q.get('args').split(' ') : [])];
  // Comma-separated, but a comma inside a value stays (GEX_NT_INPUT's
  // `700:click:960,850`): a variable starts where NAME= does.
  const env = (q.get('env') || '').split(/,(?=[A-Za-z_][A-Za-z0-9_]*=)/).filter(Boolean);
  if (!env.some((e) => e.startsWith('GEX_WINE_PREFIX='))) env.push('GEX_WINE_PREFIX=/wine');
  if (!env.some((e) => e.startsWith('GEX_NT_STRICT='))) env.push('GEX_NT_STRICT=1');
  const gl = q.get('gl') === '1';
  const preModules = await preloading;
  if (preModules) mark(`compiled ${preModules.filter(([k]) => !k.startsWith('file:')).length} unit modules ahead, ${preModules.filter(([k]) => k.startsWith('file:')).length} files`);
  state.phase = 'running';
  mark('running');
  if (persistDir) {
    const p = persist(persistDb, persistDir, await globalKernel(), {
      every: Number(q.get('persistms') || 30000),
      onSync: (m) => { state.persisted = m; },
    });
    // A driver (or a console) can ask for a snapshot now: `await __ntPersist()`.
    globalThis.__ntPersist = () => p?.sync();
  }
  const { status } = await runProgram(term, {
    elf: exeBytes,
    files,
    dirs: tree.dirs,
    modes: tree.modes,
    hard,
    lazy,
    // Symlinks the bundle kept as links: a Wine prefix's `dosdevices/c:`
    // (W3 in the page), which Wine opens by that name.
    symlinks: tree.links ?? {},
    // `mtimes=1`: the bundle's file times, for a program that compares them
    // (W3's wineboot updates a prefix whose wine.inf is newer than its
    // install). Off by default: a call per file.
    mtimes: q.get('mtimes') === '1' ? tree.mtimes : undefined,
    env,
    args,
    cwd: q.get('cwd') || exe.replace(/\/[^/]*$/, '') || '/',
    tty: false,
    gl: gl ? 'compositor' : null,
    glBackend: q.get('glbackend') || 'wgpu',
    wl: canvas,
    wlInput: canvas,
    hostX: q.get('hostx') === '1',
    preModules,
  });
  state.status = status;
  state.phase = 'done';
}

main().catch((e) => {
  state.error = String(e?.stack ?? e);
  state.phase = 'failed';
  say(`\n[nt-page] ${state.error}\n`);
});
