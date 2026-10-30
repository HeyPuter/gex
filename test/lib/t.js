// The test context `t`, the part both hosts share (lib/browser.js, lib/node.mjs
// add what is theirs). Every check is a line; the first FAIL does not stop
// the test.

/** Thrown by `t.skip`: the test ends as `skip`, not FAIL. */
export class Skip extends Error {}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * `host`: 'browser' or 'node'; `arch`: the translated arch the runner asked
 * for ('aarch64', 'x86'); `phase`, `state`: this run's phase (1 first) and
 * what the one before handed `reload`; `res`: `{ checks, fail, times }`, kept
 * across phases; `emit(line)`: a line of output.
 */
export function makeT({ host, arch, phase = 1, state = null, res, emit }) {
  const t = {
    host,
    arch,
    phase,
    state,
    say: (s) => emit(String(s)),
    check(what, cond, detail = '') {
      res.checks++;
      if (!cond) res.fail.push(what);
      emit(`${cond ? 'ok' : 'FAIL'} ${what}${!cond && detail ? `: ${detail}` : ''}`);
      return cond;
    },
    /** `more`: said too when it fails (a log's tail). */
    eq: (what, got, want, more = '') => t.check(what, JSON.stringify(got) === JSON.stringify(want), `got ${JSON.stringify(got)}, want ${JSON.stringify(want)}${more ? ` (${more})` : ''}`),
    /** A colour within `tol` of `want` in every channel (`[r, g, b]` or
     *  `[r, g, b, a]`). */
    near: (what, got, want, tol = 8) => t.check(what, want.every((w, i) => Math.abs(got[i] - w) <= tol), `got ${JSON.stringify(got)}, want ${JSON.stringify(want)} within ${tol}`),
    /** End the test as skipped, saying why. */
    skip(why) {
      throw new Skip(why);
    },
    timeout: (p, ms, what) => Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error(`${what}: timed out after ${ms} ms`)), ms))]),
    async until(fn, what, ms = 20000) {
      for (const t0 = performance.now(); performance.now() - t0 < ms;) {
        const v = await fn();
        if (v) return v;
        await sleep(25);
      }
      throw new Error(`${what}: not within ${ms} ms`);
    },
    /** `fn`'s ms, in the result's `times` as `name`. */
    async time(name, fn) {
      const t0 = performance.now();
      try {
        return await fn();
      } finally {
        res.times[name] = Math.round(performance.now() - t0);
      }
    },
    /** A stream's text as lines, as they come (a program's stdout by default). */
    lines(p, stream = p.stdout) {
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
    },
  };
  return t;
}

/** Run test `fn` with `t`: `{ result, skip }` once it ends ('ok', 'FAIL',
 *  'skip'). A throw is a FAIL line. */
export async function runTest(fn, t, res) {
  try {
    await fn(t);
  } catch (e) {
    // A skip after a failed check is still a FAIL.
    if (e instanceof Skip && !res.fail.length) return { result: 'skip', skip: e.message };
    if (e instanceof Skip) return { result: 'FAIL', skip: e.message };
    res.fail.push('threw');
    t.say(`FAIL ${e?.stack ?? e}`);
  }
  return { result: res.fail.length ? 'FAIL' : 'ok', skip: null };
}
