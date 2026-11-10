// gexcc-rlibctors: a Rust rlib's constructors (inventory's registrations)
// are linked; a C archive keeps the archive rule. The check is
// fixtures/gexcc/dl/rlibctors.sh.

import { script } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

export default async (t) => {
  await script(t, 'rlibctors');
};
