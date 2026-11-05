// A trap that leaves the page nothing to do goes straight back into the guest
// on one shim call (gex.mjs `#afterTrap`, the shim's `gex_after_trap`), and
// every other trap still goes through each step. main.c makes traps of the
// common kinds, each checked by its result: signals raised inside call-outs
// (a function-tier unit, GEX_ELF_UNITS=libcb.so, calling tier-1 code) and
// outside them, a condition variable between two threads, sleeps, and a
// fork. On the page's shim, in the SAB build and the single-thread one, with
// GEX_LOG=traps, the program must pass, its run loop must have gone straight
// on for some traps and through each step for others (its start's
// installs), and its call-outs straight on for some.
import { native, count, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 900 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('after-trap');
  await n.cc(['-O2', '-fPIC', '-shared', '-Wl,-soname,libcb.so', '-o', `${d}/libcb.so`, n.src('after-trap/libcb.c')]);
  await n.cc(['-O1', '-o', `${d}/main`, n.src('after-trap/main.c'), `-L${d}`, '-lcb', `-Wl,-rpath,${d}`, '-lpthread']);
  const cache = `${d}/cgen`;
  const run = (single, log, count) => n.g.run(seed, `${d}/main`, [count], {
    files: { ...n.ldso, [`${d}/libcb.so`]: `${d}/libcb.so`, [`${d}/main`]: `${d}/main` },
    flags: ['--cgen-prefer'], cwd: d, timeout: 300000, single,
    env: { GEX_CGEN_CACHE: cache, GEX_ELF_UNITS: 'libcb.so', GEX_LOG: log, ...(single ? {} : { GEX_SHARED: '1' }) },
  });
  // A first run finds where the loader maps libcb.so; its unit is built for that base.
  await n.units({ 'libcb.so': `${d}/libcb.so` }, cache, async () => (await run(false, '1', 2)).stderr);
  // Each raised signal is a trap: in the run loop from main, in the call-out's
  // loop from cb. Both builds wait inside their blocking calls (Atomics.wait,
  // or a JSPI suspension), so few other traps reach either loop.
  for (const single of [false, true]) {
    const m = single ? 'SINGLE' : 'SHARED';
    const r = await run(single, 'traps,unit', 400);
    const placed = count(r.stderr, /function tier for .*libcb\.so/);
    const oks = count(r.stdout, / ok$/);
    // The first process's own Worker (a thread's is `pid 1:2`, the fork's child `pid 2`).
    const line = r.stderr.split('\n').find((l) => /\] pid 1: traps straight on\/through each step/.test(l)) ?? '';
    const [lq, lf, cq, cf] = (/run loop (\d+)\/(\d+), call-outs (\d+)\/(\d+)/.exec(line)?.slice(1) ?? []).map(Number);
    t.check(`${m}: exits 0, the unit placed (${placed}), ${oks} of 5 checks ok`, r.code === 0 && placed >= 1 && oks === 5,
      `exit ${r.code}: ${tail(r.stdout)} / ${r.stderr.split('\n').filter((l) => /trap|rror/.test(l) && !/traps straight/.test(l)).slice(0, 5).join(' / ')}`);
    t.check(`${m}: run loop ${lq ?? '?'} straight on, ${lf ?? '?'} through each step; call-outs ${cq ?? '?'} straight on, ${cf ?? '?'} through each step`,
      lq > 0 && lf > 0 && cq > 0, line || 'no traps line');
  }
  n.clean();
}
