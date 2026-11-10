// gex's EGL (guest/gl/gexgl.c) leaves eglGetError a code when a call fails,
// and EGL_SUCCESS once it is read. A caller that asks why after a failure
// (Rust's khronos-egl unwraps it) must get one; here eglInitialize fails
// because the run has no /dev/gex-gl.

import { join } from 'node:path';
import { FIXTURES, gexcc } from '../lib/gexcc.mjs';

const ROOT = join(import.meta.dirname, '..', '..');

export const meta = { hosts: ['node'], timeout: 600 };

export default async function (t) {
  const c = gexcc(t);
  const d = c.dir('eglerror');
  await c.build(['-O2', '-o', `${d}/eglerror`, join(FIXTURES, 'eglerror', 'eglerror.c'), join(ROOT, 'guest/gl/gexgl.c'), '-ldl']);
  const r = await c.exec([`${d}/eglerror`], { cwd: d });
  t.eq('exit', r.code, 0);
  t.eq('output', r.out.split('\n').filter((l) => !l.startsWith('gexgl:')).join('\n').trim(), [
    'before 0x3000',
    'initialize 0 0x3001 0x3000',
    'bind 0 0x300c',
    'make-current 0 0x3001',
  ].join('\n'));
}
