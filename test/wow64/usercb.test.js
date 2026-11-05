// A user-mode callback made from a Unix call after the PE side has pushed
// frames since its last system call (usercb.c): opengl32's GL debug messages,
// delivered inside glDebugMessageInsert and at the guest GL library's next
// waiting submit, while 6 KiB of canary frames sit between the last system
// call's stack pointer and the call.
// Natively (gexc run --gl on its own Wayland compositor, no output), as a
// WoW64 process of 64-bit Wine.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('usercb', { flags: ['-Wall'], libs: ['-lgdi32', '-lopengl32'] });
  const r = await w.native(['./usercb.exe'], { files: { 'usercb.exe': exe },
    flags: ['--no-cgen', '--gl'], wayland: true });
  for (const l of r.out.split('\n').filter((l) => /^(ok  |FAIL|  GL_)/.test(l))) t.say(l);
  t.check('usercb ok', /^usercb ok/m.test(r.out), w.detail(r));
  w.clean();
}
