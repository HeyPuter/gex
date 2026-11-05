// A call-out whose return is an entry of the unit that made it: co_call
// (lib.S, a function-tier unit) calls cb, which no unit holds, and cb returns
// to co_after's entry, which dispatch reaches through the unit's bridge, so
// the call-out sees its return through the watched entry (gex.mjs
// `#watchHit`). The watch compares the guest's stack pointer with the one the
// call-out waits for, read from the thread's context at the architecture's
// place: read from anywhere else, no return was ever the call-out's, each
// call-out ran on inside the last and they nested until the depth limit.
// Every call-out must end at its own return (depth 0 throughout).
import { native, count, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 600 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('callout-entry-return');
  await n.cc(['-O2', '-fPIC', '-shared', '-Wl,-soname,libco.so', '-o', `${d}/libco.so`, n.src('callout-entry-return/lib.S')]);
  await n.cc(['-O1', '-o', `${d}/main`, n.src('callout-entry-return/main.c'), `-L${d}`, '-lco', `-Wl,-rpath,${d}`]);
  const cache = `${d}/cgen`;
  const run = (count, env = {}) => n.g.run(seed, `${d}/main`, [count], {
    files: { ...n.ldso, [`${d}/libco.so`]: `${d}/libco.so`, [`${d}/main`]: `${d}/main` },
    flags: ['--cgen-prefer'], cwd: d, timeout: 300000,
    env: { GEX_CGEN_CACHE: cache, GEX_ELF_UNITS: 'libco.so', GEX_LOG: '1', GEX_SHARED: '1', ...env },
  });
  // A first run finds where the loader maps libco.so; its unit is built for that base.
  await n.units({ 'libco.so': `${d}/libco.so` }, cache, async () => (await run(2)).stderr);
  const k = 40;
  const r = await run(k, { GEX_CALLOUT_TRACE: '1' });
  const made = count(r.stderr, /\[callout\] #/);
  const deep = [...r.stderr.matchAll(/\[callout\] #\d+ depth (\d+)/g)].filter((x) => Number(x[1]) > 0).length;
  t.check(`exits 0 and ok`, r.code === 0 && /^ok$/m.test(r.stdout), `exit ${r.code}: ${tail(r.stdout)}`);
  t.check(`${made} call-outs (at least ${k}), ${deep} made inside another`, made >= k && deep === 0,
    r.stderr.split('\n').filter((l) => /\[callout\]/.test(l)).slice(0, 8).join(' / '));
  n.clean();
}
