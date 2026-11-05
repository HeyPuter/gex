// Blocking inside a nested call-out: libcb_run (fixtures/native/single-callout)
// is a function-tier unit (GEX_ELF_UNITS=libcb.so) and calls cb, which no unit
// holds and which blocks (a pipe read, nanosleep). In the single-thread build
// call_out / ftab_call are Suspending imports that run the nested guest on its
// own promising stack, so the block parks only this fiber (before, it trapped
// with "trying to suspend JS frames"). Run under run-node.mjs in the
// single-thread build and in the SharedArrayBuffer one: both must place the
// unit and print the right sum. An aarch64 musl program.
import { join } from 'node:path';
import { native, tail, count } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 900 };

export default async function (t) {
  const n = native(t);
  const seed = n.g.pub('seed.shared.wasm');
  const d = n.dir('single-callout');
  await n.cc(['-O2', '-fPIC', '-shared', '-Wl,-soname,libcb.so', '-o', join(d, 'libcb.so'), n.src('single-callout/lib.c')]);
  await n.cc(['-O1', '-o', join(d, 'main'), n.src('single-callout/main.c'), `-L${d}`, '-lcb', `-Wl,-rpath,${d}`]);
  const cache = join(d, 'cgen');
  const run = (single) => n.g.run(seed, join(d, 'main'), [], {
    files: { ...n.ldso, [join(d, 'libcb.so')]: join(d, 'libcb.so'), [join(d, 'main')]: join(d, 'main') },
    flags: ['--cgen-prefer'], single, cwd: d, timeout: 300000,
    env: { GEX_LOG: '1', GEX_CGEN_CACHE: cache, GEX_ELF_UNITS: 'libcb.so', ...(single ? {} : { GEX_SHARED: '1' }) },
  });
  // A first run finds where the loader maps libcb.so; its unit is built for that base.
  await n.units({ 'libcb.so': join(d, 'libcb.so') }, cache, async () => (await run(true)).stderr);
  for (const [leg, single] of [['single', true], ['shared', false]]) {
    const r = await run(single);
    const placed = count(r.stderr, /function tier for .*libcb\.so/);
    t.check(`${leg}: the unit placed (${placed} times)`, placed >= 1, tail(r.stderr));
    t.check(`${leg}: the callback read and the sum`, r.code === 0 && /^callback read w$/m.test(r.stdout) && /^libcb_run = 100 ok$/m.test(r.stdout),
      `exit ${r.code}: ${tail(r.stdout)} / ${tail(r.stderr, 3)}`);
  }
  n.clean();
}
