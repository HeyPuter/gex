// Media through winegstreamer, its Unix side over FFmpeg (wine64-dist.sh's
// default install: winegstreamer.so, a side module the loader dlopens on the
// first media call): fixtures/wine/src/mediatest.c decoding the `win-media` files,
// H.264 and AAC in MP4 and WMV2 and WMA2 in ASF, through Media Foundation's
// source reader and media session, a DirectShow graph and the WM sync
// reader. Each prints "PASS MODE ..." and exits 0. Skipped for an install
// without winegstreamer.so (WINE64_MEDIA=off).
import { bootWine } from '../lib/wine.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs', 'win-media'], timeout: 2400 };

const FILES = ['h264-aac.mp4', 'wmv2-wma2.wmv'];
const MODES = ['sr', 'ms', 'ds', 'wm'];

export default async function (t) {
  const w = await bootWine(t, { nullDriver: true });
  try { await w.fs.stat('/wine64/lib/wine/x86_64-unix/winegstreamer.so'); } catch { t.skip('the Wine install has no winegstreamer.so (WINE64_MEDIA=off)'); }
  await w.fs.addTar(t.work('win-media.tar'), { at: '/pfx/drive_c/media' });
  for (const file of FILES) {
    for (const mode of MODES) {
      const name = `${mode} ${file}`;
      const r = await t.time(name, () => w.run('mediatest.exe', [mode, `C:\\media\\${file}`], { timeout: 600000 })).catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
      const line = r.stdout.split('\n').find((l) => /^(PASS|FAIL) /.test(l)) ?? '';
      const first = /^first video sample \d+ ms/m.exec(r.stdout);
      if (first) t.say(`${name}: ${first[0]}`);
      t.check(`${name}: exit 0, PASS ${mode}`, r.code === 0 && line.startsWith(`PASS ${mode} `), `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
    }
  }
  await w.sys.shutdown();
}
