// The page's audio worklet starting a stream: the 6 ms start, and a stalled
// short buffer plays (fixtures/page/audio-worklet.mjs).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default (t) => checks(t, join(FIXTURES, 'page', 'audio-worklet.mjs'));
