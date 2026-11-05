// A console window's font is a fixed-pitch raster font of Wine's own (its
// .fon fonts in the install's share/wine/fonts): with only the .ttf the
// fonts build makes them from, which win32u skips, there is no fixed-pitch
// font, and conhost squeezed Tahoma into its cells. Under gexc's Wayland
// compositor with the native PE core (GEX_TEST_WINE64_NATIVE), as a page
// runs it: `start /wait cmd /c exit` opens a console window, whose conhost
// picks its first font and saves it under HKCU\Console; `reg query` reads
// it back.
import { copyFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { graphics, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: [], timeout: 900 };

export default async function (t) {
  const w = wine64(t, { door: null, native: true });
  const fonts = ['vgafix.fon', 'coure.fon', 'sserife.fon', 'vgasys.fon'];
  const missing = fonts.filter((f) => !existsSync(join(w.dist, 'share/wine/fonts', f)));
  t.check('the install has Wine\'s raster fonts', !missing.length, `share/wine/fonts lacks ${missing.join(' ')}`);
  const r = await w.native(['cmd', '/c', 'start /wait cmd /c exit & reg query HKCU\\Console'], {
    userReg: graphics('wayland'), wayland: { size: '1280x800', out: 'null' }, wineEnv: { USER: 'gex', HOME: '/tmp' }, timeout: 240000, name: 'console-font',
    setup: (p) => copyFileSync(join(w.dist, 'lib/wine/x86_64-windows/winewayland.drv'), join(p, 'drive_c/windows/system32/winewayland.drv')) });
  const face = /^ *FaceName +REG_SZ +(.*?)[ \r]*$/m.exec(r.out)?.[1];
  const family = /^ *FontFamily +REG_DWORD +0x([0-9a-fA-F]+)/m.exec(r.out)?.[1];
  if (!t.check('HKCU\\Console has a FaceName (a console window saved its font)', !!face, w.detail(r, 4))) return w.clean();
  // conhost saves the font's TEXTMETRIC tmPitchAndFamily, whose
  // TMPF_FIXED_PITCH bit (1) is set for a variable-pitch font, despite its name
  t.check(`the console's font ${face} is fixed-pitch (family 0x${family ?? '?'})`, family !== undefined && !(parseInt(family, 16) & 1));
  t.check(`${face} is one of Wine's raster faces (Courier, Fixedsys, Terminal, System)`, ['Courier', 'Fixedsys', 'Terminal', 'System'].includes(face));
  w.clean();
}
