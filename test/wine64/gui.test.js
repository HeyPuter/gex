// 64-bit Wine's windows on the Wayland driver under gexc, without the door,
// on a compositor of the run's own (fixtures/wine/src), its frames checked
// for the colours each draws:
// - window: four quadrants and a white square painted with GDI, up 3 s;
// - redraw: a window moved while WM_SETREDRAW hides it stays mapped (the
//   last two frames have its caption and client);
// - notepad: Wine's notepad up 5 s and closed by WM_CLOSE, its caption,
//   edit field and the text of its title, menus and status bar (the prefix's
//   visual style, Wine's own, draws the caption: white, its frame #909090);
// - user_win, gdi, input: USER, GDI and input on the display (they check
//   themselves);
// - desktop: explorer's tray window, the screen the display's size;
// - display: the display devices and modes a fullscreen game asks for (at
//   1024x768: emulated modes go up to the output's size).
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 2400 };

const allOk = /(ok [^\n]*\n)+0 failures/;
const CASES = [
  { name: 'window', want: /shown\nbye/, exe: 'window.exe', args: ['3'], wayland: { colors: ['220,40,40', '40,200,60', '40,80,220', '230,210,40', '255,255,255'] } },
  { name: 'redraw', want: /visible/, exe: 'redraw.exe', wayland: { colors: ['40,200,60:5000', '220,40,40:20000'], last: 2 } },
  { name: 'notepad', want: /title [^\n]*Notepad\nclosed 0/, exe: 'notepadrun.exe', args: ['5'], wayland: { colors: ['144,144,144:1000', '255,255,255:50000', '0,0,0:600'] } },
  { name: 'user_win', want: allOk, exe: 'user_win.exe', wayland: {} },
  { name: 'gdi', want: allOk, exe: 'gdi.exe', wayland: {} },
  { name: 'input', want: allOk, exe: 'input.exe', wayland: {} },
  { name: 'desktop', want: allOk, exe: 'desktop.exe', args: ['640', '480'], wayland: {} },
  { name: 'display', want: allOk, exe: 'display.exe', wayland: { size: '1024x768' } },
];

export default async function (t) {
  const w = wine64(t, { door: 0 });
  if (!t.check('the loader has winewayland.so, the install its driver', w.loaderHas('winewayland.so') && existsSync(join(w.dist, 'lib/wine/x86_64-windows/winewayland.drv')))) return;
  for (const c of CASES) await w.nt(c);
  w.clean();
}
