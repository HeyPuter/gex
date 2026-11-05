// An append to the module store gives up when the engine will not grow it,
// and lands when a sibling won the grow (fixtures/page/module-store-oom.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'module-store-oom.mjs'));
