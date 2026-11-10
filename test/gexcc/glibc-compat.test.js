// What programs written on glibc and built as Debian packages get from
// gexcc beside musl: glibc's header names and functions (fixtures/gexcc/
// glibc/compat.c), libc++ given libstdc++'s leniencies (cxx.cc), -lcrypt
// linking a libcrypt of its own, GNU ld's -R run paths, gexcc-objdump on a
// program's container, the CMake toolchain finding the build machine's
// CMake packages in a Debian build, and the pkg-config wrapper's target
// paths (GEXCC_PKG_CONFIG_RUNTIME).

import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { FIXTURES, gexcc } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 900 };

const F = join(FIXTURES, 'glibc');

export default async function (t) {
  const c = gexcc(t);
  const bin = c.env.PATH.split(':')[0];
  const prefix = dirname(bin);

  // glibc's names
  const d = c.dir('compat');
  mkdirSync(join(d, 'tree', 'sub'), { recursive: true });
  writeFileSync(join(d, 'tree', 'a'), 'a');
  writeFileSync(join(d, 'tree', 'sub', 'b'), 'b');
  writeFileSync(join(d, 'passwd'), 'alice:x:1001:1001::/home/alice:/bin/sh\n');
  writeFileSync(join(d, 'group'), 'staff:x:50:alice\n');
  writeFileSync(join(d, 'shadow'), 'alice:$6$salt$hash:19000:0:99999:7:::\n');
  await c.build(['-O1', '-o', `${d}/compat`, join(F, 'compat.c')]);
  const r = await c.exec([`${d}/compat`, d], { cwd: d });
  t.eq('glibc names', r.out, ['basename [b] []', 'ino 4 8', 'wtmpx 1', 'fts 2', 'fgetpwent alice 1001',
    'fgetgrent staff 50', 'fgetspent_r 0 alice 99999', 'sgetspent_r 0 bob 90'].join('\n'));
  await c.build(['-O1', '-o', `${d}/posix`, join(F, 'posix-basename.c')]);
  t.eq('<libgen.h> basename wins', (await c.exec([`${d}/posix`], { cwd: d })).out, 'posix [b]');

  // libc++
  await c.build(['-O1', '-std=c++17', '-o', `${d}/cxx`, join(F, 'cxx.cc')], { cxx: true });
  t.eq('libc++ as libstdc++ allows', (await c.exec([`${d}/cxx`], { cwd: d })).out, 'bytes 3 0 deque 7 narrow 42');

  // -lcrypt: a libcrypt outside libc's directory is linked
  const xc = join(d, 'xc');
  mkdirSync(xc);
  await c.build(['-O1', '-c', '-o', `${xc}/xcrypt.o`, join(F, 'xcrypt.c')]);
  execFileSync(join(bin, 'wasm32-unknown-linux-musl-ar'), ['crs', `${xc}/libcrypt.a`, `${xc}/xcrypt.o`]);
  await c.build(['-O1', '-o', `${d}/crypt`, join(F, 'crypt.c'), `-L${xc}`, '-lcrypt']);
  t.eq('-lcrypt links its own libcrypt', (await c.exec([`${d}/crypt`], { cwd: d })).out, 'gensalt $y$');

  // -R DIR, and a program's NEEDED read from its container
  await c.build(['-O1', '-fPIC', '-shared', '-o', `${d}/libl.so`, join(F, 'lib.c'), '-Wl,-R,/opt/r1', '-Wl,-R/opt/r2', '-Wl,-soname,libl.so'], { shared: true });
  const lib = await c.run([join(bin, 'gexcc-objdump'), '-p', `${d}/libl.so`]);
  t.check('-R DIR is a run path', /RUNPATH\s+\/opt\/r1:\/opt\/r2/.test(lib.out), lib.out);
  await c.build(['-O1', '-o', `${d}/uselib`, join(F, 'uselib.c'), `-L${d}`, '-ll', '-Wl,-rpath,$ORIGIN'], { shared: true });
  const prog = await c.run([join(bin, 'gexcc-objdump'), '-p', `${d}/uselib`]);
  t.check("a program's NEEDED", /NEEDED\s+libl\.so/.test(prog.out), prog.out);
  t.eq('and it runs', (await c.exec([`${d}/uselib`], { cwd: d })).out, 'lib 42');

  // CMake: the build machine's packages in a Debian package build only
  const cmake = (env) => execFileSync('cmake', ['-S', join(F, 'cmake'), '-B', join(d, `cm-${Object.keys(env).length}`),
    `-DCMAKE_TOOLCHAIN_FILE=${prefix}/share/gexcc/toolchain.cmake`, `-DCMAKE_PREFIX_PATH=${join(F, 'cmake')}`],
  { encoding: 'utf8', env: { ...c.env, ...env } });
  let have = true;
  try { execFileSync('cmake', ['--version']); } catch { have = false; }
  if (have) {
    t.check('a target build: the sysroot\'s packages only', /FooPkg_FOUND=\s*$|FooPkg_FOUND=0|FooPkg_FOUND=FALSE/m.test(cmake({})));
    t.check('a Debian build: the build machine\'s too', /FooPkg_FOUND=(1|TRUE)/.test(cmake({ DEB_HOST_ARCH: 'musl-linux-wasm32' })));
  }

  // pkg-config: the wrapper over a sysroot of the test's own
  const sr = join(d, 'sr');
  mkdirSync(join(sr, 'usr/lib/pkgconfig'), { recursive: true });
  mkdirSync(join(sr, 'usr/share/rt'), { recursive: true });
  writeFileSync(join(sr, 'usr/lib/pkgconfig/rt.pc'), 'xdir=/usr/share/rt\ninstdir=/usr/lib/rt-install\nName: rt\nDescription: t\nVersion: 1\n');
  const wrap = join(d, 'pkg-config');
  writeFileSync(wrap, readFileSync(join(bin, 'wasm32-unknown-linux-musl-pkg-config'), 'utf8').split(`${prefix}/sysroot`).join(sr), { mode: 0o755 });
  const pc = (args, env = {}) => execFileSync(wrap, args, { encoding: 'utf8', env: { ...c.env, ...env } }).trim();
  t.eq('a directory the sysroot has: the sysroot\'s', pc(['--variable=xdir', 'rt']), `${sr}/usr/share/rt`);
  t.eq('one it has not: the target\'s', pc(['--variable=instdir', 'rt']), '/usr/lib/rt-install');
  t.eq('GEXCC_PKG_CONFIG_RUNTIME: the target\'s', pc(['--variable=xdir', 'rt'], { GEXCC_PKG_CONFIG_RUNTIME: 'other rt' }), '/usr/share/rt');
  c.clean();
}
