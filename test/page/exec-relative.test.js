// A program exec'd by a relative path (`./prog`) is named absolutely by
// `/proc/self/exe`, as Linux records it. ld.so works out `$ORIGIN` from that
// link and asserts that it starts with `/`: a launcher that execs ./prog
// from its own directory stopped at `_dl_get_origin: Assertion
// `linkval[0] == '/'' failed`. Both ways in: a fork and execv, and
// posix_spawn (the vfork path).
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 180 };

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser{infix}.shared.wasm');
  g.pub('gex_kernel{infix}.wasm');
  const prog = g.cc('exec-relative.c');
  const r = await g.run(g.seed(), prog, ['/g', 'er'], { files: { '/g/er': prog }, env: { GEX_SHARED: '1' } });
  t.eq('/proc/self/exe after ./prog, by execv and posix_spawn', r.guest, 'execv: absolute\nposix_spawn: absolute', tail(r.stderr, 3));
}
