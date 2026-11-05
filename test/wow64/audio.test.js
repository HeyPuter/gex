// An audio device through winmm, mmdevapi and wineoss.drv against
// gex-kernel's /dev/dsp (audio.c). Natively and on the page.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'audio', { want: /^audio ok [1-9]/m, build: { libs: ['-lwinmm'] } });
