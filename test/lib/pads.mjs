// Game controllers for Windows programs: the page's controllers (Gamepads,
// made up here: a headless browser has none) through Wine's HID bus,
// winebus over SDL, which a Wine install with controllers starts only on
// demand (its service's "DemandStart"=1) and WineControllers starts once
// there is a controller. fixtures/wine/src/padtest.c reads it through
// XInput, DirectInput 8 and winmm, 64-bit and 32-bit (WoW64). The tests
// wine/pads-none, pads-boot and pads-later (a page runs one system: one
// each); skipped for an install without controllers (WINE64_PADS=off).
import { bootWine } from './wine.mjs';
import { Gamepads, WineControllers } from '../../page/runtime/src/input.js';

const button = (pressed = false, value = pressed ? 1 : 0) => ({ pressed, value });
/** An Xbox 360 pad, at rest, or (`held`) with A held and the left stick
 *  about half right (`x`). */
export function xbox(held = true, x = 0.5) {
  const buttons = Array.from({ length: 17 }, () => button());
  if (held) buttons[0] = button(true);
  return { index: 0, id: 'Xbox 360 Controller (XInput STANDARD GAMEPAD)', mapping: 'standard', connected: true, buttons, axes: [held ? x : 0, 0, 0, 0] };
}

/** Run padtest.exe (ARCH) `wait SECS` with Wine: the pad in `list` at rest
 *  until the program has it, then held, the stick moving a little (a pad's
 *  state reaches Windows as changes: evdev events, SDL's, then HID reports,
 *  and DirectInput and winmm see only reports after they open it, as with
 *  a pad on Linux); `ready` (optional) runs once the program is up and
 *  waiting. `{ code, stdout, stderr }`. */
export async function waitRun(t, w, list, arch, secs, ready) {
  await w.put('padtest.exe', arch);
  if (list[0]) list[0] = xbox(false);
  const p = w.sys.spawn(['/wine64/lib/wine/x86_64-unix/wine', 'C:\\padtest.exe', 'wait', String(secs)], { env: w.env, cwd: '/pfx/drive_c' });
  let out = '';
  const dec = new TextDecoder();
  (async () => { for (const rd = p.stdout.getReader(); ;) { const { value, done } = await rd.read(); if (done) return; out += dec.decode(value); } })().catch(() => {});
  p.stderr.pipeTo(new WritableStream()).catch(() => {});
  await t.until(() => out.includes('DirectInput8Create'), `${arch}: the program up`, 300000);
  await ready?.();
  await t.until(() => /XInput slot 0 connected/.test(out), `${arch}: the pad seen`, (secs + 60) * 1000);
  let n = 0;
  const move = setInterval(() => { if (list[0]) list[0] = xbox(true, n++ % 2 ? 0.55 : 0.5); }, 100);
  const { code } = await t.timeout(p.exited, (secs + 120) * 1000, 'padtest').finally(() => clearInterval(move));
  return { code, stdout: out.replace(/\r/g, ''), stderr: '' };
}

/** The guest's processes' command lines. */
export async function procs(fs) {
  const out = [];
  for (const e of await fs.readdir('/proc')) {
    const name = typeof e === 'string' ? e : e.name;
    if (!/^\d+$/.test(name)) continue;
    try { out.push((await fs.readFile(`/proc/${name}/cmdline`, { encoding: 'utf-8' })).split('\0').filter(Boolean).join(' ')); } catch { /* gone */ }
  }
  return out;
}

/** A Wine system with the made-up controllers `list` (changed in place:
 *  connects and disconnects), its WineControllers and the codes of its
 *  starts. */
export async function boot(t, list) {
  let gp = null;
  const w = await bootWine(t, { wow64: true, parts: (sys) => { gp = new Gamepads(sys, { source: () => list }); } });
  try { await w.fs.stat('/wine64/lib/wine/x86_64-unix/libSDL2-2.0.so.0'); } catch { t.skip('the Wine install has no libSDL2-2.0.so.0 (WINE64_PADS=off)'); }
  const wc = new WineControllers(w.sys, gp, { wine: '/wine64/lib/wine/x86_64-unix/wine', env: w.env });
  const starts = [];
  wc.addEventListener('start', (e) => starts.push(e.code));
  return { ...w, gp, wc, starts };
}

/** `r` (a run's { code, stdout, stderr }) exited 0 with "0 failures". */
export function checkRun(t, name, r) {
  const fails = r.stdout.split('\n').filter((l) => l.startsWith('FAIL '));
  t.check(`${name}: exit 0, 0 failures`, r.code === 0 && /^0 failures$/m.test(r.stdout), `exit ${r.code}; ${JSON.stringify(fails.length ? fails : r.stdout.slice(-400))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
}

