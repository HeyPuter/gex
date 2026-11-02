// The translation cache (`new System({ xcache: true })`) with two DLLs that
// trade places (src/bigdll.c's xca and xcb): each run loads A then B, unloads
// both and loads B then A, so each image comes back where the other was.
// Phase 1 saves the cache; phase 2 restores it and must give every call's
// result; phase 3 loads them in the other order on the same cache, where at
// every base the cache holds translations of the other image's bytes, which
// must not run. The restored run translates under 5% of the first run's
// blocks in the two images (the runtime's exit lines, `log: 'debug'`).
import { bootWine, exitStats, exitLineOf, blocksIn, printedRanges } from '../lib/wine.mjs';
import { expect } from '../fixtures/wine/bigdll.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1200 };

const PHASES = ['save', 'restore', 'swapped'];

export default async function (t) {
  const phase = PHASES[t.phase - 1];
  // A cache of its own name: another test's on the same page path would
  // be restored and saved over this one's when the two run at once.
  const w = await bootWine(t, { system: { xcache: 'xcache2' }, debug: { log: 'debug' } });
  await w.put('xca.dll');
  await w.put('xcb.dll');
  const [p, q] = phase === 'swapped' ? ['xcb', 'xca'] : ['xca', 'xcb'];
  const line = (d) => `${d}\\.dll x=${expect(d === 'xca' ? 1000 : 700)} base=[0-9A-Fa-f]+ size=[0-9a-f]+`;
  const want = new RegExp(`^${[line(p), line(q), 'unloaded', line(q), line(p)].join('\\n')}\\n?$`);
  const r = await t.time(phase, () => w.run('xcload2.exe', [`${p}.dll`, `${q}.dll`, '-', `${q}.dll`, `${p}.dll`]))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  // The program's own lines: a process's cache save may be said among them.
  const own = r.stdout.split('\n').filter((l) => /^(xc[ab]\.dll |unloaded$)/.test(l)).join('\n');
  t.check(`${phase}: every call's result`, r.code === 0 && want.test(own), `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  // Its exit line, which says what it translated: after its save.
  t.check(`${phase}: xcload2's exit line`, await exitLineOf(t, /xcload2\.exe/), 'none in 60 s');
  await w.sys.shutdown();
  const blocks = blocksIn(await exitStats(t), printedRanges(own));
  t.record(`${phase} DLL blocks`, blocks);
  const first = t.state?.blocks ?? blocks;
  if (phase === 'save') t.check(`save: the DLLs' blocks translated (${blocks})`, blocks > 1000, `${blocks}`);
  if (phase === 'restore') t.check(`restore: under 5% of the first run's blocks (${blocks} after ${first})`, blocks < first / 20, `${blocks} after ${first}`);
  const re = /\bxcache .*: (saved|.*restored|nothing saved|not saved)/;
  const logs = [...await t.logs(re), ...w.sys.state.console.split('\n').filter((l) => re.test(l))];
  if (phase === 'save') t.check('save: the cache saved', logs.some((l) => /: saved \d/.test(l)), logs.slice(-4).join(' / ') || 'no xcache line');
  else {
    const n = Math.max(0, ...logs.map((l) => Number(/restored (\d+) modules/.exec(l)?.[1] ?? 0)));
    t.record(`${phase} restored modules`, n);
    t.check(`${phase}: the cache restored`, n > 0, logs.slice(-4).join(' / ') || 'no xcache line');
  }
  if (t.phase < PHASES.length) return t.reload({ blocks: first });
}
