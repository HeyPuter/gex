// Audio for a 64-bit program under gexc, with the door: winmm's waveOut and
// an mmdevapi stream each play 100 ms through wineoss.drv's Unix side in the
// loader to gex-kernel's /dev/dsp (audio; every x64 game that opens an
// audio device stops without it), and an ASIO host on gex audio (asiohost:
// Wine's dlls/gexasio, registered, created by its class id and run for 10 s
// of bufferSwitch, the switch count within 10% of the rate's).
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 1800 };

export default async function (t) {
  const w = wine64(t);
  if (t.check('audio: the loader has wineoss.so', w.loaderHas('wineoss.so')))
    await w.nt({ name: 'audio', want: /waveout ok\nmmdevapi ok/, exe: 'audio.exe' });
  const asio = join(w.dist, 'lib/wine/x86_64-windows/gexasio.dll');
  if (!w.loaderHas('gexasio.so') || !existsSync(asio)) t.say('asio: no gexasio in this install (Wine\'s dlls/gexasio)');
  // (gexasio.dll beside the program: the prefix predates it, and Wine loads
  // a builtin only through its file in the prefix, as wineboot installs it)
  else await w.nt({ name: 'asio', exe: 'asiohost.exe', extra: [asio],
    want: /asio driver gex audio\nasio rate [0-9]+ buffer 512 \(min 64 max 2048\) inputs [02] outputs 2 latency in [0-9]+ out [0-9]+\nasio switches [0-9]+ want [0-9]+ \(first after [0-9]+ ms\), position ok, time info steady/ });
  w.clean();
}
