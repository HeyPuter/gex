// A 64-bit program that reserves all of the free space between 2 and 4
// GiB, as a runtime's heap may, and then loads a DLL of Wine's
// (fixtures/wow64/highfill.c): natively, the DLL's data goes in the window
// reserved for it at the process's start, and the program goes on. It
// went in the first free range above 2 GiB, found none, and the process
// ended ("mapping [wine PE data]: no 0x10000 bytes free above
// 0x80000000"). The page's 64-bit Wine has its window below 2 GiB
// already, where this program's reservations do not reach.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'highfill', { want: /^highfill reserved [1-9]\d* MiB above 2 GiB, \d+ MiB left[^]*^highfill loaded cabinet\.dll/m, build: { x64: true }, page: false });
