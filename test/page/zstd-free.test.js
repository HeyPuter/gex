// The page's zstd decoder is let go when nothing decodes, and the next user
// gets a working new one (fixtures/page/zstd-free.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'zstd-free.mjs'), { node: ['--expose-gc'] });
