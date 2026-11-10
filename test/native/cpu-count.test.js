// The guest sees the host's processors, natively and on the page's shim, the
// same way at every layer: sched_getaffinity (musl's
// sysconf(_SC_NPROCESSORS_ONLN)), /proc/stat's cpu lines (Boehm GC),
// /proc/cpuinfo, and /sys/devices/system/cpu (online, which glibc's
// get_nprocs reads, and cpuN/topology, which Wine's
// GetLogicalProcessorInformation reads). Guest threads run in parallel (host
// threads, page Workers), and gex said one processor everywhere, so Wine
// reported NumberOfProcessors 1 and engines ran one job worker. Natively
// GEX_CPUS=3 must give 3; on the page's shim (a seed from `gexc seed`) the
// count must be navigator.hardwareConcurrency, at most 64, in a child
// process too (its own Worker on the page).
import { readFileSync } from 'node:fs';
import { native, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], timeout: 300 };

const line = (out, who = '') => out.split('\n').find((l) => l.startsWith(`${who}affinity=`))?.slice(who.length) ?? '';
const all = (k) => ['affinity', 'sysconf', 'stat', 'cpuinfo', 'sys', 'topology'].map((f) => `${f}=${k}`).join(' ');

export default async function (t) {
  const n = native(t);
  const prog = n.g.cc({ name: 'nc', code: readFileSync(n.src('cpu-count/nc.c'), 'utf8') });
  const r = await n.gexc(['run', prog], { env: { GEX_CPUS: '3' } });
  t.eq('native, GEX_CPUS=3', line(r.stdout), all(3), tail(r.stderr));
  t.eq('native, GEX_CPUS=3, a forked child', line(r.stdout, 'child '), all(3), tail(r.stderr));
  const want = Math.min(64, navigator.hardwareConcurrency || 1);
  const p = await n.g.run(n.g.seed(), prog, [], { env: { GEX_SHARED: '1' } });
  t.eq(`page (${want} processors)`, line(p.stdout), all(want), tail(p.stderr));
  // a forked child: a process of its own, told the count when it attaches
  t.eq(`page, a forked child (${want} processors)`, line(p.stdout, 'child '), all(want), tail(p.stderr));
  // a page that tells every process fewer (GEX_CPUS here, ?cpus= on a page)
  const q = await n.g.run(n.g.seed(), prog, [], { env: { GEX_SHARED: '1', GEX_CPUS: '3' } });
  t.eq('page, cpus=3', line(q.stdout), all(3), tail(q.stderr));
  t.eq('page, cpus=3, a forked child', line(q.stdout, 'child '), all(3), tail(q.stderr));
}
