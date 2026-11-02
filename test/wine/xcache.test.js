// The translation cache across loads (`new System({ xcache: true })`) for
// code Wine reads into memory: xcbig.dll, whose sections are not page-aligned
// in its file (MinGW's 512-byte file alignment), so Wine reads, relocates and
// protects them. Phase 1 runs xcload.exe on it and saves the cache; phase 2,
// a fresh load, restores it and gets the DLL at the same base; phase 3 loads
// it at another base (relocated differently) and still runs right. The
// blocks each run translated in the DLL (the runtime's exit lines, under
// `log: 'debug'`): the restored run under 5% of the first's, the moved one
// at least half of it (none of the first base's translations fit).
import { bootWine, exitStats, exitLineOf, blocksIn, printedRanges } from '../lib/wine.mjs';
import { expect } from '../fixtures/wine/bigdll.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1200 };

const PHASES = ['save', 'restore', 'moved'];

export default async function (t) {
  const phase = PHASES[t.phase - 1];
  // A cache of its own name: another test's on the same page path would
  // be restored and saved over this one's when the two run at once.
  const w = await bootWine(t, { system: { xcache: 'xcache' }, debug: { log: 'debug' } });
  await w.put('xcbig.dll');
  const r = await t.time(phase, () => w.run('xcload.exe', ['xcbig.dll', ...(phase === 'moved' ? ['move'] : [])]))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  const m = new RegExp(`^x=${expect(1000)} base=([0-9A-Fa-f]+) size=[0-9a-f]+$`, 'm').exec(r.stdout);
  t.check(`${phase}: the DLL ran`, r.code === 0 && m, `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  const base = m?.[1];
  // Its exit line, which says what it translated: after its save.
  t.check(`${phase}: xcload's exit line`, await exitLineOf(t, /xcload\.exe/), 'none in 60 s');
  await w.sys.shutdown();
  const stats = await exitStats(t);
  const blocks = blocksIn(stats, printedRanges(r.stdout));
  t.record(`${phase} DLL blocks`, blocks);
  const first = t.state?.blocks ?? blocks;
  if (phase === 'save') t.check(`save: the DLL's blocks translated (${blocks})`, blocks > 1000, `${blocks}`);
  if (phase === 'restore') t.check(`restore: under 5% of the first run's blocks (${blocks} after ${first})`, blocks < first / 20, `${blocks} after ${first}`);
  if (phase === 'moved') t.check(`moved: at least half the first run's blocks again (${blocks} after ${first})`, blocks >= first / 2, `${blocks} after ${first}`);
  // A process's save is on the runtime's log; the restore, made by the
  // System before the first process, on its console.
  const re = /\bxcache .*: (saved|.*restored|nothing saved|not saved)/;
  const logs = [...await t.logs(re), ...w.sys.state.console.split('\n').filter((l) => re.test(l))];
  if (phase === 'save') {
    t.check('save: the cache saved', logs.some((l) => /: saved \d/.test(l)), logs.slice(-4).join(' / ') || 'no xcache line');
  } else {
    const n = Math.max(0, ...logs.map((l) => Number(/restored (\d+) modules/.exec(l)?.[1] ?? 0)));
    t.record(`${phase} restored modules`, n);
    t.check(`${phase}: the cache restored`, n > 0, logs.slice(-4).join(' / ') || 'no xcache line');
    if (phase === 'restore') t.check('restore: the DLL at the same base', base && base === t.state?.base, `${base} after ${t.state?.base}`);
    if (phase === 'moved') t.check('moved: the DLL at another base', base && base !== t.state?.base, `${base} after ${t.state?.base}`);
  }
  if (t.phase < PHASES.length) return t.reload({ base: t.state?.base ?? base, blocks: first });
}
