// More shared libraries than libc's registry once held (256): 300 a
// program starts with and 20 it dlopens, natively and in the browser
// runtime. The check is fixtures/gexcc/dl/manylibs.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 2400 };

export default (t) => script(t, 'manylibs', { timeout: 2400000, need: ['ok   manylibs browser'] });
