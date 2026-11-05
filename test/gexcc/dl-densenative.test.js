// A native wasm32 program and its fork child number blocks densely, and
// each process's tables grow only as it needs.
// The check is fixtures/gexcc/dl/densenative.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'densenative');
