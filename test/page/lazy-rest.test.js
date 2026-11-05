// A rest bundle's failing frame costs a bounded number of requests: tries
// with waits between, then a hold in which reads fail at once; readers of one
// frame share one request (fixtures/page/lazy-rest.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'lazy-rest.mjs'));
