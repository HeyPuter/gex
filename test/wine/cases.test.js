// 64-bit Wine's console cases (the test programs in fixtures/wine/src): each
// must exit with its code and print exactly what it should. Most check
// themselves and print a line per check, then "0 failures". Each in a System
// of its own, a phase each (t.reload): a wineserver that exited leaves its
// socket's name bound (page/unix-rebind), so the next program's server could
// not start in the same system.
import { bootWine } from '../lib/wine.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1800 };

const allOk = /^(ok [^\n]*\n)+0 failures\n?$/;
const CASES = [
  ['hello_args', 42, /^hello from a Windows program with 3 args\narg 1: a\narg 2: b c\n$/, 'hello_args.exe', ['a', 'b c']],
  ['sync', 0, allOk, 'sync.exe'],
  ['mtx', 0, allOk, 'mtx.exe'],
  ['crtmem', 0, allOk, 'crtmem.exe'],
  ['apc', 0, allOk, 'apc.exe'],
  ['tls', 0, allOk, 'tls.exe'],
  ['cxx', 0, allOk, 'cxx.exe'],
  ['exitproc', 33, /^worker exiting\natexit ran\n?$/, 'exitproc.exe'],
  ['seh', 77, /^(ok [^\n]*\n)+top-level filter saw e00000ff\n0 failures\n?$/, 'seh.exe'],
  ['seh-traps', 0, allOk, 'seh.exe', ['traps']],
];

export default async function (t) {
  const [name, code, out, exe, args = []] = CASES[t.phase - 1];
  const w = await bootWine(t);
  const r = await t.time(name, () => w.run(exe, args)).catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  t.check(`${name}: exit ${code}, its output`, r.code === code && out.test(r.stdout), `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  await w.sys.shutdown();
  if (t.phase < CASES.length) return t.reload();
}
