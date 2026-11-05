// A slow lazy file mapped as an image (lazydoor.c): the wineserver's reads of
// it inside the door call must not hold another process's server calls for
// the fetch; prints "lazydoor ok MAP_MS SLOWEST_MS". On the page's shim
// (run-node.mjs --lazy, each fetch GEX_LAZY_DELAY_MS late), as a WoW64
// process of 64-bit Wine: lazy files are the page's, so there is no native
// leg.
import { copyFileSync } from 'node:fs';
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

const DELAY = 3000;

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('lazydoor');
  // the image the parent maps: a PE of its own (any will do), lazy
  const lazy = join(w.work, 'lazy.dll');
  copyFileSync(exe, lazy);
  const r = await w.page(['C:\\lazydoor.exe', DELAY], { pfxFiles: { 'lazydoor.exe': exe }, cwd: '/pfx/drive_c',
    flags: ['--lazy', `/pfx/drive_c/lazy.dll=${lazy}`], env: { GEX_LAZY_DELAY_MS: String(DELAY) } });
  t.check(`page: ${/^lazydoor ok.*$/m.exec(r.out)?.[0] ?? 'lazydoor ok'}`, /^lazydoor ok/m.test(r.out), w.detail(r));
  w.clean();
}
