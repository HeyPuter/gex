// Direct MobileGL/WebGPU replay lives off the page event loop. The guest still
// sees the same synchronous /dev/gex-gl ABI; SharedArrayBuffer carries its ring
// into this Worker while the HTML canvas is transferred as an OffscreenCanvas.
import { serveGl } from '@gex/gl-bridge.mjs';
import { MobileGlDevice } from './mobilegl.js';

let renderer = null;
let stats = null;
let trace = false;
const channels = new Map();

function updateStats() {
  if (!renderer?.ok || !stats) return;
  Atomics.store(stats, 0, renderer.presentedCount());
  Atomics.store(stats, 1, renderer.producedCount());
}

function consumeRequestedDisplay() {
  if (!renderer?.ok) return false;
  const requested = Atomics.load(stats, 5);
  const handled = Atomics.load(stats, 6);
  if (requested === handled || renderer.producedCount() <= renderer.presentedCount()) return false;
  const before = renderer.presentedCount();
  renderer.display();
  if (renderer.presentedCount() === before) return false;
  Atomics.store(stats, 6, requested);
  updateStats();
  return true;
}

function snapshot(id = 0) {
  if (!renderer?.ok) return;
  updateStats();
  postMessage({
    kind: 'diagnostic',
    id,
    state: renderer.glState(),
    commands: renderer.commands(),
  });
}

function displayCompletedSwap() {
  if (!renderer?.ok) return;
  updateStats();
  consumeRequestedDisplay();
  // Wake the display consumer even when the request serial itself has not
  // changed: this may be the snapshot an earlier page tick was waiting for.
  Atomics.notify(stats, 5);
}

async function serveDisplays() {
  while (renderer?.ok) {
    const requested = Atomics.load(stats, 5);
    if (consumeRequestedDisplay()) continue;
    // Unlike Worker rAF/setTimeout, waitAsync resumes in the same microtask
    // domain as the continuously active GL-ring consumer. Both a new browser
    // refresh and a newly completed snapshot notify this word.
    const waiting = Atomics.waitAsync(stats, 5, requested, 1000);
    if (waiting.async) await waiting.value;
    else await Promise.resolve();
  }
}

self.onmessage = async ({ data: m }) => {
  try {
    if (m.kind === 'init') {
      stats = new Int32Array(m.stats);
      // Emscripten's canvas lookup expects document.querySelector even in a
      // Worker. This selector owns exactly the transferred canvas.
      globalThis.document = { querySelector: () => m.canvas };
      // MobileGlDevice.create throws unconditionally; this worker is dead
      // code kept only so the module still builds.
      renderer = await MobileGlDevice.create(m.canvas, m.url);
      updateStats();
      if (renderer.ok) serveDisplays();
      postMessage({
        kind: 'ready',
        error: renderer.ok ? null : renderer.error,
        description: renderer.ok ? renderer.describe() : '',
        state: renderer.ok ? renderer.glState() : '',
        commands: renderer.ok ? renderer.commands() : '',
      });
    } else if (m.kind === 'channel') {
      const device = {
        submit(memory, ring) {
          // A continuously active ring can schedule its next request before
          // the display-consumer microtask. Honor a pending browser refresh
          // before starting another potentially long GL batch.
          consumeRequestedDisplay();
          const started = performance.now();
          const finish = (result) => {
            const elapsedUs = Math.round((performance.now() - started) * 1000);
            const submits = Atomics.add(stats, 2, 1) + 1;
            Atomics.add(stats, 3, elapsedUs);
            let oldMax = Atomics.load(stats, 4);
            while (elapsedUs > oldMax &&
                   Atomics.compareExchange(stats, 4, oldMax, elapsedUs) !== oldMax) {
              oldMax = Atomics.load(stats, 4);
            }
            if (result === 0) displayCompletedSwap();
            // Under a continuously active direct ring the Worker's message
            // queue may not get a turn. Emit trace snapshots from the submit
            // path itself so diagnostics still describe the live workload.
            if (trace && submits % 250 === 0) snapshot();
          };
          const result = renderer.submit(memory, ring);
          if (result instanceof Promise) {
            return result.then((rc) => {
              finish(rc);
              return rc;
            }, (error) => {
              finish(-1);
              throw error;
            });
          }
          finish(result);
          return result;
        },
      };
      channels.set(m.id, serveGl(device, m.channel));
    } else if (m.kind === 'serve') {
      await channels.get(m.id)?.();
      updateStats();
    } else if (m.kind === 'diagnostic') {
      snapshot(m.id);
    } else if (m.kind === 'trace') {
      trace = Boolean(m.on);
      renderer?.traceCommands(trace);
    }
  } catch (e) {
    postMessage({ kind: 'error', error: e?.stack || e?.message || String(e) });
  }
};
