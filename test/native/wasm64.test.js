// 64-bit Wine's LP64 programs checked against native execution: each is
// built for the host (x86-64, cc) and with wine/wcc64 (wasm64, lowered to
// wasm32, ELF64 container; GEX_WASM64_SYSROOT: toolchain/sysroot/build64.sh's),
// and its output and exit status under gexc must match. The sources are
// fixtures/native/wasm32's that do not fork (signals, threads with
// posix_spawn, dlopen's registry, setjmp, sockets) plus wasm64/lp64.c (the
// x86-64 kernel layouts). Then:
//   - wasm-elf.py's ELF32 (wasm32) containers did not change with --elf64's
//     addition: a fixed module wrapped as before (with and without
//     --guest-i386; the container carries its null and .shstrtab headers);
//   - --gex-guest-x86-64 (header flag 2): Windows' address space (winspace.c);
//   - more address space than wasm32 has (bigmap.c): refused, and not learned
//     as a program that needs a 64-bit memory (gexc's mem64 record).
import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { exec, native, tail, ROOT } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1200 };

const CONTAINERS = { '': 'f959d59e86161a7514e412f808eba4f99dbdc43832123c06d9e8c222bc8c9018', '--guest-i386': '4a375cb5b3fa0074e8efbe5a944dfa88a8e4dfae6454788e593e517f85068296' };

export default async function (t) {
  const sysroot = process.env.GEX_WASM64_SYSROOT || t.skip('GEX_WASM64_SYSROOT (toolchain/sysroot/build64.sh)');
  const n = native(t);
  const wcc64 = n.g.need(join(ROOT, 'wine/wcc64'), 'wine/wcc64');
  const d = n.dir('wasm64');
  const env = { ...n.env, GEX_WASM64_SYSROOT: sysroot };
  for (const [flag, want] of Object.entries(CONTAINERS)) {
    const out = join(d, `elf32${flag}`);
    const r = await exec(['python3', join(ROOT, 'toolchain/gexcc/wasm-elf.py'), n.src('wasm64/elf32-fixture.wasm'), out, ...(flag ? [flag] : [])], { env });
    t.eq(`elf32 container ${flag || '(plain)'}`, r.code === 0 && createHash('sha256').update(readFileSync(out)).digest('hex'), want, tail(r.stderr));
  }
  const wcc = async (name, src, more = []) => {
    const r = await exec([wcc64, '-O2', ...more, '-o', join(d, name), src], { cwd: d, env });
    return t.check(`${name}: builds with wcc64`, r.code === 0, tail(r.stderr)) ;
  };
  for (const src of [n.src('wasm64/lp64.c'), ...['signals', 'threads', 'dlopen', 'sjlj', 'inet'].map((s) => n.src(`wasm32/${s}.c`))]) {
    const name = src.split('/').pop().slice(0, -2);
    await n.hostcc(['-O1', '-o', join(d, `${name}.native`), src, '-lpthread']);
    if (!(await wcc(name, src))) continue;
    const h = await exec([`./${name}.native`], { cwd: d, env: n.env });
    const g = await n.gexc(['run', '--cwd', d, `./${name}`], { cwd: d, timeout: 120000 });
    t.check(`${name}: as natively (exit ${h.code})`, g.stdout === h.stdout && g.code === h.code, `native exit ${h.code}, gex exit ${g.code}; ${tail(g.stderr)}`);
  }
  if (await wcc('winspace', n.src('wasm64/winspace.c'), ['-Wl,--gex-guest-x86-64'])) {
    const g = await n.gexc(['run', '--cwd', d, './winspace'], { cwd: d, timeout: 120000 });
    t.eq('winspace (guest x86-64 flag)', g.stdout.replace(/\n+$/, ''), 'anonymous mapping below the USD: 1\nUSD and the page after it free: 1', tail(g.stderr));
  }
  if (await wcc('bigmap', n.src('wasm64/bigmap.c'))) {
    const xdg = join(d, 'xdg');
    const g = await n.gexc(['run', '--cwd', d, './bigmap'], { cwd: d, env: { XDG_CACHE_HOME: xdg }, timeout: 120000 });
    t.eq('bigmap: refused', g.stdout.replace(/\n+$/, ''), '6 GiB of mappings refused: 1', tail(g.stderr));
    t.check('bigmap: no 64-bit memory learned', !existsSync(join(xdg, 'gex/mem64')) && !g.stderr.includes('needs more than 4 GiB'), tail(g.stderr));
  }
  n.clean();
}
