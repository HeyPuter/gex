// A thread a spare Worker takes keeps no slot promised: four hundred threads
// one after another, more than a process has slots.
// The check is fixtures/gexcc/dl/sparepromise.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'sparepromise');
