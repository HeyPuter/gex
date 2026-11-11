// The rest bundle fetched in the background after the start-up: every frame once, at low priority, a read's
// own fetch first, none while paused (fixtures/page/lazy-rest-ahead.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 120 };

export default (t) => checks(t, join(FIXTURES, 'page', 'lazy-rest-ahead.mjs'));
