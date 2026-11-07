// 64-bit Wine's shell and GDI under gexc, without the door, on the null
// driver where a window is made (fixtures/wine/src, fixtures/wine64):
// - textext: GDI's text extents equal the drawn glyphs' advances where a
//   font draws from an embedded bitmap strike, in each quality and DC depth;
// - props, props5: a file's property sheet from its context menu, in a
//   program whose manifest names comctl32 6, and without it (comctl32 5);
// - drives: the prefix's drives listed with no services started
//   (WINEGEX_NO_WINEBOOT=1, the page's boot);
// - scmstart: in that session the first OpenSCManager starts services.exe
//   and succeeds;
// - workarea: a work area one process sets is another's, and its maximised
//   window stops there;
// - hooktest: inline hooks of user32 and kernelbase exports (a 14-byte
//   absolute jmp, a 5-byte rel32 to a relay) and their trampolines;
// - shellnotify: a folder view shows a file once the shell is told it was
//   created there, under My Computer and in Documents.
// As user gex where the prefix's profile matters.
import { join } from 'node:path';
import { ROOT, run, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 2400 };

const allOk = /(ok [^\n]*\n)+0 failures/;
const SRC = join(ROOT, 'test/fixtures/wine64');

export default async function (t) {
  const w = wine64(t, { door: 0 });
  await w.nt({ name: 'textext', want: allOk, exe: 'textext.exe' });
  // props.rc's manifest names comctl32 6
  const rc = join(w.work, 'props-rc.o');
  const r = await run([w.tool('x86_64-w64-mingw32-windres'), '-o', rc, 'props.rc'], { cwd: SRC, file: `${rc}.log` });
  if (t.check('props: windres', r.code === 0, r.out.trim().split('\n').slice(-3).join(' / '))) {
    const libs = ['-lshell32', '-lole32', '-luuid'];
    const exe = await w.mingw('props', { src: join(SRC, 'props.c'), libs: [rc, ...libs], x64: true });
    await w.nt({ name: 'props', want: allOk, exe, nullDriver: true, wineEnv: { USER: 'gex' } });
    const exe5 = await w.mingw('props', { src: join(SRC, 'props.c'), libs, out: 'props5/props.exe', x64: true });
    await w.nt({ name: 'props5', want: allOk, exe: exe5, nullDriver: true, wineEnv: { USER: 'gex' } });
  }
  await w.nt({ name: 'drives', want: allOk, exe: 'drives.exe', wineEnv: { WINEGEX_NO_WINEBOOT: '1', USER: 'gex' } });
  await w.nt({ name: 'scmstart', want: allOk, exe: 'scmstart.exe', wineEnv: { WINEGEX_NO_WINEBOOT: '1' } });
  await w.nt({ name: 'workarea', want: allOk, exe: 'workarea.exe', nullDriver: true });
  await w.nt({ name: 'hooktest', exe: 'hooktest.exe', nullDriver: true,
    want: /(hooktest: [^\n]*\n)*hooktest abs14 user32 ok\nhooktest abs14 kernelbase ok\nhooktest rel5 user32 ok\nhooktest rel5 kernelbase ok\nhooktest: 0 failures/ });
  await w.nt({ name: 'shellnotify', want: allOk, exe: 'shellnotify.exe', nullDriver: true });
  w.clean();
}
