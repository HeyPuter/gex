// What a node-host test needs to run a guest program on the page runtime's
// shim directly (hosts/browser/js/run-node.mjs), below the runtime API: the
// program built, a seed, and a run. Each piece missing (gexc, the shim, a
// compiler) is a skip.
//
//   const g = guest(t);
//   const prog = g.cc('order.c');            // fixtures/programs/order.c
//   const r = await g.run(g.seed(), prog, ['20'], { env: { GEX_SHARED: '1' } });
//   t.eq('output', r.guest, 'hi\n');

import { execFileSync, spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, realpathSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join, basename } from 'node:path';

export const ROOT = join(import.meta.dirname, '..', '..');
const PROGRAMS = join(import.meta.dirname, '..', 'fixtures', 'programs');
const PUB = join(ROOT, 'page/public/gex');

const which = (cmd) => {
  try {
    return execFileSync('sh', ['-c', 'command -v "$1"', 'sh', cmd], { encoding: 'utf8' }).trim() || null;
  } catch {
    return null;
  }
};

/** The guest's lines of a run's stdout: without the runtime's own
 *  (`[gex:1] ...`, `[host] ...`), which share it and can land inside a
 *  guest's line, being another thread's console. */
export function guestLines(out) {
  return out.split('\n').map((l) => l.replace(/\[gex[^\]]*\] .*/, '').replace(/^\[.*/, '')).filter(Boolean).join('\n');
}

let runs = 0;

export function guest(t) {
  const arch = t.arch === 'x86' ? 'x86' : 'a64';
  const x86 = arch === 'x86';
  const infix = x86 ? '.x86' : '';
  const cache = join(t.workDir, 'guest');
  mkdirSync(cache, { recursive: true });
  const need = (path, what = path) => {
    if (!existsSync(path)) t.skip(`${what} is not built`);
    return path;
  };
  const tool = (cmd) => which(cmd) ?? t.skip(`no ${cmd}`);
  const gexc = need(process.env[x86 ? 'GEXC_X86' : 'GEXC_A64'] || process.env.GEXC || join(ROOT, x86 ? 'target/x86/release/gexc' : 'target/release/gexc'), 'gexc');
  /** `cmd(out)` once per `key`, its output kept in the work directory. */
  const cached = (key, name, cmd) => {
    const out = join(cache, `${createHash('sha256').update(key).digest('hex').slice(0, 16)}-${name}`);
    if (!existsSync(out)) cmd(out);
    return out;
  };
  const g = {
    arch,
    /** The environment run-node.mjs takes this arch from. */
    env: x86 ? { GEX_ARCH: 'x86' } : {},
    need,
    tool,
    /** A file of the page's shims (`page/public/gex/NAME`, `{infix}` this
     *  arch's infix: '' or '.x86'), or a skip. */
    pub: (name) => need(join(PUB, name.replace('{infix}', infix))),
    /**
     * `src` (fixtures/programs/NAME, or `{ name, code }`) built for this
     * arch: by default static musl with zig, at the address the page's
     * programs load (`-Wl,--image-base=0x1000000`). `glibc`: dynamic,
     * against the host's glibc (gcc, x86 only) or zig's glibc 2.36 (a64).
     * `flags`: more for the compiler, `libs` after the source; `imageBase: false` leaves a musl
     * program where the linker puts it; `opt` the optimisation flag.
     */
    cc(src, { glibc = false, flags = [], libs = [], imageBase = true, opt = glibc ? '-O1' : '-O2' } = {}) {
      const { name, code } = typeof src === 'string' ? { name: basename(src, '.c'), code: readFileSync(join(PROGRAMS, src), 'utf8') } : src;
      let cmd;
      if (glibc && x86) cmd = [tool('gcc'), opt];
      else if (glibc) cmd = [tool('zig'), 'cc', '-target', 'aarch64-linux-gnu.2.36', opt];
      else cmd = [tool('zig'), 'cc', '-target', `${x86 ? 'x86_64' : 'aarch64'}-linux-musl`, opt, '-static', ...(imageBase ? ['-Wl,--image-base=0x1000000'] : [])];
      const all = [...cmd, ...flags];
      return cached(`${all.join(' ')} ${libs.join(' ')}\n${code}`, name, (out) => {
        const c = `${out}.c`;
        writeFileSync(c, code);
        execFileSync(all[0], [...all.slice(1), '-o', out, c, ...libs], { stdio: ['ignore', 'ignore', 'pipe'] });
      });
    },
    /**
     * The dynamic loader and libc a glibc program (`cc(..., { glibc: true })`)
     * runs against, as run's `files`: the host's on x86-64, and on AArch64
     * those of A64_ROOTFS (an arm64 Debian root, a .tar.zst), taken out once.
     */
    glibc() {
      if (x86) {
        const ld = '/lib64/ld-linux-x86-64.so.2';
        const libc = '/lib/x86_64-linux-gnu/libc.so.6';
        if (!existsSync(ld) || !existsSync(libc)) t.skip('no glibc on this host');
        return { [ld]: realpathSync(ld), [libc]: realpathSync(libc) };
      }
      const rootfs = process.env.A64_ROOTFS;
      if (!rootfs || !existsSync(rootfs)) t.skip('A64_ROOTFS: an arm64 Debian root (.tar.zst)');
      const dir = cached(`${rootfs}:${statSync(rootfs).mtimeMs}`, 'a64root', (out) => {
        mkdirSync(out, { recursive: true });
        execFileSync('sh', ['-c', 'zstd -dc "$1" | tar -x -C "$2" ./usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 ./usr/lib/aarch64-linux-gnu/libc.so.6', 'sh', rootfs, out]);
      });
      const lib = join(dir, 'usr/lib/aarch64-linux-gnu');
      return { '/lib/ld-linux-aarch64.so.1': join(lib, 'ld-linux-aarch64.so.1'), '/lib/aarch64-linux-gnu/libc.so.6': join(lib, 'libc.so.6') };
    },
    /** A seed from this arch's gexc (`gexc seed`); `shared: false` for the
     *  single-Worker shim's. */
    seed({ shared = true } = {}) {
      return cached(`${readFileSync(gexc).length}:${shared}`, shared ? 'seed.wasm' : 'seed.plain.wasm', (out) => {
        execFileSync(gexc, ['seed', ...(shared ? [] : ['--no-shared-memory']), '-o', out], { stdio: 'ignore' });
      });
    },
    /**
     * run-node.mjs on `seed` and `prog` with `args`: `files` (`{ guest path:
     * host path }`), `argv0`, `env` (on top of this one's), `timeout` (ms)
     * and `flags` (run-node.mjs's own, before the seed); `cwd` (this tree by
     * default) and `node` (node's own flags). `single`: the
     * single-thread build, as the page without SharedArrayBuffer runs it
     * (GEX_SINGLE=1, JSPI, no SharedArrayBuffer); `jspi` JSPI alone, where
     * this node has it only behind a flag. Resolves `{ code, signal,
     * stdout, stderr, all, guest }`, `guest` being `guestLines(stdout)`.
     */
    run(seed, prog, args = [], { files = {}, argv0, env = {}, timeout = 120000, flags = [], single = false, merge = false, jspi = single, cwd = ROOT, node = [] } = {}) {
      const argv = [
        ...node,
        ...(jspi && typeof WebAssembly.Suspending !== 'function' ? ['--experimental-wasm-jspi'] : []),
        ...(single ? ['--import', 'data:text/javascript,delete globalThis.SharedArrayBuffer'] : []),
        join(ROOT, 'hosts/browser/js/run-node.mjs'), ...flags,
      ];
      if (single) env = { ...env, GEX_SINGLE: '1' };
      for (const [to, from] of Object.entries(files)) argv.push('--file', `${to}=${from}`);
      if (argv0) argv.push('--argv0', argv0);
      argv.push(seed, prog, ...args.map(String));
      return new Promise((resolve) => {
        // Not this process's IPC channel to the runner (fork's NODE_CHANNEL_FD).
        const { NODE_CHANNEL_FD, NODE_CHANNEL_SERIALIZATION_MODE, ...base } = process.env;
        // `merge`: stdout and stderr into one file, in the order written, as
        // `2>&1` keeps them (`all`; stdout and stderr then empty).
        const file = merge ? join(cache, `run-${process.pid}-${++runs}.out`) : null;
        const fd = file ? openSync(file, 'w') : null;
        const p = spawn(process.execPath, argv, { cwd, env: { ...base, ...g.env, ...env }, stdio: ['ignore', fd ?? 'pipe', fd ?? 'pipe'] });
        if (fd !== null) closeSync(fd);
        const out = [];
        const err = [];
        p.stdout?.on('data', (d) => out.push(d));
        p.stderr?.on('data', (d) => err.push(d));
        const timer = setTimeout(() => p.kill('SIGKILL'), timeout);
        p.on('close', (code, signal) => {
          clearTimeout(timer);
          const stdout = Buffer.concat(out).toString();
          const all = file ? readFileSync(file, 'utf8') : null;
          if (file) rmSync(file, { force: true });
          resolve({ code, signal, stdout, stderr: Buffer.concat(err).toString(), all, guest: guestLines(all ?? stdout) });
        });
      });
    },
  };
  return g;
}

/** The last `n` lines of `text`, for a FAIL's detail. */
export const tail = (text, n = 5) => text.trim().split('\n').slice(-n).join(' / ');

/**
 * The page's own legs of `g`'s arch, as the page's shared builds run them:
 * on x86-64 the mem64 seed (`seed.x86.m64.shared.wasm`, GEX_MEM64=1) and, with
 * `sp`, the single-thread build on the plain one; on AArch64 the shared seed.
 * Each `{ label, seed, env, single }`, the seed a skip if not built.
 */
export function pageLegs(g, { sp = false } = {}) {
  if (g.arch === 'a64') return [{ label: 'a64', seed: g.pub('seed.shared.wasm'), env: { GEX_SHARED: '1' }, single: false }];
  const legs = [{ label: 'x86', seed: g.pub('seed.x86.m64.shared.wasm'), env: { GEX_SHARED: '1', GEX_MEM64: '1' }, single: false }];
  if (sp) legs.push({ label: 'sp', seed: g.pub('seed.x86.shared.wasm'), env: { GEX_SHARED: '1' }, single: true });
  return legs;
}

/** How many lines of `text` match `re`. */
export const count = (text, re) => text.split('\n').filter((l) => re.test(l)).length;
