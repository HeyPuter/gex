// Library constructors that dlopen runs: every library's, once, with two
// threads loading at once.
// The check is fixtures/gexcc/dl/dlctors.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'dlctors');
