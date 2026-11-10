// Debian's zlib, zstd and sqlite3 as LP64 wasm libraries replacing the arm64
// ones (toolchain/native-libs), in the page's runtime under Node: builds the
// three and the test programs zt and zstt, then runs every check
// (fixtures/native/native-libs/checks.sh) twice, each time in one guest
// process tree under a wasm32 shell: translated (GEX_WASM_DLLS=noauto) and with
// the libraries found by soname at /usr/lib/gex/aarch64. The checks include
// forks with open streams, custom allocators (guest code called from the
// library), gz* file I/O and zstd's worker threads; sqlite3 covers callbacks
// both ways, varargs and a file database. Every check's JSON must say ok in
// both, all three libraries must be replaced in the second run and only
// there, and the sqlite3 shell must print the same (bar .stats' memory lines).
//
// Env (each a loud skip when unset): GEX_TEST_NATIVE_LIBS_ROOT (a Debian arm64
// root with libz, libzstd, libsqlite3 and usr/bin/sqlite3), GEX_TEST_NATIVE_LIBS_SH
// (a wasm32 sh), GEXCC_PREFIX (the gexcc toolchain: wasm-opt), and the
// sources the build scripts take, ZLIB_SRC, ZSTD_SRC and SQLITE_SRC; A64_SYSROOT
// (the LP64 libc, toolchain/native-libs/sysroot.sh), else built here from
// MUSL_SRC and GEX_CRT_SRC.
import { existsSync, mkdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { exec, native, tail, count, ROOT } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 3600 };

const LIBS = ['libz.so.1', 'libzstd.so.1', 'libsqlite3.so.0'];

export default async function (t) {
  const env = process.env;
  const need = (k, what) => env[k] || t.skip(`${k}: ${what}`);
  const R = need('GEX_TEST_NATIVE_LIBS_ROOT', 'a Debian arm64 root with libz, libzstd, libsqlite3 and sqlite3');
  const SH = need('GEX_TEST_NATIVE_LIBS_SH', 'a wasm32 sh');
  need('GEXCC_PREFIX', 'the gexcc toolchain (wasm-opt)');
  for (const k of ['ZLIB_SRC', 'ZSTD_SRC', 'SQLITE_SRC']) need(k, 'the Debian source, unpacked');
  for (const f of [join(R, 'usr/bin/sqlite3'), join(R, 'usr/lib/aarch64-linux-gnu/libsqlite3.so.0'), SH]) if (!existsSync(f)) t.skip(`no ${f}`);
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('native-libs');
  const I3 = join(ROOT, 'toolchain/native-libs');
  const benv = { ...n.env, DEBIAN_ROOT: R };
  if (!env.A64_SYSROOT) {
    const s = await exec([join(I3, 'sysroot.sh'), join(d, 'a64libc')], { cwd: d, env: benv, timeout: 1200000 });
    if (!t.check('sysroot.sh', s.code === 0, tail(s.stdout + s.stderr))) return;
    benv.A64_SYSROOT = join(d, 'a64libc/sysroot');
  }
  const O = join(d, 'out');
  for (const b of ['build.sh', 'build-zstd.sh', 'build-sqlite.sh']) {
    const r = await exec([join(I3, b), O], { cwd: d, env: benv, timeout: 1800000 });
    if (!t.check(b, r.code === 0, tail(r.stdout + r.stderr))) return;
  }
  // and no library names a path of this machine's (an assert's __FILE__)
  for (const l of LIBS) {
    const bytes = readFileSync(join(O, `${l}.wasm`)).toString('latin1');
    const at = [d, env.ZLIB_SRC, env.ZSTD_SRC, env.SQLITE_SRC].filter(Boolean).find((p) => bytes.includes(p));
    t.check(`${l}: names no build path`, !at, at);
  }
  mkdirSync(join(d, 'tmp'));
  const fx = (f) => n.src(`native-libs/${f}`);
  const run = (mode) => n.g.run(seed, SH, ['/t/checks.sh'], {
    argv0: '/bin/sh', merge: true, timeout: 900000,
    flags: ['--dir', `/=${R}`, '--dir', `/lib=${join(R, 'usr/lib')}`, '--dir', `/tmp=${join(d, 'tmp')}`],
    files: {
      '/bin/sh': SH, '/t/checks.sh': fx('checks.sh'), '/t/sq.sql': fx('sq.sql'), '/t/cb.sql': fx('cb.sql'),
      '/t/zt': join(O, 'zt'), '/t/zstt': join(O, 'zstt'),
      ...Object.fromEntries(LIBS.map((l) => [`/usr/lib/gex/aarch64/${l}.wasm`, join(O, `${l}.wasm`)])),
    },
    // (the lines checked, "replaced by", are debug output)
    env: { GEX_SHARED: '1', GEX_LOG: '1', ...(mode === 'translated' ? { GEX_WASM_DLLS: 'noauto' } : {}) },
  });
  const out = {};
  for (const mode of ['translated', 'wasm']) {
    const r = await run(mode);
    const all = r.all;
    out[mode] = all;
    const json = all.split('\n').filter((l) => l.startsWith('{'));
    const good = json.filter((l) => l.includes('"ok":true')).length;
    const bad = json.filter((l) => l.includes('"ok":false')).length;
    const ran = count(all, /^== rc /);
    const nonzero = count(all, /^== rc (?!0$)/);
    t.check(`${mode}: ${good} checks ok, ${bad} not, ${ran} programs, ${nonzero} nonzero exits`, bad === 0 && good >= 16 && ran === 7 && nonzero === 0, `exit ${r.code}: ${tail(all, 6)}`);
    for (const l of LIBS) {
      const replaced = all.includes(`${l} replaced by`);
      t.check(`${mode}: ${l} ${mode === 'wasm' ? 'replaced' : 'not replaced (noauto)'}`, replaced === (mode === 'wasm'));
    }
  }
  const sq = (text) => {
    const at = text.indexOf('== sqlite3 sq.sql');
    return at < 0 ? '' : text.slice(at).split('\n').filter((l) => !/^(\[|gex: |gex-pe: |gex-nt: |Memory Used|Number of Outstanding|Largest Allocation)/.test(l)).join('\n');
  };
  const [a, b] = [sq(out.translated), sq(out.wasm)];
  t.check('the sqlite3 shell prints the same in both', a !== '' && a === b, a.split('\n').find((l, i) => l !== b.split('\n')[i]) ?? '');
  n.clean();
}
