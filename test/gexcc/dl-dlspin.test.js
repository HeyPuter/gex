// A function pointer from a dlopen called by a thread that spins with no
// system call, reaching the kernel only at gexcc's loop polls.
// The check is fixtures/gexcc/dl/dlspin.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1200 };

export default (t) => script(t, 'dlspin');
