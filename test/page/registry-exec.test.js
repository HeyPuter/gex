// A threaded program that execs makes a new module store for the program it
// execs (`Gex#programStore`), and the registry starts over (`reset_registry`).
// The store used to outlive the exec in its Worker while appends carried on
// after the old program's modules, so the new program held both, and the
// registry it catches a joining thread up on kept the old program's rows.
// Here a threaded guest translates a few thousand functions, execs itself,
// and the new program translates a few hundred and starts a thread, which
// catches up on the new registry from the new store. Two stores must be made,
// one per program, and both programs' joining threads must agree with the
// native run.
import { execFileSync } from 'node:child_process';
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser.x86.m64.shared.wasm');
  const seed = g.pub('seed.x86.m64.shared.wasm');
  const prog = g.cc('jit-registry.c', { imageBase: false, opt: '-O1' });
  const native = execFileSync(prog, ['exec'], { encoding: 'utf8' }).trim();
  // In the guest's tree, so the exec finds it by its argv[0].
  const r = await g.run(seed, prog, ['exec'], { files: { '/t/registry': prog }, argv0: '/t/registry', env: { GEX_SHARED: '1', GEX_MEM64: '1', GEX_REGISTRY_TRACE: '1' }, timeout: 300000 });
  t.eq('the output is the native run\'s', r.guest, native, tail(r.stderr, 4));
  const made = [...r.stderr.matchAll(/module store max \d+ GiB/g)].length;
  t.check(`${made} module stores made, one for each program`, made === 2);
}
