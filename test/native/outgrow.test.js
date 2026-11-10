// A 64-bit program that outgrows the lowered 32-bit memory gets a 64-bit
// one from its next start, if it ended for it: one whose 3.5 GiB mapping
// found no room and that then aborts (fixtures/native/outgrow) is recorded
// (user.gex.mem64 = 1), its crash notice says it needs 64-bit memory and
// will get it, and its next start maps the 3.5 GiB above 4 GiB. One that
// falls back to a smaller mapping and exits 0 is not recorded and keeps the
// lowered memory. Natively (a run's memory is one flavour: gexc reads the
// record as it starts) and on the page, where the program runs itself
// twice in one session and the second exec goes on in a 64-bit memory.
import { copyFileSync } from 'node:fs';
import { native, trim, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

const lines = (out) => out.split('\n').filter((l) => l.startsWith('outgrow ')).join('\n');
const SAID = /\/g\/outgrow needs 64-bit memory \(a reservation of 0xe0000000 bytes did not fit\); it gets it from its next start/;
const NOTICE = /crashed: SIGABRT\b[^\n]*; needs 64-bit memory; it will get it the next time it starts/;

export default async function (t) {
  const n = native(t);
  const d = n.dir('outgrow');
  await n.cc(['-O1', '-static', '-o', `${d}/outgrow`, n.src('outgrow/outgrow.c')]);
  copyFileSync(`${d}/outgrow`, `${d}/fallback`);

  // natively: the crash records it, the notice says so, the next run has 64 bits
  const c = await n.gexc(['run', `${d}/outgrow`, 'twice', 'crash'], { timeout: 180000 });
  t.eq('native: the crash after falling short records it', trim(c.stdout), 'outgrow short\noutgrow child signal 6, record 1\noutgrow short\noutgrow child signal 6, record 1', tail(c.stderr));
  t.check('native: its crash notice says it needs 64-bit memory', NOTICE.test(c.stderr), tail(c.stderr));
  const c2 = await n.gexc(['run', `${d}/outgrow`, 'crash'], { timeout: 180000 });
  t.eq('native: its next start has a 64-bit memory', trim(c2.stdout), 'outgrow big high', tail(c2.stderr));
  const f = await n.gexc(['run', `${d}/fallback`, 'twice', 'fallback'], { timeout: 180000 });
  t.eq('native: one that falls back and exits 0 is not recorded', trim(f.stdout), 'outgrow fallback ok\noutgrow child exit 0, record none\noutgrow fallback ok\noutgrow child exit 0, record none', tail(f.stderr));
  t.check('native: and is no crash', !/crashed/.test(f.stderr), tail(f.stderr));

  // on the page: the second exec, in the same session, has 64 bits
  const g = n.g;
  for (const p of ['gex_browser.x86.shared.wasm', 'gex_kernel.x86.wasm', 'gex_browser.x86.m64.shared.wasm', 'seed.x86.m64.shared.wasm']) g.pub(p);
  const pc = await g.run(n.seed(), `${d}/outgrow`, ['twice', 'crash'], { files: { '/g/outgrow': `${d}/outgrow` }, argv0: '/g/outgrow', env: { GEX_SHARED: '1' }, timeout: 300000 });
  t.eq('page: the crash records it, and the next exec has a 64-bit memory', lines(pc.stdout), 'outgrow short\noutgrow child signal 6, record 1\noutgrow big high\noutgrow child exit 0, record 1', tail(pc.stderr));
  // (this runner shows no crash notices: the line gex says as the record
  // is made; the notice's reason is the kernel's, as natively)
  t.check('page: gex says it needs 64-bit memory', SAID.test(pc.stdout + pc.stderr), tail(pc.stderr));
  const pf = await g.run(n.seed(), `${d}/outgrow`, ['twice', 'fallback'], { files: { '/g/outgrow': `${d}/outgrow` }, argv0: '/g/outgrow', env: { GEX_SHARED: '1' }, timeout: 300000 });
  t.eq('page: one that falls back and exits 0 is not recorded', lines(pf.stdout), 'outgrow fallback ok\noutgrow child exit 0, record none\noutgrow fallback ok\noutgrow child exit 0, record none', tail(pf.stderr));
  n.clean();
}
