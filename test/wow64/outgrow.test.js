// A 64-bit Windows program that outgrows the lowered 32-bit memory gets a
// 64-bit one from its next start, if it ended for it: one whose 3.5 GiB
// VirtualAlloc found no room (64-bit Wine says so to gex: PR_GEX_SHORT) and
// that then crashes (fixtures/wine/src/outgrow.c) has its .exe recorded
// (user.gex.mem64 = 1), its crash notice says it needs 64-bit memory and
// will get it, and its next start reserves the 3.5 GiB above 4 GiB. One
// that falls back to a smaller block and exits 0 is not recorded and keeps
// the lowered memory. Natively, by a Z:\ path to a file that outlives the
// run's prefix (the record is what is checked: natively a Windows program
// runs in the lowered memory only), and on the page, where the program
// starts itself twice and the second goes on in the memory64 twin
// (WINE64_TWIN=1). Each second start comes the moment the first ends,
// so the record must be made before anything can see the first gone:
// on the page, a run of copies started that way, each found recorded.
import { copyFileSync, existsSync, mkdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1500 };

const SRC = fileURLToPath(new URL('../fixtures/wine/src/outgrow.c', import.meta.url));
const SAID = /outgrow\.exe needs 64-bit memory \(a reservation of 0xe0000000 bytes did not fit\); it gets it from its next start/;
const NOTICE = /needs 64-bit memory; it will get it the next time it starts/;
const lines = (out) => out.replace(/\r/g, '').split('\n').filter((l) => l.startsWith('outgrow ')).join('\n');
// (where the fallback's blocks ended is checked on the page only)
const fell = (out) => lines(out).split('\n').filter((l) => l !== 'outgrow reached 2g').join('\n');
const KUSER_LINE = /memory reached the old KUSER page/;
// The host file's user.gex.mem64, or null.
const recorded = (p) => {
  const r = spawnSync('python3', ['-c', 'import os, sys\ntry: print(os.getxattr(sys.argv[1], "user.gex.mem64").decode())\nexcept OSError: print("")', p], { encoding: 'utf8' });
  return r.stdout.trim() || null;
};
const dos = (p) => `Z:${p.replace(/\//g, '\\')}`;

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('outgrow', { x64: true, src: SRC, out: 'outgrow.exe' });
  const keep = join(t.workDir, 'outgrow-keep');
  mkdirSync(keep, { recursive: true });
  copyFileSync(exe, join(keep, 'outgrow.exe'));
  copyFileSync(exe, join(keep, 'fallback.exe'));

  // natively: by a Z:\ path, so the record stays with the file
  const c = await w.native([dos(join(keep, 'outgrow.exe')), 'crash'], { name: 'native-crash' });
  t.eq('native: it falls short and crashes', lines(c.out), 'outgrow short', w.detail(c));
  t.check('native: its crash notice says it needs 64-bit memory', NOTICE.test(c.out), w.detail(c));
  // (natively a Windows program runs in the lowered memory only: the
  // memory64 twin is the page's, so the record is what is checked)
  t.eq('native: its .exe is recorded for a 64-bit memory', recorded(join(keep, 'outgrow.exe')), '1', w.detail(c));
  const f = await w.native([dos(join(keep, 'fallback.exe')), 'twice', 'fallback'], { name: 'native-fallback' });
  t.eq('native: one that falls back and exits 0 keeps the lowered memory', fell(f.out),
    'outgrow fallback ok\noutgrow child exit 00000000\noutgrow fallback ok\noutgrow child exit 00000000', w.detail(f));
  t.check('native: and is not recorded', !NOTICE.test(f.out) && recorded(join(keep, 'fallback.exe')) === null, w.detail(f));

  // on the page: the second start, in the same session, has 64 bits
  if (!existsSync(join(process.env.GEX_TEST_WOW64_DIST, 'lib/wine/x86_64-unix/wine.m64'))) t.skip('the WoW64 install has no memory64 twin (WINE64_TWIN=1)');
  // 600 s: the twin's first start compiles its memory64 modules
  const p = await w.page(['/t/outgrow.exe', 'twice', 'crash'], { files: { '/t/outgrow.exe': exe }, name: 'page-crash', timeout: 600000 });
  t.eq('page: it crashes for want of room, and its next start has a 64-bit memory', lines(p.out),
    'outgrow short\noutgrow child exit c0000005\noutgrow big high\noutgrow child exit 00000000', w.detail(p));
  // (run-node shows no crash notices: the line gex says as the record is
  // made; the notice's reason is the kernel's, as natively)
  t.check('page: gex says it needs 64-bit memory', SAID.test(p.out), w.detail(p));
  // the same race, more times: each copy's second start, at once, finds
  // its record
  const N = 6, copies = {}, names = [];
  for (let i = 0; i < N; i++) { copies[`/t/og${i}.exe`] = exe; names.push(`Z:\\t\\og${i}.exe`); }
  copies['/t/outgrow.exe'] = exe;
  const pl = await w.page(['/t/outgrow.exe', 'each', 'crash', ...names], { files: copies, name: 'page-relaunch', timeout: 1200000 });
  t.eq(`page: ${N} programs each relaunched the moment they crash have a 64-bit memory at once`, lines(pl.out),
    Array(N).fill('outgrow short\noutgrow child exit c0000005\noutgrow big high\noutgrow child exit 00000000').join('\n'), w.detail(pl));
  const pf = await w.page(['/t/outgrow.exe', 'twice', 'fallback'], { files: { '/t/outgrow.exe': exe }, name: 'page-fallback', timeout: 600000 });
  t.eq('page: one that falls back and exits 0 is not recorded and keeps the lowered memory', lines(pf.out),
    'outgrow fallback ok\noutgrow reached 2g\noutgrow child exit 00000000\noutgrow fallback ok\noutgrow reached 2g\noutgrow child exit 00000000', w.detail(pf));
  // its last block takes the memory past Windows' old KUSER page, which matters only to an
  // unguarded unit's computed reads: no unit here, so nothing is said by
  // default (debug output only)
  t.check('page: reaching the old KUSER page without units says nothing by default', !KUSER_LINE.test(pf.out + pf.err), w.detail(pf));
  const pd = await w.page(['/t/outgrow.exe', 'fallback'], { files: { '/t/outgrow.exe': exe }, env: { GEX_LOG: 'debug' }, name: 'page-fallback-debug', timeout: 600000 });
  t.check('page: and says it as debug output', KUSER_LINE.test(pd.out + pd.err), w.detail(pd));
  w.clean();
}
