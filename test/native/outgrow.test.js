// A 64-bit program that outgrows the lowered 32-bit memory gets a 64-bit
// one from its next start, if it ended for it: one whose 3.5 GiB mapping
// found no room and that then aborts (fixtures/native/outgrow) is recorded
// (user.gex.mem64 = 1), its crash notice says it needs 64-bit memory and
// will get it, and its next start maps the 3.5 GiB above 4 GiB. One that
// falls back to a smaller mapping and exits 0 is not recorded and keeps the
// lowered memory. Natively (a run's memory is one flavour: gexc reads the
// record as it starts) and on the page, where the program runs itself
// twice in one session and the second exec goes on in a 64-bit memory.
// On the page, too, one whose mappings leave gex's own memory no room to
// grow is recorded the same way, its end no runtime crash.
import { copyFileSync } from 'node:fs';
import { native, trim, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

const lines = (out) => out.split('\n').filter((l) => l.startsWith('outgrow ')).join('\n');
const SAID = /\/g\/outgrow needs 64-bit memory \(a reservation of 0xe0000000 bytes did not fit\); it gets it from its next start/;
const RUNTIME_SAID = /\/g\/outgrow needs 64-bit memory \(gex's own memory for it could not grow by 0x[0-9a-f]+ bytes\); it gets it from its next start/;
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
  // gex's own memory running out in the lowered memory (the shim's heap,
  // which shares it with the guest's mappings): no runtime crash, the
  // same record and line, and the next exec has a 64-bit memory
  const pr = await g.run(n.seed(), `${d}/outgrow`, ['twice', 'runtime'], { files: { '/g/outgrow': `${d}/outgrow` }, argv0: '/g/outgrow', env: { GEX_SHARED: '1' }, timeout: 300000 });
  t.eq('page: gex\'s own memory running out records it, and the next exec has a 64-bit memory', lines(pr.stdout),
    'outgrow runtime full\noutgrow child signal 9, record 1\noutgrow big high\noutgrow child exit 0, record 1', tail(pr.stderr));
  t.check('page: gex says it needs 64-bit memory for its own', RUNTIME_SAID.test(pr.stdout + pr.stderr), tail(pr.stderr));
  t.check('page: and says nothing of a runtime crash', !/trapped inside the shim|worker threw|threw:|crashed;/.test(pr.stdout + pr.stderr), tail(pr.stderr, 40));
  n.clean();
}
