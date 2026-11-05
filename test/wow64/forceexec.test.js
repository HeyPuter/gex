// A program linked without NX_COMPAT (forceexec.c): every view turns
// executable and Wine's own bound code keeps running. Natively and on the
// page.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'forceexec', { want: /^forceexec ok/m, build: { flags: ['-Wl,--disable-nxcompat'], libs: ['-lshlwapi'] } });
