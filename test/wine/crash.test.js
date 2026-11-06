// A Windows program that crashes, on the page with its windows (HostWindows):
// its window shows, then closes once the program has crashed, the System
// reports the crash (`crash`: the .exe, the exception, its code), and the
// program that started it hears the exception's code (crashlaunch.c). An
// i386 program through WoW64 and an x64 one, a phase each.
import { bootWine } from '../lib/wine.mjs';
import { HostWindows } from '../../page/runtime/src/windows/host.js';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1800, alone: true };

const CASES = [['crash.exe', 'x86'], ['crash.exe', 'x64']];

export default async function (t) {
  const [exe, arch] = CASES[t.phase - 1];
  const what = `${exe} (${arch})`;
  const wins = [];
  const crashes = [];
  let hw;
  const w = await bootWine(t, {
    wow64: true,
    nullDriver: false,
    parts: (sys) => {
      hw = new HostWindows(sys, { width: 800, height: 600, scale: 1 });
      hw.addEventListener('window', (e) => {
        const win = { title: () => e.window.title, opened: performance.now(), closed: 0 };
        e.window.addEventListener('closed', () => { win.closed = performance.now(); });
        wins.push(win);
      });
      sys.addEventListener('crash', (e) => crashes.push({ at: performance.now(), pid: e.pid, name: e.name, reason: e.reason, code: e.code, signal: e.signal }));
      return hw;
    },
  });
  await w.put(exe, arch);
  const r = await w.run('crashlaunch.exe', [exe, 'h'], { arch: 'x86', timeout: 300000, env: hw.env });
  t.say(r.stdout.trim().replace(/\n/g, ' / '));
  const m = r.stdout.match(/^launch: ended, exit 0x([0-9a-f]+), (\d+) ms$/m);
  t.check(`${what}: ended`, !!m, `stdout ${JSON.stringify(r.stdout.slice(-400))}; stderr ${JSON.stringify(r.stderr.slice(-400))}`);
  if (m) {
    t.eq(`${what}: the exception's code`, m[1], 'c0000005');
    t.check(`${what}: within 5 s`, Number(m[2]) < 5000, `${m[2]} ms`);
  }
  t.check(`${what}: its window gone (wineserver's)`, /^launch: window gone$/m.test(r.stdout), r.stdout);
  // the page's: the window it showed, closed
  const win = wins.find((x) => x.title() === 'crash');
  t.check(`${what}: its window showed`, !!win, `windows: ${JSON.stringify(wins.map((x) => x.title()))}`);
  for (let i = 0; i < 100 && win && !win.closed; i++) await new Promise((res) => setTimeout(res, 50));
  t.check(`${what}: its window closed`, !!win?.closed, 'still open 5 s after the program ended');
  const c = crashes.find((x) => x.name === exe);
  t.check(`${what}: reported`, !!c, `crash events: ${JSON.stringify(crashes)}`);
  if (c) {
    t.eq(`${what}: the report's code and signal`, [c.code, c.signal], [0xc0000005, 0]);
    t.check(`${what}: the report's reason`, /^page fault on (read|write) access to [0-9A-F]+ at address [0-9A-F]+$/.test(c.reason), c.reason);
  }
  await w.sys.shutdown();
  if (t.phase < CASES.length) return t.reload();
}
