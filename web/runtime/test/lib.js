// What the test pages share: `globalThis.__rt`, which run.mjs reads.
export const rt = (globalThis.__rt = { done: false, ok: true, lines: [] });
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
