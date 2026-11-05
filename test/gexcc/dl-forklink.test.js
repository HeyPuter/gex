// A fork's child takes its parent's wasm32 link instead of binding every
// library again, and still dlopens, dlsyms, forks and execs; forks while
// another thread dlopens. In the browser runtime and natively.
// The check is fixtures/gexcc/dl/forklink.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1800 };

export default (t) => script(t, 'forklink', { need: ['ok forklink (browser runtime', 'ok forklink (native)'] });
