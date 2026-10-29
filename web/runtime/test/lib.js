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
