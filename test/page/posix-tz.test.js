// The page's time zone as a POSIX TZ string matches Intl at every hour of the
// year, for zones of every kind (fixtures/page/posix-tz.mjs). CPU-bound: no
// waits.

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'posix-tz.mjs'));
