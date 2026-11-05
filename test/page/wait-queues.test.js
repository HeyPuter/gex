// The kernel's wait queues wake exactly the waiters registered on them, at
// once (fixtures/page/wait-queues.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'wait-queues.mjs'), { summary: true, need: ['page/public/gex/gex_kernel.wasm'] });
