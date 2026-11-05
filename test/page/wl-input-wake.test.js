// Page input reaches a process whose every thread is parked, well before the
// park's bound (fixtures/page/wl-input-wake.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'wl-input-wake.mjs'), { summary: true, need: ['page/public/gex/gex_kernel.wasm'] });
