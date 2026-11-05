// Persistent buffer mappings written by the guest (glpersist.c): coherent
// ones published at glFlush, SwapBuffers and fences, non-coherent ones at
// glMemoryBarrier or their explicit flushes, without sending a GPU write back
// over.
// Natively (gexc run --gl on its own Wayland compositor, no output), as a
// WoW64 process of 64-bit Wine.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('glpersist', { flags: ['-Wall'], libs: ['-lgdi32', '-lopengl32'] });
  const r = await w.native(['./glpersist.exe'], { files: { 'glpersist.exe': exe },
    flags: ['--no-cgen', '--gl'], wayland: true });
  for (const l of r.out.split('\n').filter((l) => /^(ok  |FAIL|  GL_)/.test(l))) t.say(l);
  t.check('glpersist ok', /^glpersist ok/m.test(r.out), w.detail(r));
  w.clean();
}
