// glibc programs, i386 and x86-64, against the host's glibc (/usr/lib32 and
// /usr/lib), dynamically linked, static and static-pie
// (fixtures/x86-programs/glibc-hello.c): natively and under gexc, output and
// exit status must match. The static musl programs beside this cannot see
// what ld.so and libc check at startup (the CPU's x86 ISA level, IFUNC
// selection by cpuid), which is how a cpuid change that broke every i386
// glibc program got past the gate. A static program applies its own
// IRELATIVE relocations, calling libc's IFUNC resolvers from
// __libc_start_main before main. A variant the host cannot build (no gcc
// -m32, no libc.a) is said and left out. And glibc's own if_nameindex,
// getifaddrs and getaddrinfo(AI_ADDRCONFIG) over the route socket
// (netlink-ifaces.c), 64-bit: what apt's http method resolves with.
import { join } from 'node:path';
import { ROOT } from '../lib/guest.mjs';
import { FIX, x86Programs, differ } from './programs.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

export default async function (t) {
  const x = x86Programs(t);
  const gexc = x.g.need(process.env.GEXC_X86 || process.env.GEXC || join(ROOT, 'target/x86/release/gexc'), 'gexc');
  const gcc = x.g.tool('gcc');
  const legs = ['32', '64', '32-static', '64-static', '32-static-pie', '64-static-pie'].map((v) => {
    const link = v.endsWith('-static') ? ['-static'] : v.endsWith('-static-pie') ? ['-static-pie'] : [];
    return { name: `glibc-${v}`, cc: [`-m${v.split('-')[0]}`, '-O1', ...link], src: 'glibc-hello.c', libs: ['-lm'], args: ['arg'] };
  });
  legs.push({ name: 'glibc-netlink-ifaces', cc: ['-O1'], src: 'netlink-ifaces.c', libs: [], args: [], must: true });
  for (const l of legs) {
    const bin = join(x.dir, `${l.name}-${process.pid}`);
    const b = await x.exec([gcc, ...l.cc, '-o', bin, join(FIX, l.src), ...l.libs]);
    if (b.code !== 0) {
      if (l.must) t.check(`${l.name}: builds`, false, b.err.trim().split('\n').slice(-3).join(' / '));
      else t.say(`${l.name}: left out (gcc ${l.cc.join(' ')} cannot build it here)`);
      continue;
    }
    const n = await x.exec([bin, ...l.args]);
    const g = await x.exec([gexc, 'run', bin, ...l.args]);
    // (gex's stderr with its stdout, as the old check had it: a line of gex's own fails it)
    const gout = `${g.out}${g.err ? `\n${g.err}` : ''}`.replace(/\n+$/, '');
    t.check(`${l.name}: as natively (exit ${n.code})`, n.out === gout && n.code === g.code, `native exit ${n.code}, gex exit ${g.code}; ${differ(n.out, gout)}`);
  }
  x.done();
}
