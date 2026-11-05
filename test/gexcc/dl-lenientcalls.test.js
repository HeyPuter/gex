// Indirect calls with more trailing arguments than the function takes, as
// GLib makes them, and the return-value mismatches beside them.
// The check is fixtures/gexcc/dl/lenientcalls.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'lenientcalls');
