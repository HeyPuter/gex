// A publish merges damage the compositor has not drawn yet, and copies exactly
// the damaged pixels (fixtures/page/wl-publish.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'wl-publish.mjs'));
