// An exception handler that never returns, jumping back to main
// (fixtures/wow64/tebframes.c), as a runtime that resumes a catch itself
// does (.NET's, after an RtlUnwindEx with no resume address): the frames
// Wine's native dispatcher registered while calling it go with its native
// frames, so a second exception, raised once that stack is written over,
// is handled, where it called a handler from the old frame's bytes (a
// table index 0: "indirect call type mismatch"); and the same from a fiber
// and the thread between switches, which end an entry on another stack
// and leave the list as it is. 64-bit, natively and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

const WANT = 'tebframes jumped\ntebframes continued\ntebframes fibers 3';
const lines = (out) => out.replace(/\r/g, '').split('\n').filter((l) => l.startsWith('tebframes ')).join('\n');

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('tebframes', { x64: true });
  const n = await w.native(['C:\\tebframes.exe'], { files: { 'tebframes.exe': exe }, name: 'native' });
  t.eq('native: the second exception is handled', lines(n.out), WANT, w.detail(n));
  const p = await w.page(['/t/tebframes.exe'], { files: { '/t/tebframes.exe': exe }, name: 'page' });
  t.eq('page: the second exception is handled', lines(p.out), WANT, w.detail(p));
  w.clean();
}
