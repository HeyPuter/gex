// A spare thread Worker lets go after five seconds with no thread to take.
// The check is fixtures/gexcc/dl/spareidle.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'spareidle');
