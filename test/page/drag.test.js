// The page's drop of host files on a guest window: the events, the guest's
// answer and the drag's uri-list (fixtures/page/drag.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'drag.mjs'));
