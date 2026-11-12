// Escape closes the shell desktop's menus while a program had the keyboard
// (menuesc.c, menuesc.wl), as on Windows. Two halves: explorer makes the
// menu's thread foreground when it opens the Start menu or the desktop's
// context menu (the desktop window itself is never foreground, so the
// desktop's menu takes a hidden window of its thread), and Wine's Wayland
// driver sends each key for the foreground window, whose thread the server
// gives it to (the compositor's keyboard follows Win32 activation only to a
// window with a surface, and stayed with the program). Under gexc's Wayland
// compositor with the native PE core (GEX_TEST_WINE64_NATIVE), menuesc.exe
// started on a default desktop that is the shell (with its taskbar).
import { copyFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { graphics, ROOT, wine64 } from '../lib/winerun.mjs';

// the default desktop is "shell", 1024x768: explorer's taskbar and desktop menu
const SHELL = '\n[Software\\\\Wine\\\\Explorer]\n"Desktop"="shell"\n\n[Software\\\\Wine\\\\Explorer\\\\Desktops]\n"shell"="1024x768"\n';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 900 };

export default async function (t) {
  const w = wine64(t, { door: null, native: true });
  const r = await w.native(['C:\\menuesc.exe', '60'], {
    files: { 'menuesc.exe': w.prog('menuesc.exe', 'x64') },
    userReg: `${graphics('wayland')}${SHELL}`, wayland: { size: '1024x768', out: 'null' },
    env: { GEX_WL_INPUT: join(ROOT, 'test/fixtures/wine/menuesc.wl') },
    wineEnv: { USER: 'gex', HOME: '/tmp', WINEGEX_DOOR: '1', WINEDLLOVERRIDES: 'mscoree=;mshtml=' },
    timeout: 300000, name: 'menuesc', until: /^\d+ failures/m,
    setup: (p) => {
      const drv = join(p, 'drive_c/windows/system32/winewayland.drv');
      if (!existsSync(drv)) copyFileSync(join(w.dist, 'lib/wine/x86_64-windows/winewayland.drv'), drv);
    } });
  const lines = r.out.split('\n').filter((l) => /^(ok|FAIL) /.test(l));
  for (const l of lines) t.check(l.replace(/^(ok|FAIL) /, ''), l.startsWith('ok '), w.detail(r, 4));
  t.check('menuesc finished with no failures', /^0 failures/m.test(r.out),
    `${(r.out.match(/^menuesc: .*$/gm) ?? []).join('; ') || 'no menuesc lines'}; ${w.detail(r, 4)}`);
  w.clean();
}
