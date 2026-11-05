// A site's rest bundle has one zstd decoder, in the page's lazy-file owner,
// and none in any Worker; none for a site without one
// (fixtures/page/rest-bundle-decoders.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'rest-bundle-decoders.mjs'));
