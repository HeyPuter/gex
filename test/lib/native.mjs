// What a node-host test needs to build a Linux program with gcc (a musl
// cross compiler, as the kernel's own tests are built) and run it under this
// arch's gexc, and on the page's shim through guest.mjs. Each piece missing
// (gexc, the compiler) is a skip.
//
//   const n = native(t);
//   const d = n.dir('fault');
//   await n.cc(['-O1', '-static', '-o', `${d}/fault`, n.src('branch-fault/fault.c'), '-lpthread']);
//   const r = await n.gexc(['run', `${d}/fault`]);
//   t.eq('native', r.stdout.trim(), 'handled');
//   const p = await n.g.run(n.seed(), `${d}/fault`, [], { files: { [`${d}/fault`]: `${d}/fault` }, env: { GEX_SHARED: '1' } });
//
// The compiler: GEX_TOOLCHAIN (an aarch64-linux-musl-cross tree, for a64) or
// GEX_TOOLCHAIN_X86 (x86_64-linux-musl-cross), by default under ~/toolchains;
// `hostcc` is the host's (HOSTCC, else cc).
// gexc: guest.mjs's (GEXC_A64 or GEXC_X86, GEXC, then target/release and
// target/x86/release).

import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, rmSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { guest, ROOT } from './guest.mjs';

export { ROOT };
export { count, tail } from './guest.mjs';
export const FIXTURES = join(import.meta.dirname, '..', 'fixtures', 'native');

/**
 * `argv` as a process: `{ code, signal, stdout, stderr, timedOut }`, `code`
 * as a shell gives it (128 + the signal's number for a signal). `cwd`,
 * `env` (the whole environment), `timeout` (ms, then SIGKILL).
 */
export function exec(argv, { cwd = ROOT, env = process.env, timeout = 300000 } = {}) {
  return new Promise((resolve) => {
    const p = spawn(argv[0], argv.slice(1), { cwd, env, stdio: ['ignore', 'pipe', 'pipe'] });
    const out = [];
    const err = [];
    p.stdout.on('data', (d) => out.push(d));
    p.stderr.on('data', (d) => err.push(d));
    const timer = setTimeout(() => p.kill('SIGKILL'), timeout);
    p.on('error', (e) => { clearTimeout(timer); resolve({ code: 127, signal: null, stdout: '', stderr: String(e.message) }); });
    p.on('close', (code, signal) => {
      clearTimeout(timer);
      const sig = { SIGABRT: 6, SIGKILL: 9, SIGSEGV: 11, SIGTERM: 15 }[signal];
      resolve({ code: code ?? 128 + (sig ?? 0), signal, timedOut: signal === 'SIGKILL',
        stdout: Buffer.concat(out).toString(), stderr: Buffer.concat(err).toString() });
    });
  });
}

/** `text` without its trailing newlines, as `$(...)` gives it. */
export const trim = (text) => text.replace(/\n+$/, '');

export function native(t) {
  const x86 = t.arch === 'x86';
  const g = guest(t);
  const { NODE_CHANNEL_FD, NODE_CHANNEL_SERIALIZATION_MODE, ...base } = process.env;
  const env = { ...base, GEX_LOG: base.GEX_LOG || 'off' };
  const gexcPath = env[x86 ? 'GEXC_X86' : 'GEXC_A64'] || env.GEXC || join(ROOT, x86 ? 'target/x86/release/gexc' : 'target/release/gexc');
  const tc = (x86 ? env.GEX_TOOLCHAIN_X86 : env.GEX_TOOLCHAIN) || join(homedir(), 'toolchains', x86 ? 'x86_64-linux-musl-cross' : 'aarch64-linux-musl-cross');
  const triple = x86 ? 'x86_64-linux-musl' : 'aarch64-linux-musl';
  const gcc = join(tc, 'bin', `${triple}-gcc`);
  const top = join(t.workDir, 'native', `${t.arch}-${process.pid}`);
  rmSync(top, { recursive: true, force: true });
  mkdirSync(top, { recursive: true });

  const build = async (what, argv, cwd) => {
    const r = await exec(argv, { cwd, env });
    if (r.code !== 0) throw new Error(`${what} ${argv.slice(1).join(' ')}: ${r.stderr.trim().split('\n').slice(-5).join(' / ')}`);
  };
  return {
    g,
    gexcPath,
    env,
    gccPath: gcc,
    /** The toolchain's shared libc, as the guest's dynamic loader: run's `files`. */
    ldso: { [`/lib/ld-musl-${x86 ? 'x86_64' : 'aarch64'}.so.1`]: join(tc, triple, 'lib', 'libc.so') },
    /** fixtures/native/PATH. */
    src: (path) => join(FIXTURES, path),
    /** A fresh work directory of this test's, `name`. */
    dir(name) {
      const d = join(top, name);
      rmSync(d, { recursive: true, force: true });
      mkdirSync(d, { recursive: true });
      return d;
    },
    /** The cross gcc with `args` (a skip if it is not there); throws its output if it fails. */
    cc(args, { cwd = top } = {}) {
      if (!existsSync(gcc)) t.skip(`no ${gcc} (GEX_TOOLCHAIN${x86 ? '_X86' : ''})`);
      return build('gcc', [gcc, ...args], cwd);
    },
    /** The host's cc with `args`. */
    hostcc: (args, { cwd = top } = {}) => build('cc', [env.HOSTCC || g.tool('cc'), ...args], cwd),
    /** gexc with `args` (a skip if it is not built); `env` on top of this one's. */
    gexc(args, { env: more = {}, ...o } = {}) {
      if (!existsSync(gexcPath)) t.skip(`${gexcPath} is not built`);
      return exec([gexcPath, ...args], { cwd: top, ...o, env: { ...env, ...g.env, ...more } });
    },
    /** The page's shared seed for this arch, a skip if it is not built. */
    seed: () => g.pub('seed{infix}.shared.wasm'),
    /**
     * A function-tier unit for each of `libs` (`{ name: host path }`, a
     * library the program loads): run `probe` (a run with GEX_ELF_UNITS
     * naming them, resolving its stderr) once to find where the loader maps
     * each, then `gexc cgen-image` for that base into `cache` (the runs'
     * GEX_CGEN_CACHE). Throws where one never mapped.
     */
    async units(libs, cache, probe) {
      const err = await probe();
      for (const [name, path] of Object.entries(libs)) {
        const m = new RegExp(`${name.replace(/[.]/g, '\\.')} at (0x[0-9a-f]+)`).exec(err);
        if (!m) throw new Error(`${name} never mapped: ${err.trim().split('\n').slice(-5).join(' / ')}`);
        const r = await this.gexc(['cgen-image', '--elf', path, '--base', m[1], '--jobs', '2'], { env: { GEX_CGEN_GUARD: '1', GEX_CGEN_CACHE: cache } });
        if (r.code !== 0) throw new Error(`cgen-image ${name}: ${(r.stdout + r.stderr).trim().split('\n').slice(-5).join(' / ')}`);
      }
    },
    /** This test's work directories gone (at its end). */
    clean: () => rmSync(top, { recursive: true, force: true }),
  };
}
