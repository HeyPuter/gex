// A Wine thread that ends inside its start routine's call-out, after native
// code called back into it (threadexit.c): the process must go on and print
// "threads ok N".
// Natively (gexc run) and on the page's shim (run-node.mjs), as a WoW64
// process of 64-bit Wine.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'threadexit', { want: /^threads ok 8/m, args: [8] });
