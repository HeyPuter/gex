// 64-bit Wine's console programs (fixtures/wine/src, the win-progs image)
// under gexc: each must exit with its code and print exactly what it
// should, most a line per check and "0 failures". Each with the door
// (WINEGEX_DOOR=1, the LP64 door call, as the page runs Wine) and without
// it. highdata: data above 2 GiB, code below it, and Wine's log line for
// the protection it refused.
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 3600 };

const allOk = /(ok [^\n]*\n)+0 failures/;
const hello = /hello from a Windows program with 3 args\narg 1: a\narg 2: b c/;
const CASES = [
  { name: 'hello_args', code: 42, want: hello, exe: 'hello_args.exe', args: ['a', 'b c'] },
  { name: 'sync', want: allOk, exe: 'sync.exe' },
  { name: 'crtmem', want: allOk, exe: 'crtmem.exe' },
  { name: 'apc', want: allOk, exe: 'apc.exe' },
  { name: 'tls', want: allOk, exe: 'tls.exe' },
  { name: 'cxx', want: allOk, exe: 'cxx.exe' },
  { name: 'exitproc', code: 33, want: /worker exiting\natexit ran/, exe: 'exitproc.exe' },
  { name: 'seh', code: 77, want: /(ok [^\n]*\n)+top-level filter saw e00000ff\n0 failures/, exe: 'seh.exe' },
  { name: 'seh-traps', want: allOk, exe: 'seh.exe', args: ['traps'] },
  { name: 'highdata', want: allOk, exe: 'highdata.exe' },
];

export default async function (t) {
  for (const door of [1, 0]) {
    const w = wine64(t, { door });
    for (const c of CASES) {
      const r = await w.nt({ ...c, name: `${c.name} door=${door}` });
      if (c.name === 'highdata' && r)
        t.check(`highdata door=${door}: Wine says why it refused the protection`, /made executable above gex's code space/.test(r.err), w.detail(r, 3));
    }
    w.clean();
  }
}
