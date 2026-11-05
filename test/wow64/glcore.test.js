// A core-profile GL program (glcore.c), shaped like wined3d: a 3.2 core
// context, GLSL attached before it is compiled, an RGBA8 FBO, blits into the
// window (plain, flipped, scaled), a shared context presenting from a second
// thread and wined3d's hidden helper GL windows.
// Natively (gexc run --gl on its own Wayland compositor, no output), as a
// WoW64 process of 64-bit Wine.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('glcore', { flags: ['-Wall'], libs: ['-lgdi32', '-lopengl32'] });
  const r = await w.native(['./glcore.exe'], { files: { 'glcore.exe': exe },
    flags: ['--no-cgen', '--gl'], wayland: true });
  for (const l of r.out.split('\n').filter((l) => /^(ok  |FAIL|  GL_)/.test(l))) t.say(l);
  t.check('glcore ok', /^glcore ok/m.test(r.out), w.detail(r));
  w.clean();
}
