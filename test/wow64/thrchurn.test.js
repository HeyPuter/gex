// Threads made and joined one after another, 1500 of them (thrchurn.c):
// more than a process has register-context slots for, and more provider
// stacks than its address space holds, unless each thread that ends gives
// its own back.
// Natively (gexc run) and on the page's shim (run-node.mjs), as a WoW64
// process of 64-bit Wine.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

export default (t) => legs(t, 'thrchurn', { want: /^thrchurn ok 1500/m, args: [1500] });
