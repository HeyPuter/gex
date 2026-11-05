// A rest bundle's frames are fetched once each, however a program's reads
// move between them: 17 reads over 8 frames, more than the owner keeps
// decoded, are 8 fetches (fixtures/page/rest-bundle-refetch.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'rest-bundle-refetch.mjs'));
