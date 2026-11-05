// The function JIT's recovery reads the slot of a `call [addr]` whose address
// is a constant, to bring the callee into the batch. The shim's view of the
// guest spans the whole address space while the memory is smaller: a slot
// past the memory's end (Wine's system call stubs' `call [0x7ffe1000]` in a
// process whose memory stops below the shared user data) was read anyway,
// out of bounds, and the trap in the shim ended the thread as a page fault at
// its last PC. fixtures/programs/fjit-slot-oob.c holds such a call on a path
// it never takes, on the page's shim with the function JIT: it must run to
// its end, and the JIT must have made a module.
import { readFileSync } from 'node:fs';
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 300 };

export default async function (t) {
  const g = guest(t);
  const seed = g.pub('seed.x86.shared.wasm');
  g.pub('gex_browser.x86.shared.wasm');
  const prog = g.cc({ name: 'fjit-slot-oob', code: readFileSync(new URL('../fixtures/programs/fjit-slot-oob.c', import.meta.url), 'utf8') }, { opt: '-O1', imageBase: false });
  const r = await g.run(seed, prog, [], { files: { '/g/oob': prog }, argv0: '/g/oob', env: { GEX_SHARED: '1', GEX_FJIT: '1' }, timeout: 180000 });
  const got = r.stdout.split('\n').find((l) => /^(ok|FAIL)/.test(l)) ?? '';
  t.check('runs to its end', got.startsWith('ok:'), `${got || 'no result'}; ${tail(r.stderr, 6)}`);
  t.check('the function JIT made a module', /\] fjit: [1-9]/.test(r.stderr), tail(r.stderr, 6));
}
