// A terminal's polling read (non-canonical, VMIN 0, VTIME 0) in the browser
// runtime, where ptys live in the global kernel.
// The check is fixtures/gexcc/dl/ttypoll.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'ttypoll', { need: ['ok ttypoll (browser runtime)'] });
