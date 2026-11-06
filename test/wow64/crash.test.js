// A program that crashes ends (crash.c, started by crashlaunch.c, which
// reports how): its process exits with the exception's code at once, its
// window goes, and gex reports the crash ("[gex] pid N (crash.exe) crashed:
// ..."; the page's System has it as a `crash` event, test/wine/crash). An
// i386 program through WoW64 (an access violation, an illegal instruction,
// a fault in Wine's own native code) and an x64 one; natively under gexc's
// compositor and on the page's shim (no display there: the window is
// wineserver's alone).
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

const SRC = join(import.meta.dirname, '../fixtures/wine/src');

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

// [program, crash, the exit code (the exception's), what the report says
// (natively the fault's address and kind are not always known)]
const AV = /crashed: page fault on (read|write) access to [0-9A-F]+ at address [0-9A-F]+/;
const CASES = [
  ['crash.exe', 'h', 'c0000005', AV],
  ['crash.exe', 'u', 'c000001d', /crashed: illegal instruction at address [0-9A-F]+/],
  ['crash.exe', 's', 'c0000005', AV],
  ['crash64.exe', 'h', 'c0000005', AV],
];

export default async function (t) {
  const w = wow64(t);
  const files = {
    'crash.exe': await w.mingw('crash', { libs: ['-luser32'], src: join(SRC, 'crash.c') }),
    'crash64.exe': await w.mingw('crash', { libs: ['-luser32'], x64: true, out: 'crash64.exe', src: join(SRC, 'crash.c') }),
    'crashlaunch.exe': await w.mingw('crashlaunch', { src: join(SRC, 'crashlaunch.c') }),
  };
  const judge = (what, r, code, said) => {
    const m = r.out.match(/^launch: ended, exit 0x([0-9a-f]+), (\d+) ms$/m);
    t.check(`${what}: ended`, !!m, w.detail(r));
    if (!m) return;
    t.check(`${what}: exit 0x${code}`, m[1] === code, `exit 0x${m[1]}`);
    // no debugger: the time to end the process, not winedbg's seconds
    t.check(`${what}: within 5 s`, Number(m[2]) < 5000, `${m[2]} ms`);
    t.check(`${what}: window gone`, /^launch: window gone$/m.test(r.out), w.detail(r));
    t.check(`${what}: never went on`, !/crash survived/.test(r.out), w.detail(r));
    if (said) t.check(`${what}: reported`, new RegExp(`\\[gex\\] pid \\d+ \\(${what.split(' ')[1].replace('.', '\\.')}\\) ${said.source}`).test(r.out), w.detail(r));
  };
  for (const [exe, m, code, said] of CASES) {
    const n = await w.native(['C:\\crashlaunch.exe', exe, m], { files, wayland: true, name: `n-${exe}-${m}`, timeout: 240000 });
    judge(`native ${exe} ${m}`, n, code, said);
    // the window's surface gone too: the compositor's last window list
    // (each change of it a [wl-place] line) has no window of the program's
    const lists = n.out.match(/^\[wl-place\].*$/gm) ?? [];
    t.check(`native ${exe} ${m}: its surface gone`, lists.length > 0 && !/ 200x200\+/.test(lists[lists.length - 1]), lists.slice(-3).join(' / '));
    const p = await w.page(['C:\\crashlaunch.exe', exe, m], { pfxFiles: files, prefix: w.pagePrefix(), cwd: '/pfx/drive_c', name: `p-${exe}-${m}`, timeout: 240000 });
    judge(`page ${exe} ${m}`, p, code, null);
  }
  w.clean();
}
