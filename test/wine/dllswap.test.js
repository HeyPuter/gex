// Three images that take turns at one address, each loaded, run and unloaded
// in turn, as Wine's ACM codecs are (xcload2.exe on src/bigdll.c's swap1000,
// swap700 and swap400, all based at 0x340000000), with no translation cache:
// each comes back at the base the others used, and every call must give its
// own image's result. Phase 1 takes one round, phase 2 eight.
import { bootWine } from '../lib/wine.mjs';
import { expect } from '../fixtures/wine/bigdll.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1800 };

const ROUNDS = [1, 8];
const SIZES = [1000, 700, 400];

export default async function (t) {
  const rounds = ROUNDS[t.phase - 1];
  const w = await bootWine(t);
  for (const n of SIZES) await w.put(`swap${n}.dll`);
  const ops = [];
  const want = [];
  for (let r = 0; r < rounds; r++) {
    for (const n of SIZES) {
      ops.push(`swap${n}.dll`, '-');
      want.push(`swap${n}\\.dll x=${expect(n)} base=[0-9A-Fa-f]+ size=[0-9a-f]+`, 'unloaded');
    }
  }
  const name = `dllswap x${rounds}`;
  const r = await t.time(name, () => w.run('xcload2.exe', ops, { timeout: 600000 }))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  t.check(`${name}: every call's result`, r.code === 0 && new RegExp(`^${want.join('\\n')}\\n?$`).test(r.stdout), `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  await w.sys.shutdown();
  if (t.phase < ROUNDS.length) return t.reload();
}
