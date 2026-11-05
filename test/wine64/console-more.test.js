// More of 64-bit Wine's console programs under gexc, with the door
// (fixtures/wine/src): mutexes, fibers, MSVC's x64 C++ exceptions (msvcrt's
// and msvcp140's, built with clang-cl), a locked write through a null
// pointer's field (an access violation for the program's handler),
// KUSER_SHARED_DATA read by constant and through a computed pointer (the
// page is there natively), a window proc's uncaught exception (the null
// driver), the performance counter across the PE and Unix sides, and
// win32u's session reads against another thread's writes.
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 3600 };

const allOk = /(ok [^\n]*\n)+0 failures/;
const CASES = [
  { name: 'mtx', want: allOk, exe: 'mtx.exe' },
  { name: 'fiber', want: allOk, exe: 'fiber.exe' },
  { name: 'msvc_cxxeh', want: allOk, exe: 'msvc_cxxeh.exe' },
  { name: 'msvc_msvcp', want: allOk, exe: 'msvc_msvcp.exe' },
  { name: 'lownull-xadd', want: allOk, exe: 'lownull.exe', args: ['xadd'] },
  { name: 'lownull-aligned', want: allOk, exe: 'lownull.exe', args: ['aligned'] },
  { name: 'lownull-cmpxchg16b', want: allOk, exe: 'lownull.exe', args: ['cmpxchg16b'] },
  { name: 'kuser', want: allOk, exe: 'kuser.exe', args: ['const'] },
  { name: 'kuser-pointer', code: 3, want: /read [0-9]+ at 0x7ffe026c/, exe: 'kuser.exe', args: ['pointer'] },
  { name: 'wndproc_uncaught', want: allOk, exe: 'wndproc_uncaught.exe', nullDriver: true },
  { name: 'qpcmono', want: allOk, exe: 'qpcmono.exe' },
  // 20000 writes a case with the door; every read is a server request without it
  { name: 'seqrace', want: allOk, exe: 'seqrace.exe', args: ['20000', '7'], timeout: 900000 },
];

export default async function (t) {
  const w = wine64(t);
  for (const c of CASES) await w.nt(c);
  w.clean();
}
