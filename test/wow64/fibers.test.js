// Fibers (fibers.c): kernelbase's switch_fiber for the wasm32 build, where a
// fiber is the guest's state. Two fibers and the thread's own switch
// thousands of times, keeping their locals, stacks, fiber data and
// fiber-local storage, one from deep in a recursion.
// Natively (gexc run) and on the page's shim (run-node.mjs), as a WoW64
// process of 64-bit Wine.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'fibers', { want: /^fibers ok/m });
