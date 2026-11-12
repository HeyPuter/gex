// Guest i386 code in a WoW64 process calling Wine's 32-bit side through COM
// and C++ vtables (stdcall methods with 8-byte structs by value, msvcrt's
// thiscall exception methods) and exports of each i386 convention (cdecl,
// stdcall, fastcall, an 8-byte struct result): fixtures/wow64/comvt.c,
// natively and on the page. wine64/guestnative runs it as x86-64.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  await legs(t, 'comvt', { want: /^(ok [^\n]*\n)+0 failures/m, build: { libs: ['-lole32', '-luuid'] } });
}
