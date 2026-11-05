// Media through winegstreamer over FFmpeg (its Unix side winegstreamer.so,
// a side module the loader dlopens on the first media call), natively under
// gexc: fixtures/wine/src/mediatest.c for x86_64, in the WoW64 install
// and prefix (wow64/media the i686 build, a WoW64 process), decoding the win-media
// image's files (made with the host's ffmpeg) through Media Foundation's
// source reader (sr) and media session (ms), a DirectShow graph (ds) and the
// WM sync reader (wm), each run printing "PASS MODE ...". On the null
// driver. (wine/media runs them in the gex runtime.)
import { copyFileSync, existsSync, mkdirSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { graphics, wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs', 'win-media'], timeout: 2400 };

export default async function (t) {
  const w = wow64(t);
  if (!t.check('the install has winegstreamer.so', existsSync(join(w.dist, 'lib/wine/x86_64-unix/winegstreamer.so')), 'built with WINE64_MEDIA=off?')) return;
  const media = join(t.workDir, 'win-media');
  const setup = (p) => {
    mkdirSync(join(p, 'drive_c/media'));
    for (const f of readdirSync(media)) copyFileSync(join(media, f), join(p, 'drive_c/media', f));
  };
  for (const [arch, a] of [['x64', 'x86_64']]) {
    let exe;
    try { exe = w.prog('mediatest.exe', arch); } catch (e) { t.check(`${a}: mediatest.exe`, false, e.message); continue; }
    for (const f of ['h264-aac.mp4', 'wmv2-wma2.wmv'])
      for (const m of ['sr', 'ms', 'ds', 'wm']) {
        const r = await w.native([`C:\\mediatest-${a}.exe`, m, `C:\\media\\${f}`], { files: { [`mediatest-${a}.exe`]: exe },
          userReg: graphics('null'), setup, name: `${a}-${m}-${f.split('.')[0]}` });
        const fails = r.out.split('\n').filter((l) => /^FAIL/.test(l)).slice(0, 3).join(' / ');
        t.check(`${a} ${m} ${f}: PASS ${m}`, r.code === 0 && new RegExp(`^PASS ${m} `, 'm').test(r.out), `${fails}; ${w.detail(r, 6)}`);
      }
  }
  w.clean();
}
