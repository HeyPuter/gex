// gexcc-rpath: a module's run path rewritten in place, natively and in the
// browser runtime (BROWSER). The check is fixtures/gexcc/dl/rpath.sh.

import { join } from 'node:path';
import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default async (t) => {
  await script(t, 'rpath');
  await script(t, 'rpath', { env: { BROWSER: join(import.meta.dirname, '..', '..') } });
};
