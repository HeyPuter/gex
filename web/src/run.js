// `runProgram`, the page's: one program as pid 1 of a system of its own.
// Here and not in runtime.js, which every Worker takes in: through it a
// Worker would take in the runtime API's System (run-program.js) and the
// single-thread build's system Worker (system-worker.js), and they take in
// runtime.js again, a cycle of Workers a bundler refuses.

import { SP_CODE } from '../../shims/browser/js/mode.mjs';
import { loadedRuntime, pageGlobals, singleMode } from './runtime.js';
import { dialFor } from './net.js';

const GEX_COMPOSITOR = typeof __GEX_COMPOSITOR__ === 'undefined' ? true : __GEX_COMPOSITOR__;

/**
 * `runProgram` for the single-thread build, from the page: the whole system
 * runs in one Worker (web/src/system-worker.js); this side holds the terminal
 * and forwards keys, sizes and output.
 */
async function runInSystemWorker(term, opts) {
  // `onFilesLoaded` and `onSystem` are functions, which cannot cross to the
  // Worker (nt.html passes `onSystem`: its every run failed to start here).
  // So is `glSource` (app.js `mesaSource`), and it names the threaded Mesa's
  // files besides: the system Worker fetches the single-thread one (st/).
  const { network, onNetEvent, onFilesLoaded, onDesktop, onSystem, transfer, wl, wlInput, gl, hostX, preModules, closure, glSource, ...rest } = opts;
  // Said once, so a person (and a test) can tell which build ran.
  term.writeln?.('gex: single-thread build (no SharedArrayBuffer): every guest thread in one Worker');
  // The compositor: a canvas only (not the DOM desktop), drawn by the system
  // Worker, which holds every guest memory it reads pixels out of.
  const canvas = typeof wl?.getContext === 'function' && typeof wl.transferControlToOffscreen === 'function' ? wl : null;
  // GL: the compositor's own (`gl: 'compositor'`) runs in the system Worker
  // beside it, the non-pthread Mesa (web/public/gex/st/); a page-side device
  // does not exist there.
  const glHere = gl === 'compositor' && !!canvas;
  // The X server too: in the system Worker, beside the compositor.
  const xHere = !!hostX && !!canvas;
  if ((wl && !canvas) || (gl && !glHere) || (hostX && !xHere)) {
    term.writeln?.(`gex: the single-thread build has no ${[wl && !canvas && 'DOM desktop', gl && !glHere && 'GL', hostX && !xHere && 'X server'].filter(Boolean).join(', ')} yet${canvas ? '' : '; running without the screen'}`);
  }
  const w = new Worker(new URL('./system-worker.js', import.meta.url), { type: 'module', name: 'system' });
  globalThis.__gexSystemWorker = w;
  const ready = new Promise((resolve, reject) => {
    w.addEventListener('message', (e) => { if (e.data?.kind === 'ready') resolve(); });
    w.addEventListener('error', (e) => reject(new Error(e.message || 'the system Worker failed to start')));
  });
  await ready;
  // The compositor's page side (listeners, canvas size) talks to its worker
  // half through the system Worker: `comp` messages in, `comp-out` back.
  const compListeners = new Set();
  let compStats = null;
  const compPort = {
    postMessage(m, transfer = []) {
      if (m?.kind === 'init' && m.stats) compStats = new Int32Array(m.stats);
      w.postMessage({ kind: 'comp', m }, transfer);
    },
    addEventListener(type, f) { if (type === 'message') compListeners.add(f); },
    removeEventListener(type, f) { compListeners.delete(f); },
    terminate() {},
  };
  w.addEventListener('message', (e) => {
    const m = e.data;
    if (m?.kind === 'comp-out') for (const f of [...compListeners]) f({ data: m.m });
    else if (m?.kind === 'comp-stats' && compStats) compStats.set(m.stats.subarray(0, compStats.length));
  });
  let compositor = null;
  if (canvas && GEX_COMPOSITOR) {
    const { Compositor } = await import('./compositor.js');
    // Before `run`: the Worker takes the canvas first, then starts the guest.
    w.postMessage({ kind: 'comp-open', width: canvas.width, height: canvas.height });
    compositor = await Compositor.create(canvas, null, wlInput || canvas, { makeWorker: () => compPort });
    globalThis.__gexCompositor = compositor;
  }
  const keys = term.onData((data) => w.postMessage({ kind: 'key', data }));
  const resize = term.onResize(({ rows, cols }) => w.postMessage({ kind: 'resize', rows, cols }));
  term.focus?.();
  try {
    return await new Promise((resolve, reject) => {
      w.addEventListener('message', (e) => {
        const m = e.data;
        if (m.kind === 'write') term.write(m.data);
        else if (m.kind === 'net') onNetEvent?.(m.event);
        else if (m.kind === 'net-dial') dialFor(network?.network, m);
        else if (m.kind === 'done') resolve({ status: m.status, host: null, tenv: null });
        else if (m.kind === 'failed') reject(Object.assign(new Error(m.message), { workerStack: m.stack }));
      });
      w.postMessage({
        kind: 'run',
        globals: pageGlobals(),
        // Already fetched by the page (loadRuntime): copied, not
        // fetched again by the Worker.
        runtime: loadedRuntime(),
        rows: term.rows,
        cols: term.cols,
        opts: {
          ...rest,
          // The page's network stays here; the Worker dials through it
          // (`net-dial`, net.js `pageRelay`).
          netRelay: network?.network ? { datagram: typeof network.network.datagram === 'function' } : null,
          pageHosts: network?.pageHosts,
          wl: canvas ? { inRealm: true } : undefined,
          gl: glHere ? 'compositor' : undefined,
          hostX: xHere ? hostX : undefined,
        },
      });
    });
  } finally {
    keys.dispose();
    resize.dispose();
    compositor?.stop();
    w.terminate();
  }
}

/**
 * Run one program, `opts.args[0]`, as pid 1 of a system of its own, its
 * terminal `term` (an xterm, or anything with its `write`, `onData`,
 * `onResize`, `rows` and `cols`): a preset over the runtime API
 * (run-program.js), in either build. A single-thread page with a screen, GL
 * or an X server runs the whole system in one Worker instead (system-worker.js,
 * runtime.js `runSingle` there). Resolves `{ status }` when the program ends.
 */
export async function runProgram(term, opts) {
  // The single-thread System has no GL beside a DOM desktop yet: a page that
  // asks for it runs the whole system in a Worker of its own as before.
  if ((SP_CODE && singleMode()) && opts.gl && typeof opts.wl?.getContext !== 'function') return runInSystemWorker(term, opts);
  // Said once, so a person (and the smoke test) can tell which build ran; by
  // the page's preset, not the runtime API, which says nothing by default.
  if ((SP_CODE && singleMode())) console.info(`[gex] single-thread build: the system runs in a Worker, no SharedArrayBuffer (crossOriginIsolated=${globalThis.crossOriginIsolated})`);
  const { runPreset } = await import('./run-program.js');
  return runPreset(term, opts);
}

