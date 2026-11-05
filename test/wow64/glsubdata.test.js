// glBufferSubData of the bytes a buffer already holds (glsubdata.c): the
// page's Mesa skips such a repeat, so a CPU map write, a GPU copy or another
// subdata in between must make the next identical write land again.
// Natively (gexc run --gl on its own Wayland compositor, no output), as a
// WoW64 process of 64-bit Wine.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('glsubdata', { flags: ['-Wall'], libs: ['-lgdi32', '-lopengl32'] });
  const r = await w.native(['./glsubdata.exe'], { files: { 'glsubdata.exe': exe },
    flags: ['--no-cgen', '--gl'], wayland: true });
  for (const l of r.out.split('\n').filter((l) => /^(ok  |FAIL|  GL_)/.test(l))) t.say(l);
  t.check('glsubdata ok', /^glsubdata ok/m.test(r.out), w.detail(r));
  w.clean();
}
