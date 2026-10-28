// The single-thread build's one Worker: the whole guest system -- the global
// kernel, every process and thread as a fiber, the Wisp connection -- on this
// thread, with no SharedArrayBuffer. The page keeps
// the terminal and talks to this by messages.
//
// A Worker rather than the page's own thread because a Worker may compile a
// large module synchronously and make synchronous requests (the page refuses
// both above 8 MB / at all), and the runtime does both; and because a guest
// that runs long stalls this thread, not the page's input and drawing.
// Demo build: runtime .wasm as .wasm.zst (zfetch.js); a no-op elsewhere.
import './zfetch.js';
import { enableSingle } from '../../shims/browser/js/single.mjs';
import { assetUrl } from './page-env.js';

enableSingle();
// After `enableSingle`: these compile modules as they load.
await import('../../shims/browser/js/worker.mjs');
const { runProgram, primeRuntime } = await import('./runtime.js');

/** xterm's surface, as far as `runProgram` uses it, over messages. */
class TermProxy {
  rows = 24;
  cols = 80;
  #data = [];
  #resize = [];
  write(data) {
    // Bytes are transferred, not copied: they are always a fresh slice.
    if (data instanceof Uint8Array) self.postMessage({ kind: 'write', data }, [data.buffer]);
    else self.postMessage({ kind: 'write', data: String(data) });
  }
  writeln(text) {
    self.postMessage({ kind: 'write', data: `${text}\r\n` });
  }
  onData(f) {
    this.#data.push(f);
    return { dispose: () => this.#data.splice(this.#data.indexOf(f), 1) };
  }
  onResize(f) {
    this.#resize.push(f);
    return { dispose: () => this.#resize.splice(this.#resize.indexOf(f), 1) };
  }
  focus() {}
  reset() {}
  key(d) {
    for (const f of this.#data) f(d);
  }
  resized(rows, cols) {
    this.rows = rows;
    this.cols = cols;
    for (const f of this.#resize) f({ rows, cols });
  }
}

const term = new TermProxy();

/**
 * The compositor, in this realm (compositor-worker.js's `inRealm`): it reads
 * every guest's pixels out of memories only this thread has. The page keeps
 * its listeners and sends what it would post to a compositor Worker as `comp`
 * messages; what that Worker would post back goes out as `comp-out`.
 */
let comp = null;
async function openCompositor(width, height) {
  globalThis.GEX_COMPOSITOR_IN_REALM = true;
  const [{ inRealm }, { makeWlChannel, setOutputSize }] = await Promise.all([
    // gex-eject's `"compositor": false` builds drop it (runtime.js, GEX_COMPOSITOR).
    (typeof __GEX_COMPOSITOR__ === 'undefined' || __GEX_COMPOSITOR__)
      ? import('./compositor-worker.js')
      : Promise.reject(new Error('this build has no compositor')),
    import('../../shims/browser/js/wl-bridge.mjs'),
  ]);
  const channel = makeWlChannel();
  setOutputSize(channel, width, height);
  const stats = new Int32Array(4);
  const send = inRealm((m, transfer) => self.postMessage({ kind: 'comp-out', m }, transfer ?? []));
  // The page reads frames and windows without asking (`counters`): a copy.
  const timer = setInterval(() => self.postMessage({ kind: 'comp-stats', stats: stats.slice() }), 500);
  return {
    channel,
    stats,
    send,
    // What `runProgram` calls on a compositor.
    compositor: {
      addMemory: (token, memory, kernel) => send({ kind: 'memory', token, memory, kernel }),
      dropMemory: (token) => send({ kind: 'drop-memory', token }),
      stop: () => {
        clearInterval(timer);
        send({ kind: 'stop' });
      },
    },
  };
}

self.onmessage = async (ev) => {
  const m = ev.data;
  if (m.kind === 'comp-open') comp = openCompositor(m.width, m.height);
  else if (m.kind === 'comp') {
    const c = await comp;
    // The page's own stats and channel mean nothing here: these are the ones
    // the guests write.
    if (m.m.kind === 'init') c.send({ ...m.m, channel: c.channel, stats: c.stats.buffer });
    else c.send(m.m);
  } else if (m.kind === 'key') term.key(m.data);
  else if (m.kind === 'resize') term.resized(m.rows, m.cols);
  else if (m.kind === 'run') {
    // The page's switches: `?name=` (read through `GEX_PAGE_SEARCH`, since
    // this thread's `location` is the script's) and its `GEX_*` globals.
    Object.assign(globalThis, m.globals ?? {});
    primeRuntime(m.runtime);
    term.rows = m.rows ?? term.rows;
    term.cols = m.cols ?? term.cols;
    try {
      const opts = { ...m.opts };
      if (opts.wl?.inRealm) {
        const c = await comp;
        opts.wl = { inRealm: { channel: c.channel, compositor: c.compositor } };
        // The guest's GL, in the compositor (its frames are textures in the
        // scene's device): the non-pthread Mesa, with no ring to serve.
        if (opts.gl === 'compositor') {
          const q = new URLSearchParams(globalThis.GEX_PAGE_SEARCH ?? '');
          c.send({
            kind: 'gl',
            channel: null,
            api: 'core',
            url: q.get('glurl') || assetUrl('/gex/st/gex-wgpu-replay.js'),
            ...(q.get('mesaenv') ? { mesaEnv: q.get('mesaenv') } : {}),
            ...(q.get('wgslcache') ? { wgslCache: q.get('wgslcache') } : {}),
            ...(q.get('gltrace') === '1' ? { glTrace: true } : {}),
          });
        }
      } else delete opts.gl;
      if (opts.networkUrl || opts.pageHosts) {
        const { WispNetwork } = await import('./net.js');
        const net = new WispNetwork(opts.networkUrl ?? null, (e) => self.postMessage({ kind: 'net', event: e }),
          { pageHosts: opts.pageHosts ? new Map(opts.pageHosts) : null });
        try {
          await net.start();
        } catch (e) {
          self.postMessage({ kind: 'net', event: `error ${e?.message ?? e}` });
        }
        opts.network = net;
      }
      delete opts.networkUrl;
      delete opts.pageHosts;
      opts.onNetEvent = (e) => self.postMessage({ kind: 'net', event: e });
      const { status } = await runProgram(term, opts);
      self.postMessage({ kind: 'done', status });
    } catch (e) {
      self.postMessage({ kind: 'failed', message: e?.message ?? String(e), stack: e?.stack ?? '' });
    }
  }
};

console.info('[gex] single-thread build: the system runs in this Worker, no SharedArrayBuffer (crossOriginIsolated=' + globalThis.crossOriginIsolated + ')');
self.postMessage({ kind: 'ready' });
