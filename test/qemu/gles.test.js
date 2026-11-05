// An unmodified OpenGL ES 2 program (fixtures/integration/gles-triangle.c),
// linked against libEGL and libGLESv2 by their usual sonames from the guest's
// GL (guest/gl/build.sh, aarch64), run under gexc --gl on the host's GL: a
// real context, its own GLSL compiled, a draw, and the pixels it drew read
// back. QEMU is no oracle here (no graphics device behind it): the pixels are
// the test; the version string is the host's and only has to be there.
import { cpSync, existsSync, mkdirSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { exec, ROOT } from '../lib/native.mjs';
import { PROGRAMS, qemu } from '../lib/qemu.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 300 };

export default async function (t) {
  const q = qemu(t);
  const { n } = q;
  for (const d of ['EGL', 'GLES2', 'KHR']) if (!existsSync(`/usr/include/${d}`)) t.skip(`no /usr/include/${d}`);
  const glout = join(q.dir, 'glout');
  const toolchain = dirname(dirname(n.gccPath));
  const b = await exec(['sh', join(ROOT, 'guest/gl/build.sh'), glout], { env: { ...n.env, GEX_TOOLCHAIN: toolchain } });
  if (b.code !== 0 || !existsSync(join(glout, 'libGLESv2.so.2'))) throw new Error(`guest/gl/build.sh: ${(b.stdout + b.stderr).trim().split('\n').slice(-3).join(' / ')}`);
  // The GL headers have no architecture in them: the host's, copied out so
  // its libc headers are not dragged into a cross build.
  const inc = join(q.dir, 'glinc');
  rmSync(inc, { recursive: true, force: true });
  for (const d of ['EGL', 'GLES2', 'KHR']) cpSync(`/usr/include/${d}`, join(inc, d), { recursive: true });
  const root = q.sysroot();
  mkdirSync(join(root, 'usr/lib'), { recursive: true });
  for (const f of ['libGLESv2.so.2', 'libEGL.so.1']) cpSync(join(glout, f), join(root, 'usr/lib', f));
  const prog = join(root, 'triangle');
  await n.cc(['-O2', '-o', prog, join(PROGRAMS, 'gles-triangle.c'), '-I', inc, '-L', glout, '-lEGL', '-lGLESv2']);
  const r = await n.gexc(['run', '--gl', '--sysroot', root, '--env', 'LD_LIBRARY_PATH=/usr/lib', prog], { cwd: q.dir });
  const version = /^GL_VERSION (.+)$/m.exec(r.stdout)?.[1];
  if (/^EGL /m.test(r.stdout) && !version) t.skip('the host has no GL');
  t.eq('gles-triangle: exit status', r.code, 0, r.stderr.trim().split('\n').slice(0, 3).join(' / '));
  const got = r.stdout.split('\n').filter((l) => /^(gl error|inside|outside)/.test(l)).join('|');
  t.eq(`gles-triangle: error, inside, outside (${version})`, got, 'gl error 0|inside  0 255 0|outside 0 0 255');
}
