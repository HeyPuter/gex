// The console functions taking a COORD by value (consolecoord.c): a
// one-word struct, which the .spec spells as an int and wasm32 takes by
// reference, so an adapter passing the word made the COORD of an address
// (a 32-bit SetConsoleScreenBufferSize failed, its SetConsoleCursorPosition
// went to 0,0). In a console of the program's own (a child with
// CREATE_NEW_CONSOLE: Wine's conhost). Natively from an i386 program in a
// WoW64 process and from the same program built for x86_64, and on the page.
import { wow64, graphics } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^consolecoord ok/m;
  // conhost opens a window for a console of the program's own and quits
  // when it cannot: Wine's null display driver, as the prefix has none
  const userReg = graphics('null');
  const exe = await w.mingw('consolecoord');
  const exe64 = await w.mingw('consolecoord', { out: 'consolecoord64.exe', x64: true });
  const n = await w.native(['C:\\consolecoord.exe'], { files: { 'consolecoord.exe': exe }, userReg });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const n64 = await w.native(['C:\\consolecoord64.exe'], { files: { 'consolecoord64.exe': exe64 }, userReg, name: 'native64' });
  t.check(`native64: ${want.source}`, want.test(n64.out), w.detail(n64));
  const p = await w.page(['/t/consolecoord.exe'], { files: { '/t/consolecoord.exe': exe }, userReg });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
