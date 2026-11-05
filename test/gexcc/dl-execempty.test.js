// Empty strings among an exec's arguments and environment reach the new
// program, in place, through fork and posix_spawn.
// The check is fixtures/gexcc/dl/execempty.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'execempty', { need: ['ok execempty'] });
