// A resize of the page's output reaches Wayland clients that are already
// connected: the page's half, the browser shim under node (MP and SP) and the
// native shim (fixtures/page/output-resize.mjs, its client
// fixtures/programs/wl-output-resize.c). Needs gcc, the page's x86 shared
// build and the x86 gexc (GEXC_X86, GEXC, then target/x86/release).

import { join } from 'node:path';
import { checks, FIXTURES } from '../lib/checks.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

const ROOT = join(import.meta.dirname, '..', '..');

export default (t) => checks(t, join(FIXTURES, 'page', 'output-resize.mjs'), {
  timeout: 540000,
  env: { GEXC: process.env.GEXC_X86 || process.env.GEXC || join(ROOT, 'target/x86/release/gexc'), WORK: join(t.workDir, 'tmp') },
});
