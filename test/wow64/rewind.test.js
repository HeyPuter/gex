// rewind() from ucrtbase (rewind.c): ucrtbase's i386 rewind export,
// rewind_preserve_stack, was missing from the wasm32 build and trapped.
// Natively (gexc run) and on the page's shim (run-node.mjs), as a WoW64
// process of 64-bit Wine.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'rewind', { want: /^rewind ok/m });
