// A test in a page of its own (run.mjs `--host browser`): the page is
// /__t/page?test=GROUP/NAME&src=URL&arch=ARCH. `globalThis.__t` is what the
// runner reads: `lines` as they come, `input` it sends over CDP, and `done`
// with the result.

import { makeT, runTest } from './t.js';

const q = new URLSearchParams(location.search);
// What a phase before this one kept (`reload`).
let kept = null;
try {
  kept = JSON.parse(sessionStorage.getItem('__t') ?? 'null');
  sessionStorage.removeItem('__t');
} catch {}
const res = kept?.res ?? { checks: 0, fail: [], times: {} };
const g = (globalThis.__t = { done: false, lines: kept?.lines ?? [], input: [], inputDone: 0, res });
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
  /** Real input through the browser (the runner sends each over CDP):
   *  `{ method: 'Input.dispatchMouseEvent', ... }` and the like. Resolves once
   *  they are sent. */
  async input(...events) {
    g.input.push(...events);
    const want = g.input.length;
    await t.until(() => g.inputDone >= want, 'the test runner sending input', 10000);
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
