// A Worker reads what a stream receives from its ring, in order, with no call
// to the page (fixtures/page/net-rings.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'net-rings.mjs'));
