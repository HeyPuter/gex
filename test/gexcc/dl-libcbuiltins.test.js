// gexcc-libcbuiltins: libc.so exports the compiler-rt builtins a library
// takes from it (128-bit integers, long double). The check is
// fixtures/gexcc/dl/libcbuiltins.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

export default async (t) => {
  await script(t, 'libcbuiltins');
};
