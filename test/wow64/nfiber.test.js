// Fibers from native code (fixtures/wow64/nfiber.c, a gexcc-win program,
// GEX_TEST_GEXCC_WIN): CreateFiber and CreateFiberEx refuse a native start
// routine with ERROR_NOT_SUPPORTED, and a switch called from native code
// ends the process with a message, not at a wrong place. Guest fibers go
// on as before: fixtures/wow64/fibers.c as an x86-64 program here (and
// 32-bit in wow64/fibers, from a handler in wow64/tebframes). Natively.
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

const lines = (out, p) => out.replace(/\r/g, '').split('\n').filter((l) => l.startsWith(p)).join('\n');

export default async function (t) {
  const w = wow64(t);
  const x86 = await w.mingw('fibers', { x64: true, out: 'fibers64.exe' });
  const g = await w.native(['C:\\fibers64.exe'], { files: { 'fibers64.exe': x86 }, name: 'fibers64' });
  t.check('x86-64 guest fibers switch', /^fibers ok/m.test(g.out.replace(/\r/g, '')), w.detail(g));
  const exe = await w.gexccWin(join(import.meta.dirname, '../fixtures/wow64/nfiber.c'), 'nfiber.exe', ['-O1']);
  const files = { 'nfiber.exe': exe, 'nfiber.exe.wasm': `${exe}.wasm` };
  const n = await w.native(['C:\\nfiber.exe'], { files, name: 'refused' });
  t.eq('a native start routine is refused', lines(n.out, 'nfiber'), 'nfiber refused 50 50', w.detail(n));
  const s = await w.native(['C:\\nfiber.exe', 'switch'], { files, name: 'switch' });
  const out = `${s.out}\n${s.err ?? ''}`;
  t.check('a switch from native code ends the process with a message',
    out.includes('wine: fibers from native code are not supported') && !out.includes('nfiber: switched'), w.detail(s));
  w.clean();
}
