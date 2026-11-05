// Thread-local data in shared libraries, per thread and across a fork.
// The check is fixtures/gexcc/dl/dltls.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'dltls');
