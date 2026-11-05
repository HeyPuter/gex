// An exec from a threaded wasm32 program linked against shared libraries:
// the new program's threads must not bring the old program back.
// The check is fixtures/gexcc/dl/execdata.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'execdata');
