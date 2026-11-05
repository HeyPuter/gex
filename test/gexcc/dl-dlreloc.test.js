// A library's function pointers in data used by the thread that dlopened
// it while other threads instantiate the library and apply its relocations.
// The check is fixtures/gexcc/dl/dlreloc.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'dlreloc');
