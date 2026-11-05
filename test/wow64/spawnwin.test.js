// A Wayland client with a waiting thread spawns processes (spawnwin.c): its
// windows must keep working after each spawn, and it prints "spawnwin ok N".
// The prefix's graphics driver is wayland. Natively (gexc run with a
// compositor of its own) and on the page's shim (with the shim's
// compositor, GEX_WL_STUB), where a Windows process's child runs in place in
// the cloning thread's Worker.
import { graphics, legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

const N = 4;

export default (t) => legs(t, 'spawnwin', {
  want: new RegExp(`^spawnwin ok ${N}`, 'm'),
  args: [N],
  build: { flags: ['-mwindows', '-Wl,--subsystem,console'] },
  native: { wayland: true, userReg: graphics('wayland') },
  page: { userReg: graphics('wayland'), env: { GEX_WL_STUB: '1' }, wineEnv: { XDG_RUNTIME_DIR: '/tmp', WAYLAND_DISPLAY: 'wayland-0' } },
});
