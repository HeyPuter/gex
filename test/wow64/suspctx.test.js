// Another thread's x87/SSE context while it runs guest code (suspctx.c),
// read and set through suspend, in a WoW64 process. Natively and on the page.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'suspctx', { want: /^suspctx 0 failures/m });
