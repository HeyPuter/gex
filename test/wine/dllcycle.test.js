// Images loaded, run and unloaded one after another, as a plugin scan does
// (dllcycle.exe: 24 distinct DLLs of a thousand functions each at distinct
// bases, src/bigdll.c's plugNN): each one's code must run right, and come
// back right when it is loaded again at its base, translated again. Phase 1
// loads the 24 once, phase 2 four times over. Between the two, each further
// load may cost under 4096 dispatch slots (the runtime's exit lines, under
// `log: 'debug'`) and under 18 MiB of the run's peak RSS
// (each load currently keeps about 13 MiB): an unloaded image's
// translations must go with it.
import { bootWine, exitStats } from '../lib/wine.mjs';
import { expect } from '../fixtures/wine/bigdll.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1800 };

const ROUNDS = [1, 4];

export default async function (t) {
  const rounds = ROUNDS[t.phase - 1];
  const w = await bootWine(t, { debug: { log: 'debug' } });
  for (let i = 0; i < 24; i++) await w.put(`plug${String(i).padStart(2, '0')}.dll`);
  const name = `dllcycle x${rounds}`;
  const r = await t.time(name, () => w.run('dllcycle.exe', ['24', String(rounds)], { timeout: 300000 * rounds }))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  const want = `dllcycle ${24 * rounds} x ${expect(1000, 24 * rounds)}`;
  t.check(`${name}: ${want}`, r.code === 0 && r.stdout.trim() === want, `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  // This phase's process (Node) or the page's renderer: its Workers are its
  // threads. The browser's is unknown when other jobs' renderers run too.
  const rss = await t.peakRss();
  await w.sys.shutdown();
  const stats = await exitStats(t);
  const left = Math.min(...stats.map((s) => s.slotsLeftK).filter((n) => n >= 0));
  const dc = stats.filter((s) => /dllcycle/.test(s.cmd));
  t.record(`${name} kernel MiB`, Math.max(...stats.map((s) => s.kernelMib).filter((n) => n >= 0)));
  t.record(`${name} memory MiB`, Math.max(...dc.map((s) => s.memoryMib)));
  t.record(`${name} ftab`, Math.max(...dc.map((s) => s.tables?.[1] ?? 0)));
  t.record(`${name} slots left k`, left);
  if (rss) t.record(`${name} peak rss KiB`, rss);
  if (t.phase < ROUNDS.length) return t.reload({ left, rss });
  const further = 24 * (rounds - ROUNDS[0]);
  const per = Math.round((t.state.left - left) * 1000 / further);
  t.check(`dispatch slots per further load: ${per} (${t.state.left}k -> ${left}k left over ${further} loads), want under 4096`, per < 4096, `${per}`);
  if (!rss || !t.state.rss) t.record('peak RSS unknown (other renderers ran)', 1);
  else {
    const mib = (rss - t.state.rss) / 1024 / further;
    t.check(`peak RSS per further load: ${mib.toFixed(1)} MiB (${t.state.rss} -> ${rss} KiB), want under 18`, mib < 18, mib.toFixed(1));
  }
}
