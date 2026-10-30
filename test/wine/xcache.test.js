// The translation cache across loads (`new System({ xcache: true })`) for
// code Wine reads into memory: xcbig.dll, whose sections are not page-aligned
// in its file (MinGW's 512-byte file alignment), so Wine reads, relocates and
// protects them. Phase 1 runs xcload.exe on it and saves the cache; phase 2,
// a fresh load, restores it and gets the DLL at the same base; phase 3 loads
// it at another base (relocated differently) and still runs right.
import { bootWine } from '../lib/wine.mjs';
import { expect } from '../fixtures/wine/bigdll.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1200 };

const PHASES = ['save', 'restore', 'moved'];

export default async function (t) {
  const phase = PHASES[t.phase - 1];
  const w = await bootWine(t, { system: { xcache: true } });
  await w.put('xcbig.dll');
  const r = await t.time(phase, () => w.run('xcload.exe', ['xcbig.dll', ...(phase === 'moved' ? ['move'] : [])]))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  const m = new RegExp(`^x=${expect(1000)} base=([0-9A-Fa-f]+) size=[0-9a-f]+$`, 'm').exec(r.stdout);
  t.check(`${phase}: the DLL ran`, r.code === 0 && m, `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  const base = m?.[1];
  await w.sys.shutdown();
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
  if (t.phase < PHASES.length) return t.reload({ base: t.state?.base ?? base });
}
