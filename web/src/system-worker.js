// The single-thread build's one Worker: the whole guest system -- the global
// kernel, every process and thread as a fiber, the guest's sockets -- on this
// thread, with no SharedArrayBuffer. The page keeps
// the terminal and talks to this by messages.
//
// A Worker rather than the page's own thread because a Worker may compile a
// large module synchronously and make synchronous requests (the page refuses
// both above 8 MB / at all), and the runtime does both; and because a guest
// that runs long stalls this thread, not the page's input and drawing.
import { enableSingle } from '../../shims/browser/js/single.mjs';
import { assetUrl } from './page-env.js';

enableSingle();
// After `enableSingle`: these compile modules as they load. Awaited by each
// message (below), not at the top: a bundler's classic-script Worker cannot
// hold a top-level await. Messages resume in the order they came.
const loaded = import('../../shims/browser/js/worker.mjs').then(() => import('./runtime.js'));

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

/** The compositor, in this realm (compositor-in-realm.js), with the
 *  page's counters sent a copy at a time. */
let comp = null;
async function openHere(width, height) {
  const { openCompositor } = await import('./compositor-in-realm.js');
  const c = await openCompositor(width, height, 1, (m, transfer) => self.postMessage({ kind: 'comp-out', m }, transfer ?? []));
  // The page reads frames and windows without asking (`counters`): a copy.
  const timer = setInterval(() => self.postMessage({ kind: 'comp-stats', stats: c.stats() }), 500);
  const stop = c.compositor.stop;
  c.compositor.stop = () => {
    clearInterval(timer);
    stop();
  };
  return c;
}

self.onmessage = async (ev) => {
  const { runSingle, primeRuntime } = await loaded;
  const m = ev.data;
  if (m.kind === 'comp-open') comp = openHere(m.width, m.height);
  else if (m.kind === 'comp') (await comp).send(m.m);
  else if (m.kind === 'key') term.key(m.data);
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
      if (opts.netRelay || opts.pageHosts) {
        const { Sockets, pageRelay } = await import('./net.js');
        const dial = (m, t) => self.postMessage({ kind: 'net-dial', ...m }, t);
        const net = new Sockets(opts.netRelay ? pageRelay(dial, opts.netRelay) : null,
          { pageHosts: opts.pageHosts ?? null, onEvent: (e) => self.postMessage({ kind: 'net', event: e }) });
        opts.network = net;
      }
      delete opts.netRelay;
      delete opts.pageHosts;
      opts.onNetEvent = (e) => self.postMessage({ kind: 'net', event: e });
      const { status } = await runSingle(term, opts);
      self.postMessage({ kind: 'done', status });
    } catch (e) {
      self.postMessage({ kind: 'failed', message: e?.message ?? String(e), stack: e?.stack ?? '' });
    }
  }
};

console.info('[gex] single-thread build: the system runs in this Worker, no SharedArrayBuffer (crossOriginIsolated=' + globalThis.crossOriginIsolated + ')');
self.postMessage({ kind: 'ready' });
