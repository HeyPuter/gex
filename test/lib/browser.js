// A test in a page of its own (run.mjs `--host browser`): the page is
// /__t/page?test=GROUP/NAME&src=URL&arch=ARCH. `globalThis.__t` is what the
// runner reads: `lines` as they come, `asks` it sends over CDP (answering
// each in `replies`), and `done` with the result.

import { makeT, runTest } from './t.js';

const q = new URLSearchParams(location.search);
// What a phase before this one kept (`reload`).
let kept = null;
try {
  kept = JSON.parse(sessionStorage.getItem('__t') ?? 'null');
  sessionStorage.removeItem('__t');
} catch {}
const res = kept?.res ?? { checks: 0, fail: [], times: {} };
const g = (globalThis.__t = { done: false, lines: kept?.lines ?? [], asks: [], replies: {}, res });
const emit = (s) => {
  g.lines.push(s);
  console.log(s);
};
const t = makeT({ host: 'browser', arch: q.get('arch'), phase: kept?.phase ?? 1, state: kept?.state ?? null, res, emit });

Object.assign(t, {
  /** The runtime's assets, as a System's `baseUrl`. */
  baseUrl: '/',
  /** The runtime's debug switches for this run: vite dev's Workers need
   *  workergraph=0; `arch` the runner's. */
  debug: (more = {}) => ({ workergraph: '0', ...(t.arch ? { arch: t.arch } : {}), ...more }),
  /** A file of the runner's work directory (fixtures/images.mjs), as a URL. */
  work: (name) => new URL(`/__t/work/${name}`, location.href).href,
  /** How many times the page asked for work file `name`. */
  hits: async (name) => (await fetch(`/__t/hits?${encodeURIComponent(name)}`)).json(),
  /** Run this test again in a fresh page, as phase `t.phase + 1`, with
   *  `state` as `t.state`; what was checked so far is kept. */
  reload(state = null) {
    sessionStorage.setItem('__t', JSON.stringify({ res, lines: g.lines, phase: t.phase + 1, state }));
    location.reload();
    return new Promise(() => {});
  },
  /** CDP commands on this page, sent by the runner in order: their results.
   *  `save: NAME` on one whose result has `data` (a screenshot): the runner
   *  writes it to its artefacts directory, and the result is `{ path }`. */
  async cdp(...asks) {
    const from = g.asks.length;
    g.asks.push(...asks);
    await t.until(() => from + asks.length - 1 in g.replies, 'the test runner answering', 30000);
    return asks.map((a, i) => {
      const r = g.replies[from + i];
      delete g.replies[from + i];
      if (r.error) throw new Error(`${a.method}: ${r.error}`);
      return r.result;
    });
  },
  /** Real input through the browser: `{ method: 'Input.dispatchMouseEvent',
   *  ... }` and the like. Resolves once they are sent. */
  async input(...events) {
    await t.cdp(...events);
  },
  /** The page as the browser shows it, saved as `NAME` (.png) among the
   *  run's artefacts: its path. `clip`: `{ x, y, width, height }` only. */
  async screenshot(name, clip) {
    const [r] = await t.cdp({ method: 'Page.captureScreenshot', format: 'png', save: `${name}.png`, ...(clip ? { clip: { ...clip, scale: 1 } } : {}) });
    return r.path;
  },
  /** The page's pixels in `clip` (`{ x, y, width, height }`, CSS pixels), as
   *  the browser composited them: what a WebGPU canvas showed, which the page
   *  itself cannot read back. An ImageData. */
  async pixels(clip) {
    const [r] = await t.cdp({ method: 'Page.captureScreenshot', format: 'png', clip: { ...clip, scale: 1 } });
    const bytes = Uint8Array.from(atob(r.data), (c) => c.charCodeAt(0));
    const img = await createImageBitmap(new Blob([bytes], { type: 'image/png' }));
    const c = new OffscreenCanvas(img.width, img.height).getContext('2d');
    c.drawImage(img, 0, 0);
    return c.getImageData(0, 0, img.width, img.height);
  },
  /** One pixel at page coordinates `(x, y)`: `[r, g, b, a]`. */
  async pixel(x, y) {
    return [...(await t.pixels({ x, y, width: 1, height: 1 })).data];
  },
  /** A real click at page coordinates `(x, y)`. */
  click: (x, y) => t.input(
    { method: 'Input.dispatchMouseEvent', type: 'mousePressed', x, y, button: 'left', buttons: 1, clickCount: 1 },
    { method: 'Input.dispatchMouseEvent', type: 'mouseReleased', x, y, button: 'left', buttons: 0, clickCount: 1 },
  ),
});

(async () => {
  let r;
  try {
    const mod = await import(/* @vite-ignore */ q.get('src'));
    // The page's own elements (`meta.html`), before the test looks for them.
    if (mod.meta?.html) document.body.insertAdjacentHTML('beforeend', mod.meta.html);
    r = await runTest(mod.default, t, res);
  } catch (e) {
    res.fail.push('load');
    emit(`FAIL ${e?.stack ?? e}`);
    r = { result: 'FAIL', skip: null };
  }
  Object.assign(g, r, { done: true });
})();
