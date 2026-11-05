// A vfork child that calls mmap, on the page's shim: it must end as a crash,
// its parent must go on, and the warning must name the call
// (hosts/browser/src/vfork.rs `check`). A wasm32 program (gexcc), on this
// arch's shim.
import { native } from '../lib/native.mjs';
import { gexcc } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 300 };

export default async function (t) {
  const n = native(t);
  const c = gexcc(t);
  const seed = n.seed();
  const prog = `${c.dir('vfork-mmap')}/vfork-mmap`;
  await c.build(['-O2', '-o', prog, n.src('vfork-mmap/main.c')]);
  const r = await n.g.run(seed, prog, [], { files: { '/t/vfork-mmap': prog }, flags: ['--cwd', '/t'], argv0: './vfork-mmap', merge: true, env: { GEX_SHARED: '1' } });
  const out = r.all;
  const detail = out.split('\n').filter((l) => !/^\[host\]|^\[worker\] /.test(l)).slice(-10).join(' / ');
  // A crashed process's status is 137 (the shell's 128 + SIGKILL) here, as
  // for any process that crashes in the browser runtime.
  t.check('the child was ended', /^(child killed by a signal|child exited 137)/m.test(out), detail);
  t.check('the parent went on', /^parent goes on/m.test(out), detail);
  t.check('the warning names mmap', /a vfork child called mmap/.test(out), detail);
  c.clean();
}
