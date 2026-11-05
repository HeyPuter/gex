// A program's threads loading libraries at once, each Worker copying the
// others' libraries while the shim gives them up.
// The check is fixtures/gexcc/dl/librace.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'librace');
