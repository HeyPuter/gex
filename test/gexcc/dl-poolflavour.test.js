// A Worker given back to the pool keeps its modules, and the next process
// must not take one that only fit the last: wasm32 after AArch64 programs.
// Needs zig and A64_ROOTFS. AArch64's shim only: an AArch64 program is
// one of the two flavours.
// The check is fixtures/gexcc/dl/poolflavour.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 1200 };

export default (t) => script(t, 'poolflavour', { need: ['ok poolflavour'] });
