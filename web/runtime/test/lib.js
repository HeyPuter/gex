// What the test pages share: `globalThis.__rt`, which run.mjs reads.
export const rt = (globalThis.__rt = { done: false, ok: true, lines: [], input: [], inputDone: 0 });
// A page that reloads itself (`reload`) keeps what it said before.
try {
  const kept = JSON.parse(sessionStorage.getItem('__rt') ?? 'null');
  if (kept) Object.assign(rt, { ok: kept.ok, lines: kept.lines });
} catch {}
/** Reload the page with `search`, keeping the results so far. */
export function reload(search) {
  sessionStorage.setItem('__rt', JSON.stringify({ ok: rt.ok, lines: rt.lines }));
  const u = new URL(location.href);
  for (const [k, v] of new URLSearchParams(search)) u.searchParams.set(k, v);
  location.href = u.href;
}
export const say = (s) => { rt.lines.push(String(s)); console.log(String(s)); };
export function check(what, cond, detail = '') {
  if (!cond) rt.ok = false;
  say(`${cond ? 'ok' : 'FAIL'} ${what}${!cond && detail ? `: ${detail}` : ''}`);
  return cond;
}
export const eq = (what, got, want) => check(what, JSON.stringify(got) === JSON.stringify(want), `got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
export async function main(fn) {
  try {
    await fn();
  } catch (e) {
    rt.ok = false;
    say(`FAIL ${e?.stack ?? e}`);
  }
  rt.done = true;
}
export const timeout = (p, ms, what) => Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error(`${what}: timed out after ${ms} ms`)), ms))]);

/** The runtime's debug switches for a test page: vite dev's Workers need
 *  workergraph=0; `?arch=` (run.mjs RT_ARCH) picks the translated arch. */
export function testDebug() {
  const arch = new URLSearchParams(location.search).get('arch');
  return { workergraph: '0', ...(arch ? { arch } : {}) };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export async function until(fn, what, ms = 20000) {
  for (const t0 = performance.now(); performance.now() - t0 < ms;) {
    const v = await fn();
    if (v) return v;
    await sleep(25);
  }
  throw new Error(`${what}: not within ${ms} ms`);
}

/** A program's stdout as lines, as they come. */
export function lines(p, stream = p.stdout) {
  const got = [];
  (async () => {
    const dec = new TextDecoder();
    let buf = '';
    for (const r = stream.getReader(); ;) {
      const { value, done } = await r.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      for (let i; (i = buf.indexOf('\n')) >= 0;) { got.push(buf.slice(0, i)); buf = buf.slice(i + 1); }
    }
  })();
  return got;
}

/** Real input through the browser (run.mjs sends each with CDP): `events`
 *  are `{ method: 'Input.dispatchMouseEvent', ... }` and the like. Resolves
 *  once they are sent. */
export async function input(...events) {
  rt.input.push(...events);
  const want = rt.input.length;
  await until(() => rt.inputDone >= want, 'the test runner sending input', 10000);
}

/** A real click at page coordinates `(x, y)`. */
export const click = (x, y) => input(
  { method: 'Input.dispatchMouseEvent', type: 'mousePressed', x, y, button: 'left', buttons: 1, clickCount: 1 },
  { method: 'Input.dispatchMouseEvent', type: 'mouseReleased', x, y, button: 'left', buttons: 0, clickCount: 1 },
);
