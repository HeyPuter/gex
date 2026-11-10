// Mesa's files are fetched once each when the GL starts, the packed ones from
// the pack and never as plain files (fixtures/page/mesa-source.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 120 };

export default (t) => checks(t, join(FIXTURES, 'page', 'mesa-source.mjs'));
