// The GL device's WebGPU validation policy: errors counted and the first kept,
// the first few logged, then one line per interval
// (fixtures/page/gl-validation.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'gl-validation.mjs'));
