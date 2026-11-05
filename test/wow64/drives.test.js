// The prefix's drives (drivelist.c): GetLogicalDrives and
// GetLogicalDriveStrings list C: in a session wineboot started, which takes
// MountMgr's \??\X: links (the 64-bit session starts it at boot,
// wine/wine64-session.reg). Natively, from an i386 program, a WoW64
// process, and from the same program built for x86_64.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^drivelist ok/m;
  const exe = await w.mingw('drivelist');
  const exe64 = await w.mingw('drivelist', { out: 'drivelist64.exe', x64: true });
  const n = await w.native(['C:\\drivelist.exe'], { files: { 'drivelist.exe': exe } });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const n64 = await w.native(['C:\\drivelist64.exe'], { files: { 'drivelist64.exe': exe64 }, name: 'native64' });
  t.check(`native64: ${want.source}`, want.test(n64.out), w.detail(n64));
  w.clean();
}
