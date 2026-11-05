// A call-out's loop keeps nothing per trap (gex.mjs `#startThread`):
// libcb_run is a function-tier unit (GEX_ELF_UNITS=libcb.so) calling cb,
// which no unit holds, N times, and cb raises a signal, a trap inside the
// call-out. A Wine thread's whole life can be one call-out, so such a loop
// never reaches the event loop: a promise made per trap (and its handler,
// holding the trap's error) waited in the microtask queue for good, and a
// page queued 4 GB of JS heap that way until the renderer died ("V8
// javascript OOM"). On the page's shim (shared build), stretch.mjs counts the
// most promises a thread made with no async callback between: it must not
// grow with N.
import { native, count, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 900 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('callout-heap');
  await n.cc(['-O2', '-fPIC', '-shared', '-Wl,-soname,libcb.so', '-o', `${d}/libcb.so`, n.src('after-trap/libcb.c')]);
  await n.cc(['-O1', '-o', `${d}/main`, n.src('callout-heap/main.c'), `-L${d}`, '-lcb', `-Wl,-rpath,${d}`]);
  const cache = `${d}/cgen`;
  const run = (count) => n.g.run(seed, `${d}/main`, [count], {
    files: { ...n.ldso, [`${d}/libcb.so`]: `${d}/libcb.so`, [`${d}/main`]: `${d}/main` },
    flags: ['--cgen-prefer'], node: ['--import', n.src('callout-heap/stretch.mjs')], cwd: d, timeout: 300000,
    env: { GEX_CGEN_CACHE: cache, GEX_ELF_UNITS: 'libcb.so', GEX_LOG: '1', GEX_SHARED: '1' },
  });
  // A first run finds where the loader maps libcb.so; its unit is built for that base.
  await n.units({ 'libcb.so': `${d}/libcb.so` }, cache, async () => (await run(2)).stderr);
  const most = {};
  for (const k of [100, 3000]) {
    const r = await run(k);
    const placed = count(r.stderr, /function tier for .*libcb\.so/);
    most[k] = Math.max(0, ...[...r.stderr.matchAll(/\[stretch\] thread \d+: (\d+) promises/g)].map((x) => Number(x[1])));
    t.check(`${k} call-outs: exits 0, ok, the unit placed (${placed})`, r.code === 0 && placed >= 1 && / ok$/m.test(r.stdout),
      `exit ${r.code}: ${tail(r.stdout)} / ${r.stderr.split('\n').filter((l) => /trap|rror/.test(l)).slice(0, 5).join(' / ')}`);
  }
  // Before the fix: 256 for 100 call-outs, 8192 for 3000 (about 2.7 a trap).
  t.check(`most promises in one stretch: ${most[100]} for 100 call-outs, ${most[3000]} for 3000`, most[3000] <= 64 && most[3000] <= most[100] * 2 + 16);
  n.clean();
}
