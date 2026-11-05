// /proc's processor time, uptime, load and memory, measured natively and in
// the browser runtime.
// The check is fixtures/gexcc/dl/procusage.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'procusage', { need: ['ok procusage (browser runtime)'] });
