// Every switch reaches a Worker as the page or the environment gave it
// (fixtures/page/options.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'options.mjs'), { summary: true });
