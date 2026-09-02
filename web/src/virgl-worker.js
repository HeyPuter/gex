// virglrenderer lives here rather than on the page's event loop. The guest's
// synchronous vtest requests arrive through SharedArrayBuffer channels, so
// expensive WebGL replay cannot starve input, rAF, or browser UI work.
import { VirglRenderer } from './virgl.js';
import { serveVirgl } from '@gex/virgl-bridge.mjs';

let renderer = null;
let stats = null;
let lastDiagnostic = 0;
const channels = new Map();

function snapshot(id = 0) {
  if (!renderer?.ok) return;
  postMessage({
    kind: 'diagnostic',
    id,
    state: renderer.glState(),
    commands: renderer.commands(),
  });
}

function afterServe() {
  Atomics.store(stats, 0, renderer.presentedCount());
  const now = performance.now();
  if (now - lastDiagnostic >= 2000) {
    lastDiagnostic = now;
    snapshot();
  }
}

async function serveDirect(id, channel) {
  const ctl = new Int32Array(channel.control);
  const serve = channels.get(id);
  while (channels.has(id)) {
    const state = Atomics.load(ctl, 0);
    if (state === 1) {
      await serve();
      afterServe();
      continue;
    }
    // Wait without occupying this Worker's event loop, so diagnostics and
    // teardown messages still run. Guest REQUEST and IDLE transitions notify
    // this same word; serveVirgl notifies the guest on REPLY.
    const waiting = Atomics.waitAsync(ctl, 0, state);
    if (waiting.async) await waiting.value;
  }
}

self.onmessage = async ({ data: m }) => {
  try {
    if (m.kind === 'init') {
      stats = new Int32Array(m.stats);
      globalThis.__gexDumpWgsl = m.dumpWgsl || 0;
      // Emscripten resolves the C selector "#gex-gl" through document even
      // for an OffscreenCanvas. Supply the one selector this worker owns.
      globalThis.document = { querySelector: () => m.canvas };
      renderer = await VirglRenderer.create(m.canvas, m.url);
      Atomics.store(stats, 0, renderer.presentedCount());
      postMessage({
        kind: 'ready',
        error: renderer.ok ? null : renderer.error,
        description: renderer.ok ? renderer.describe() : '',
        state: renderer.ok ? renderer.glState() : '',
        commands: renderer.ok ? renderer.commands() : '',
      });
    } else if (m.kind === 'channel') {
      channels.set(m.id, serveVirgl(renderer, m.channel));
      if (m.channel.direct) serveDirect(m.id, m.channel);
    } else if (m.kind === 'serve') {
      await channels.get(m.id)?.();
      afterServe();
    } else if (m.kind === 'diagnostic') {
      snapshot(m.id);
    } else if (m.kind === 'trace') {
      renderer?.traceCommands(m.on);
    } else if (m.kind === 'close') {
      renderer?.close(m.session);
    }
  } catch (e) {
    postMessage({ kind: 'error', error: e?.stack || e?.message || String(e) });
  }
};
