// Detours-style hooks on user32 exports, then calls to the untouched exports
// beside them (hookpage.c), as IE8 does at start-up. 32-bit Wine's user32
// exports were `ud2` stubs bound to its wasm, and tier-1, translating a
// caller's `call [iat]` after the hook writes retired the page, lifted the
// stub of an export still bound (RegisterClassW: c000001d). Under WoW64
// user32 is a whole x86 PE, its exports not packed beside RegisterClassW's,
// so no hook need share its page: the hooks and the calls after them must
// still work.
// Natively (gexc run) and on the page's shim (run-node.mjs), as a WoW64
// process of 64-bit Wine.
import { legs } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default (t) => legs(t, 'hookpage', { want: /^hookpage ok [0-9]/m });
