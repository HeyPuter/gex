// Threads started one after another on a Worker whose last thread ended:
// none of a thread's state reaches the next.
// The check is fixtures/gexcc/dl/threadpark.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'threadpark');
