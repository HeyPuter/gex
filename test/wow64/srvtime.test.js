// The wineserver's clock as an i386 client sees it (fixtures/wow64/srvtime.c),
// a WoW64 process, natively and on the page's shim: a server-stamped
// creation time inside the client's window, wait timeouts and relative and
// absolute waitable timers, with time-free requests in between. Under
// doors the server reads its clock lazily, only when a request looks at it.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'srvtime', { want: /^srvtime ok 5/m, args: [5], build: { libs: ['-lntdll'] } });
