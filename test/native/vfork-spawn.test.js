// A vfork child may spawn (posix_spawn, CLONE_VM | CLONE_VFORK), natively and
// on the page's shim: Qt's startDetached does, and KDE starts its services
// so. With gexcc (GEXCC), a wasm32 program's page leg too.
import { native, trim, tail } from '../lib/native.mjs';
import { gexcc } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 600 };

// exit 102: posix_spawn's ENOENT (100 + 2); 2: ENOENT from the memory the
// grandchild shared
const want = [
  'spawn: exited 0', 'grandchild ran', 'spawn missing: exited 102',
  'fork: exited 0', 'grandchild ran',
  'shared: exited 0', 'grandchild ran', 'shared missing: exited 2'];

export default async function (t) {
  const n = native(t);
  const c = gexcc(t);
  const seed = n.seed();
  const d = n.dir('vfork-spawn');
  await n.cc(['-O1', '-static', '-o', `${d}/spawn`, n.src('vfork-spawn/spawn.c')]);
  // Its own root: the grandchild execs /proc/self/exe, a guest path. The
  // pipe cases alone: `gexc run` copies a CLONE_VM | CLONE_VFORK child as a
  // fork, so what the grandchild writes before it exits never reaches its
  // parent (and the parent then waits on the pipe for a grandchild that
  // never ran).
  const r = await n.gexc(['run', '--sysroot', d, `${d}/spawn`, 'pipe'], { timeout: 120000 });
  t.eq('native', trim(r.stdout), want.slice(0, 5).join('\n'), tail(r.stderr));
  const p = await n.g.run(seed, `${d}/spawn`, [], { files: { [`${d}/spawn`]: `${d}/spawn` }, cwd: d, env: { GEX_SHARED: '1' } });
  t.eq('page', p.guest, want.join('\n'), tail(p.stderr));
  // No fork: a wasm32 program's fork maps memory, which a vfork child may not.
  const s32 = `${c.dir('vfork-spawn')}/spawn32`;
  await c.build(['-O1', '-o', s32, n.src('vfork-spawn/spawn.c')]);
  const w = await n.g.run(seed, s32, ['nofork'], { files: { [s32]: s32 }, cwd: d, env: { GEX_SHARED: '1' } });
  t.eq('page, wasm32', w.guest, want.filter((l, i) => i !== 3 && i !== 4).join('\n'), tail(w.stderr));
  c.clean();
  n.clean();
}
