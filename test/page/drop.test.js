// Files dropped on the page land in the guest's filesystem, tree, bytes and
// modes, and an aborted drop takes back what it wrote (fixtures/page/drop.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'drop.mjs'), { summary: true, need: ['page/public/gex/gex_kernel.x86.wasm'] });
