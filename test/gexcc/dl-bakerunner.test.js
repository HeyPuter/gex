// Containers linked with GEXCC_BAKE_RUNNER empty, set, and unset: that only
// a given path is baked, and that each still runs on the build machine.
// The check is fixtures/gexcc/dl/bakerunner.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'bakerunner');
