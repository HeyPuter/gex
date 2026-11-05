// An x64 program whose preferred base is out of 64-bit Wine's reach
// (MinGW's 0x140000000) starts as the first process, relocated, natively
// (no cgen) and on the page's shim, without the door: no start.exe /exec
// restart (mainexe.c checks the process list, its base and its relocated
// pointers), and one exec of it, no second process for it. Wine's ntdll
// takes the relocated main image (STATUS_IMAGE_NOT_AT_BASE).
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 900 };

export default async function (t) {
  const w = wine64(t, { door: 0 });
  const exe = w.prog('mainexe.exe');
  const verdict = (leg, r) => {
    const execs = r.out.split('\n').filter((l) => /exec .*mainexe\.exe/.test(l)).length;
    const fails = r.out.split('\n').filter((l) => /^FAIL/.test(l)).map((l) => l.trim());
    t.check(`${leg}: mainexe ok, one exec of it`, /^mainexe ok/m.test(r.out) && execs <= 1, `execs of mainexe.exe: ${execs}; ${fails.join(' / ')}; ${w.detail(r, 6)}`);
  };
  verdict('native', await w.native(['C:\\mainexe.exe'], { files: { 'mainexe.exe': exe }, flags: ['--no-cgen'] }));
  verdict('page', await w.page(['C:\\mainexe.exe'], { pfxFiles: { 'mainexe.exe': exe }, cwd: '/pfx/drive_c' }));
  w.clean();
}
