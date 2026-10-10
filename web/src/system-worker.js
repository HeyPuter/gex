// The single-thread build's one Worker: the whole guest system -- the global
// kernel, every process and thread as a fiber, the Wisp connection -- on this
// thread, with no SharedArrayBuffer (docs/single-thread.md). The page keeps
// the terminal and talks to this by messages.
//
// A Worker rather than the page's own thread because a Worker may compile a
// large module synchronously and make synchronous requests (the page refuses
// both above 8 MB / at all), and the runtime does both; and because a guest
// that runs long stalls this thread, not the page's input and drawing.
import { enableSingle } from '../../shims/browser/js/single.mjs';

enableSingle();
// After `enableSingle`: these compile modules as they load.
await import('../../shims/browser/js/worker.mjs');
const { runProgram } = await import('./runtime.js');

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

self.onmessage = async (ev) => {
  const m = ev.data;
  if (m.kind === 'key') term.key(m.data);
  else if (m.kind === 'resize') term.resized(m.rows, m.cols);
  else if (m.kind === 'run') {
    // The page's switches: `?name=` (read through `GEX_PAGE_SEARCH`, since
    // this thread's `location` is the script's) and its `GEX_*` globals.
    Object.assign(globalThis, m.globals ?? {});
    term.rows = m.rows ?? term.rows;
    term.cols = m.cols ?? term.cols;
    try {
      const opts = { ...m.opts };
      if (opts.networkUrl) {
        const { WispNetwork } = await import('./net.js');
        const net = new WispNetwork(opts.networkUrl, (e) => self.postMessage({ kind: 'net', event: e }));
        try {
          await net.start();
        } catch (e) {
          self.postMessage({ kind: 'net', event: `error ${e?.message ?? e}` });
        }
        opts.network = net;
      }
      delete opts.networkUrl;
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
