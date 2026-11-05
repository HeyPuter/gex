// msvcp120's _Thrd_current (thrd.c), with and without gex's msvcp120_thrd
// provider: every call's id is the calling thread's, no handle leaks, and
// with msvcp120's function-tier unit in place the provider answers every
// call (zero=ALL). The first run finds where msvcp120 is placed and its unit
// is built there, with and without GEX_PROVIDER_SKIP=_Thrd_current; each
// leg then runs with the provider and with it skipped (zero=0: Wine's own
// code). With a native i386 side msvcp120 is native code, which the
// provider, a translated unit's, does not replace, so Wine's own code
// answers in both. Natively (gexc run) and on the page's shim.
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 2400 };

const N = 4;
const CALLS = 2000;

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('thrd');
  const all = w.i386Native ? 0 : (N + 1) * CALLS;
  const units = await w.providers(join(w.work, 'units'));
  const msvcp = join(w.dist, 'lib/wine/i386-windows/msvcp120.dll');
  const skip = { GEX_PROVIDER_SKIP: '_Thrd_current' };
  const build = async (base) => {
    await w.unit(msvcp, units, { base, modes: [{ GEX_CGEN_PRECOMPILE: '1' }, { GEX_CGEN_PRECOMPILE: '1', ...skip }] });
  };
  const verdict = (leg, r, zero) => t.check(`${leg}: thrd ok ${N + 1} ${CALLS} zero=${zero}`,
    new RegExp(`^thrd ok ${N + 1} ${CALLS} zero=${zero}\\s*$`, 'm').test(r.out), w.detail(r));
  const base = (r) => { const m = /^msvcp120 at (?:0x)?([0-9a-fA-F]+)/m.exec(r.out); return m ? `0x${m[1]}` : null; };
  const native = (more, name) => w.native(['C:\\thrd.exe', N, CALLS], { files: { 'thrd.exe': exe }, env: { GEX_CGEN_CACHE: units, ...more }, name });
  const page = (more, name) => w.page(['/t/thrd.exe', N, CALLS], { files: { '/t/thrd.exe': exe }, flags: ['--cgen-prefer'],
    env: { GEX_CGEN_CACHE: units, ...more }, name });

  const n0 = await native({}, 'n0');
  verdict('native, no unit', n0, 0);
  const nbase = base(n0);
  if (t.check('native: msvcp120 placed', !!nbase, w.detail(n0))) {
    await build(nbase);
    verdict('native, provider', await native({}, 'n1'), all);
    verdict('native, provider skipped', await native(skip, 'n2'), 0);
  }
  let p0 = await page({}, 'p0');
  const pbase = base(p0);
  if (pbase && pbase !== nbase) {
    await build(pbase);
    p0 = await page({}, 'p0b');
  }
  verdict('page, provider', p0, nbase || pbase ? all : 0);
  verdict('page, provider skipped', await page(skip, 'p1'), 0);
  w.clean();
}
