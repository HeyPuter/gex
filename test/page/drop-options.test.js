// handle.drop()'s argument checks: what passes, and what each mistake says
// (fixtures/page/drop-options.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'drop-options.mjs'), { summary: true });
