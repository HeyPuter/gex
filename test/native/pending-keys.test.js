// The keys the shim gives a translated module as it queues it
// (`gex_jit_pending_key`) are the ones gex.mjs hashed from the module's bytes
// before: its content key, and the page's unit cache key with the dense base
// word zeroed. GEX_CHECK_INSTALL hashes the bytes as well and says where the
// two differ. Runs after-trap's program (threads, signals, a fork) on the
// page's shim, in the SAB build and the single-thread one: it must pass,
// with every check matching and one of a module with a dense section.
import { native, count, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 900 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('pending-keys');
  await n.cc(['-O2', '-fPIC', '-shared', '-Wl,-soname,libcb.so', '-o', `${d}/libcb.so`, n.src('after-trap/libcb.c')]);
  await n.cc(['-O1', '-o', `${d}/main`, n.src('after-trap/main.c'), `-L${d}`, '-lcb', `-Wl,-rpath,${d}`, '-lpthread']);
  for (const single of [false, true]) {
    const m = single ? 'SINGLE' : 'SHARED';
    const r = await n.g.run(seed, `${d}/main`, [400], {
      files: { ...n.ldso, [`${d}/libcb.so`]: `${d}/libcb.so`, [`${d}/main`]: `${d}/main` }, cwd: d, timeout: 300000, single,
      env: { GEX_CHECK_INSTALL: '1', ...(single ? {} : { GEX_SHARED: '1' }) },
    });
    const oks = count(r.stdout, / ok$/);
    const bad = count(r.stderr, /\[install-check\] pid [0-9:]*: pending keys/);
    const good = count(r.stderr, /\[install-check\] pid [0-9:]*: the pending keys match the bytes'/);
    const dense = count(r.stderr, /the pending keys match the bytes' \(dense base word zeroed\)/);
    t.check(`${m}: exits 0 with ${oks} of 5 checks ok`, r.code === 0 && oks === 5, `exit ${r.code}: ${tail(r.stdout)} / ${tail(r.stderr)}`);
    t.check(`${m}: keys checked in ${good} Workers, ${dense} with a dense section, ${bad} mismatches`, bad === 0 && good >= 1 && dense >= 1,
      r.stderr.split('\n').filter((l) => /\[install-check\]/.test(l)).slice(0, 5).join(' / '));
  }
  n.clean();
}
