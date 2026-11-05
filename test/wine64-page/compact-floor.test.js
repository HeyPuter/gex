// 64-bit Wine's session programs in the compact space start low: on the
// page (run-node.mjs) with the native PE core (GEX_TEST_WINE64_NATIVE, whose
// skeletons the compact space needs) and the door on, so the server's
// starter runs its program again in its place, each session program's
// memory when loaded (GEX_DEBUG_GROWTH's first line for its pid) is under
// 450 MiB, not the 512 MiB an island ending at 0x20000000 set, at least
// three of them start, and the program they serve (compact-floor.c, which
// sleeps 6 s) runs to its end.
import { graphics, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 1200 };

const FLOOR = 450;

export default async function (t) {
  const w = wine64(t, { door: 1, native: true });
  const r = await w.page(['C:\\compact-floor.exe', '6000'], { pfxFiles: { 'compact-floor.exe': w.prog('compact-floor.exe') }, userReg: graphics('null'),
    cwd: '/pfx/drive_c', wineEnv: { GEX_DEBUG_GROWTH: '1' }, timeout: 600000, name: 'compact-floor' });
  const text = r.out.replace(/\r/g, '');
  t.check('the program ran to its end', r.code === 0 && /^compact-floor end/m.test(text), w.detail(r, 4));
  // each pid's first growth line, and the pids that exec a Windows program in system32
  const first = new Map();
  for (const l of text.split('\n')) {
    if (!l.includes('growth pid')) continue;
    const f = l.split(/\s+/);
    const pid = f[f.indexOf('pid') + 1], mem = f[f.indexOf('memory') + 1];
    if (pid && mem && !first.has(pid)) first.set(pid, Number(mem));
  }
  const programs = new Map();
  for (const [, pid, name] of text.matchAll(/^\[gex\] pid ([0-9]+): exec .*system32\\(\S+)/gm)) programs.set(pid, name);
  let n = 0;
  for (const [pid, name] of [...programs].sort((a, b) => a[0] - b[0])) {
    if (!first.has(pid)) continue;
    n++;
    t.check(`pid ${pid} (${name}) at load: ${first.get(pid)} MiB, want under ${FLOOR}`, first.get(pid) < FLOOR);
  }
  t.check(`${n} session programs seen, want 3 or more`, n >= 3);
  w.clean();
}
