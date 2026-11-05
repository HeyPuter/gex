// The native PE core's modules on the page (GEX_TEST_WINE64_NATIVE), with
// the door and run-node's audio player and its loopback: their host-data
// views and entries, through audio (waveOut, mmdevapi), the ASIO host,
// GetModuleHandleExW(FROM_ADDRESS) on a builtin's data and window
// procedures (fromaddr), a skeleton's view unmapped and its addresses
// reused (stalebind), native DLLs freed and loaded again (dllreload),
// gex's memory outside every image (hostdata), and gexcc-win programs'
// own native modules (GEX_TEST_GEXCC_WIN; skipped without it); and the
// console set, with the audio player on (compact.test.js runs it without).
import { allOk, asio, CONSOLE, each, gexccWin } from '../wine64/cases.mjs';
import { wine64 } from '../lib/winerun.mjs';
import { Skip } from '../lib/t.js';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 5400 };

export default async function (t) {
  const w = wine64(t, { door: 1, native: true });
  const o = { page: {}, env: { GEX_AUDIO_PLAYER: '1', GEX_AUDIO_LOOP: '1' }, timeout: 900000 };
  const pre = 'page pe: ';
  await each(w, CONSOLE, o, pre);
  if (t.check(`${pre}audio: the loader has wineoss.so`, w.loaderHas('wineoss.so')))
    await w.nt({ ...o, name: `${pre}audio`, want: /waveout ok\nmmdevapi ok/, exe: 'audio.exe' });
  await asio(t, w, o, pre);
  await each(w, ['fromaddr', 'hostdata', 'stalebind', 'dllreload'].map((n) => ({ name: n, want: allOk, exe: `${n}.exe` })), o, pre);
  try {
    await gexccWin(t, w, o, pre, ['cxxcatch', 'xthr', 'xhoist', 'cxxthr', 'dllimps', 'lresult', 'cbfault', 'ptval']);
  } catch (e) {
    if (!(e instanceof Skip)) throw e;
    t.say(`${pre}gexcc-win: ${e.message}`);
  }
  w.clean();
}
