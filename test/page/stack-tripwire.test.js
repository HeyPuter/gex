// A Worker's shim stack that overran its bottom is said, never silent: with
// each Worker's stack guard broken at its start (GEX_TEST_BREAK_STACK_GUARD,
// as an overrun leaves it), its next syscall or dispatch miss must report the
// overrun, once. wasm has no guard page, so a stack running past its
// megabyte lands on whatever the arena put below it, and only this check
// notices.
import { guest } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 120 };

export default async function (t) {
  const g = guest(t);
  const seed = g.pub('seed.x86.shared.wasm');
  const prog = g.cc({ name: 'hello', code: '#include <stdio.h>\nint main(void) { puts("hello"); return 0; }\n' }, { imageBase: false, opt: '-O1' });
  const r = await g.run(seed, prog, [], { env: { GEX_TEST_BREAK_STACK_GUARD: '1', GEX_SHARED: '1' }, timeout: 60000 });
  const all = `${r.stdout}\n${r.stderr}`;
  const n = all.split('shim stack overran its bottom').length - 1;
  t.eq('it ran', [r.code, r.guest.split('\n').includes('hello')], [0, true]);
  t.eq('the overrun said once', n, 1, /worker \d+.s shim stack overran.*found at [a-z ]*/.exec(all)?.[0] ?? '');
}
