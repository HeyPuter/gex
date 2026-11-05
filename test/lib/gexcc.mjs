// What a node-host test needs to build programs with gexcc and run them
// natively under this arch's gexc, beside the host's own build of the same
// source. Each piece missing (gexcc, gexc, the host's compiler) is a skip.
//
//   const c = gexcc(t);
//   const dir = c.dir('dltls');                    // a work directory of the test's own
//   await c.build(['-O2', '-o', `${dir}/m`, src]);  // gexcc (gexc++ with { cxx: true })
//   const r = await c.exec([`${dir}/m`], { cwd: dir });
//   t.eq('output', r.out, 'ok');
//
// GEXCC: the gexcc to use (default the one on PATH); its directory goes first
// on PATH, so gexc++, gexcc-exec and the rest are its install's. A gexcc
// program is an ELF launcher that execs gexcc-exec, which runs it under
// GEXCC_GEXC: set here to this arch's gexc (GEXC_X86, GEXC_A64, GEXC, then
// target/x86/release and target/release).

import { execFileSync, spawn } from 'node:child_process';
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';

const ROOT = join(import.meta.dirname, '..', '..');
export const FIXTURES = join(import.meta.dirname, '..', 'fixtures', 'gexcc');

const which = (cmd, path = process.env.PATH) => {
  try {
    return execFileSync('sh', ['-c', 'command -v "$1"', 'sh', cmd], { encoding: 'utf8', env: { ...process.env, PATH: path } }).trim() || null;
  } catch {
    return null;
  }
};

let runs = 0;

export function gexcc(t) {
  const x86 = t.arch === 'x86';
  const cc = process.env.GEXCC || which('gexcc') || t.skip('no gexcc (GEXCC, or on PATH: toolchain/gexcc/install.sh)');
  if (!existsSync(cc)) t.skip(`GEXCC=${cc} is not there`);
  const bin = dirname(cc);
  const gexc = process.env[x86 ? 'GEXC_X86' : 'GEXC_A64'] || process.env.GEXC || join(ROOT, x86 ? 'target/x86/release/gexc' : 'target/release/gexc');
  if (!existsSync(gexc)) t.skip(`${gexc} is not built`);
  const sysroot = execFileSync(cc, ['--print-sysroot'], { encoding: 'utf8' }).trim();
  // Not this process's IPC channel to the runner (fork's NODE_CHANNEL_FD).
  const { NODE_CHANNEL_FD, NODE_CHANNEL_SERIALIZATION_MODE, ...base } = process.env;
  // Output is compared with the host's: not even gex's default lines.
  const env = { ...base, PATH: `${bin}:${base.PATH}`, GEXCC_GEXC: gexc, GEX_LOG: base.GEX_LOG || 'off' };
  const top = join(t.workDir, 'gexcc', `${t.arch}-${process.pid}`);
  rmSync(top, { recursive: true, force: true });
  mkdirSync(top, { recursive: true });

  /**
   * `argv` as a process: `{ code, out }`, `code` as a shell gives it (128 +
   * the signal's number for a signal) and `out` its stdout and stderr in the
   * order written (`2>&1`; `stderr: false` drops stderr), trailing newlines
   * dropped (`$(...)`). `cwd`, `env` (on top of this one's), `timeout` (ms,
   * then SIGKILL).
   */
  const run = (argv, { cwd = top, env: more = {}, timeout = 300000, stderr = true } = {}) => new Promise((resolve) => {
    const file = join(top, `.run-${++runs}.out`);
    const fd = openSync(file, 'w');
    const p = spawn(argv[0], argv.slice(1), { cwd, env: { ...env, ...more }, stdio: ['ignore', fd, stderr ? fd : 'ignore'] });
    closeSync(fd);
    const timer = setTimeout(() => p.kill('SIGKILL'), timeout);
    const done = (code, signal) => {
      clearTimeout(timer);
      const out = readFileSync(file, 'utf8').replace(/\n+$/, '');
      rmSync(file, { force: true });
      const sig = { SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5, SIGABRT: 6, SIGBUS: 7, SIGFPE: 8, SIGKILL: 9, SIGSEGV: 11, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15 }[signal];
      resolve({ code: code ?? 128 + (sig ?? 0), out, timedOut: signal === 'SIGKILL' });
    };
    p.on('error', (e) => { clearTimeout(timer); resolve({ code: 127, out: String(e.message) }); });
    p.on('close', done);
  });

  const c = {
    gexc,
    sysroot,
    env,
    run,
    /** A fresh work directory of this test's, `name`. */
    dir(name) {
      const d = join(top, name);
      rmSync(d, { recursive: true, force: true });
      mkdirSync(d, { recursive: true });
      return d;
    },
    /** gexcc (gexc++ with `cxx`) with `args`; `shared`: GEXCC_SHARED=1. Throws its output if it fails. */
    async build(args, { cxx = false, shared = false, cwd = top } = {}) {
      const r = await run([join(bin, cxx ? 'gexc++' : 'gexcc'), ...args], { cwd, env: shared ? { GEXCC_SHARED: '1' } : {} });
      if (r.code !== 0) throw new Error(`${cxx ? 'gexc++' : 'gexcc'} ${args.join(' ')}: ${r.out.split('\n').slice(-5).join(' / ')}`);
    },
    /** The host's compiler (cc, c++ with `cxx`), or a skip. */
    async hostBuild(args, { cxx = false, cwd = top } = {}) {
      const tool = which(cxx ? 'c++' : 'cc') ?? t.skip(`no ${cxx ? 'c++' : 'cc'} on this host`);
      const r = await run([tool, ...args], { cwd });
      if (r.code !== 0) throw new Error(`${cxx ? 'c++' : 'cc'} ${args.join(' ')}: ${r.out.split('\n').slice(-5).join(' / ')}`);
    },
    /** `argv` under gexcc-exec (a gexcc program, or a shared one by path). */
    exec: (argv, opts = {}) => run([join(bin, 'gexcc-exec'), ...argv], opts),
    /** A shared libc's directory, for LD_LIBRARY_PATH. */
    libdir: join(sysroot, 'lib'),
    /** This test's work directories gone (at its end). */
    clean: () => rmSync(top, { recursive: true, force: true }),
  };
  return c;
}

/** `fn(item)` over `items`, `width` at once (GEX_TEST_INNER, default 4,
 *  inside each of a run's --jobs). */
export async function pool(items, fn, width = Number(process.env.GEX_TEST_INNER) || 4) {
  const out = new Array(items.length);
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(width, items.length) }, async () => {
    while (next < items.length) {
      const i = next++;
      out[i] = await fn(items[i], i);
    }
  }));
  return out;
}

/**
 * fixtures/gexcc/dl/NAME.sh, a dynamic linking check written as a shell
 * script, as a test: run with a work directory of its own (on the disk the
 * work directory is on: gex cannot read libraries from a tmpfs), gexcc on
 * PATH, GEX_ROOT this tree and GEX_ARCH this arch, plus `env`. Each of its
 * `ok ...` and `FAIL ...` lines is a check, and so is its exit status; a
 * `skip ...` line with exit 0 and nothing else is a skip, and after a check
 * a failed one (a leg not run). `need`: `ok` lines that must be there (the
 * leg a script may skip, its browser runtime one).
 */
export async function script(t, name, { env = {}, timeout = 1200000, need = [] } = {}) {
  const c = gexcc(t);
  const d = c.dir(name);
  const r = await c.run(['sh', join(FIXTURES, 'dl', `${name}.sh`), d], { cwd: d, timeout,
    env: { GEX_ROOT: ROOT, GEX_ARCH: t.arch === 'x86' ? 'x86' : 'aarch64', GEXC: c.gexc, ...env } });
  const lines = r.out.split('\n');
  const oks = [];
  let checks = 0;
  for (const l of lines) {
    const m = /^(ok|FAIL)\s+(.*)$/.exec(l);
    if (m) {
      t.check(m[2] || name, m[1] === 'ok');
      if (m[1] === 'ok') oks.push(l);
      checks++;
    } else if (/^skip\b/.test(l)) {
      if (r.code === 0 && !checks && !need.length) t.skip(l.replace(/^skip:?\s*/, ''));
      t.check(l, false, 'a leg not run');
    }
  }
  for (const n of need) t.check(`${n}`, oks.some((l) => l.startsWith(n)), 'not printed');
  t.check(`${name}.sh exits 0`, r.code === 0, `exit ${r.code}${r.timedOut ? ' (timed out)' : ''}: ${lines.slice(-6).join(' / ')}`);
  c.clean();
}
