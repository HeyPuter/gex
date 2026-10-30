// A unix socket's name, once unlinked, can be bound again: after the first
// socket closed, and while it is still open (fixtures/programs/unix-rebind.c).
// wineserver unlinks a stale socket and binds its own.
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 300 };

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser{infix}.shared.wasm');
  g.pub('gex_kernel{infix}.wasm');
  const r = await g.run(g.seed(), g.cc('unix-rebind.c'), [], { env: { GEX_SHARED: '1' }, timeout: 240000 });
  const lines = r.guest.split('\n');
  t.check('closed, unlinked, bound again', lines.includes('closed, unlinked, bound again: yes'), `${tail(r.guest, 6)} (${tail(r.stderr, 3)})`);
  t.check('open, unlinked, bound again', lines.includes('open, unlinked, bound again: yes'), tail(r.guest, 6));
}
