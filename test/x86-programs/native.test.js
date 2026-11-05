// The x86 programs (programs.mjs) natively: each target and tier under gexc
// against the host's own run of the same build; threads-many once more
// under a task cap of 160.
import { join } from 'node:path';
import { ROOT } from '../lib/guest.mjs';
import { pool } from '../lib/gexcc.mjs';
import { PROGRAMS, plan, env, x86Programs, differ } from './programs.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 3600 };

const FLAGS = { t1: [], fjit: ['--fjit'], cgen: ['--cgen'] };

export default async function (t) {
  const x = x86Programs(t);
  const gexc = x.g.need(process.env.GEXC_X86 || process.env.GEXC || join(ROOT, 'target/x86/release/gexc'), 'gexc');
  const legs = [];
  for (const name of PROGRAMS) {
    const p = plan(name);
    for (const target of p.targets) for (const tier of p.tiers) legs.push({ name, target, tier, mem64: p.mem64 });
  }
  await pool(legs, async ({ name, target, tier, mem64 }) => {
    const leg = `${name} (${target}, ${tier})`;
    let bin;
    try {
      bin = await x.build(name, target);
    } catch (e) {
      return t.check(`${leg}: builds`, false, e.message);
    }
    const args = await x.args(name);
    const n = await x.exec([bin, ...args]);
    const flags = [...(mem64 ? ['--mem64'] : []), ...FLAGS[tier]];
    const g = await x.exec([gexc, 'run', ...flags, bin, ...args], { env: env(name, false) });
    t.check(`${leg}: as natively (exit ${n.code})`, n.out === g.out && n.code === g.code, `native exit ${n.code}, gex exit ${g.code}; ${differ(n.out, g.out)}`);
    if (name === 'threads-many' && tier === 't1') {
      // the host refusing a thread is EAGAIN to the guest, natively and on gex
      const capped = (argv) => x.exec(['systemd-run', '--user', '--scope', '-q', '-p', 'TasksMax=160', ...argv]);
      const nc = await capped([bin, 'capped']);
      const gc = await capped([gexc, 'run', ...flags, bin, 'capped']);
      t.check(`${leg} capped: as natively (exit ${nc.code})`, nc.out === gc.out && nc.code === gc.code, `native exit ${nc.code}, gex exit ${gc.code}; ${differ(nc.out, gc.out)}`);
    }
  });
  x.done();
}
