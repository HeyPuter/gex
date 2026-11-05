// One library unloaded and another loaded at the same address
// (fixtures/native/elf-units-remap: main.c, liba.so and libb.so from lib.c),
// with function-tier units for both (GEX_ELF_UNITS): each load must run its
// own code, and a library mapped again must get its unit again (a unit whose
// code was unmapped is retired, not revived by the next object's mapping).
// The interpreter has a unit too: gex's loader places it, and the kernel
// announces it as the guest's own mmap would have, and its unit is installed
// before ld.so's first instruction (its line before the first system call's
// under --strace). The program has one as well, at the base gex's loader
// gives it. On tier-1 and under the function JIT; then the two libraries
// built for i386 (the load bias of an ELFCLASS32 object found from its own
// program headers), said and left out without gcc -m32. Host glibc as the
// guest's.
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { exec, native, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1200 };

const BASE = '0x30000000';
const count = (text, re) => text.split('\n').filter((l) => re.test(l)).length;
const esc = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

export default async function (t) {
  const n = native(t);
  for (const bits of ['64', '32']) {
    const d = n.dir(`remap${bits}`);
    const m = bits === '32' ? ['-m32'] : [];
    try {
      await n.hostcc([...m, '-O1', '-shared', '-fPIC', '-DK=3', '-DC=1', '-o', join(d, 'liba.so'), n.src('elf-units-remap/lib.c')]);
    } catch (e) {
      if (bits === '32') { t.say(`i386: left out (gcc -m32: ${e.message.split('\n')[0]})`); continue; }
      throw e;
    }
    await n.hostcc([...m, '-O1', '-shared', '-fPIC', '-DK=7', '-DC=-2', '-o', join(d, 'libb.so'), n.src('elf-units-remap/lib.c')]);
    await n.hostcc([...m, '-O1', '-o', join(d, 'main'), n.src('elf-units-remap/main.c'), '-ldl']);
    // both at one pinned base, so the units built for it apply
    writeFileSync(join(d, 'pins.txt'), `${BASE} 0x10000 liba.so\n${BASE} 0x10000 libb.so\n`);
    const interp = bits === '64' ? execFileSync('readelf', ['-lW', join(d, 'main')], { encoding: 'utf8' }).match(/Requesting program interpreter: (.*)\]/)[1] : null;
    const units = bits === '64' ? `liba.so,libb.so,${interp.split('/').pop()},main` : 'liba.so,libb.so';
    const env = { GEX_LOG: '1', GEX_CGEN_CACHE: join(d, 'cgen'), GEX_ELF_UNITS: units, GEX_ELF_UNITS_BUILD: '0' };
    // the libraries at the pin; the program at gex's PIE base and the interpreter at its base
    const images = [[join(d, 'liba.so'), BASE], [join(d, 'libb.so'), BASE], ...(interp ? [[join(d, 'main'), '0x1000000'], [interp, '0x4a00000']] : [])];
    for (const [elf, base] of images) {
      const r = await n.gexc(['cgen-image', '--elf', elf, '--base', base, '--jobs', '2'], { env, timeout: 600000 });
      if (r.code !== 0) throw new Error(`cgen-image ${elf}: ${tail(r.stdout + r.stderr)}`);
    }
    const host = (await exec([join(d, 'main'), d])).stdout;
    const leg = bits === '32' ? 'i386 ' : '';
    for (const flags of [[], ['--fjit']]) {
      const tier = flags.length ? 'fjit' : 't1';
      const r = await n.gexc(['run', ...flags, '--pin', join(d, 'pins.txt'), join(d, 'main'), d], { env, timeout: 60000 });
      t.eq(`${leg}${tier}: as natively`, r.stdout, host, tail(r.stderr, 8));
      t.eq(`${leg}${tier}: library units placed`, count(r.stderr, /function tier for .*\/lib[ab]\.so/), 4);
      if (interp) {
        t.eq(`${tier}: the interpreter's unit placed`, count(r.stderr, new RegExp(`function tier for ${esc(interp)} at 0x4a00000`)), 1);
        t.eq(`${tier}: the program's unit placed`, count(r.stderr, new RegExp(`function tier for ${esc(join(d, 'main'))} at 0x1000000`)), 1);
      }
    }
    if (interp) {
      // ld.so's own start (relocating itself, up to its first system call)
      // runs on the unit, not on the runtime translator
      const s = await n.gexc(['run', '--strace', '--pin', join(d, 'pins.txt'), join(d, 'main'), d], { env, timeout: 60000 });
      const lines = s.stderr.split('\n');
      const unit = lines.findIndex((l) => l.includes(`function tier for ${interp} at 0x4a00000`));
      const sys = lines.findIndex((l) => l.startsWith('[gex:1] pc='));
      t.check(`the interpreter's unit before its first system call (unit line ${unit + 1}, first system call ${sys + 1})`, unit >= 0 && sys >= 0 && unit < sys, lines.slice(0, 6).join(' / '));
    }
  }
  n.clean();
}
