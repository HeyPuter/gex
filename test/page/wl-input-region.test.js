// A surface with an empty input region lets the pointer through
// (fixtures/page/wl-input-region.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'wl-input-region.mjs'), { summary: true });
