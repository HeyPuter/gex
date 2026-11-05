// gexcc programs against the host: each fixtures/gexcc/programs/NAME.c
// (NAME.cpp) built with cc (c++) and with gexcc (gexc++), and run three
// ways: natively, natively under this arch's gexc, and on the page's shim
// (hosts/browser/js/run-node.mjs, the page's shared build under Node). The
// output and the exit status must be the host's on each.
//
// NAME.sh, when there is one, runs the program (given its path) in place of
// running it directly: host FIFOs, host stdin. Those are native only, as is
// any `system()` call without BUSYBOX (a gexcc busybox for the guest's
// /bin/sh). A source with `gexcc-test: as root` runs as root on both sides,
// in a user namespace with root and 65535 other ids mapped.

import { existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { FIXTURES, gexcc, pool } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1800 };

const DIR = join(FIXTURES, 'programs');
const ROOT = join(import.meta.dirname, '..', '..');

/** The guest's lines of run-node.mjs's stdout, less the runtime's own. */
const guestOf = (out) => out.split('\n').filter((l) => !/^\[host\]|^\[gex\] |^\[worker\] |^\[gex-console/.test(l)).join('\n').replace(/\n+$/, '');

export default async function (t) {
  const c = gexcc(t);
  const x86 = t.arch === 'x86';
  const seed = join(ROOT, 'page/public/gex', `seed${x86 ? '.x86' : ''}.shared.wasm`);
  const page = existsSync(seed) && existsSync(join(ROOT, `page/public/gex/gex_browser${x86 ? '.x86' : ''}.shared.wasm`));
  if (!page) t.say(`no ${seed} (make assets): the page legs are not run`);
  const busybox = process.env.BUSYBOX && existsSync(process.env.BUSYBOX) ? process.env.BUSYBOX : null;
  const srcs = readdirSync(DIR).filter((f) => /\.(c|cpp)$/.test(f)).sort();
  const asRoot = ['unshare', '--map-auto', '--map-root-user'];

  await pool(srcs, async (f) => {
    const cxx = f.endsWith('.cpp');
    const name = f.replace(/\.(c|cpp)$/, '');
    const src = join(DIR, f);
    const code = readFileSync(src, 'utf8');
    const d = c.dir(name);
    try {
      await c.hostBuild(['-O1', '-w', '-o', `${d}/${name}.native`, src, '-lpthread'], { cxx });
      // function pointer cast emulation is static-only
      await c.build(['-O2', '-w', ...(name.startsWith('fpcast') ? ['-fgex-fpcast-emu'] : []), '-o', `${d}/${name}`, src], { cxx });
    } catch (e) {
      t.check(`${name}: builds`, false, e.message);
      return;
    }
    const root = code.includes('gexcc-test: as root') ? asRoot : [];
    const driver = join(DIR, `${name}.sh`);
    const go = (prog) => c.run(existsSync(driver) ? ['sh', driver, prog] : [...root, prog], { cwd: d });
    const native = await go(`./${name}.native`);
    const gex = await go(`./${name}`);
    t.check(`${name}`, native.out === gex.out && native.code === gex.code,
      native.code !== gex.code ? `native exit ${native.code}, gex exit ${gex.code}: ${gex.out.split('\n').slice(-3).join(' / ')}` : diff(native.out, gex.out));
    if (!page || existsSync(driver)) return;
    const bb = code.includes('system(') ? busybox : undefined;
    if (code.includes('system(') && !bb) return t.say(`${name} (page): needs BUSYBOX, not run`);
    const web = await c.run([...root, process.execPath, join(ROOT, 'hosts/browser/js/run-node.mjs'), ...(bb ? ['--busybox', bb] : []),
      '--file', `/t/${name}=${d}/${name}`, '--cwd', '/t', '--argv0', `./${name}`, seed, `${d}/${name}`],
    { cwd: d, stderr: false, env: { GEX_ARCH: x86 ? 'x86' : 'aarch64', GEX_SHARED: '1' } });
    const out = guestOf(web.out);
    t.check(`${name} (page)`, native.out === out && native.code === web.code,
      native.code !== web.code ? `native exit ${native.code}, page exit ${web.code}` : diff(native.out, out));
  });

  // A program linked against libc.so starts (gex's loader fills libc's
  // registry of shared libraries, which a musl without it does not have), in
  // C and in C++ with a throw caught.
  const d = c.dir('shared');
  writeFileSync(join(d, 'sh.c'), '#include <stdio.h>\nint main(void){puts("shared c");return 0;}\n');
  writeFileSync(join(d, 'shx.cpp'), '#include <cstdio>\nint main(){try{throw 7;}catch(int e){std::printf("shared c++ %d\\n",e);}return 0;}\n');
  for (const [src, out, want, cxx] of [['sh.c', 'sh', 'shared c', false], ['shx.cpp', 'shx', 'shared c++ 7', true]]) {
    try {
      await c.build(['-O2', '-o', out, src], { cxx, shared: true, cwd: d });
      const r = await c.run([`./${out}`], { cwd: d, env: { LD_LIBRARY_PATH: c.libdir } });
      t.eq(`shared-${out}`, r.out, want);
    } catch (e) {
      t.check(`shared-${out}: builds`, false, e.message);
    }
  }
  c.clean();
}

/** The first lines where `a` and `b` differ, for a FAIL's detail. */
function diff(a, b) {
  if (a === b) return '';
  const x = a.split('\n');
  const y = b.split('\n');
  const i = x.findIndex((l, k) => l !== y[k]);
  const k = i < 0 ? x.length : i;
  return `line ${k + 1}: host ${JSON.stringify(x[k] ?? '(end)')}, gex ${JSON.stringify(y[k] ?? '(end)')}`;
}
