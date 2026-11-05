// AArch64 programs (fixtures/integration) under gexc and under QEMU, whose
// stdout and exit status must be byte-identical: QEMU is the oracle rather
// than a second gex build, so a bug shared by our decoder and lifter cannot
// make a test pass. a64 only.
//
//   const q = qemu(t);
//   const suite = await q.build('suite', 'suite.c', ['-static', '-no-pie', '-O2']);
//   await q.compare('static-arith', suite, ['arith']);
//   await q.expect('init', init, [], 'init: pid=1 ...\n');
//
// QEMU: GEX_QEMU (a qemu-aarch64), else ~/toolchains/qemu-aarch64, else the
// one on PATH; a skip when there is none. The compiler and gexc: native.mjs's.

import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { exec, native } from './native.mjs';

export const PROGRAMS = join(import.meta.dirname, '..', 'fixtures', 'integration');

/** The runtime's narration on stderr, never the guest's: what a compare of stderr leaves out. */
const NARRATION = /^(\[(gex|host|sig|worker|pending|gex-console-error)[^\]]*\]|gexc?: )/;
export const unnarrated = (text) => text.split('\n').filter((l) => !NARRATION.test(l)).join('\n');

function findQemu() {
  if (process.env.GEX_QEMU) return process.env.GEX_QEMU;
  const home = join(homedir(), 'toolchains/qemu-aarch64');
  if (existsSync(home)) return home;
  try {
    return execFileSync('sh', ['-c', 'command -v qemu-aarch64'], { encoding: 'utf8' }).trim() || null;
  } catch {
    return null;
  }
}

/** The first lines of `text`, indented, for a failed check's detail. */
const head = (text, n = 20) => text.split('\n').slice(0, n).join('\n');

/** A unified-ish diff of two outputs: the first lines that differ. */
function differ(want, got) {
  const a = want.split('\n');
  const b = got.split('\n');
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    if (a[i] !== b[i]) return `line ${i + 1}: qemu ${JSON.stringify(a[i] ?? null)}, gex ${JSON.stringify(b[i] ?? null)}`;
  }
  return '';
}

export function qemu(t) {
  const n = native(t);
  const bin = findQemu();
  if (!bin || !existsSync(bin)) t.skip('no qemu-aarch64 (GEX_QEMU)');
  const d = n.dir('qemu');
  const toolchain = dirname(dirname(n.gccPath));
  const q = {
    n,
    qemuPath: bin,
    dir: d,
    /** fixtures/integration/SRC built with the cross gcc and `flags`, as `name`. */
    async build(name, src, flags = []) {
      const out = join(d, name);
      await n.cc([...flags, '-o', out, join(PROGRAMS, src)]);
      return out;
    },
    /** The same, with the toolchain's g++. */
    async buildCxx(name, src, flags = []) {
      const out = join(d, name);
      const gxx = n.gccPath.replace(/gcc$/, 'g++');
      const r = await exec([gxx, ...flags, '-o', out, join(PROGRAMS, src)], { cwd: d, env: n.env });
      if (r.code !== 0) throw new Error(`g++ ${src}: ${r.stderr.trim().split('\n').slice(-5).join(' / ')}`);
      return out;
    },
    /** A sysroot with the toolchain's musl as the interpreter and its libstdc++ and libgcc_s: dynamic programs' `-L` and `--sysroot`. */
    sysroot() {
      const root = join(d, 'sysroot');
      mkdirSync(join(root, 'lib'), { recursive: true });
      // The interpreter in the toolchain is a symlink that resolves only inside
      // a sysroot: the real libc.so, under the interpreter's name.
      copyFileSync(join(toolchain, 'aarch64-linux-musl/lib/libc.so'), join(root, 'lib/ld-musl-aarch64.so.1'));
      for (const [from, to] of [['libstdc++.so.6.0.29', 'libstdc++.so.6'], ['libgcc_s.so.1', 'libgcc_s.so.1']]) {
        const src = join(toolchain, 'aarch64-linux-musl/lib', from);
        if (existsSync(src)) copyFileSync(src, join(root, 'lib', to));
      }
      return root;
    },
    /** QEMU's run of `prog args`: exec's result. */
    qemu: (prog, args = [], { flags = [], cwd = d, timeout } = {}) => exec([bin, ...flags, prog, ...args], { cwd, env: n.env, timeout }),
    /** gexc run's. */
    gex: (prog, args = [], { flags = [], cwd = d, timeout } = {}) => n.gexc(['run', ...flags, prog, ...args], { cwd, timeout }),
    /**
     * One check, `label`: gexc and QEMU give the same exit status and stdout
     * (and stderr, narration aside, with `stderr`). A failed comparison is
     * retried once; a pass the second time is a pass with the first failure
     * in its detail (flaky, not swallowed). `gexc`/`qemu`: extra flags (e.g.
     * --no-jit, --sysroot and -L). `oracle`: a host-native build of the same
     * program to compare against when QEMU itself dies with no output.
     */
    async compare(label, prog, args = [], { stderr = false, gexc = [], qemu: qflags = [], oracle = null } = {}) {
      const once = async () => {
        let want = await q.qemu(prog, args, { flags: qflags });
        let note = '';
        if (oracle && want.code >= 128 && !want.stdout) {
          want = await exec([oracle, ...args], { cwd: d, env: n.env });
          note = ' (qemu failed; against the native build)';
        }
        const got = await q.gex(prog, args, { flags: gexc });
        if (want.code !== got.code) return `exit status qemu=${want.code} gex=${got.code}${note}\n${head(got.stderr, 3)}`;
        if (want.stdout !== got.stdout) return `stdout differs${note}: ${differ(want.stdout, got.stdout)}`;
        if (stderr && unnarrated(want.stderr) !== unnarrated(got.stderr)) return `stderr differs (narration aside): ${differ(unnarrated(want.stderr), unnarrated(got.stderr))}`;
        return null;
      };
      const first = await once();
      if (!first) return t.check(label, true);
      const second = await once();
      if (!second) t.say(`flaky ${label}: passed on a retry; first: ${first}`);
      t.check(label, !second, second ?? '');
    },
    /** One check: gexc's run exits 0 with exactly `want` on stdout (where QEMU cannot be the oracle). */
    async expect(label, prog, args, want, { gexc = [] } = {}) {
      const got = await q.gex(prog, args, { flags: gexc });
      if (got.code !== 0) return t.check(label, false, `exit status ${got.code}\n${head(got.stderr, 3)}`);
      t.check(label, got.stdout === want, differ(want, got.stdout));
    },
    read: (p) => readFileSync(p, 'utf8'),
  };
  return q;
}
