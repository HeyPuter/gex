// The page's cgen=0 switches every unit off, ELF objects' too
// (fixtures/page/cgen-off.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'cgen-off.mjs'));
