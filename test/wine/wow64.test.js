// 32-bit Windows programs through 64-bit Wine's WoW64 (fixtures/wine/src
// wow32.c and wow64c.c): exceptions, threads, nested callbacks, 32-bit and
// 64-bit processes starting each other. Each case prints "wowNN CASE ok"
// when it passes. Needs a WoW64 Wine install (lib/wine/i386-windows) and a
// gex that runs one; skipped without the former. A phase per case, as in
// wine/cases.
import { bootWine } from '../lib/wine.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 4800 };

// x87conv.c's digests on the hardware (every conversion under all 16
// precision and rounding modes, fbld and fbstp among them)
const X87_DIGESTS = [
  ['fstp m64', '15be51c3c46743ca'],
  ['fstp m32', '01f2d10bff4f18a5'],
  ['fstp m80', '8ac5f863c2bb09d0'],
  ['fistp m16', '7e283010ef5ed5df'],
  ['fistp m32', '1726425ce24446fa'],
  ['fistp m64', '84e0dd0425f356d1'],
  ['fisttp m16', 'bcb357664a27c82d'],
  ['fisttp m32', '5742d4b8580fa5b3'],
  ['fisttp m64', 'ce799954d1a9e474'],
  ['fld m64', 'a34344f0530754d8'],
  ['fld m32', 'b32e7a6fbff8b743'],
  ['fild m16', 'e89afd776a2570bc'],
  ['fild m32', 'c2236a313bb837e8'],
  ['fild m64', '5159b85fbb56b35a'],
  ['fld const', '058675d71874449d'],
  ['stack faults', 'cba6f7d43db5b725'],
  ['fbstp', '19b49b113215e0f0'],
  ['fild fbstp', '628955cf60e8c1b0'],
  ['fbld', 'ffa1cf42be60b3b1'],
];
const X87_WANT = new RegExp(X87_DIGESTS.map(([n, d]) => `^${n.replace(/ /g, ' +')} +\\d+ cases +${d}\\r?$`).join('\\n'), 'm');

// [case, program, its arch, args, programs it starts, what it prints when it
// passes (default "wowNN CASE ok"), the null driver (default yes)]
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
  // the processor architecture each side reports: AMD64 to 64-bit code,
  // INTEL (system) and AMD64 (native) to 32-bit code
  ['sysarch64', 'wow64c.exe', 'x64', ['sysarch']],
  ['sysarch32', 'wow32.exe', 'x86', ['sysarch']],
  ['bcd', 'wow32.exe', 'x86', ['bcd']],
  ['maskmov32', 'wow32.exe', 'x86', ['maskmov']],
  ['maskmov64', 'wow64c.exe', 'x64', ['maskmov']],
  // a prefix update's fake explorer.exe, and a console app in a new console
  ['fakedll', 'wow64c.exe', 'x64', ['fakedll']],
  ['newconsole', 'wow64c.exe', 'x64', ['newconsole']],
  // the C runtime's RtlGenRandom: advapi32 forwards it to cryptbase, not yet loaded
  ['genrandom', 'wow64c.exe', 'x64', ['genrandom']],
  ['x87conv32', 'x87conv.exe', 'x86', [], [], X87_WANT],
  ['x87conv64', 'x87conv.exe', 'x64', [], [], X87_WANT],
  // a timer procedure of 32-bit Wine's own (comctl32's list view)
  ['timerproc', 'timerproc.exe', 'x86', [], [], /^(ok [^\n]*\n)+0 failures\n?$/],
  // 32-bit OpenGL and Direct3D with no context ever current on the thread,
  // with Wine's usual driver (as wine/cases has it)
  ['wglnoctx', 'wglnoctx.exe', 'x86', [], [], /^(ok [^\n]*\n)+0 failures\n?$/, false],
];

export default async function (t) {
  const [name, exe, arch, args, also = [], want, nullDriver = true] = CASES[t.phase - 1];
  const w = await bootWine(t, { wow64: true, nullDriver });
  const ok = want ?? new RegExp(`^wow(32|64c) ${name.replace(/\d+$/, '')}[a-z0-9]* ok`, 'm');
  const r = await t.time(name, async () => {
    for (const [e, a] of also) await w.put(e, a);
    return w.run(exe, args, { arch, timeout: 400000 });
  }).catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  t.check(`${name}: "${ok.source}"`, ok.test(r.stdout), `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  await w.sys.shutdown();
  if (t.phase < CASES.length) return t.reload();
}
