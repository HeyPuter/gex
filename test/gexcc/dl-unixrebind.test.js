// A unix socket's name is its file's: unlinked it can be bound again, renamed
// it moves with the file, natively and in the browser runtime.
// The check is fixtures/gexcc/dl/unixrebind.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'unixrebind', { need: ['ok unixrebind (browser runtime)'] });
