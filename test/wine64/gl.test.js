// OpenGL for 64-bit Wine under gexc with the host's GL (gexc run --gl),
// without the door, on the Wayland driver and a compositor of the run's own
// (fixtures/wine/src, fixtures/wine64):
// - glcompat, glfast: opengl32 through the compatibility-profile transport
//   and the calls the unix-call provider writes into the command ring;
// - wglnoctx: GL and Direct3D with no context ever current on a thread;
// - glnulldrv: GL on a null driver window (win32u's framebuffer surface);
// - glwindow: the four quadrants and the white square drawn with GL, its
//   frames checked;
// - glbig: a window taller than the screen keeps its GDI strip while its GL
//   child presents (the last of the window's own frames has the strip);
// - d2dsize: COM methods that return a struct, from a gexcc-win C++
//   program, on a WIC bitmap's Direct2D render target.
import { join } from 'node:path';
import { ROOT, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 2400 };

const allOk = /(ok [^\n]*\n)+0 failures/;
const frames3 = /shown\nframes [0-9]+\nbye/;
const CASES = [
  { name: 'glcompat', want: allOk, exe: 'glcompat.exe', wayland: {} },
  { name: 'glfast', want: allOk, exe: 'glfast.exe', wayland: {} },
  { name: 'wglnoctx', want: allOk, exe: 'wglnoctx.exe', wayland: {} },
  { name: 'glnulldrv', want: /(ok [^\n]*\n|  pixel [^\n]*\n)+0 failures/, exe: 'glnulldrv.exe', nullDriver: true },
  { name: 'glwindow', want: frames3, exe: 'glwindow.exe', args: ['3'], wayland: { colors: ['220,40,40', '40,200,60', '40,80,220', '230,210,40', '255,255,255:100'] } },
  { name: 'glbig', want: frames3, exe: 'glbig.exe', args: ['3'], wayland: { colors: ['200,30,30:5000'], skip: '590x370', last: 1 } },
];

export default async function (t) {
  const w = wine64(t, { door: 0 });
  if (!t.check('the loader has winewayland.so and gex-gl\'s EGL', w.loaderHas('winewayland.so') && w.loaderHas('libEGL.so.1'))) return;
  for (const c of CASES) await w.nt({ ...c, gl: true });
  if (process.env.GEX_TEST_GEXCC_WIN) {
    const exe = await w.gexccWin(join(ROOT, 'test/fixtures/wine64/d2dsize.cpp'), 'd2dsize.exe', ['-O2', '-ld2d1', '-lole32', '-lwindowscodecs', '-luuid']);
    await w.nt({ name: 'd2dsize', want: allOk, exe, extra: [`${exe}.wasm`], gl: true, wayland: {} });
  } else t.say('d2dsize: GEX_TEST_GEXCC_WIN is not set (a gexcc-win install)');
  w.clean();
}
