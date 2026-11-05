// Wine's USER and LOGNAME are gex unless the caller names them
// (fixtures/page/wine-user-env.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'wine-user-env.mjs'));
