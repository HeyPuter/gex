// A guest's GL submit posts no wake-up message to a device that waits on the
// channel itself (fixtures/page/gl-wake.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'gl-wake.mjs'), { summary: true });
