// Code a program writes through one view of a section and runs through
// another, read+execute, view (.NET's W^X), dualmap.c: every write is what
// runs next. The read+execute view was a private copy (gex copies a
// private mapping), which never saw the writes, and translations of its
// pages went stale with nothing to say the code changed. x86_64 and i386,
// natively and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^dualmap ok 300$/m;
  for (const x64 of [true, false]) {
    const name = x64 ? 'dualmap64.exe' : 'dualmap32.exe';
    const exe = await w.mingw('dualmap', { out: name, x64 });
    for (const args of [[], ['rwfirst']]) {
      const what = `${x64 ? 'x86_64' : 'i386'}${args.length ? ` ${args[0]}` : ''}`;
      const n = await w.native([`C:\\${name}`, ...args], { files: { [name]: exe } });
      t.check(`native ${what}: ${want.source}`, want.test(n.out), w.detail(n));
      const p = await w.page([`/t/${name}`, ...args], { files: { [`/t/${name}`]: exe } });
      t.check(`page ${what}: ${want.source}`, want.test(p.out), w.detail(p));
    }
  }
  w.clean();
}
