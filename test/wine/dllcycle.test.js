// Images loaded, run and unloaded one after another, as a plugin scan does
// (dllcycle.exe: 24 distinct DLLs of a thousand functions each at distinct
// bases, src/bigdll.c's plugNN): each one's code must run right, and come
// back right when it is loaded again at its base, translated again. Phase 1
// loads the 24 once, phase 2 four times over.
import { bootWine } from '../lib/wine.mjs';
import { expect } from '../fixtures/wine/bigdll.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1800 };

const ROUNDS = [1, 4];

export default async function (t) {
  const rounds = ROUNDS[t.phase - 1];
  const w = await bootWine(t);
  for (let i = 0; i < 24; i++) await w.put(`plug${String(i).padStart(2, '0')}.dll`);
  const name = `dllcycle x${rounds}`;
  const r = await t.time(name, () => w.run('dllcycle.exe', ['24', String(rounds)], { timeout: 300000 * rounds }))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  const want = `dllcycle ${24 * rounds} x ${expect(1000, 24 * rounds)}`;
  t.check(`${name}: ${want}`, r.code === 0 && r.stdout.trim() === want, `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  await w.sys.shutdown();
  if (t.phase < ROUNDS.length) return t.reload();
}
