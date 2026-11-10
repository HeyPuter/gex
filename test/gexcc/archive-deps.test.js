// A static library under a shared name (gexcc's -shared without
// GEXCC_SHARED, as a package's loadable modules are built) records what it
// was linked with, so a program linking it gets those too. The record names
// no path of the build machine: the sysroot's are `=`, the rest relative to
// the archive. A program links it where it was built, and again once the
// library and the archive it needs are installed somewhere else.

import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync, readFileSync, renameSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { FIXTURES, gexcc } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

const F = join(FIXTURES, 'archdeps');

export default async function (t) {
  const c = gexcc(t);
  const bin = c.env.PATH.split(':')[0];
  const sysroot = join(dirname(bin), 'sysroot');
  const d = c.dir('archdeps');
  const build = join(d, 'build');
  mkdirSync(join(build, 'mods'), { recursive: true });
  await c.build(['-O1', '-c', '-o', `${build}/base.o`, join(F, 'base.c')]);
  execFileSync(join(bin, 'wasm32-unknown-linux-musl-ar'), ['crs', `${build}/libbase.a`, `${build}/base.o`]);
  await c.build(['-O1', '-shared', '-o', `${build}/mods/libmod.so`, join(F, 'mod.c'), `${build}/libbase.a`,
    `-L${sysroot}/usr/lib`, '-lm']);
  const rec = readFileSync(`${build}/mods/libmod.so`, 'latin1');
  const record = rec.slice(rec.indexOf('GEXCC-DEPS'), rec.indexOf('GEXCC-END'));
  t.check('the record names no build directory', !record.includes(d), record);
  t.check('nor the sysroot', !record.includes(sysroot), record);
  await c.build(['-O1', '-o', `${d}/here`, join(F, 'main.c'), `${build}/mods/libmod.so`]);
  t.eq('linked where it was built', (await c.exec([`${d}/here`], { cwd: d })).out, 'mod 42');
  const inst = join(d, 'inst', 'lib');
  mkdirSync(join(inst, 'modules'), { recursive: true });
  copyFileSync(`${build}/mods/libmod.so`, `${inst}/modules/libmod.so`);
  copyFileSync(`${build}/libbase.a`, `${inst}/libbase.a`);
  // (the build tree gone, as on the machine the package is installed on)
  renameSync(build, `${build}.gone`);
  await c.build(['-O1', '-o', `${d}/there`, join(F, 'main.c'), `${inst}/modules/libmod.so`, `-L${inst}`]);
  t.eq('and where it was installed', (await c.exec([`${d}/there`], { cwd: d })).out, 'mod 42');
}
