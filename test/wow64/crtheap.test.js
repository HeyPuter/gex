// msvcr120's heap with and without gex's crt_heap_wine provider
// (crtheap.c): the same answers both ways (sizes as _msize gives them,
// contents kept across realloc, zeroing, alignment, the NULL edges, a
// 200000-operation churn), and the blocks from dlmalloc with the provider
// ("owner other") and from Wine's process heap without it ("owner wine").
// With a native i386 side msvcr120 is itself a native module, with no unit
// for the provider to take the place of, and its heap is Wine's either way.
// The first run finds where msvcr120 is placed; its unit is built there,
// with and without GEX_PROVIDER_SKIP for the heap exports. Natively (gexc
// run) and on the page's shim.
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 2400 };

const SKIP = { GEX_PROVIDER_SKIP: ['malloc', '_malloc_crt', 'calloc', '_calloc_crt', 'free', 'realloc', '_realloc_crt', '_recalloc',
  '_msize', '_expand', '??2@YAPAXI@Z', '??_U@YAPAXI@Z', '??2@YAPAXIHPBDH@Z', '??_U@YAPAXIHPBDH@Z', '??3@YAXPAX@Z', '??_V@YAXPAX@Z',
  '_aligned_offset_realloc', '_aligned_realloc'].join(',') };
const ANSWER = /^(size |crt|calloc|recalloc|expand|realloc |msize |new:|aligned|churn)/;

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('crtheap');
  const pown = w.i386Native ? 'wine' : 'other';
  const units = await w.providers(join(w.work, 'units'));
  const msvcr = join(w.dist, 'lib/wine/i386-windows/msvcr120.dll');
  const build = (base) => w.unit(msvcr, units, { base, modes: [{ GEX_CGEN_PRECOMPILE: '1' }, { GEX_CGEN_PRECOMPILE: '1', ...SKIP }] });
  const lines = (r) => r.out.split('\n').map((l) => l.replace(/\r$/, ''));
  const answers = (r) => lines(r).filter((l) => ANSWER.test(l));
  const verdict = (leg, r, owner, ref) => {
    const l = lines(r);
    const why = [];
    if (!l.includes('crtheap done')) why.push("no 'crtheap done'");
    else if (!l.includes(`owner ${owner}`)) why.push(`want owner ${owner}, got '${l.find((x) => x.startsWith('owner')) ?? ''}'`);
    else if (l.some((x) => /^churn: \d+ bytes, [1-9]/.test(x))) why.push(`churn: ${l.find((x) => x.startsWith('churn'))}`);
    else if (ref) {
      const a = answers(ref), b = answers(r);
      const i = a.findIndex((x, k) => x !== b[k]);
      if (i >= 0 || a.length !== b.length) why.push(`answers differ from Wine's own at ${i >= 0 ? i : a.length}: ${a[i] ?? '(end)'} | ${b[i] ?? '(end)'}`);
    }
    t.check(leg, why.length === 0, `${why.join('; ')}; ${w.detail(r, 6)}`);
  };
  const base = (r) => { const m = /^msvcr120 at (?:0x)?([0-9a-fA-F]+)/m.exec(r.out); return m ? `0x${m[1]}` : null; };
  const native = (more, name) => w.native(['C:\\crtheap.exe'], { files: { 'crtheap.exe': exe }, env: { GEX_CGEN_CACHE: units, ...more }, name });
  const page = (more, name) => w.page(['/t/crtheap.exe'], { files: { '/t/crtheap.exe': exe }, flags: ['--cgen-prefer'],
    env: { GEX_CGEN_CACHE: units, ...more }, name });

  const n0 = await native({}, 'n0');
  verdict('native, no unit', n0, 'wine');
  const nbase = base(n0);
  if (t.check('native: msvcr120 placed', !!nbase, w.detail(n0, 3))) {
    await build(nbase);
    verdict('native, provider skipped', await native(SKIP, 'n2'), 'wine', n0);
    verdict('native, provider', await native({}, 'n1'), pown, n0);
  }
  let p0 = await page({}, 'p0');
  const pbase = base(p0);
  if (pbase && pbase !== nbase) {
    await build(pbase);
    p0 = await page({}, 'p0b');
  }
  verdict('page, provider', p0, pown, n0);
  verdict('page, provider skipped', await page(SKIP, 'p1'), 'wine', n0);
  w.clean();
}
