// The shell desktop's taskbar is on top of other windows, and a full-screen
// foreground window covers it, as on Windows (tbtop.c): explorer's
// Shell_TrayWnd is WS_EX_TOPMOST and goes below a full-screen window while
// that is foreground. Under gexc's Wayland compositor with the native PE core
// (GEX_TEST_WINE64_NATIVE), tbtop.exe started on a default desktop that is
// the shell (with its taskbar).
import { copyFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { graphics, wine64 } from '../lib/winerun.mjs';

// the default desktop is "shell", 1024x768: explorer's taskbar and desktop menu
const SHELL = '\n[Software\\\\Wine\\\\Explorer]\n"Desktop"="shell"\n\n[Software\\\\Wine\\\\Explorer\\\\Desktops]\n"shell"="1024x768"\n';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 900 };

export default async function (t) {
  const w = wine64(t, { door: null, native: true });
  const r = await w.native(['C:\\tbtop.exe'], {
    files: { 'tbtop.exe': w.prog('tbtop.exe', 'x64') },
    userReg: `${graphics('wayland')}${SHELL}`, wayland: { size: '1024x768', out: 'null' },
    wineEnv: { USER: 'gex', HOME: '/tmp', WINEGEX_DOOR: '1', WINEDLLOVERRIDES: 'mscoree=;mshtml=' },
    timeout: 300000, name: 'tbtop', until: /^\d+ failures/m,
    setup: (p) => {
      const drv = join(p, 'drive_c/windows/system32/winewayland.drv');
      if (!existsSync(drv)) copyFileSync(join(w.dist, 'lib/wine/x86_64-windows/winewayland.drv'), drv);
    } });
  const lines = r.out.split('\n').filter((l) => /^(ok|FAIL) /.test(l));
  for (const l of lines) t.check(l.replace(/^(ok|FAIL) /, ''), l.startsWith('ok '), w.detail(r, 4));
  t.check('tbtop finished with no failures', /^0 failures/m.test(r.out),
    `${(r.out.match(/^tbtop: .*$/gm) ?? []).join('; ') || 'no tbtop lines'}; ${w.detail(r, 4)}`);
  w.clean();
}
