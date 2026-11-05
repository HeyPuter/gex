// A GL batch's reply against the ring's reply area: what fits goes back
// whole, more is cut to the area, never refused (fixtures/page/gl-reply.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'gl-reply.mjs'));
