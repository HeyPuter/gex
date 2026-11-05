// -fgex-no-fork: a link that never forks. fork fails with ENOSYS,
// posix_spawn still starts a child, and no module carries asyncify.
// The check is fixtures/gexcc/dl/nofork.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'nofork');
