// 64-bit Wine on the page with function-tier units (--cgen-prefer), with
// the door: a first run of seh and wndproc_seh with an empty unit cache
// names the images and their bases; their units are built with gexc
// (cgen-image, in the page process's flavour: a compact-space process folds
// its KUSER and entry pages), with the trampolines' when the run asked for
// them; then the cases run with them. With a translated PE core ntdll's
// system call stubs run in a unit and call native Wine's entry page from
// there, and USER's NtUser* calls with their window procedures as user
// callbacks; with the native PE core (every builtin compiled) the cases'
// own programs are the images given units.
import { readFileSync } from 'node:fs';
import { basename, join } from 'node:path';
import { allOk, CONSOLE, each } from '../wine64/cases.mjs';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 5400 };

const IMAGES = /\/(ntdll|kernel32|kernelbase|msvcrt|ucrtbase|win32u|user32|gdi32|imm32)\.dll$|\/(seh|wndproc_seh)\.exe$/;

export default async function (t) {
  const w = wine64(t, { door: 1 });
  const pre = 'page units: ';
  const units = join(w.work, 'units');
  const page = { flags: ['--cgen-prefer'] };
  const probe = { seh: [], wndproc_seh: [] };
  let said = '';
  for (const n of Object.keys(probe)) {
    const r = await w.page([`C:\\${n}.exe`], { pfxFiles: { [`${n}.exe`]: w.prog(`${n}.exe`) }, userReg: n === 'wndproc_seh' ? '\n[Software\\\\Wine\\\\Drivers]\n"Graphics"="null"\n' : '',
      prefix: w.pagePrefix(), cwd: '/pfx/drive_c', flags: page.flags, env: { GEX_LOG: 'info', GEX_CGEN_CACHE: units }, split: true, name: `probe-${n}`, timeout: 900000 });
    said += `${r.out}\n${r.err}\n`;
  }
  const images = new Map();
  for (const [, p, b] of said.matchAll(/image (\S+) at base (0x[0-9a-f]+): no function-tier unit/g)) if (IMAGES.test(p)) images.set(`${p} ${b}`, [p, b]);
  const nativeSide = readFileSync(join(w.dist, 'lib/wine/x86_64-windows/ntdll.dll')).includes('.wasmref');
  const named = [...images.values()].map(([p]) => p);
  if (!t.check(`${pre}the probe named ${nativeSide ? 'a case program\'s base' : 'ntdll\'s base'}`,
    nativeSide ? named.some((p) => /\.exe$/.test(p)) : named.some((p) => /\/ntdll\.dll$/.test(p)), [...images.keys()].join(', ') || 'none')) return w.clean();
  // the page process's unit flavour, as the runtime names it
  const compact = (await w.gexcRun(['cgen-flavour', '--page', w.loader], { log: 'flavour' })).trim().split(/\s+/).filter(Boolean);
  const cgen = (args) => w.gexcRun(['cgen-image', ...args, ...compact], { env: { GEX_CGEN_GUARD: '1', GEX_CGEN_CACHE: units }, log: 'units' });
  try {
    if (said.includes('gex-pe: missing unit: [the trampolines]')) await cgen(['--nt-trampolines64']);
    for (const [p, b] of images.values()) {
      const host = p.startsWith('/wine64/') ? join(w.dist, p.slice('/wine64'.length)) : w.prog(basename(p));
      await cgen([host, '--pe-base', b, '--jobs', '8']);
    }
  } catch (e) {
    t.check(`${pre}the units`, false, String(e.message));
    return w.clean();
  }
  const o = { page, env: { GEX_CGEN_CACHE: units }, timeout: 900000 };
  await each(w, [...CONSOLE, { name: 'user_win', want: allOk, exe: 'user_win.exe', nullDriver: true },
    { name: 'wndproc_seh', want: allOk, exe: 'wndproc_seh.exe', nullDriver: true }], o, pre);
  w.clean();
}
