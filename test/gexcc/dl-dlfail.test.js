// A dlopen that fails part way leaves the link map as it was: the next
// library takes its place, a symbol only the failed one defines is not found.
// In the browser runtime and natively.
// The check is fixtures/gexcc/dl/dlfail.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

export default (t) => script(t, 'dlfail', { need: ['ok dlfail (browser runtime', 'ok dlfail (native)'] });
