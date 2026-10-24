// A Windows program on gex's NT layer in a page (docs/nt-layer.md §10.2):
// the bundle is a tar (a staged Wine prefix under /wine plus the program),
// the program's windows are toplevels in the page's compositor, and its GL
// (opengl32 → /dev/gex-gl) runs on Mesa in the compositor's worker.
//
//   /nt.html?tar=/gex/nt/bundle.tar&exe=/t/window.exe&args=30&size=1024x768
//            [&gl=1&glbackend=wgpu][&env=K=V,K=V][&cwd=/t][&hostx=1]
//            [&cgen=2&cgenurl=/project/build/web/cgen&aot=1]   (an ejected project)
//            [&persist=/guest/dir&persistdb=NAME]   (that directory in IndexedDB)
//            [&compositor=0]   (no scene compositor: a canvas per window, bare-desktop.js)
//
// The exe may also be a Linux ELF (an i386 or x86-64 program with its
// libraries in the bundle); `hostx=1` gives it the page's X server
// (Xwayland as wasm, web/build-xwayland-wasm.sh) at DISPLAY=:0.
//
// What a driver reads: `window.__nt` = { out, status, compositor }.

import { start } from './app.js';

/** Whether this build has the scene compositor: see runtime.js. */
const GEX_COMPOSITOR = typeof __GEX_COMPOSITOR__ === 'undefined' ? true : __GEX_COMPOSITOR__;
const q = new URLSearchParams(location.search);
const logEl = document.getElementById('log');

// This is gex's test page: its query is the library's options (app.js
// `start`), one switch per parameter. An ejected program's page is the
// user's, and calls `start` itself (docs/eject.md, "The web build").
async function main() {
  // `size=WxH`, or `size=fit`: the browser's viewport at load. The desktop is
  // then the whole page, which does not scroll.
  const fit = q.get('size') === 'fit';
  if (fit) document.documentElement.style.overflow = 'hidden';
  const [w, h] = fit
    ? [Math.max(640, document.documentElement.clientWidth), Math.max(480, document.documentElement.clientHeight)]
    : (q.get('size') || '1024x768').split('x').map(Number);
  const switches = Object.fromEntries(q);
  for (const k of ['tar', 'exe', 'args', 'argv', 'env', 'cwd', 'size', 'persist', 'persistdb', 'lazy', 'preload', 'pack', 'zstd', 'mtimes', 'gl', 'glbackend', 'hostx', 'provsrc', 'stateful', 'memories']) delete switches[k];
  // `provsrc=url,...`: provider sources from outside gex (GEX_PROVIDER_DIRS
  // natively), each named by its file's stem.
  const providerSources = q.get('provsrc')
    ? (await Promise.all(q.get('provsrc').split(',').filter(Boolean).map(async (u) => {
      const r = await fetch(u);
      if (!r.ok) throw new Error(`provsrc: ${u}: ${r.status}`);
      return `${u.split('/').pop().replace(/\.c$/, '')}\n${await r.text()}`;
    }))).join('\0')
    : null;
  const exe = q.get('exe') || '/t/window.exe';
  // Comma-separated, but a comma inside a value stays (GEX_NT_INPUT's
  // `700:click:960,850`): a variable starts where NAME= does.
  const env = (q.get('env') || '').split(/,(?=[A-Za-z_][A-Za-z0-9_]*=)/).filter(Boolean);
  const app = await start({
    canvas: document.getElementById('out'),
    // `wisp=wss://…/`: the guest's network over that Wisp proxy (none without it)
    ...(q.get('wisp') ? { wisp: q.get('wisp') } : {}),
    size: { w, h },
    baseUrl: new URL('/', location.href),
    onLog: () => { logEl.textContent = globalThis.__nt.out.slice(-4000); },
    build: {
      // `tar=a.tar,b.tar`: bundles laid over one another, later ones winning.
      tar: (q.get('tar') || '/gex/nt/bundle.tar').split(','),
      exe,
      // `argv=` (a JSON array) keeps arguments with spaces whole; `args=` splits.
      argv: q.get('argv') ? JSON.parse(q.get('argv')) : q.get('args') ? q.get('args').split(' ') : [],
      env,
      cwd: q.get('cwd') || undefined,
      size: { w, h },
      mtimes: q.get('mtimes') === '1',
      featuresOff: GEX_COMPOSITOR ? [] : ['compositor'],
      compositor: q.get('compositor') !== '0',
      aot: q.get('aot'),
      jitFallback: true,
      gl: q.get('gl') === '1',
      glBackend: q.get('glbackend') || 'wgpu',
      hostx: q.get('hostx') === '1' || q.get('hostx') === 'rootful',
      // `hostx=rootful`: one fullscreen X screen under the session's own window manager
      hostxRootful: q.get('hostx') === 'rootful',
      mem64: q.get('mem64') === '1',
      pack: q.get('pack'),
      zstd: q.get('zstd') || '/vendor/zstddec-stream.mjs',
      preload: q.get('preload'),
      lazy: q.get('lazy'),
      persist: q.get('persist') ? { dir: q.get('persist'), db: q.get('persistdb') || `gex-persist:${location.pathname}` } : null,
      // `stateful=dll=file,...`: stateful providers, modules under the unit cache.
      stateful: q.get('stateful'),
      providerSources,
      switches,
    },
  });
  await app.exited;
}

main().catch((e) => {
  const state = globalThis.__nt ?? (globalThis.__nt = { out: '' });
  state.error = String(e?.stack ?? e);
  state.phase = 'failed';
  state.out += `\n[nt-page] ${state.error}\n`;
  logEl.textContent = state.out.slice(-4000);
});
