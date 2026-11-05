// Function-tier units instantiated in a Worker at their first call, not when
// it catches up (hosts/browser/js/gex.mjs `#cgenDefer`, main.c): a thread's
// Worker instantiates libd.so's unit for its constructor, and libb.so's from
// inside that call; b_two, rewritten before, runs its new code there. On the
// page's shim (the SAB build): it must print what it should, and instantiate
// units in the thread's Worker at their first call (the "[unit]" lines,
// debug output).
import { native, count, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 600 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('lazy-units');
  const lib = (l, ...more) => n.cc(['-O2', '-fPIC', '-shared', `-Wl,-soname,${l}.so`, '-o', `${d}/${l}.so`, n.src(`lazy-units/${l}.c`), ...more]);
  await lib('libb');
  await lib('libd', `-L${d}`, '-lb');
  await lib('libe', `-L${d}`, '-lb');
  await n.cc(['-O1', '-pthread', '-o', `${d}/main`, n.src('lazy-units/main.c'), `-L${d}`, '-le', '-lb', `-Wl,-rpath,${d}`, '-ldl']);
  const libs = Object.fromEntries(['libb', 'libd', 'libe'].map((l) => [`${l}.so`, `${d}/${l}.so`]));
  const cache = `${d}/cgen`;
  const run = () => n.g.run(seed, `${d}/main`, [d], {
    files: { ...n.ldso, ...Object.fromEntries(Object.values(libs).map((p) => [p, p])), [`${d}/main`]: `${d}/main` },
    flags: ['--cgen-prefer'], cwd: d, timeout: 300000,
    env: { GEX_CGEN_CACHE: cache, GEX_ELF_UNITS: Object.keys(libs).join(','), GEX_SHARED: '1', GEX_TEST_KEEP_PROBE: '1', GEX_LOG: 'unit' },
  });
  // A first run finds where the loader maps the libraries; their units are built for those bases.
  await n.units(libs, cache, async () => (await run()).stderr);
  const r = await run();
  const placed = count(r.stderr, /lib[bde]\.so at 0x[0-9a-f]*: wanting its unit/);
  const missing = count(r.stderr, /no function-tier unit/);
  const late = count(r.stderr, /tid \d+: unit at \d+ instantiated at its first call/);
  t.eq('output', r.guest, 'main e_two 1007\nthread d_get 99\nthread e_two 42\nthread b_mul 42',
    r.code === 124 || r.signal === 'SIGKILL' ? 'hung' : r.stderr.split('\n').filter((l) => /trap|rror/.test(l)).slice(0, 5).join(' / '));
  t.check(`exits 0, units placed ${placed} times (3), ${missing} without their unit`, r.code === 0 && placed === 3 && missing === 0, `exit ${r.code}`);
  t.check(`${late} instantiated in the thread at a first call (at least 2)`, late >= 2);
  n.clean();
}
