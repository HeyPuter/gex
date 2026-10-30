// 32-bit Windows programs through 64-bit Wine's WoW64 (fixtures/wine/src
// wow32.c and wow64c.c): exceptions, threads, nested callbacks, 32-bit and
// 64-bit processes starting each other. Each case prints "wowNN CASE ok"
// when it passes. Needs a WoW64 Wine install (lib/wine/i386-windows) and a
// gex that runs one; skipped without the former. A phase per case, as in
// wine/cases.
import { bootWine } from '../lib/wine.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 4800 };

// [case, program, its arch, args, programs it starts]
const CASES = [
  ['hello', 'wow32.exe', 'x86', ['hello']],
  ['wow', 'wow32.exe', 'x86', ['wow']],
  ['seh', 'wow32.exe', 'x86', ['seh']],
  ['sehstack', 'wow32.exe', 'x86', ['sehstack']],
  ['faultctx', 'wow32.exe', 'x86', ['faultctx']],
  ['threads', 'wow32.exe', 'x86', ['threads']],
  ['cbnest8', 'wow32.exe', 'x86', ['cbnest', '8']],
  ['cbnest16', 'wow32.exe', 'x86', ['cbnest', '16']],
  ['spawn64', 'wow32.exe', 'x86', ['spawn64', 'C:\\wow64c.exe'], [['wow64c.exe', 'x64']]],
  ['spawn32', 'wow64c.exe', 'x64', ['spawn32', 'C:\\wow32.exe'], [['wow32.exe', 'x86']]],
  ['xmmcb', 'wow64c.exe', 'x64', ['xmmcb']],
];

export default async function (t) {
  const [name, exe, arch, args, also = []] = CASES[t.phase - 1];
  const w = await bootWine(t, { wow64: true });
  const ok = new RegExp(`^wow(32|64c) ${name.replace(/\d+$/, '')}[a-z0-9]* ok`, 'm');
  const r = await t.time(name, async () => {
    for (const [e, a] of also) await w.put(e, a);
    return w.run(exe, args, { arch, timeout: 400000 });
  }).catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  t.check(`${name}: "${ok.source}"`, ok.test(r.stdout), `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  await w.sys.shutdown();
  if (t.phase < CASES.length) return t.reload();
}
