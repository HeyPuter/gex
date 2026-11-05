// `explorer /n` under a shell desktop (`explorer /desktop=shell`) opens its
// window: the new explorer asks the desktop's explorer, over COM
// (ncalrpc), to open it, and the server side's handoff and io thread must
// run. Under gexc's Wayland compositor with the native PE core
// (GEX_TEST_WINE64_NATIVE) and the builtins at their pins (wine/dll-pins.txt
// through wine/dll-bases.py, so each process places them alike): the
// compositor must place a second client's window (a `[wl-place]` line with
// a `2:` surface; the first client is the desktop and its taskbar) within
// 240 s. A server thread stuck after RPCRT4_CreateConnection never hands the
// connection off, and only the desktop's surfaces are placed.
import { copyFileSync, existsSync, readFileSync, realpathSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { join } from 'node:path';
import { graphics, ROOT, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: [], timeout: 900 };

export default async function (t) {
  const w = wine64(t, { door: null, native: true });
  const pe = join(realpathSync(w.dist), 'lib/wine/x86_64-windows');
  const pins = readFileSync(join(ROOT, 'wine/dll-pins.txt'), 'utf8').split('\n').map((l) => l.trim())
    .filter((l) => /^[a-z0-9_.]+\.(dll|exe|drv)$/.test(l) && existsSync(join(pe, l))).map((l) => join(pe, l));
  let why = '';
  const r = await w.native(['explorer', '/desktop=shell,1280x800', 'explorer', '/n'], {
    userReg: graphics('wayland'), wayland: { size: '1280x800', out: 'null' }, wineEnv: { USER: 'gex', HOME: '/tmp' }, timeout: 240000, name: 'explorer-window',
    until: /^\[wl-place\].* 2:[0-9]/m,
    setup: (p) => {
      copyFileSync(join(pe, 'winewayland.drv'), join(p, 'drive_c/windows/system32/winewayland.drv'));
      const bases = join(p, 'dll-bases');
      const a = spawnSync('python3', [join(ROOT, 'wine/dll-bases.py'), 'assign', bases], { input: pins.join('\n') + '\n', encoding: 'utf8' });
      const c = a.status === 0 && spawnSync('python3', [join(ROOT, 'wine/dll-bases.py'), 'check', bases], { encoding: 'utf8' });
      if (a.status !== 0 || c.status !== 0) why = `${a.stderr ?? ''}${c ? c.stderr : ''}`.trim().split('\n').slice(-3).join(' / ');
    } });
  if (!t.check('the builtins pinned (dll-bases.py assign, check)', !why, why)) return w.clean();
  t.check(`explorer /n's window placed${r.until !== null ? ` after ${(r.until / 1000).toFixed(0)} s` : ''}, within 240 s`, r.until !== null, w.detail(r, 4));
  w.clean();
}
