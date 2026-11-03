// 64-bit Wine's console cases (the test programs in fixtures/wine/src): each
// must exit with its code and print exactly what it should. Most check
// themselves and print a line per check, then "0 failures". One System for
// the cases with Wine's usual driver and one (a second phase, t.reload) for
// those with the null driver, which is the prefix's setting: each case's
// wineserver may exit before the next starts one, which binds the socket's
// name again (page/unix-rebind).
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
  ['fiber', 0, allOk, 'fiber.exe'],
  // Built with clang-cl for the MSVC ABI.
  ['msvc_cxxeh', 0, allOk, 'msvc_cxxeh.exe'],
  // USER with no display (Wine's null driver).
  ['user_win', 0, allOk, 'user_win.exe', [], true],
  ['wndproc_seh', 0, allOk, 'wndproc_seh.exe', [], true],
  ['wndproc_uncaught', 0, allOk, 'wndproc_uncaught.exe', [], true],
  ['nestsp', 0, allOk, 'nestsp.exe', [], true],
  // A timer procedure of Wine's own (comctl32's list view), which
  // DispatchMessage calls.
  ['timerproc', 0, allOk, 'timerproc.exe', [], true],
  // OpenGL and Direct3D with no context ever current on the thread, with
  // Wine's usual driver: the null driver's windows draw GL into framebuffer
  // objects (win32u), which take GL 4.5's glCreateFramebuffers.
  ['wglnoctx', 0, allOk, 'wglnoctx.exe'],
  ['qpcmono', 0, allOk, 'qpcmono.exe'],
  // 2000 writes a case: on the page every read is a server request.
  // Fewer writes in the single-thread build (a page without
  // SharedArrayBuffer): one thread serializes every server round trip, so
  // the race is exercised less there anyway, and the full count outlasts
  // the run's timeout.
  ['seqrace', 0, allOk, 'seqrace.exe', typeof SharedArrayBuffer === 'function' ? ['2000', '7'] : ['500', '7']],
  ['authattrs', 0, allOk, 'authattrs.exe'],
  ['rpccom', 0, allOk, 'rpccom.exe'],
  ['dwrite', 0, allOk, 'dwrite.exe'],
];

export default async function (t) {
  const nullDriver = t.phase === 2;
  const w = await bootWine(t, { nullDriver });
  for (const [name, code, out, exe, args = [], wantNull = false] of CASES) {
    if (wantNull !== nullDriver) continue;
    const r = await t.time(name, () => w.run(exe, args)).catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
    t.check(`${name}: exit ${code}, its output`, r.code === code && out.test(r.stdout), `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  }
  await w.sys.shutdown();
  if (!nullDriver) return t.reload();
}
